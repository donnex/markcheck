//! Publishing a commit, and the startup catch-up. Every push names an
//! explicit `<sha>:<branch-ref>`, so it can never publish more than what was
//! validated.

use std::path::Path;

use super::guards::{repo_sync_blocked, unpushed_history_blocks};
use super::inspect::{
    RemoteBranch, commits_ahead_of_upstream, current_head, index_entry, is_ancestor,
    push_goes_to_upstream, push_url, remote_branch_state, resolve_upstream, upstream_parts,
};
use super::process::{PUSH_TIMEOUT, command_error, git_command, run_with_timeout};
use super::shared::{RacePoint, SyncOutcome, race_point};

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
    // The resolvable `@{u}`, not the config, decides whether there is an
    // upstream, so this agrees with the unpushed-history guard.
    let no_upstream = SyncOutcome::CommittedNotPushed {
        message: "this branch has no upstream yet; \
                  run `git push -u` once"
            .to_string(),
        commit: expected_commit.to_string(),
    };
    if !matches!(resolve_upstream(repo_root), Ok(Some(_))) {
        return no_upstream;
    }
    let Some((remote, branch_ref)) = upstream_parts(repo_root) else {
        return no_upstream;
    };
    // A remote of `.` is this repository: the "push" would move another
    // local branch and publish nothing.
    if remote == "." {
        let local = branch_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&branch_ref);
        return SyncOutcome::CommittedNotPushed {
            message: format!(
                "this branch tracks the local branch {local}, not a remote; \
                 push it yourself"
            ),
            commit: expected_commit.to_string(),
        };
    }
    // The upstream is where changes are fetched from, not necessarily where
    // `git push` sends them (#98).
    if !push_goes_to_upstream(repo_root, &remote) {
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
    let Some(url) = push_url(repo_root, &remote) else {
        return SyncOutcome::CommittedNotPushed {
            message: format!("could not work out where {remote} pushes to"),
            commit: expected_commit.to_string(),
        };
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
/// Pushes a commit an earlier session left local-only, and never builds a
/// commit: opening a checklist must not publish uncommitted edits. An
/// untracked checklist is always reported, since git-sync can never act on
/// it; a blocked repository or a refused history is `Failed`; nothing to
/// push, or a tip that can't be read, is a quiet `Skipped`.
pub(super) fn catch_up_push(repo_root: &Path, file_path: &Path) -> SyncOutcome {
    if let Some(reason) = repo_sync_blocked(repo_root) {
        return SyncOutcome::Failed(reason);
    }
    // A lookup error only matters once there is something to push.
    let relpath = match index_entry(repo_root, file_path) {
        Ok(Some((_mode, relpath))) => Ok(relpath),
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => Err(err),
    };
    // Capture the tip before validating anything, and push that exact SHA.
    let Some(head) = current_head(repo_root) else {
        return SyncOutcome::Skipped;
    };
    if commits_ahead_of_upstream(repo_root, &head).is_none_or(|ahead| ahead == 0) {
        return SyncOutcome::Skipped;
    }
    let relpath = match relpath {
        Ok(relpath) => relpath,
        Err(err) => return SyncOutcome::Failed(err),
    };
    if let Some(refusal) = unpushed_history_blocks(repo_root, Some(&head), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);
    push(repo_root, &head)
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;
