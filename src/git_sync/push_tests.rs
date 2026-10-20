use super::super::guards::OWN_COMMIT_TRAILER;
use super::super::test_support::*;
use super::*;
use std::fs;
use std::path::PathBuf;

#[test]
fn push_if_head_unchanged_refuses_to_push_a_commit_that_landed_after_verification() {
    let (work, remote) = init_repo_with_remote();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let created_commit = current_head(&work).unwrap();

    fs::write(work.join("other.md"), "concurrent\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "concurrent change"]);

    let outcome = push_if_head_unchanged(&work, &created_commit);
    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, commit }
                if message.contains("changed after the commit was made") && commit == &created_commit),
        "{outcome:?}"
    );

    let remote_log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s", "main"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&remote_log.stdout).trim(),
        "init",
        "neither the markcheck commit nor the concurrent one must reach the remote"
    );
}
#[test]
fn push_if_head_unchanged_pushes_normally_when_nothing_raced_it() {
    let (work, remote) = init_repo_with_remote();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let created_commit = current_head(&work).unwrap();

    let outcome = push_if_head_unchanged(&work, &created_commit);
    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

    let remote_log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s", "main"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&remote_log.stdout).contains("markcheck commit"),
        "{}",
        String::from_utf8_lossy(&remote_log.stdout)
    );
}
#[test]
fn a_branch_tracking_a_local_branch_is_committed_but_never_pushed() {
    let (work, _remote) = init_repo_with_remote();
    run(
        &work,
        &["checkout", "-q", "-b", "feature", "--track", "main"],
    );
    // Makes a plain `git push` agree to publish to the upstream.
    run(&work, &["config", "push.default", "upstream"]);
    let main_before = git_stdout(&work, &["rev-parse", "refs/heads/main"]);
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let commit = current_head(&work).unwrap();

    let outcome = push(&work, &commit);

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, commit: c }
                if message.contains("tracks the local branch main") && c == &commit),
        "{outcome:?}"
    );
    assert_eq!(
        git_stdout(&work, &["rev-parse", "refs/heads/main"]),
        main_before,
        "the tracked local branch must not move"
    );
    assert!(still_on_branch(&work, &commit));
}
#[test]
fn push_refuses_an_empty_commit_instead_of_deleting_the_remote_branch() {
    // `<remote> :<branch>` would delete the branch and exit 0.
    let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();

    let outcome = push(&work, "");

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("which commit to push")),
        "{outcome:?}"
    );
    let branches = crate::test_support::git()
        .current_dir(&remote)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(
        branches.lines().any(|b| b == branch),
        "the remote branch must still exist: {branches:?}"
    );
}
/// A deletion between the remote check and the push must not recreate the
/// branch.
#[test]
fn a_branch_deleted_between_the_check_and_the_push_is_not_recreated() {
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let commit = current_head(&work).unwrap();

    let victim = remote.clone();
    let _hook = RaceHook::at(RacePoint::BeforePush, move || {
        run(&victim, &["update-ref", "-d", "refs/heads/main"]);
    });

    let outcome = push(&work, &commit);

    assert!(
        !matches!(outcome, SyncOutcome::Synced),
        "a push into the deletion window must not report success: {outcome:?}"
    );
    assert_eq!(
        git_stdout(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"]
        ),
        "",
        "the branch must still be absent — the lease is what makes the \
             earlier existence check hold rather than merely have been true"
    );
}

/// The same window, but the branch moves instead of vanishing.
#[test]
fn a_branch_that_moves_between_the_check_and_the_push_is_not_clobbered() {
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let commit = current_head(&work).unwrap();

    let racer = work.clone();
    let racer_remote = remote.clone();
    let _hook = RaceHook::at(RacePoint::BeforePush, move || {
        let sha = commit_unrelated(&racer, "other.md");
        run(
            &racer,
            &[
                "push",
                "-q",
                &racer_remote.to_string_lossy(),
                &format!("{sha}:refs/heads/main"),
            ],
        );
    });

    let before = git_stdout(&remote, &["rev-parse", "refs/heads/main"]);
    let outcome = push(&work, &commit);
    let after = git_stdout(&remote, &["rev-parse", "refs/heads/main"]);

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("stale info") && !message.contains("To ")),
        "pushing over a tip that moved must be reported with git's reason, \
             not the destination URL: {outcome:?}"
    );
    assert_ne!(
        after, before,
        "test setup: the racer must have moved the branch"
    );
    assert_ne!(
        after, commit,
        "the commit that raced in must still be the branch tip"
    );
}

/// The lease must never license a non-fast-forward.
#[test]
fn a_commit_not_based_on_the_remote_tip_is_refused_rather_than_forced() {
    let (work, remote) = init_repo_with_remote();

    // Made on a side branch so `main` keeps its upstream.
    run(&work, &["checkout", "-q", "-b", "side"]);
    let ahead = commit_unrelated(&work, "ahead.md");
    run(
        &work,
        &[
            "push",
            "-q",
            &remote.to_string_lossy(),
            &format!("{ahead}:refs/heads/main"),
        ],
    );

    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let commit = current_head(&work).unwrap();

    let outcome = push(&work, &commit);

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.ends_with("is not based on; pull first, then it will sync")),
        "must refuse rather than force, in a single-spaced message: {outcome:?}"
    );
    assert_eq!(
        git_stdout(&remote, &["rev-parse", "refs/heads/main"]),
        ahead,
        "the remote tip must be untouched"
    );
}

/// `ls-remote` matches ref tails, so a branch named
/// `a/refs/heads/main` must not be read as the tip of `main` (#120).
#[test]
fn a_branch_whose_name_ends_in_the_upstream_ref_does_not_block_the_push() {
    let (work, remote) = init_repo_with_remote();
    run(&work, &["checkout", "-q", "-b", "side"]);
    let decoy = commit_unrelated(&work, "decoy.md");
    run(
        &work,
        &[
            "push",
            "-q",
            &remote.to_string_lossy(),
            &format!("{decoy}:refs/heads/a/refs/heads/main"),
        ],
    );
    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "markcheck commit"]);
    let commit = current_head(&work).unwrap();

    let outcome = push(&work, &commit);

    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["rev-parse", "refs/heads/main"]),
        commit
    );
    assert_eq!(
        git_stdout(&remote, &["rev-parse", "refs/heads/a/refs/heads/main"]),
        decoy,
        "the look-alike branch must be untouched"
    );
}

#[test]
fn push_never_sends_more_than_the_explicitly_targeted_commit() {
    // Push an older commit explicitly with a newer one already on top.
    let (work, remote) = init_repo_with_remote();

    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "older commit"]);
    let older_commit = current_head(&work).unwrap();

    fs::write(work.join("other.md"), "newer\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "newer commit"]);

    let outcome = push(&work, &older_commit);
    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

    let remote_log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s", "main"])
        .output()
        .unwrap();
    let subjects = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        !subjects.contains("newer commit"),
        "must never publish anything beyond the targeted commit: {subjects}"
    );
    assert!(subjects.contains("older commit"), "{subjects}");
}
#[test]
fn retry_commit_abandons_silently_when_head_has_moved_on() {
    // A stale retry gives up rather than rebuilding from old content.
    let work = init_repo_without_remote();
    let file_path = work.join("tracked.md");
    let stale_commit = current_head(&work).unwrap();

    fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
    run(&work, &["commit", "-q", "-am", "newer content"]);
    let newer_commit = current_head(&work).unwrap();
    assert_ne!(stale_commit, newer_commit);

    let outcome = retry_commit(&work, &stale_commit);

    assert_eq!(outcome, SyncOutcome::RetryAbandoned);
    assert_eq!(
        current_head(&work).unwrap(),
        newer_commit,
        "the newer commit must be left completely alone"
    );
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "- [x] one\n- [x] two\n",
        "no stale commit was rebuilt on top"
    );
}
#[test]
fn retry_commit_pushes_when_head_still_matches() {
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "Check \"one\""]);
    let commit = current_head(&work).unwrap();

    let outcome = retry_commit(&work, &commit);

    assert_eq!(outcome, SyncOutcome::Synced);
    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", "HEAD:tracked.md"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");
}
#[test]
fn an_unrelated_commit_landing_after_validation_is_not_published_by_catch_up() {
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(
        &work,
        &["commit", "-q", "-am", "checklist", "-m", OWN_COMMIT_TRAILER],
    );
    let validated = git_stdout(&work, &["rev-parse", "HEAD"]);

    let racer = work.clone();
    let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
        commit_unrelated(&racer, "other.txt");
    });

    let outcome = catch_up_push(&work, &work.join("tracked.md"));

    assert_eq!(outcome, SyncOutcome::CaughtUp { pushed: 1 }, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["rev-parse", "main"]),
        validated,
        "catch-up must publish only the tip it validated"
    );
}

#[test]
fn catch_up_push_never_commits_uncommitted_working_tree_changes() {
    // Opening a checklist with uncommitted editor changes must publish
    // nothing.
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let edited = "- [x] edited outside markcheck\n";
    fs::write(&file, edited).unwrap();

    let outcome = catch_up_push(&work, &file);

    assert_eq!(
        outcome,
        SyncOutcome::Skipped,
        "nothing was ahead of upstream, so there was nothing to do"
    );
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s", "main"]),
        "init",
        "no commit may be created"
    );
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "init",
        "nothing may reach the remote"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        edited,
        "the uncommitted edit stays exactly as the user left it"
    );
}
#[test]
fn catch_up_push_pushes_a_local_only_commit_without_committing_a_dirty_tree() {
    // The existing commit goes out; the uncommitted edit does not.
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    fs::write(&file, "- [x] one\n").unwrap();
    run(
        &work,
        &[
            "commit",
            "-q",
            "-am",
            "tracked.md: Check \"one\"",
            "-m",
            OWN_COMMIT_TRAILER,
        ],
    );
    let dirty = "- [x] one\n- [ ] two\n";
    fs::write(&file, dirty).unwrap();

    let outcome = catch_up_push(&work, &file);

    assert_eq!(outcome, SyncOutcome::CaughtUp { pushed: 1 });
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "tracked.md: Check \"one\"\ninit",
        "the local-only commit reached the remote"
    );
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s", "main"]),
        "tracked.md: Check \"one\"\ninit",
        "and no second commit was created for the dirty tree"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), dirty);
}
#[test]
fn catch_up_push_is_a_no_op_when_everything_is_already_pushed() {
    let (work, remote) = init_repo_with_remote();

    assert_eq!(
        catch_up_push(&work, &work.join("tracked.md")),
        SyncOutcome::Skipped
    );
    assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");
}
#[test]
fn catch_up_push_stays_silent_when_no_upstream_is_configured() {
    let work = init_repo_without_remote();

    assert_eq!(
        catch_up_push(&work, &work.join("tracked.md")),
        SyncOutcome::Skipped
    );
}
#[test]
fn catch_up_push_refuses_when_unrelated_commits_are_unpushed() {
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.txt"]);
    run(&work, &["commit", "-q", "-m", "unrelated"]);

    let outcome = catch_up_push(&work, &work.join("tracked.md"));

    assert!(
        matches!(&outcome, SyncOutcome::NotCaughtUp(msg)
                if msg.contains("\"unrelated\" is not pushed")),
        "{outcome:?}"
    );
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "init",
        "the unrelated commit must not be published"
    );
}
#[test]
fn catch_up_push_reports_an_untracked_checklist() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.txt"]);
    run(&work, &["commit", "-q", "-m", "ahead"]);
    let untracked = work.join("not-tracked.md");
    fs::write(&untracked, "- [ ] one\n").unwrap();

    assert_eq!(
        catch_up_push(&work, &untracked),
        SyncOutcome::SkippedUntracked
    );
}
#[test]
fn catch_up_push_reports_an_untracked_checklist_on_a_fully_pushed_branch() {
    let (work, _remote) = init_repo_with_remote();
    let untracked = work.join("not-tracked.md");
    fs::write(&untracked, "- [ ] one\n").unwrap();

    assert_eq!(
        catch_up_push(&work, &untracked),
        SyncOutcome::SkippedUntracked
    );
}
#[test]
fn catch_up_push_is_quiet_about_an_index_lookup_error_with_nothing_to_push() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("other.md"), "x\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "other"]);
    run(&work, &["push", "-q"]);

    assert_eq!(catch_up_push(&work, &work), SyncOutcome::Skipped);
}
#[test]
fn catch_up_push_fails_on_an_index_lookup_error_with_commits_to_push() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("other.md"), "x\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "ahead"]);

    let outcome = catch_up_push(&work, &work);

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("more than one tracked file")),
        "{outcome:?}"
    );
}
#[test]
fn catch_up_push_refuses_while_the_repository_is_mid_merge() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "ahead"]);
    let git_dir = work.join(".git");
    fs::write(git_dir.join("MERGE_HEAD"), "deadbeef\n").unwrap();

    let outcome = catch_up_push(&work, &work.join("tracked.md"));

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("a merge is in progress")),
        "{outcome:?}"
    );
}

fn second_bare_repo(work: &Path, name: &str) -> PathBuf {
    let repo = work.parent().unwrap().join(name);
    fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "--bare", "-q", "-b", "main"]);
    run(
        work,
        &[
            "push",
            "-q",
            repo.to_str().unwrap(),
            "refs/heads/main:refs/heads/main",
        ],
    );
    repo
}
fn commit_checklist_change(work: &Path) -> String {
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(work, &["commit", "-q", "-am", "markcheck commit"]);
    current_head(work).unwrap()
}
/// `git switch -c notes origin/main` tracks `main`; with the default
/// `push.default = simple`, `git push` refuses (#98).
#[test]
fn a_branch_whose_upstream_has_another_name_is_not_pushed_to_that_upstream() {
    let (work, remote) = init_repo_with_remote();
    run(&work, &["config", "push.default", "simple"]);
    run(&work, &["switch", "-q", "-c", "notes", "origin/main"]);
    let commit = commit_checklist_change(&work);

    let outcome = push(&work, &commit);

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("would not push this branch")),
        "{outcome:?}"
    );
    assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");
    assert_eq!(
        git_stdout(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/heads/notes"]
        ),
        ""
    );
}
/// Where `git push` itself would publish to the differently named
/// upstream, so does the sync.
#[test]
fn a_differently_named_upstream_is_pushed_when_push_default_is_upstream() {
    let (work, remote) = init_repo_with_remote();
    run(&work, &["config", "push.default", "upstream"]);
    run(&work, &["switch", "-q", "-c", "notes", "origin/main"]);
    let commit = commit_checklist_change(&work);

    assert_eq!(push(&work, &commit), SyncOutcome::Synced);
    assert_eq!(git_stdout(&remote, &["rev-parse", "main"]), commit);
}
fn assert_never_pushed_to_the_fetch_remote(config: &[&str]) {
    let (work, remote) = init_repo_with_remote();
    let fork = second_bare_repo(&work, "fork.git");
    run(&work, &["remote", "add", "fork", fork.to_str().unwrap()]);
    run(&work, &["fetch", "-q", "fork"]);
    run(&work, &["config", "push.default", "simple"]);
    run(&work, &["config", config[0], config[1]]);
    let commit = commit_checklist_change(&work);

    let outcome = push(&work, &commit);

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("would not push this branch")),
        "{config:?}: {outcome:?}"
    );
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "init",
        "{config:?}: nothing may reach the fetch remote"
    );
    assert_ne!(git_stdout(&fork, &["rev-parse", "main"]), commit);
}
#[test]
fn a_push_default_remote_keeps_the_sync_off_the_fetch_remote() {
    assert_never_pushed_to_the_fetch_remote(&["remote.pushDefault", "fork"]);
}
#[test]
fn a_branch_push_remote_keeps_the_sync_off_the_fetch_remote() {
    assert_never_pushed_to_the_fetch_remote(&["branch.main.pushRemote", "fork"]);
}
/// `ls-remote <remote>` asks the fetch `url`; the lease must describe the
/// `pushurl` repository the push actually reaches.
#[test]
fn a_push_url_is_checked_and_pushed_instead_of_the_fetch_url() {
    let (work, remote) = init_repo_with_remote();
    let push_repo = second_bare_repo(&work, "push.git");
    run(
        &work,
        &[
            "config",
            "remote.origin.pushurl",
            push_repo.to_str().unwrap(),
        ],
    );
    // The fetch repository moves on with a commit this clone never fetched.
    let other = work.parent().unwrap().join("other");
    run(
        work.parent().unwrap(),
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    run(&other, &["config", "user.email", "test@example.com"]);
    run(&other, &["config", "user.name", "test"]);
    commit_unrelated(&other, "elsewhere.md");
    run(&other, &["push", "-q", "origin", "main"]);
    let fetch_tip = git_stdout(&remote, &["rev-parse", "main"]);
    let commit = commit_checklist_change(&work);

    assert_eq!(push(&work, &commit), SyncOutcome::Synced);
    assert_eq!(git_stdout(&push_repo, &["rev-parse", "main"]), commit);
    assert_eq!(git_stdout(&remote, &["rev-parse", "main"]), fetch_tip);
}

#[test]
fn catch_up_push_leaves_a_hand_made_checklist_commit_alone() {
    // The user reworked the checklist, committed it to review before sharing,
    // and only opened it: nothing may be published (#62).
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    fs::write(&file, "- [ ] two\n- [ ] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "WIP: reorder steps"]);

    let outcome = catch_up_push(&work, &file);

    assert!(
        matches!(&outcome, SyncOutcome::NotCaughtUp(msg)
                if msg.contains("\"WIP: reorder steps\"") && msg.contains("push it yourself")),
        "{outcome:?}"
    );
    assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");
}
#[test]
fn catch_up_push_counts_every_earlier_markcheck_commit_it_pushes() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    for content in ["- [x] one\n", "- [ ] one\n"] {
        fs::write(&file, content).unwrap();
        run(
            &work,
            &["commit", "-q", "-am", "toggle", "-m", OWN_COMMIT_TRAILER],
        );
    }

    assert_eq!(
        catch_up_push(&work, &file),
        SyncOutcome::CaughtUp { pushed: 2 }
    );
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "main"]),
        "toggle\ntoggle\ninit"
    );
}
