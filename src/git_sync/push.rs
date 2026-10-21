//! Publishing a commit, and the startup catch-up. Every push names an
//! explicit `<sha>:<branch-ref>`, so it can never publish more than what was
//! validated.

use std::path::Path;

use super::guards::{
    Publishable, UnpushedHistory, repo_sync_blocked, unpushed_check_failed, unpushed_history,
};
use super::inspect::{
    RemoteBranch, commits_ahead_of_upstream, current_head, index_entry, is_ancestor,
    push_goes_to_upstream, push_url, remote_branch_state, resolve_upstream, upstream_parts,
};
use super::process::{PUSH_TIMEOUT, command_error, git_command, run_with_timeout};
use super::shared::{RacePoint, RemoteCheck, SyncOutcome, race_point};

/// Pushes `expected_commit` as an explicit refspec. A failure is
/// `CommittedNotPushed`, since the commit is safely local.
///
/// An empty `expected_commit` is `Failed`: `<remote> :<branch>` would delete
/// the remote branch. With no resolvable upstream the push is refused rather
/// than falling back to a bare `git push`, which can publish the whole branch.
pub(super) fn push(repo_root: &Path, expected_commit: &str) -> SyncOutcome {
    if expected_commit.is_empty() {
        return SyncOutcome::Failed("could not work out which commit to push".to_string());
    }
    let PushTarget {
        remote,
        branch_ref,
        url,
    } = match push_target(repo_root) {
        Ok(target) => target,
        Err(message) => {
            return SyncOutcome::CommittedNotPushed {
                message,
                commit: expected_commit.to_string(),
            };
        }
    };
    // Ask the remote: an explicit-SHA refspec recreates a deleted branch.
    let remote_tip = match remote_branch_state(repo_root, &url, &branch_ref) {
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
    if !is_ancestor(repo_root, &remote_tip, expected_commit) {
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
    let mut cmd = git_command(repo_root);
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
/// Where a git-sync push goes: the upstream's remote and branch, and the
/// URL `git push` sends to.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PushTarget {
    pub(super) remote: String,
    pub(super) branch_ref: String,
    pub(super) url: String,
}

/// The [`PushTarget`], or why git sync can't push this branch.
pub(super) fn push_target(repo_root: &Path) -> Result<PushTarget, String> {
    // The resolvable `@{u}`, not the config, decides whether there is an
    // upstream, so this agrees with the unpushed-history guard.
    let no_upstream = || "this branch has no upstream yet; run `git push -u` once".to_string();
    if !matches!(resolve_upstream(repo_root), Ok(Some(_))) {
        return Err(no_upstream());
    }
    let Some((remote, branch_ref)) = upstream_parts(repo_root) else {
        return Err(no_upstream());
    };
    // A remote of `.` is this repository: the "push" would move another
    // local branch and publish nothing.
    if remote == "." {
        let local = branch_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch_ref);
        return Err(format!(
            "this branch tracks the local branch {local}, not a remote; push it yourself"
        ));
    }
    // The upstream is where changes are fetched from, not necessarily where
    // `git push` sends them (#98).
    if !push_goes_to_upstream(repo_root, &remote) {
        return Err(format!(
            "`git push` would not push this branch to {branch_ref} on {remote}; push it yourself"
        ));
    }
    // Ask the repository the push will reach, which `pushurl` can make
    // different from the one `ls-remote <remote>` would ask.
    let Some(url) = push_url(repo_root, &remote) else {
        return Err(format!("could not work out where {remote} pushes to"));
    };
    Ok(PushTarget {
        remote,
        branch_ref,
        url,
    })
}
/// Pushes only if `HEAD` is still `expected_commit`; otherwise reports
/// `CommittedNotPushed` so the retry machinery tries again.
pub(super) fn push_if_head_unchanged(repo_root: &Path, expected_commit: &str) -> SyncOutcome {
    if current_head(repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::CommittedNotPushed {
            message: "the repository changed after the commit was made".to_string(),
            commit: expected_commit.to_string(),
        };
    }
    push(repo_root, expected_commit)
}
/// Retries pushing a specific commit, silently giving up if `HEAD` has moved
/// on: rebuilding from the stale content would revert what superseded it.
pub(super) fn retry_commit(repo_root: &Path, expected_commit: &str) -> SyncOutcome {
    if current_head(repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::RetryAbandoned;
    }
    push(repo_root, expected_commit)
}
/// Asks the remote the push would reach whether its branch has moved past
/// `head`, without fetching anything: `RemoteAhead` when it has, so the user
/// pulls before ticking rather than learning it from a refused push (#253).
/// A remote that can't be reached, or a branch git-sync wouldn't push, is a
/// quiet `Skipped`: a tick reports those in its own words.
pub(super) fn remote_has_moved_on(repo_root: &Path, head: &str) -> SyncOutcome {
    match remote_check(repo_root, head) {
        RemoteCheck::Ahead(message) => SyncOutcome::RemoteAhead(message),
        _ => SyncOutcome::Skipped,
    }
}
/// The [`RemoteCheck`] behind [`remote_has_moved_on`], kept apart so
/// `--doctor` can tell an unreachable remote from an up-to-date one.
pub(super) fn remote_check(repo_root: &Path, head: &str) -> RemoteCheck {
    let Some((remote, branch_ref)) = upstream_parts(repo_root) else {
        return RemoteCheck::NotChecked;
    };
    if remote == "." || !push_goes_to_upstream(repo_root, &remote) {
        return RemoteCheck::NotChecked;
    }
    let Some(url) = push_url(repo_root, &remote) else {
        return RemoteCheck::NotChecked;
    };
    match remote_branch_state(repo_root, &url, &branch_ref) {
        RemoteBranch::Present(tip) if !is_ancestor(repo_root, &tip, head) => {
            RemoteCheck::Ahead(format!(
                "{branch_ref} on {remote} has changes this checklist hasn't pulled; \
                 run `git pull --rebase` before ticking"
            ))
        }
        RemoteBranch::Present(_) => RemoteCheck::UpToDate,
        RemoteBranch::Absent => RemoteCheck::Absent,
        RemoteBranch::Unknown => RemoteCheck::Unreachable,
    }
}
/// [`remote_has_moved_on`] for the current `HEAD`, for a reload: the file
/// changed on disk, perhaps from a pull.
pub(super) fn check_remote(repo_root: &Path) -> SyncOutcome {
    match current_head(repo_root) {
        Some(head) => remote_has_moved_on(repo_root, &head),
        None => SyncOutcome::Skipped,
    }
}
/// Pushes a commit an earlier session left local-only, and never builds a
/// commit: opening a checklist must not publish uncommitted edits. An
/// untracked checklist is always reported, since git-sync can never act on
/// it; a blocked repository, or a history that can't be checked, is
/// `Failed`; nothing to push, or a tip that can't be read, is a quiet
/// `Skipped`, unless the remote has moved on (`RemoteAhead`). Only commits
/// carrying markcheck's own trailer are pushed (#62): one the user made and
/// kept local is left alone, with `NotCaughtUp` saying so.
pub(super) fn catch_up_push(repo_root: &Path, file_path: &Path) -> SyncOutcome {
    if let Some(reason) = repo_sync_blocked(repo_root) {
        return SyncOutcome::Failed(reason);
    }
    // A lookup error only matters once there is something to push.
    let relpath = match index_entry(repo_root, file_path) {
        Ok(Some(entry)) => Ok(entry.path),
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => Err(err),
    };
    // Capture the tip before validating anything, and push that exact SHA.
    let Some(head) = current_head(repo_root) else {
        return SyncOutcome::Skipped;
    };
    match commits_ahead_of_upstream(repo_root, &head) {
        None => return SyncOutcome::Skipped,
        Some(0) => return remote_has_moved_on(repo_root, &head),
        Some(_) => {}
    }
    let relpath = match relpath {
        Ok(relpath) => relpath,
        Err(err) => return SyncOutcome::Failed(err),
    };
    let pushed = match unpushed_history(repo_root, Some(&head), &relpath, Publishable::OwnOnly) {
        Ok(UnpushedHistory::Safe(commits)) => commits,
        Ok(UnpushedHistory::NoUpstream) => return SyncOutcome::Skipped,
        Ok(UnpushedHistory::Blocked(blocking)) => {
            return SyncOutcome::NotCaughtUp(format!(
                "{} is not pushed and wasn't made by this version of markcheck, so nothing \
                 was pushed at startup; push it yourself",
                blocking.describe()
            ));
        }
        Err(err) => return SyncOutcome::Failed(unpushed_check_failed(&err)),
    };
    race_point(RacePoint::AfterHistoryValidation);
    match push(repo_root, &head) {
        SyncOutcome::Synced => SyncOutcome::CaughtUp { pushed },
        other => other,
    }
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;
