use super::super::inspect::{blob_at, current_head, hash_object, upstream_parts};
use super::super::push::push;
use super::super::test_support::*;
use super::*;
use std::fs;
use std::io::Write as _;
use std::process::Stdio;

/// The guard must not refuse when a clean filter normalises the staged
/// content markcheck loaded.
#[test]
fn a_matching_staged_checklist_is_not_refused_under_a_content_filter() {
    let work = init_repo_with_crlf_filter();
    let head_blob = blob_at(&work, &current_head(&work).unwrap(), "tracked.md");

    let loaded = "- [x] one\r\n";
    fs::write(work.join("tracked.md"), loaded).unwrap();
    run(&work, &["add", "tracked.md"]);

    assert_ne!(
        index_blob(&work, "tracked.md"),
        head_blob,
        "test setup: something must actually be staged, or the guard \
             returns early and proves nothing"
    );
    assert!(
        !staged_target_would_be_lost(&work, "tracked.md", head_blob.as_deref(), Some(loaded)),
        "staged content identical to what markcheck loaded loses nothing"
    );

    // The guard still fires when the index holds something else.
    let only_in_index = "- [x] one\r\n- [ ] staged only\r\n";
    fs::write(work.join("tracked.md"), only_in_index).unwrap();
    run(&work, &["add", "tracked.md"]);
    fs::write(work.join("tracked.md"), loaded).unwrap();
    assert!(
        staged_target_would_be_lost(&work, "tracked.md", head_blob.as_deref(), Some(loaded)),
        "a staged snapshot the working tree does not have must still refuse"
    );
}
#[test]
fn verify_commit_scope_checks_the_captured_commit_not_whatever_head_is_now() {
    // A commit landing after the one being verified must be ignored.
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let created_commit = current_head(&work).unwrap();

    fs::write(work.join("other.md"), "concurrent\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "concurrent change"]);
    let concurrent_commit = current_head(&work).unwrap();

    let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        current_head(&work),
        Some(concurrent_commit),
        "must never touch the concurrent commit sitting on top"
    );
}
#[test]
fn verify_commit_scope_names_the_stray_commit_when_the_undo_itself_fails() {
    // An unrelated commit on top makes the compare-and-swap undo refuse.
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    fs::write(work.join("other.md"), "hook-added\n").unwrap();
    run(&work, &["add", "tracked.md", "other.md"]);
    run(&work, &["commit", "-q", "-m", "checklist plus hook spill"]);
    let created_commit = current_head(&work).unwrap();

    fs::write(work.join("third.md"), "concurrent\n").unwrap();
    run(&work, &["add", "third.md"]);
    run(&work, &["commit", "-q", "-m", "concurrent change"]);
    let concurrent_commit = current_head(&work).unwrap();

    let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);

    let err = result.expect_err("an out-of-scope commit must be reported");
    assert!(
        err.contains("undoing the commit failed"),
        "must say the rollback itself failed: {err}"
    );
    assert!(
        err.contains(&created_commit),
        "must name the stray commit for manual cleanup: {err}"
    );
    assert_eq!(
        current_head(&work),
        Some(concurrent_commit),
        "nothing may be rewound when the undo refuses"
    );
}
#[test]
fn undo_commit_refuses_to_rewind_a_commit_that_landed_after_the_one_being_undone() {
    let work = init_repo_without_remote();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "to be undone"]);
    let created_commit = current_head(&work).unwrap();

    fs::write(work.join("other.md"), "concurrent\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "concurrent change"]);
    let concurrent_commit = current_head(&work).unwrap();

    let result = undo_commit(&work, &created_commit);
    // Requiring `update-ref` in the message pins that the CAS refused.
    let message = result.expect_err("the undo must refuse");
    assert!(
        message.contains("update-ref"),
        "must fail at the compare-and-swap, not before reaching it: {message}"
    );
    assert_eq!(
        current_head(&work),
        Some(concurrent_commit),
        "the concurrent commit must not be rewound"
    );
}
#[test]
fn undo_commit_refuses_to_delete_a_root_commit_that_already_has_a_child() {
    let work = unique_dir("repo-root-commit-cas").join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "root commit to be undone"]);
    let created_commit = current_head(&work).unwrap();

    fs::write(work.join("other.md"), "concurrent\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "concurrent change"]);
    let concurrent_commit = current_head(&work).unwrap();

    let result = undo_commit(&work, &created_commit);
    let message = result.expect_err("the undo must refuse");
    assert!(
        message.contains("update-ref"),
        "must fail at the compare-and-swap, not before reaching it: {message}"
    );
    assert_eq!(
        current_head(&work),
        Some(concurrent_commit),
        "the branch ref must not be deleted out from under the concurrent commit"
    );
}
#[test]
fn range_has_unrelated_commits_reads_a_whole_batched_range_correctly() {
    // Mixes checklist-only commits, a multi-file commit and a root commit.
    let work = init_repo_without_remote();

    let root = current_head(&work).unwrap();
    for n in 1..=5 {
        fs::write(work.join("tracked.md"), format!("- [x] {n}\n")).unwrap();
        run(&work, &["commit", "-q", "-am", &format!("check {n}")]);
    }
    let checklist_only_tip = current_head(&work).unwrap();
    assert_eq!(
        range_has_unrelated_commits(&work, Some(&root), &checklist_only_tip, "tracked.md"),
        Ok(false),
        "a long run of checklist-only commits is not unrelated work"
    );

    assert_eq!(
        range_has_unrelated_commits(&work, None, &checklist_only_tip, "tracked.md"),
        Ok(false),
        "the root commit must be diffed, and it only touched the checklist"
    );

    fs::write(work.join("tracked.md"), "- [x] six\n").unwrap();
    fs::write(work.join("other.md"), "unrelated\n").unwrap();
    run(&work, &["add", "tracked.md", "other.md"]);
    run(&work, &["commit", "-q", "-m", "two files at once"]);
    let mixed_tip = current_head(&work).unwrap();
    assert_eq!(
        range_has_unrelated_commits(&work, Some(&root), &mixed_tip, "tracked.md"),
        Ok(true),
        "a multi-file commit anywhere in the batch must be caught"
    );

    assert_eq!(
        range_has_unrelated_commits(&work, Some(&mixed_tip), &mixed_tip, "tracked.md"),
        Ok(false),
        "an empty range has nothing to diff"
    );
}
#[test]
fn verify_commit_scope_catches_a_revert_cancelled_unrelated_change() {
    // A second nested commit reverting the first's unrelated change.
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "checklist change"]);

    fs::write(work.join("other.md"), "unrelated\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "unrelated nested commit"]);

    run(&work, &["rm", "-q", "other.md"]);
    run(
        &work,
        &["commit", "-q", "-m", "revert unrelated nested commit"],
    );
    let created_commit = current_head(&work).unwrap();

    let result = verify_commit_scope(&work, &Some(parent.clone()), "tracked.md", &created_commit);
    assert!(
        result.is_err_and(|e| e.contains("changed files other than the checklist")),
        "the revert-cancelled unrelated commit must still be caught"
    );
    // Only markcheck's own commit is rewound; the hook's nested commit
    // survives.
    let nested = git_stdout(&work, &["rev-parse", &format!("{created_commit}^")]);
    assert_eq!(
        current_head(&work).unwrap(),
        nested,
        "exactly one commit — markcheck's own — is rewound"
    );
    assert_ne!(
        current_head(&work).unwrap(),
        parent,
        "the commits markcheck did not create are deliberately left in place"
    );
}
#[test]
fn repo_sync_blocked_is_none_for_a_clean_repo() {
    let work = init_repo_without_remote();
    assert_eq!(repo_sync_blocked(&work), None);
}
#[test]
fn repo_sync_blocked_detects_a_merge_in_progress() {
    let work = init_repo_without_remote();
    fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason.as_ref().is_some_and(|r| r.contains("merge")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_detects_a_rebase_in_progress() {
    let work = init_repo_without_remote();
    fs::create_dir_all(work.join(".git").join("rebase-merge")).unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason.as_ref().is_some_and(|r| r.contains("rebase")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_detects_detached_head() {
    let work = init_repo_without_remote();
    let head = current_head(&work).unwrap();
    run(&work, &["checkout", "-q", &head]);
    let reason = repo_sync_blocked(&work);
    assert!(
        reason.as_ref().is_some_and(|r| r.contains("detached")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_detects_a_real_merge_conflict() {
    let work = init_repo_without_remote();
    run(&work, &["checkout", "-q", "-b", "branch-a"]);
    fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
    run(&work, &["commit", "-q", "-am", "a"]);
    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
    run(&work, &["commit", "-q", "-am", "b"]);
    let _ = crate::test_support::git()
        .current_dir(&work)
        .args(["merge", "-q", "branch-a"])
        .output();

    assert!(repo_sync_blocked(&work).is_some());
}
#[test]
fn repo_sync_blocked_detects_unmerged_entries_without_a_merge_marker() {
    // Unmerged entries can exist without MERGE_HEAD, and on a path other than
    // the checklist.
    let work = init_repo_without_remote();
    fs::write(work.join("other.md"), "base\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "add other.md"]);

    let base = hash_object(&work, "tracked.md", "base\n").unwrap();
    let ours = hash_object(&work, "tracked.md", "ours\n").unwrap();
    let theirs = hash_object(&work, "tracked.md", "theirs\n").unwrap();
    let index_info = format!(
        "100644 {base} 1\tother.md\n100644 {ours} 2\tother.md\n100644 {theirs} 3\tother.md\n"
    );
    let mut child = crate::test_support::git()
        .current_dir(&work)
        .args(["update-index", "--index-info"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(index_info.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!work.join(".git").join("MERGE_HEAD").exists());

    let reason = repo_sync_blocked(&work);
    assert!(
        reason.as_ref().is_some_and(|r| r.contains("conflict")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_detects_a_multi_commit_cherry_pick_after_a_plain_commit() {
    // `git commit` instead of `--continue` removes CHERRY_PICK_HEAD but
    // leaves the sequence in progress.
    let work = init_repo_without_remote();
    fs::create_dir_all(work.join(".git").join("sequencer")).unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("cherry-pick or revert is in progress")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_refuses_when_the_git_dir_cannot_be_found() {
    let dir = unique_dir("no-git-dir");
    fs::create_dir_all(&dir).unwrap();
    let reason = repo_sync_blocked(&dir);
    assert!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("could not locate the repository's git directory")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_refuses_when_the_conflict_check_fails() {
    let work = init_repo_without_remote();
    fs::write(work.join(".git").join("index"), "not an index").unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("could not check for unresolved merge conflicts")),
        "{reason:?}"
    );
}
#[test]
fn undo_commit_rewinds_one_commit_and_keeps_the_branch() {
    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "to be undone"]);
    let created = current_head(&work).unwrap();

    undo_commit(&work, &created).expect("undo should succeed");

    assert_eq!(current_head(&work).as_deref(), Some(parent.as_str()));
    assert!(
        current_branch_ref(&work).is_some(),
        "the branch ref must still exist, not be deleted"
    );
}
#[test]
fn a_tipless_branch_has_no_unrelated_unpushed_commits() {
    let work = init_repo_without_remote();
    assert_eq!(
        unpushed_history(&work, None, "x.md", Publishable::OwnOrThisChecklist),
        Ok(UnpushedHistory::Safe(0))
    );
}
#[test]
fn staged_target_guard_fails_closed_without_a_pre_write_hash() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [ ] one\n- [ ] staged\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    let head = git_stdout(&work, &["rev-parse", "HEAD:tracked.md"]);

    assert!(
        staged_target_would_be_lost(&work, "tracked.md", Some(&head), None),
        "no pre-write hash means the staged version cannot be cleared as safe"
    );
    assert!(!staged_target_would_be_lost(
        &work,
        "other-untouched.md",
        None,
        None
    ));
}

#[test]
fn an_unanswerable_history_check_refuses_instead_of_permitting_a_push() {
    let (work, _remote) = init_repo_with_remote();
    let bogus = "0".repeat(40);

    assert!(
        unpushed_history(
            &work,
            Some(&bogus),
            "tracked.md",
            Publishable::OwnOrThisChecklist
        )
        .is_err(),
        "an unresolvable range must surface as an error, not a verdict"
    );
    let refusal = unpushed_history_blocks(&work, Some(&bogus), "tracked.md");
    assert!(
        matches!(&refusal, Some(SyncOutcome::Failed(msg)) if msg.contains("could not check")),
        "the caller must refuse to publish: {refusal:?}"
    );
}

#[test]
fn a_head_that_moves_after_the_tip_is_captured_cannot_broaden_the_push() {
    // The validated SHA and the pushed SHA must be the same commit, even when
    // `HEAD` moves in between.
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "checklist"]);
    let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

    assert_eq!(
        unpushed_history(
            &work,
            Some(&tip),
            "tracked.md",
            Publishable::OwnOrThisChecklist
        ),
        Ok(UnpushedHistory::Safe(1))
    );

    fs::write(work.join("other.txt"), "unrelated\n").unwrap();
    run(&work, &["add", "other.txt"]);
    run(&work, &["commit", "-q", "-m", "unrelated"]);
    let moved = git_stdout(&work, &["rev-parse", "HEAD"]);
    assert_ne!(moved, tip, "test setup: HEAD must have moved");

    assert_eq!(push(&work, &tip), SyncOutcome::Synced);
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "checklist\ninit",
        "the unrelated commit must not reach the remote"
    );

    assert!(matches!(
        unpushed_history(&work, Some(&moved), "tracked.md", Publishable::OwnOrThisChecklist),
        Ok(UnpushedHistory::Blocked(blocking)) if blocking.subject == "unrelated"
    ));
}
#[test]
fn a_branch_without_an_upstream_is_not_a_verification_failure() {
    // A fresh repository has nothing to compare against; committing proceeds.
    let work = init_repo_without_remote();
    let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

    assert_eq!(
        unpushed_history(
            &work,
            Some(&tip),
            "tracked.md",
            Publishable::OwnOrThisChecklist
        ),
        Ok(UnpushedHistory::NoUpstream)
    );
    assert!(unpushed_history_blocks(&work, Some(&tip), "tracked.md").is_none());
}
#[test]
fn a_configured_but_unfetched_upstream_still_counts_as_no_upstream() {
    // An upstream set in config whose `@{u}` doesn't resolve is still "no
    // upstream", not a failure.
    let work = init_repo_without_remote();
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
    let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

    assert!(
        upstream_parts(&work).is_some(),
        "test setup: the config claims an upstream exists"
    );
    assert_eq!(
        resolve_upstream(&work),
        Ok(None),
        "but the tracking ref does not resolve"
    );
    assert_eq!(
        unpushed_history(
            &work,
            Some(&tip),
            "tracked.md",
            Publishable::OwnOrThisChecklist
        ),
        Ok(UnpushedHistory::NoUpstream)
    );
}

#[test]
fn an_unpushed_merge_is_never_published() {
    let (work, _remote) = init_repo_with_remote();
    run(&work, &["checkout", "-q", "-b", "side"]);
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    commit_as_markcheck(
        &work,
        &["commit", "-q", "-am", "side", "-m", OWN_COMMIT_TRAILER],
    );
    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("other.md"), "- [ ] two\n").unwrap();
    run(&work, &["add", "other.md"]);
    commit_as_markcheck(
        &work,
        &["commit", "-q", "-m", "main side", "-m", OWN_COMMIT_TRAILER],
    );
    let tip = commit_as_markcheck(&work, &["merge", "-q", "--no-edit", "side", "-m", "merge"]);

    assert!(matches!(
        unpushed_history(&work, Some(&tip), "tracked.md", Publishable::OwnOrThisChecklist),
        Ok(UnpushedHistory::Blocked(blocking)) if blocking.subject == "merge"
    ));
}

#[test]
fn an_unpushed_empty_commit_is_never_published() {
    for (rule, trailer) in [
        (Publishable::OwnOrThisChecklist, "plain"),
        (Publishable::OwnOrThisChecklist, OWN_COMMIT_TRAILER),
        (Publishable::OwnOnly, OWN_COMMIT_TRAILER),
    ] {
        let (work, _remote) = init_repo_with_remote();
        let tip = commit_as_markcheck(
            &work,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "trigger deploy",
                "-m",
                trailer,
            ],
        );
        assert!(
            matches!(
                unpushed_history(&work, Some(&tip), "tracked.md", rule),
                Ok(UnpushedHistory::Blocked(blocking)) if blocking.subject == "trigger deploy"
            ),
            "{rule:?} with {trailer:?}"
        );
    }
}

#[test]
fn a_recorded_commit_to_any_checklist_is_markchecks_own() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("TODO.txt"), "- [x] one\n").unwrap();
    run(&work, &["add", "TODO.txt"]);
    let tip = commit_as_markcheck(
        &work,
        &[
            "commit",
            "-q",
            "-m",
            "TODO.txt: Check",
            "-m",
            OWN_COMMIT_TRAILER,
        ],
    );
    for rule in [Publishable::OwnOnly, Publishable::OwnOrThisChecklist] {
        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md", rule),
            Ok(UnpushedHistory::Safe(1)),
            "{rule:?}"
        );
    }
}

#[test]
fn a_trailer_alone_does_not_make_a_commit_markchecks_own() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("README.md"), "x\n").unwrap();
    run(&work, &["add", "README.md"]);
    run(
        &work,
        &[
            "commit",
            "-q",
            "-m",
            "README.md: Reword",
            "-m",
            OWN_COMMIT_TRAILER,
        ],
    );
    let tip = git_stdout(&work, &["rev-parse", "HEAD"]);
    for rule in [Publishable::OwnOnly, Publishable::OwnOrThisChecklist] {
        assert!(
            matches!(
                unpushed_history(&work, Some(&tip), "tracked.md", rule),
                Ok(UnpushedHistory::Blocked(blocking)) if blocking.sha == tip
            ),
            "{rule:?}"
        );
    }
}

#[test]
fn the_own_commit_record_keeps_the_latest_commits_once_each() {
    let work = init_repo_without_remote();
    let shas: Vec<String> = (0..OWN_COMMITS_KEPT + 2)
        .map(|n| format!("{n:040x}"))
        .collect();
    for sha in &shas {
        record_own_commit(&work, sha);
    }
    record_own_commit(&work, &shas[5]);
    let own = own_commits(&work).unwrap();
    assert_eq!(own.len(), OWN_COMMITS_KEPT);
    assert!(own.contains(&shas[5]), "a re-recorded commit is the newest");
    assert!(!own.contains(&shas[0]) && !own.contains(&shas[1]));
    assert!(own.contains(&shas[2]));
    assert!(own.contains(shas.last().unwrap()));
    let leftovers: Vec<_> = fs::read_dir(git_dir(&work).unwrap())
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-own-commits-")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn an_unreadable_own_commit_record_refuses_instead_of_guessing() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    let tip = commit_as_markcheck(&work, &["commit", "-q", "-am", "toggle"]);
    let record = git_dir(&work).unwrap().join(OWN_COMMITS_FILE);
    fs::remove_file(&record).unwrap();
    fs::create_dir(&record).unwrap();

    let err = unpushed_history(&work, Some(&tip), "tracked.md", Publishable::OwnOnly)
        .expect_err("a record that can't be read has no answer");
    assert!(err.contains("record of its commits"), "{err}");
    record_own_commit(&work, &tip);
    assert!(record.is_dir(), "a failed rewrite leaves the record alone");
}

#[test]
fn a_repository_without_a_git_dir_has_no_own_commits_to_read_or_record() {
    let dir = unique_dir("no-git-dir");
    fs::create_dir_all(&dir).unwrap();
    record_own_commit(&dir, &"a".repeat(40));
    assert!(own_commits(&dir).is_err());
}

#[test]
fn unresolved_conflicts_and_a_detached_head_are_named() {
    let (work, _remote) = init_repo_with_remote();
    run(&work, &["checkout", "-q", "-b", "side"]);
    fs::write(work.join("tracked.md"), "- [x] side\n").unwrap();
    run(&work, &["commit", "-q", "-am", "side"]);
    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("tracked.md"), "- [x] main\n").unwrap();
    run(&work, &["commit", "-q", "-am", "main"]);
    let _ = crate::test_support::git()
        .current_dir(&work)
        .args(["merge", "-q", "side"])
        .output();
    // Past the merge-in-progress marker, to the conflict check itself.
    fs::remove_file(work.join(".git").join("MERGE_HEAD")).unwrap();
    assert_eq!(
        repo_sync_blocked(&work).as_deref(),
        Some("the repository has unresolved merge conflicts")
    );

    run(&work, &["checkout", "-q", "-f", "main"]);
    run(&work, &["checkout", "-q", "--detach"]);
    assert!(
        repo_sync_blocked(&work).is_some_and(|reason| reason.contains("detached HEAD")),
        "{:?}",
        repo_sync_blocked(&work)
    );
}

#[test]
fn the_range_walk_ignores_config_that_would_hide_a_path() {
    let work = init_repo_without_remote();
    for (key, value) in [
        ("diff.renames", "true"),
        ("log.showRoot", "false"),
        ("color.ui", "always"),
    ] {
        run(&work, &["config", key, value]);
    }
    let root = current_head(&work).unwrap();
    assert_eq!(
        range_has_unrelated_commits(&work, None, &root, "other.md"),
        Ok(true),
        "the root commit's own paths must be listed"
    );

    fs::write(work.join("notes.txt"), "- [ ] same\n").unwrap();
    run(&work, &["add", "notes.txt"]);
    run(&work, &["commit", "-q", "-m", "notes"]);
    let base = current_head(&work).unwrap();
    run(&work, &["mv", "notes.txt", "renamed.md"]);
    run(&work, &["commit", "-q", "-m", "rename into a checklist"]);
    let tip = current_head(&work).unwrap();
    assert_eq!(
        range_has_unrelated_commits(&work, Some(&base), &tip, "renamed.md"),
        Ok(true),
        "a rename must list the path it came from"
    );
}

#[test]
fn a_path_named_like_a_commit_is_not_read_as_one() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "checklist"]);
    let sha_name = current_head(&work).unwrap();
    fs::write(work.join(&sha_name), "x\n").unwrap();
    run(&work, &["add", "--", &sha_name]);
    run(&work, &["commit", "-q", "-m", "hex-named file"]);
    let tip = current_head(&work).unwrap();

    assert!(matches!(
        unpushed_history(&work, Some(&tip), "tracked.md", Publishable::OwnOrThisChecklist),
        Ok(UnpushedHistory::Blocked(blocking))
            if blocking.subject == "hex-named file" && blocking.sha == tip
    ));
}

#[test]
fn a_log_listing_is_split_into_commits_without_guessing() {
    let listing = b"\0aaaa\0\0own\0\n\nodd.md\0bbbb\0\
\0cccc\0aaaa\0empty\0\
\0dddd\0aaaa cccc\0merge\0";
    let commits = parse_range_commits(listing).unwrap();
    let summary: Vec<_> = commits
        .iter()
        .map(|c| (c.sha.as_str(), c.is_merge, c.paths.clone()))
        .collect();
    assert_eq!(
        summary,
        [
            ("aaaa", false, vec![b"\nodd.md".to_vec(), b"bbbb".to_vec()]),
            ("cccc", false, vec![]),
            ("dddd", true, vec![]),
        ]
    );
    assert_eq!(commits[2].subject, "merge");
    assert!(parse_range_commits(b"").unwrap().is_empty());
}

#[test]
fn an_unexpected_log_listing_is_an_error() {
    for listing in [
        &b"stray\0"[..],
        b"\0aaaa\0",
        b"\0not-hex\0\0\0subject\0",
        b"\0\0\0\0subject\0",
    ] {
        assert!(
            parse_range_commits(listing).is_err(),
            "{:?}",
            String::from_utf8_lossy(listing)
        );
    }
}
#[test]
fn the_own_trailer_check_fails_on_an_unknown_commit() {
    let work = init_repo_without_remote();
    let err = carries_own_trailer(&work, &"0".repeat(40))
        .expect_err("a commit git can't read has no answer");
    assert!(err.contains("git log"), "{err}");
}
#[test]
fn the_scope_check_counts_a_merge_as_unrelated() {
    let work = init_repo_without_remote();
    let base = current_head(&work).unwrap();
    run(&work, &["checkout", "-q", "-b", "side"]);
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "side"]);
    run(&work, &["checkout", "-q", "main"]);
    run(&work, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let tip = current_head(&work).unwrap();
    assert_eq!(
        range_has_unrelated_commits(&work, Some(&base), &tip, "tracked.md"),
        Ok(true),
        "a merge touching only the checklist must still count as unrelated"
    );
}
#[test]
fn a_scope_check_git_cannot_answer_leaves_the_commit_and_says_so() {
    let work = init_repo_without_remote();
    let bogus = "0".repeat(40);
    let err = verify_commit_scope(&work, &None, "tracked.md", &bogus)
        .expect_err("an unreadable commit must not pass the scope check");
    assert!(err.contains("could not check what commit"), "{err}");
    assert!(err.contains("left on the branch"), "{err}");
}
#[test]
fn the_staged_target_guard_fails_closed_when_the_loaded_content_cannot_be_hashed() {
    let work = init_repo_without_remote();
    let head_blob = blob_at(&work, &current_head(&work).unwrap(), "tracked.md");
    fs::write(work.join("tracked.md"), "- [ ] one\n- [ ] staged\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["config", "filter.broken.clean", "false"]);
    run(&work, &["config", "filter.broken.required", "true"]);
    fs::write(
        work.join(".git").join("info").join("attributes"),
        "*.md filter=broken\n",
    )
    .unwrap();
    assert!(
        staged_target_would_be_lost(
            &work,
            "tracked.md",
            head_blob.as_deref(),
            Some("- [ ] one\n- [ ] staged\n")
        ),
        "a loaded content git can't hash can't be cleared as matching the index"
    );
}
#[test]
fn undo_commit_refuses_a_commit_git_cannot_list() {
    let work = init_repo_without_remote();
    let head = current_head(&work).unwrap();
    let err = undo_commit(&work, &"0".repeat(40)).expect_err("nothing to rewind");
    assert!(err.contains("git rev-list"), "{err}");
    assert_eq!(current_head(&work), Some(head));
}
#[test]
fn undo_commit_refuses_when_the_history_listing_is_empty() {
    let work = init_repo_without_remote();
    let head = current_head(&work).unwrap();
    let err = undo_commit(&work, "HEAD..HEAD").expect_err("an empty listing is not a root");
    assert!(err.contains("could not read commit"), "{err}");
    assert_eq!(
        current_head(&work),
        Some(head),
        "the branch must be untouched"
    );
}
#[test]
fn undo_commit_refuses_to_delete_a_root_commit_on_a_detached_head() {
    let work = init_repo_without_remote();
    let root = current_head(&work).unwrap();
    run(&work, &["checkout", "-q", "--detach"]);
    let err = undo_commit(&work, &root).expect_err("no branch to delete");
    assert!(err.contains("branch reference could not be read"), "{err}");
    assert_eq!(current_head(&work), Some(root));
}
#[test]
fn repo_sync_blocked_refuses_when_a_marker_cannot_be_checked() {
    let work = init_repo_without_remote();
    let marker = work.join(".git").join("MERGE_HEAD");
    std::os::unix::fs::symlink(&marker, &marker).unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("could not check for a merge in progress")),
        "{reason:?}"
    );
}
#[test]
fn repo_sync_blocked_refuses_when_the_branch_check_fails() {
    let work = init_repo_without_remote();
    fs::write(
        work.join(".git").join("HEAD"),
        "ref: refs/heads/bad..name\n",
    )
    .unwrap();
    let reason = repo_sync_blocked(&work);
    assert!(
        reason
            .as_ref()
            .is_some_and(|r| r.contains("could not check whether the repository is on a branch")),
        "{reason:?}"
    );
}
