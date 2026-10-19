use super::super::inspect::{
    current_branch_ref, current_head, index_blob, resolve_upstream, upstream_parts,
};
use super::super::test_support::*;
use super::*;
use std::fs;
use std::process::Command;

/// The local tracking ref survives an upstream deletion; a sync must not
/// recreate the branch.
#[test]
fn a_branch_deleted_on_the_remote_is_not_recreated_by_a_sync() {
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let content = "- [x] one\n";
    fs::write(&file, content).unwrap();

    // Nothing fetches, so the local view goes stale.
    run(&remote, &["update-ref", "-d", "refs/heads/main"]);
    assert!(
        !git_stdout(&work, &["rev-parse", "--verify", "-q", "origin/main"]).is_empty(),
        "test setup: the stale tracking ref must still be present, or \
             this exercises the no-upstream path instead"
    );

    let outcome = run_sync(
        &work,
        &file,
        content,
        &commit_message(&file, "Check \"one\""),
        &no_race(content),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("no longer exists")),
        "the sync must commit locally and refuse to publish: {outcome:?}"
    );
    assert_eq!(
        git_stdout(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"]
        ),
        "",
        "the deleted branch must still be absent from the remote"
    );
    assert!(
        git_stdout(&work, &["log", "--format=%s", "-1", "main"]).contains("Check"),
        "the commit itself must have been made locally"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_a_non_utf8_file_name_without_committing() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let work = init_repo_without_remote();
    let file = work.join(OsStr::from_bytes(b"list-\xff.md"));
    fs::write(&file, "- [ ] one\n").unwrap();
    let add = crate::test_support::git()
        .current_dir(&work)
        .arg("add")
        .arg(&file)
        .status()
        .unwrap();
    assert!(add.success());
    run(&work, &["commit", "-q", "-m", "add list"]);
    let before = git_stdout(&work, &["rev-parse", "HEAD"]);
    fs::write(&file, "- [x] one\n").unwrap();

    let outcome = run_sync(
        &work,
        &file,
        "- [x] one\n",
        "should not commit",
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("not a UTF-8 file name")),
        "{outcome:?}"
    );
    assert_eq!(git_stdout(&work, &["rev-parse", "HEAD"]), before);

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_reports_an_untracked_file_without_adding_it() {
    let work = init_repo_without_remote();
    let untracked = work.join("untracked.md");
    fs::write(&untracked, "- [ ] new\n").unwrap();

    assert_eq!(
        run_sync(
            &work,
            &untracked,
            "- [ ] new\n",
            "should not commit",
            &no_race("- [ ] new\n"),
            None,
        ),
        SyncOutcome::SkippedUntracked
    );
    let status = crate::test_support::git()
        .current_dir(&work)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("?? untracked.md"));

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_skips_when_tracked_file_is_unchanged() {
    let work = init_repo_without_remote();
    assert_eq!(
        run_sync(
            &work,
            &work.join("tracked.md"),
            "- [ ] one\n",
            "no changes",
            &no_race("- [ ] one\n"),
            None,
        ),
        SyncOutcome::Skipped
    );
    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_commits_and_pushes_a_tracked_change() {
    let (work, remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    let file_path = work.join("tracked.md");
    let message = commit_message(&file_path, "Check \"one\"");

    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "tracked.md: Check \"one\""
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_leaves_the_real_index_matching_head_after_a_commit() {
    // The real index must be realigned, or `git status` shows the file as both
    // staged and unstaged.
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    let file_path = work.join("tracked.md");
    let message = commit_message(&file_path, "Check \"one\"");

    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    let status = crate::test_support::git()
        .current_dir(&work)
        .args(["status", "--porcelain", "--", "tracked.md"])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "real index must match HEAD and the working tree after a commit: {:?}",
        String::from_utf8_lossy(&status.stdout)
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_reports_failure_when_push_has_no_remote() {
    let work = init_repo_without_remote();
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

    let outcome = run_sync(
        &work,
        &work.join("tracked.md"),
        "- [x] one\n",
        "Check \"one\"",
        &no_race("- [x] one\n"),
        None,
    );
    assert!(matches!(outcome, SyncOutcome::CommittedNotPushed { .. }));

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_commits_and_pushes_on_a_non_default_branch() {
    // A bare remote refuses to delete `main`, which would mask a refspec
    // mistake; a topic branch has no such protection.
    let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");

    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", &format!("{branch}:tracked.md")])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&show.stdout),
        "- [x] one\n",
        "the topic branch must carry the change, not be deleted or left behind"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_never_publishes_unrelated_work_when_no_upstream_is_configured() {
    // With no upstream and `push.default = current`, a bare `git push` would
    // publish the unrelated local commit too.
    let root = unique_dir("repo-no-upstream");
    let remote = root.join("remote.git");
    let work = root.join("work");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&work).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    run(&work, &["config", "push.default", "current"]);
    fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    // No `-u`, so `@{u}` can't be resolved.
    run(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    assert!(
        upstream_parts(&work).is_none(),
        "test setup: no upstream must be configured"
    );

    fs::write(work.join("other.md"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("no upstream yet")),
        "{outcome:?}"
    );
    let branches = crate::test_support::git()
        .current_dir(&remote)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "the unrelated commit must never reach the remote: {:?}",
        String::from_utf8_lossy(&branches.stdout)
    );

    fs::remove_dir_all(&root).ok();
}
#[test]
fn run_sync_refuses_to_commit_content_that_went_stale_before_the_worker_ran() {
    // A request gone stale must be refused, without absorbing B.
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    let message = commit_message(&file_path, "Check \"A\"");

    fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();

    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] A\n",
        &message,
        &no_race("- [x] A\n"),
        None,
    );
    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed after this sync was queued")),
        "{outcome:?}"
    );

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "init",
        "nothing must be committed at all -- neither the stale snapshot nor a mix with B"
    );

    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "- [x] A\n- [x] B\n"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_to_recommit_a_file_deleted_after_the_request_was_queued() {
    // A deleted file must not be resurrected by stale queued content.
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    let message = commit_message(&file_path, "Check \"one\"");

    fs::remove_file(&file_path).unwrap();

    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        None,
    );
    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed after this sync was queued")),
        "{outcome:?}"
    );

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "init",
        "the deleted file must not be resurrected in a commit"
    );
    assert!(!file_path.exists(), "the deletion must be left alone");

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_recovers_on_the_next_request_after_a_stale_refusal() {
    // A refusal skips only this request; the next sync succeeds.
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");

    fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
    let stale_message = commit_message(&file_path, "Check \"A\"");
    assert!(matches!(
        run_sync(
            &work,
            &file_path,
            "- [x] A\n",
            &stale_message,
            &no_race("- [x] A\n"),
            None,
        ),
        SyncOutcome::Failed(_)
    ));

    let fresh_message = commit_message(&file_path, "Edited in vim");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] A\n- [x] B\n",
        &fresh_message,
        &no_race("- [x] A\n- [x] B\n"),
        None,
    );
    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", "HEAD:tracked.md"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_never_includes_an_unrelated_staged_file() {
    // A file the user staged must never ride along in markcheck's commit.
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    fs::write(work.join("secret.env"), "SECRET_API_KEY=xyz123\n").unwrap();
    run(&work, &["add", "secret.env"]);

    let message = commit_message(&file_path, "Check \"one\"");
    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", "--stat", "HEAD"])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&show.stdout);
    assert!(
        !stat.contains("secret.env"),
        "the unrelated staged file must never appear in the pushed commit: {stat}"
    );

    let index = crate::test_support::git()
        .current_dir(&work)
        .args(["ls-files", "--stage", "secret.env"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&index.stdout).is_empty(),
        "secret.env must remain staged in the real index"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_undoes_the_commit_when_a_hook_stages_extra_files() {
    // A `pre-commit` hook staging an unrelated file into the temporary index:
    // the commit must be undone.
    use std::os::unix::fs::PermissionsExt;

    let work = init_repo_without_remote();
    let parent = current_head(&work).unwrap();
    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    fs::write(
        &hook_path,
        "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed files other than the checklist")),
        "{outcome:?}"
    );
    assert_eq!(
        current_head(&work).unwrap(),
        parent,
        "the commit must be fully undone, not just refused going forward"
    );
    let head_has_other = crate::test_support::git()
        .current_dir(&work)
        .args(["cat-file", "-e", "HEAD:other.md"])
        .status()
        .unwrap()
        .success();
    assert!(
        !head_has_other,
        "the hook-staged file must never reach a reachable commit"
    );
    let real_index = crate::test_support::git()
        .current_dir(&work)
        .args(["ls-files", "other.md"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&real_index.stdout).is_empty(),
        "the real index must stay untouched by the hook too"
    );
    let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("markcheck-index-")
    });
    assert!(!leftover, "temp index file was not cleaned up");

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_undoes_a_hook_violated_root_commit_by_deleting_the_branch_ref() {
    // A root commit is undone by deleting the branch ref.
    use std::os::unix::fs::PermissionsExt;

    let work = unique_dir("repo-root-commit-hook").join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    let hooks_dir = work.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    fs::write(
        &hook_path,
        "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);

    let message = commit_message(&file_path, "Check \"one\"");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        staged_matches("- [x] one\n"),
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed files other than the checklist")),
        "{outcome:?}"
    );
    assert_eq!(
        current_head(&work),
        None,
        "the repository must be back to having no commits at all"
    );
    assert_eq!(
        current_branch_ref(&work).as_deref(),
        Some("refs/heads/main"),
        "HEAD must still symbolically resolve to the branch, just with no commit on it yet"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_during_an_unresolved_merge_conflict() {
    // A conflicted merge on the checklist must be refused, leaving the merge
    // as the user left it.
    let work = init_repo_without_remote();
    run(&work, &["checkout", "-q", "-b", "branch-a"]);
    fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
    run(&work, &["commit", "-q", "-am", "a"]);
    run(&work, &["checkout", "-q", "main"]);
    fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
    run(&work, &["commit", "-q", "-am", "b"]);
    let head_before_merge_attempt = current_head(&work).unwrap();
    let _ = crate::test_support::git()
        .current_dir(&work)
        .args(["merge", "-q", "branch-a"])
        .output();
    assert!(
        work.join(".git").join("MERGE_HEAD").exists(),
        "test setup: real conflict expected"
    );

    let file_path = work.join("tracked.md");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] resolved\n",
        "should not commit",
        &no_race("- [x] resolved\n"),
        None,
    );
    assert!(matches!(outcome, SyncOutcome::Failed(_)), "{outcome:?}");

    assert_eq!(current_head(&work).unwrap(), head_before_merge_attempt);
    assert!(work.join(".git").join("MERGE_HEAD").exists());
    let contents = fs::read_to_string(&file_path).unwrap();
    assert!(
        contents.contains("<<<<<<<"),
        "conflict markers must still be present, untouched: {contents:?}"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn a_later_request_commits_the_concurrent_change_separately() {
    // Disk already holds the second toggle's content, which is also `latest`,
    // so the first request must not refuse.
    let (work, remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
    let latest = no_race("- [x] A\n- [x] B\n");

    let first_message = commit_message(&file_path, "Check \"A\"");
    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] A\n",
            &first_message,
            &latest,
            None
        ),
        SyncOutcome::Synced
    );

    let second_message = commit_message(&file_path, "Check \"B\"");
    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] A\n- [x] B\n",
            &second_message,
            &latest,
            None,
        ),
        SyncOutcome::Synced
    );

    let log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    let subjects = String::from_utf8_lossy(&log.stdout);
    assert!(subjects.contains("tracked.md: Check \"A\""));
    assert!(subjects.contains("tracked.md: Check \"B\""));

    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", "HEAD:tracked.md"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_skips_when_expected_content_already_matches_head_and_upstream() {
    // A remote is required: `Skipped` needs HEAD both matching and pushed.
    let (work, _remote) = init_repo_with_remote();
    let file_path = work.join("tracked.md");
    // The working tree has drifted, but this request is already committed
    // and pushed.
    fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [ ] one\n",
            "already committed",
            &no_race("- [ ] one\n"),
            None,
        ),
        SyncOutcome::Skipped
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
/// Under an eol filter the committed blob is LF while the request's text is
/// CRLF, so only a filtered comparison can see they are the same content.
#[test]
fn run_sync_recognises_already_committed_content_under_an_eol_filter() {
    let (work, _remote) = init_repo_with_remote();
    fs::write(work.join(".gitattributes"), "*.md text eol=crlf\n").unwrap();
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [ ] one\r\n").unwrap();
    run(&work, &["add", ".gitattributes", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "attributes"]);
    run(&work, &["push", "-q"]);
    let head_before = current_head(&work);

    let newer = "- [ ] one\r\n- [x] two\r\n";
    fs::write(&file_path, newer).unwrap();

    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [ ] one\r\n",
            "already committed",
            &no_race(newer),
            None,
        ),
        SyncOutcome::Skipped
    );
    assert_eq!(current_head(&work), head_before);

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_retries_the_push_when_expected_content_matches_head_but_is_unpushed() {
    // HEAD matching but unpushed must retry the push.
    let work = init_repo_without_remote();
    run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

    let outcome = run_sync(
        &work,
        &file_path,
        "- [ ] one\n",
        "already committed",
        &no_race("- [ ] one\n"),
        None,
    );
    assert!(
        matches!(outcome, SyncOutcome::CommittedNotPushed { .. }),
        "must attempt (and report failure of) the push, not silently skip: {outcome:?}"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_when_branch_has_unrelated_unpushed_commits() {
    // `git push` sends the whole branch, so an unrelated local commit must
    // make the sync refuse.
    let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.
    fs::write(work.join("other.md"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
    let head_before = current_head(&work).unwrap();

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("is not pushed yet")),
        "{outcome:?}"
    );
    assert_eq!(
        current_head(&work).unwrap(),
        head_before,
        "no checklist commit should have been created on top"
    );
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "- [x] one\n",
        "the on-disk edit itself is untouched, just not synced yet"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_the_fast_path_push_when_branch_has_unrelated_unpushed_commits() {
    // The fast path must run the unrelated-history guard too: the checklist's
    // blob at HEAD still matches even with an unrelated commit on top.
    let (work, remote) = init_repo_with_remote(); // origin/main = A, pushed.

    // A1: a checklist commit ahead of upstream.
    fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
    run(&work, &["commit", "-q", "-am", "Check \"one\""]);

    fs::write(work.join("other.md"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
    let head_before = current_head(&work).unwrap();

    // A newer uncommitted edit carries the request into the fast path.
    fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();

    let file_path = work.join("tracked.md");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        "Catch up a pending push",
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("is not pushed yet")),
        "{outcome:?}"
    );
    assert_eq!(
        current_head(&work).unwrap(),
        head_before,
        "nothing should have been committed or pushed"
    );

    let remote_log = crate::test_support::git()
        .current_dir(&remote)
        .args(["log", "--format=%s", "main"])
        .output()
        .unwrap();
    let subjects = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        !subjects.contains("unrelated local commit"),
        "the unrelated commit must never reach the remote: {subjects}"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_when_an_unrelated_commit_is_reverted_before_the_checklist_commit() {
    // A commit and its revert net out in a plain diff, but both are still
    // published.
    let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

    fs::write(work.join("other.md"), "unrelated work\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

    run(&work, &["rm", "-q", "other.md"]);
    run(
        &work,
        &["commit", "-q", "-m", "revert unrelated local commit"],
    );
    let head_before = current_head(&work).unwrap();

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &message,
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("is not pushed yet")),
        "the reverted commit must still count as unrelated history: {outcome:?}"
    );
    assert_eq!(
        current_head(&work).unwrap(),
        head_before,
        "no checklist commit should have been created on top"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_refuses_when_a_merge_commit_is_in_the_unpushed_range() {
    // Merges count as unrelated, even trivial ones.
    let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

    run(&work, &["checkout", "-q", "-b", "side"]);
    fs::write(work.join("other.md"), "side\n").unwrap();
    run(&work, &["add", "other.md"]);
    run(&work, &["commit", "-q", "-m", "side change"]);
    run(&work, &["checkout", "-q", "main"]);
    run(
        &work,
        &["merge", "-q", "--no-ff", "side", "-m", "merge side"],
    );
    let head_before = current_head(&work).unwrap();

    let file_path = work.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let outcome = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        "already committed",
        &no_race("- [x] one\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("is not pushed yet")),
        "a merge commit in range must refuse, even a clean one: {outcome:?}"
    );
    assert_eq!(current_head(&work).unwrap(), head_before);

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_does_not_refuse_for_markchecks_own_stacked_unpushed_commits() {
    // markcheck's own earlier commits (stacked while offline) are allowed.
    let work = init_repo_without_remote();
    run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
    let file_path = work.join("tracked.md");

    fs::write(&file_path, "- [x] one\n").unwrap();
    let first = run_sync(
        &work,
        &file_path,
        "- [x] one\n",
        &commit_message(&file_path, "Check \"one\""),
        &no_race("- [x] one\n"),
        None,
    );
    assert!(matches!(first, SyncOutcome::CommittedNotPushed { .. }));

    fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
    let second = run_sync(
        &work,
        &file_path,
        "- [x] one\n- [x] two\n",
        &commit_message(&file_path, "Check \"two\""),
        &no_race("- [x] one\n- [x] two\n"),
        None,
    );
    assert!(
        matches!(second, SyncOutcome::CommittedNotPushed { .. }),
        "must still commit (and only then fail to push), not refuse: {second:?}"
    );
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "- [x] one\n- [x] two\n"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_works_when_the_file_is_in_a_repo_subdirectory() {
    // `repo_dir` isn't the repo root here.
    let root = unique_dir("repo-nested");
    let remote = root.join("remote.git");
    let repo = root.join("repo");
    let sub = repo.join("checklists");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&sub).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.com"]);
    run(&repo, &["config", "user.name", "test"]);
    fs::write(sub.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&repo, &["add", "checklists/tracked.md"]);
    run(&repo, &["commit", "-q", "-m", "init"]);
    run(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&repo, &["push", "-q", "-u", "origin", "main"]);

    let file_path = sub.join("tracked.md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    assert_eq!(
        run_sync(
            &repo,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    let show = crate::test_support::git()
        .current_dir(&remote)
        .args(["show", "HEAD:checklists/tracked.md"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

    fs::remove_dir_all(&root).ok();
}
#[test]
fn run_sync_handles_filenames_with_spaces_dashes_colons_and_unicode() {
    // Names that break naive parsing of `git` output; non-ASCII is C-quoted
    // without `-z`.
    for file_name in [
        "with space.md",
        "-leading-dash.md",
        "colon:name.md",
        "unicode-\u{2705}\u{65e5}\u{672c}.md",
        "tab\tname.md",
        "newline\nname.md",
    ] {
        let (work, remote) = init_repo_with_remote_named(file_name);
        let file_path = work.join(file_name);
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced,
            "file name: {file_name:?}"
        );

        let show = crate::test_support::git()
            .current_dir(&remote)
            .args(["show", &format!("HEAD:{file_name}")])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&show.stdout),
            "- [x] one\n",
            "file name: {file_name:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
#[test]
fn run_sync_commits_a_checklist_whose_name_has_glob_characters_as_itself() {
    // #70: `notes[1].md` must not match the tracked `notes1.md`.
    let (work, remote) = init_repo_with_remote_named("notes[1].md");
    fs::write(work.join("notes1.md"), "unrelated\n").unwrap();
    run(&work, &["add", "notes1.md"]);
    run(&work, &["commit", "-q", "-m", "sibling"]);
    run(&work, &["push", "-q"]);

    let file_path = work.join("notes[1].md");
    fs::write(&file_path, "- [x] one\n").unwrap();
    let message = commit_message(&file_path, "Check \"one\"");
    assert_eq!(
        run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        ),
        SyncOutcome::Synced
    );

    assert_eq!(
        git_stdout(&remote, &["show", "main:notes[1].md"]),
        "- [x] one"
    );
    assert_eq!(
        git_stdout(&remote, &["show", "main:notes1.md"]),
        "unrelated"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_commits_a_glob_named_checklist_as_itself_under_ambient_icase_pathspecs() {
    // The ambient half of #70 needs `GIT_ICASE_PATHSPECS` on the process, so
    // the test above is re-run in a child copy of the test binary.
    let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_sync::sync::tests::run_sync_commits_a_checklist_whose_name_has_glob_characters_as_itself",
                "--test-threads=1",
            ])
            .env("GIT_ICASE_PATHSPECS", "1")
            .output()
            .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn commit_message_prefixes_the_file_name() {
    assert_eq!(
        commit_message(Path::new("/a/b/checklist.md"), "Check \"x\""),
        "checklist.md: Check \"x\""
    );
}
#[test]
fn commit_message_truncates_a_long_description() {
    let long_item = "x".repeat(100);
    let message = commit_message(
        Path::new("/a/b/checklist.md"),
        &format!("Check \"{long_item}\""),
    );
    assert_eq!(message.chars().count(), 80);
    assert!(message.starts_with("checklist.md: Check \"xxx"));
    assert!(
        message.ends_with("\u{2026}\""),
        "closes the quote opened by the item text: {message:?}"
    );
    assert_eq!(message.matches('"').count(), 2);
}
#[test]
fn commit_message_truncation_without_a_quote_gets_no_closing_quote() {
    let message = commit_message(
        Path::new("/a/b/checklist.md"),
        &format!(
            "Reset all tasks to not done and then some more {}",
            "x".repeat(50)
        ),
    );
    assert_eq!(message.chars().count(), 80);
    assert!(message.ends_with('\u{2026}'));
    assert!(!message.ends_with("\u{2026}\""));
}
#[test]
fn commit_message_stays_within_the_cap_for_a_very_long_file_name() {
    for name_len in [MAX_FILE_NAME_LEN - 1, MAX_FILE_NAME_LEN, 79, 100, 200] {
        let name = format!("{}.md", "x".repeat(name_len));
        let message = commit_message(
            Path::new(&format!("/a/b/{name}")),
            "Check \"a task with a reasonably long title\"",
        );
        assert!(
            message.chars().count() <= MAX_COMMIT_MESSAGE_LEN,
            "name_len {name_len}: {} chars: {message:?}",
            message.chars().count()
        );
        assert!(
            message.contains(": "),
            "the prefix separator survives: {message:?}"
        );
    }
}
#[test]
fn commit_message_keeps_an_ordinary_file_name_intact() {
    // A normal file name is untouched.
    let message = commit_message(Path::new("/a/b/deployment-runbook.md"), "Check \"x\"");
    assert!(message.starts_with("deployment-runbook.md: "), "{message}");
}
#[test]
fn commit_message_leaves_a_short_description_untruncated() {
    let message = commit_message(Path::new("/a/b/checklist.md"), "Check \"short\"");
    assert_eq!(message, "checklist.md: Check \"short\"");
    assert!(!message.contains('\u{2026}'));
}
#[test]
fn an_untracked_checklist_is_reported_even_when_git_status_stays_silent() {
    // `git status` is silent about an untracked file in both configurations.
    for (label, configure) in [
        (
            "status.showUntrackedFiles=no",
            &["config", "status.showUntrackedFiles", "no"][..],
        ),
        (
            "gitignored",
            &["config", "core.excludesFile", ".ignore"][..],
        ),
    ] {
        let (work, _remote) = init_repo_with_remote();
        run(&work, configure);
        if label == "gitignored" {
            fs::write(work.join(".ignore"), "untracked.md\n").unwrap();
        }
        let untracked = work.join("untracked.md");
        fs::write(&untracked, "- [ ] new\n").unwrap();

        assert_eq!(
            git_stdout(&work, &["status", "--porcelain", "--", "untracked.md"]),
            "",
            "{label}: test setup expects git status to say nothing"
        );

        assert_eq!(
            run_sync(
                &work,
                &untracked,
                "- [ ] new\n",
                "should not commit",
                &no_race("- [ ] new\n"),
                None,
            ),
            SyncOutcome::SkippedUntracked,
            "{label}: an untracked checklist must still be reported"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
// --- Check-then-race-then-use interleavings ---

#[test]
fn an_unrelated_commit_landing_after_validation_is_not_published_by_the_fast_path() {
    // The guard clears the branch, then an unrelated commit lands, then the
    // push runs.
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let content = "- [x] one\n";
    // Committed but unpushed, so HEAD already holds the synced content.
    fs::write(&file, content).unwrap();
    run(&work, &["commit", "-q", "-am", "checklist"]);
    let validated = git_stdout(&work, &["rev-parse", "HEAD"]);
    // Keeps `git status` non-empty, reaching the fast path.
    fs::write(&file, "- [x] one\n- [ ] two\n").unwrap();

    let racer = work.clone();
    let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
        commit_unrelated(&racer, "other.txt");
    });

    let outcome = run_sync(
        &work,
        &file,
        content,
        &commit_message(&file, "Check \"one\""),
        &no_race(content),
        None,
    );

    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["rev-parse", "main"]),
        validated,
        "the remote must hold exactly the validated tip, not the commit that raced in"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn a_commit_landing_before_ours_is_detected_rather_than_built_on_stale_state() {
    // HEAD moves between the guards and the commit; committing anyway would
    // revert the commit that raced in.
    let (work, _remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let content = "- [x] one\n";
    fs::write(&file, content).unwrap();

    let racer = work.clone();
    let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
        let sha = commit_unrelated(&racer, "other.txt");
        fs::write(racer.join(".racer-sha"), &sha).unwrap();
    });

    let outcome = run_sync(
        &work,
        &file,
        content,
        &commit_message(&file, "Check \"one\""),
        &no_race(content),
        None,
    );

    // The commit markcheck did not make must still be on the branch.
    assert!(
        matches!(&outcome, SyncOutcome::Failed(_)),
        "the sync must not succeed on a superseded parent: {outcome:?}"
    );
    let unrelated = racer_sha_probe(&work);
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s", "main"]),
        "unrelated\ninit",
        "the commit that raced in must still be on the branch"
    );
    assert_eq!(
        git_stdout(&work, &["rev-parse", "HEAD"]),
        unrelated,
        "HEAD must be the concurrent commit, not rewound past it"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn a_version_staged_during_the_sync_survives_index_alignment() {
    let (work, _remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    let content = "- [x] one\n";
    fs::write(&file, content).unwrap();

    let racer = work.clone();
    let _hook = RaceHook::at(RacePoint::BeforeIndexAlignment, move || {
        fs::write(racer.join("tracked.md"), "- [ ] staged mid-sync\n").unwrap();
        run(&racer, &["add", "tracked.md"]);
    });

    let outcome = run_sync(
        &work,
        &file,
        content,
        &commit_message(&file, "Check \"one\""),
        &no_race(content),
        None,
    );

    assert!(
        matches!(
            outcome,
            SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        staged_bytes(&work, "tracked.md").as_deref(),
        Some(&b"- [ ] staged mid-sync\n"[..]),
        "a version staged during the sync must not be overwritten by the realignment"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
// --- Two concurrent syncs in one repository ---

#[test]
fn a_second_sync_committing_mid_flight_cannot_revert_the_first() {
    // Two instances on different checklists in the same repository: the
    // loser's temp index, seeded from the same tip, must not revert the
    // winner's file.
    let (work, _remote) = init_repo_with_remote();
    let a = work.join("a.md");
    let b = work.join("b.md");
    fs::write(&a, "- [ ] a\n").unwrap();
    fs::write(&b, "- [ ] b\n").unwrap();
    run(&work, &["add", "a.md", "b.md"]);
    run(&work, &["commit", "-q", "-m", "add both checklists"]);
    run(&work, &["push", "-q", "origin", "main"]);

    let racer_dir = work.clone();
    let racer_a = a.clone();
    let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
        fs::write(&racer_a, "- [x] a\n").unwrap();
        let outcome = run_sync(
            &racer_dir,
            &racer_a,
            "- [x] a\n",
            "a.md: Check \"a\"",
            &no_race("- [x] a\n"),
            None,
        );
        assert!(
            matches!(
                outcome,
                SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
            ),
            "the other instance's sync should land: {outcome:?}"
        );
    });

    fs::write(&b, "- [x] b\n").unwrap();
    let outcome = run_sync(
        &work,
        &b,
        "- [x] b\n",
        "b.md: Check \"b\"",
        &no_race("- [x] b\n"),
        None,
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(_)),
        "the losing sync must refuse, not commit a reverting tree: {outcome:?}"
    );
    assert_eq!(
        git_stdout(&work, &["show", "HEAD:a.md"]),
        "- [x] a",
        "the other instance's commit must survive untouched"
    );
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s", "-1", "main"]),
        "a.md: Check \"a\"",
        "and must still be the branch tip"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn a_configured_but_unresolvable_upstream_does_not_bypass_the_unrelated_guard() {
    // The config names an upstream whose tracking ref doesn't exist. `push` and
    // the guard must agree there is no upstream.
    let root = unique_dir("upstream-disagreement");
    let remote = root.join("remote.git");
    let work = root.join("work");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&work).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    run(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    // Config only, so no remote-tracking ref exists.
    run(&work, &["config", "branch.main.remote", "origin"]);
    run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
    assert!(
        upstream_parts(&work).is_some(),
        "test setup: the config claims an upstream"
    );
    assert_eq!(
        resolve_upstream(&work),
        Ok(None),
        "test setup: but @{{u}} does not resolve"
    );

    fs::write(work.join("secret.txt"), "unrelated\n").unwrap();
    run(&work, &["add", "secret.txt"]);
    run(&work, &["commit", "-q", "-m", "unrelated work"]);

    let file = work.join("tracked.md");
    let expected = "- [x] one\n";
    fs::write(&file, expected).unwrap();
    let outcome = run_sync(
        &work,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        None,
    );

    assert!(
        !matches!(outcome, SyncOutcome::Synced),
        "must not publish when the unrelated-commits guard could not run: {outcome:?}"
    );
    assert_eq!(
        git_stdout(&remote, &["log", "--oneline", "-1", "main"]),
        "",
        "the unrelated commit must not have reached the remote"
    );

    fs::remove_dir_all(&root).ok();
}
// --- Randomised operation sequences ---

/// A tiny deterministic PRNG, seeded per case so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
#[test]
fn randomised_sequences_never_lose_a_commit_markcheck_did_not_make() {
    // markcheck must never move the branch across a commit it did not create.
    for seed in 0..10u64 {
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let broken = work.parent().unwrap().join("gone.git");
        let good = remote.clone();

        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut foreign: Vec<String> = Vec::new();
        let mut content = "- [ ] one\n".to_string();
        fs::write(&file, &content).unwrap();

        for step in 0..10 {
            match rng.below(5) {
                0 => {
                    let previous = content.clone();
                    content = if content.contains("- [ ] one") {
                        content.replace("- [ ] one", "- [x] one")
                    } else {
                        content.replace("- [x] one", "- [ ] one")
                    };
                    fs::write(&file, &content).unwrap();
                    let _ = run_sync(
                        &work,
                        &file,
                        &content,
                        &commit_message(&file, "Toggle"),
                        &no_race(&content),
                        Some(previous.as_str()),
                    );
                }
                1 => {
                    content = format!("{content}- [ ] edit{step}\n");
                    fs::write(&file, &content).unwrap();
                }
                2 => {
                    let name = format!("foreign{step}.txt");
                    fs::write(work.join(&name), "not the checklist\n").unwrap();
                    run(&work, &["add", &name]);
                    run(&work, &["commit", "-q", "-m", &format!("foreign {step}")]);
                    foreign.push(git_stdout(&work, &["rev-parse", "HEAD"]));
                }
                3 => {
                    run(&work, &["add", "tracked.md"]);
                }
                _ => {
                    let url = if rng.below(2) == 0 {
                        broken.to_str().unwrap()
                    } else {
                        good.to_str().unwrap()
                    };
                    run(&work, &["remote", "set-url", "origin", url]);
                }
            }

            for sha in &foreign {
                assert!(
                    still_on_branch(&work, sha),
                    "seed {seed}, step {step}: commit {sha} was made outside markcheck \
                         and must still be on the branch"
                );
            }
            assert!(
                fs::read_to_string(&file).is_ok_and(|on_disk| !on_disk.is_empty()),
                "seed {seed}, step {step}: the checklist must survive every sequence"
            );
            // A commit markcheck did not make must never be pushed.
            for sha in &foreign {
                assert!(
                    !reached_the_remote(&remote, sha),
                    "seed {seed}, step {step}: commit {sha} is not the checklist's \
                         and must never have been published"
                );
            }
        }

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
// --- Hostile repository shapes ---

#[test]
fn run_sync_works_when_the_checklist_lives_in_a_subdirectory() {
    let (work, remote) = init_repo_with_remote();
    let sub = work.join("docs").join("runbooks");
    fs::create_dir_all(&sub).unwrap();
    let file = sub.join("list.md");
    fs::write(&file, "- [ ] one\n").unwrap();
    run(&work, &["add", "docs/runbooks/list.md"]);
    run(&work, &["commit", "-q", "-m", "add nested checklist"]);
    run(&work, &["push", "-q", "origin", "main"]);

    let expected = "- [x] one\n";
    fs::write(&file, expected).unwrap();
    let outcome = run_sync(
        &work,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        None,
    );

    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["show", "HEAD:docs/runbooks/list.md"]),
        expected.trim_end(),
        "the nested path must be committed at its repo-root-relative location"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_works_inside_a_linked_worktree() {
    // A linked worktree's MERGE_HEAD and temp index live under
    // `.git/worktrees/<name>`.
    let (work, remote) = init_repo_with_remote();
    let linked = work.parent().unwrap().join("linked");
    run(
        &work,
        &[
            "worktree",
            "add",
            "-b",
            "wt",
            linked.to_str().unwrap(),
            "main",
        ],
    );
    run(&linked, &["config", "user.email", "test@example.com"]);
    run(&linked, &["config", "user.name", "test"]);
    run(&linked, &["push", "-q", "-u", "origin", "wt"]);
    assert!(
        linked.join(".git").is_file(),
        "test setup: a linked worktree's .git is a file"
    );

    let file = linked.join("tracked.md");
    let expected = "- [x] one\n";
    fs::write(&file, expected).unwrap();
    let outcome = run_sync(
        &linked,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        None,
    );

    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["show", "wt:tracked.md"]),
        expected.trim_end()
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn detect_and_sync_cope_with_a_shallow_clone() {
    // The unrelated-history guard walks a range reaching the graft boundary.
    let (origin, remote) = init_repo_with_remote();
    for n in 1..=3 {
        fs::write(origin.join("tracked.md"), format!("- [ ] {n}\n")).unwrap();
        run(&origin, &["commit", "-q", "-am", &format!("edit {n}")]);
    }
    run(&origin, &["push", "-q", "origin", "main"]);

    let shallow = origin.parent().unwrap().join("shallow");
    run(
        origin.parent().unwrap(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            remote.to_str().unwrap(),
            shallow.to_str().unwrap(),
        ],
    );
    run(&shallow, &["config", "user.email", "test@example.com"]);
    run(&shallow, &["config", "user.name", "test"]);

    let file = shallow.join("tracked.md");
    let expected = "- [x] shallow\n";
    fs::write(&file, expected).unwrap();
    let outcome = run_sync(
        &shallow,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        None,
    );

    assert_eq!(
        outcome,
        SyncOutcome::Synced,
        "a shallow clone must still sync: {outcome:?}"
    );

    fs::remove_dir_all(origin.parent().unwrap()).ok();
}
// --- Staged-target guard ---

#[test]
fn run_sync_refuses_when_a_staged_checklist_version_would_be_dropped() {
    // A staged snapshot that differs from the working tree would be lost.
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");

    // The user stages a version carrying work that exists only in the index...
    fs::write(&file, "- [ ] one\n- [ ] IMPORTANT\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    let staged = index_blob(&work, "tracked.md").unwrap();

    // ...then keeps editing, so the working tree no longer has it.
    let pre_write = "- [ ] one\n- [ ] later\n";
    fs::write(&file, pre_write).unwrap();

    let expected = "- [x] one\n- [ ] later\n";
    fs::write(&file, expected).unwrap();

    let outcome = run_sync(
        &work,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        staged_matches(pre_write),
    );

    assert!(
        matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("staged changes")),
        "{outcome:?}"
    );
    assert_eq!(
        index_blob(&work, "tracked.md").as_deref(),
        Some(staged.as_str()),
        "the staged version must still be staged"
    );
    assert_eq!(
        git_stdout(&work, &["log", "--format=%s", "main"]),
        "init",
        "no commit may be built"
    );
    assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

    fs::remove_dir_all(work.parent().unwrap()).ok();
}
#[test]
fn run_sync_proceeds_when_the_staged_checklist_matches_the_working_tree() {
    // Staging and then toggling loses nothing, so it is allowed.
    let (work, remote) = init_repo_with_remote();
    let file = work.join("tracked.md");

    let pre_write = "- [ ] one\n- [ ] extra\n";
    fs::write(&file, pre_write).unwrap();
    run(&work, &["add", "tracked.md"]);

    fs::write(work.join("other.txt"), "unrelated staged\n").unwrap();
    run(&work, &["add", "other.txt"]);
    let other_staged = index_blob(&work, "other.txt");

    let expected = "- [x] one\n- [ ] extra\n";
    fs::write(&file, expected).unwrap();

    let outcome = run_sync(
        &work,
        &file,
        expected,
        &commit_message(&file, "Check \"one\""),
        &no_race(expected),
        staged_matches(pre_write),
    );

    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["show", "HEAD:tracked.md"]),
        expected.trim_end(),
        "the commit carries exactly the toggled content, staged work included"
    );
    assert_eq!(
        index_blob(&work, "other.txt"),
        other_staged,
        "an unrelated staged file is still untouched"
    );

    fs::remove_dir_all(work.parent().unwrap()).ok();
}

#[test]
fn a_second_checklist_syncs_past_the_first_one_s_stranded_commit() {
    // Two git-synced checklists in one repository: a.md's commit is left
    // unpushed while the remote is away, and b.md's later toggle must still
    // commit and push, taking a.md's commit with it (#99).
    let (work, remote) = init_repo_with_remote();
    let a = work.join("tracked.md");
    let b = work.join("b.md");
    fs::write(&b, "- [ ] two\n").unwrap();
    run(&work, &["add", "b.md"]);
    run(&work, &["commit", "-q", "-m", "add b"]);
    run(&work, &["push", "-q", "origin", "main"]);

    let away = work.parent().unwrap().join("gone.git");
    run(
        &work,
        &["remote", "set-url", "origin", away.to_str().unwrap()],
    );
    fs::write(&a, "- [x] one\n").unwrap();
    let stranded = run_sync(
        &work,
        &a,
        "- [x] one\n",
        &commit_message(&a, "Check \"one\""),
        &no_race("- [x] one\n"),
        None,
    );
    assert!(
        matches!(stranded, SyncOutcome::CommittedNotPushed { .. }),
        "{stranded:?}"
    );
    run(
        &work,
        &["remote", "set-url", "origin", remote.to_str().unwrap()],
    );

    fs::write(&b, "- [x] two\n").unwrap();
    let outcome = run_sync(
        &work,
        &b,
        "- [x] two\n",
        &commit_message(&b, "Check \"two\""),
        &no_race("- [x] two\n"),
        None,
    );
    assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
    assert_eq!(
        git_stdout(&remote, &["log", "--format=%s", "-2", "main"]),
        "b.md: Check \"two\"\ntracked.md: Check \"one\""
    );
    assert_eq!(
        super::super::push::catch_up_push(&work, &b),
        SyncOutcome::Skipped,
        "opening b.md afterwards reports nothing"
    );
    fs::remove_dir_all(work.parent().unwrap()).ok();
}

#[test]
fn markcheck_s_commits_carry_its_trailer() {
    let (work, _remote) = init_repo_with_remote();
    let file = work.join("tracked.md");
    fs::write(&file, "- [x] one\n").unwrap();
    let outcome = run_sync(
        &work,
        &file,
        "- [x] one\n",
        &commit_message(&file, "Check \"one\""),
        &no_race("- [x] one\n"),
        None,
    );
    assert_eq!(outcome, SyncOutcome::Synced);
    assert_eq!(
        git_stdout(
            &work,
            &["log", "-1", "--format=%(trailers:key=Synced-by,valueonly)"]
        ),
        "markcheck"
    );
}
