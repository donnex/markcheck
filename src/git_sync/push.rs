//! Publishing a commit, and the startup catch-up. Every push names an
//! explicit `<sha>:<branch-ref>`, so it can never publish more than what was
//! validated.

use std::path::{Path, PathBuf};

use super::guards::{repo_sync_blocked, unpushed_history_blocks};
use super::inspect::{
    RemoteBranch, commits_ahead_of_upstream, current_head, index_entry, is_ancestor,
    push_goes_to_upstream, push_url, remote_branch_state, resolve_upstream, upstream_parts,
};
use super::process::{
    PLUMBING_TIMEOUT, PUSH_TIMEOUT, command_error, git_command, run_with_timeout,
};
use super::shared::{RacePoint, SyncOutcome, race_point};

/// Pushes `expected_commit` as an explicit refspec. A failure is
/// `CommittedNotPushed`, since the commit is safely local.
///
/// An empty `expected_commit` is `Failed`: `<remote> :<branch>` would delete
/// the remote branch. With no resolvable upstream the push is refused rather
/// than falling back to a bare `git push`, which can publish the whole branch.
pub(super) fn push(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    if expected_commit.is_empty() {
        return SyncOutcome::Failed("could not work out which commit to push".to_string());
    }
    // The resolvable `@{u}`, not the config, decides whether there is an
    // upstream, so this agrees with the unpushed-history guard.
    let no_upstream = SyncOutcome::CommittedNotPushed {
        message: "this branch has no upstream yet; \
                  run `git push -u` once"
            .to_string(),
        commit: expected_commit.to_string(),
    };
    if !matches!(resolve_upstream(repo_dir), Ok(Some(_))) {
        return no_upstream;
    }
    let Some((remote, branch_ref)) = upstream_parts(repo_dir) else {
        return no_upstream;
    };
    // The upstream is where changes are fetched from, not necessarily where
    // `git push` sends them (#98).
    if !push_goes_to_upstream(repo_dir, &remote) {
        return SyncOutcome::CommittedNotPushed {
            message: format!(
                "`git push` would not push this branch to {branch_ref} on \
                 {remote}; push it yourself"
            ),
            commit: expected_commit.to_string(),
        };
    }
    // Ask the repository the push will reach, which `pushurl` can make
    // different from the one `ls-remote <remote>` would ask.
    let Some(url) = push_url(repo_dir, &remote) else {
        return SyncOutcome::CommittedNotPushed {
            message: format!("could not work out where {remote} pushes to"),
            commit: expected_commit.to_string(),
        };
    };
    // Ask the remote: an explicit-SHA refspec recreates a deleted branch.
    let remote_tip = match remote_branch_state(repo_dir, &url, &branch_ref) {
        RemoteBranch::Present(sha) => sha,
        RemoteBranch::Absent => {
            return SyncOutcome::CommittedNotPushed {
                message: format!(
                    "{branch_ref} no longer exists on {remote}; \
                     pushing would recreate it"
                ),
                commit: expected_commit.to_string(),
            };
        }
        // Refuse when the remote can't be consulted; the push would not have
        // reached it either.
        RemoteBranch::Unknown => {
            return SyncOutcome::CommittedNotPushed {
                message: format!(
                    "could not reach {remote} to check whether {branch_ref} \
                     still exists"
                ),
                commit: expected_commit.to_string(),
            };
        }
    };

    // Only ever a fast-forward. Checked here because the lease below permits a
    // non-fast-forward whenever it matches.
    if !is_ancestor(repo_dir, &remote_tip, expected_commit) {
        return SyncOutcome::CommittedNotPushed {
            message: format!(
                "{branch_ref} on {remote} has commits this change is not based on; \
                 pull first, then it will sync"
            ),
            commit: expected_commit.to_string(),
        };
    }

    race_point(RacePoint::BeforePush);

    // The lease makes the remote check hold across the two round trips: git
    // rejects the push if the branch was deleted or moved in between.
    let mut cmd = git_command(repo_dir);
    cmd.args([
        "push",
        &format!("--force-with-lease={branch_ref}:{remote_tip}"),
        &remote,
        &format!("{expected_commit}:{branch_ref}"),
    ]);
    let output = run_with_timeout(cmd, PUSH_TIMEOUT);
    match output {
        Ok(output) if output.status.success() => SyncOutcome::Synced,
        Ok(output) => SyncOutcome::CommittedNotPushed {
            message: command_error("git push", &output),
            commit: expected_commit.to_string(),
        },
        Err(err) => SyncOutcome::CommittedNotPushed {
            message: format!("git push failed: {err}"),
            commit: expected_commit.to_string(),
        },
    }
}
/// Pushes only if `HEAD` is still `expected_commit`; otherwise reports
/// `CommittedNotPushed` so the retry machinery tries again.
pub(super) fn push_if_head_unchanged(
    repo_dir: &Path,
    repo_root: &Path,
    expected_commit: &str,
) -> SyncOutcome {
    if current_head(repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::CommittedNotPushed {
            message: "the repository changed after the commit was made".to_string(),
            commit: expected_commit.to_string(),
        };
    }
    push(repo_dir, expected_commit)
}
/// Retries pushing a specific commit, silently giving up if `HEAD` has moved
/// on: rebuilding from the stale content would revert what superseded it.
pub(super) fn retry_commit(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };
    if current_head(&repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::RetryAbandoned;
    }
    push(repo_dir, expected_commit)
}
/// Pushes a commit an earlier session left local-only, and never builds a
/// commit: opening a checklist must not publish uncommitted edits. An
/// untracked checklist is always reported, since git-sync can never act on
/// it; a blocked repository or a refused history is `Failed`; nothing to
/// push, or a tip that can't be read, is a quiet `Skipped`.
pub(super) fn catch_up_push(repo_dir: &Path, file_path: &Path) -> SyncOutcome {
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };
    if let Some(reason) = repo_sync_blocked(&repo_root) {
        return SyncOutcome::Failed(reason);
    }
    // A lookup error only matters once there is something to push.
    let relpath = match index_entry(repo_dir, file_path) {
        Ok(Some((_mode, relpath))) => Ok(relpath),
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => Err(err),
    };
    // Capture the tip before validating anything, and push that exact SHA.
    let Some(head) = current_head(&repo_root) else {
        return SyncOutcome::Skipped;
    };
    if commits_ahead_of_upstream(&repo_root, &head).is_none_or(|ahead| ahead == 0) {
        return SyncOutcome::Skipped;
    }
    let relpath = match relpath {
        Ok(relpath) => relpath,
        Err(err) => return SyncOutcome::Failed(err),
    };
    if let Some(refusal) = unpushed_history_blocks(&repo_root, Some(&head), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);
    push(repo_dir, &head)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn push_if_head_unchanged_refuses_to_push_a_commit_that_landed_after_verification() {
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, commit }
                if message.contains("changed after the commit was made") && commit == &created_commit),
            "{outcome:?}"
        );

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote_log.stdout).trim(),
            "init",
            "neither the markcheck commit nor the concurrent one must reach the remote"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn push_if_head_unchanged_pushes_normally_when_nothing_raced_it() {
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&remote_log.stdout).contains("markcheck commit"),
            "{}",
            String::from_utf8_lossy(&remote_log.stdout)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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
        let branches = Command::new("git")
            .current_dir(&remote)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&branches.stdout);
        assert!(
            branches.lines().any(|b| b == branch),
            "the remote branch must still exist: {branches:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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
            !matches!(outcome, SyncOutcome::Synced),
            "pushing over a tip that moved must not report success: {outcome:?}"
        );
        assert_ne!(
            after, before,
            "test setup: the racer must have moved the branch"
        );
        assert_ne!(
            after, commit,
            "the commit that raced in must still be the branch tip"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        let remote_log = Command::new("git")
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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
        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn an_unrelated_commit_landing_after_validation_is_not_published_by_catch_up() {
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let validated = git_stdout(&work, &["rev-parse", "HEAD"]);

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
            commit_unrelated(&racer, "other.txt");
        });

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "main"]),
            validated,
            "catch-up must publish only the tip it validated"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_pushes_a_local_only_commit_without_committing_a_dirty_tree() {
        // The existing commit goes out; the uncommitted edit does not.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        fs::write(&file, "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "tracked.md: Check \"one\""]);
        let dirty = "- [x] one\n- [ ] two\n";
        fs::write(&file, dirty).unwrap();

        let outcome = catch_up_push(&work, &file);

        assert_eq!(outcome, SyncOutcome::Synced);
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_is_a_no_op_when_everything_is_already_pushed() {
        let (work, remote) = init_repo_with_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );
        assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_stays_silent_when_no_upstream_is_configured() {
        let work = init_repo_without_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_refuses_when_unrelated_commits_are_unpushed() {
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg)
                if msg.contains("not part of this change")),
            "{outcome:?}"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "init",
            "the unrelated commit must not be published"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_is_quiet_about_an_index_lookup_error_with_nothing_to_push() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("other.md"), "x\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "other"]);
        run(&work, &["push", "-q"]);

        assert_eq!(catch_up_push(&work, &work), SyncOutcome::Skipped);

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
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

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
