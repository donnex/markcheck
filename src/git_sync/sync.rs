//! The sync itself: `run_sync` classifies the file, runs the guards, builds
//! the commit, verifies what landed and publishes it. Each guard runs before
//! the work it protects.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::model::hash_bytes;

use super::commit::commit_via_temp_index;
use super::guards::{
    STAGED_TARGET_REFUSAL, repo_sync_blocked, staged_target_would_be_lost, unpushed_history_blocks,
};
use super::inspect::{
    ahead_of_upstream, blob_at, blob_bytes_at, hash_object, index_entry, resolve_parent,
};
use super::process::{
    PLUMBING_TIMEOUT, command_error, git_command, literal_pathspec_command, run_with_timeout,
};
use super::push::{push, push_if_head_unchanged};
use super::shared::{RacePoint, SyncOutcome, lock_hash, race_point};

/// One-line commit messages are capped here; the description is cut first,
/// marked with `…`.
const MAX_COMMIT_MESSAGE_LEN: usize = 80;
/// The smallest description budget worth keeping after a cut.
const MIN_DESC_LEN: usize = 8;
/// Bounds the file-name prefix so a very long name can't push the message
/// past `MAX_COMMIT_MESSAGE_LEN`.
const MAX_FILE_NAME_LEN: usize = MAX_COMMIT_MESSAGE_LEN - (2 + MIN_DESC_LEN + 1);
pub(super) fn commit_message(file_path: &Path, change_desc: &str) -> String {
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checklist".to_string());
    let file_name = if file_name.chars().count() > MAX_FILE_NAME_LEN {
        let kept: String = file_name
            .chars()
            .take(MAX_FILE_NAME_LEN.saturating_sub(1))
            .collect();
        format!("{kept}\u{2026}")
    } else {
        file_name
    };
    let prefix = format!("{file_name}: ");
    let full = format!("{prefix}{change_desc}");
    if full.chars().count() <= MAX_COMMIT_MESSAGE_LEN {
        return full;
    }
    let budget = MAX_COMMIT_MESSAGE_LEN.saturating_sub(prefix.chars().count() + 1);
    let truncated: String = change_desc.chars().take(budget).collect();
    // Close a quote the cut left open.
    if truncated.matches('"').count() % 2 == 1 {
        let shorter: String = change_desc.chars().take(budget.saturating_sub(1)).collect();
        format!("{prefix}{shorter}\u{2026}\"")
    } else {
        format!("{prefix}{truncated}\u{2026}")
    }
}
/// Runs the commit+push sequence synchronously; a free function so tests
/// can drive it without a thread.
///
/// The commit is built from `expected_content`, never by re-reading the
/// working tree, so a later change on disk can't be absorbed under this
/// request's message. It goes through a temporary index and an ordinary
/// `git commit`, so the user's staged content never rides along.
pub(super) fn run_sync(
    repo_dir: &Path,
    file_path: &Path,
    expected_content: &str,
    message: &str,
    latest_requested_hash: &Mutex<[u8; 32]>,
    previous_content: Option<&str>,
) -> SyncOutcome {
    // Plumbing runs from the repo root with a root-relative path.
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

    // Empty output is not taken as "tracked and unchanged"; the index lookup
    // below settles trackedness.
    let mut cmd = literal_pathspec_command(repo_dir);
    cmd.args(["status", "--porcelain", "--"]).arg(file_path);
    let status = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) => output,
        Err(err) => return SyncOutcome::Failed(format!("git status failed: {err}")),
    };
    if !status.status.success() {
        return SyncOutcome::Failed(command_error("git status", &status));
    }
    if status.stdout.starts_with(b"??") {
        return SyncOutcome::SkippedUntracked;
    }

    // Resolved before the empty-status short-circuit: `git status` is silent
    // about an untracked file under `status.showUntrackedFiles=no` or a
    // matching `.gitignore` rule.
    let (mode, relpath) = match index_entry(repo_dir, file_path) {
        Ok(Some(entry)) => entry,
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if status.stdout.is_empty() {
        return SyncOutcome::Skipped;
    }

    // Resolve the branch tip once and ask every question below about that
    // exact commit — the unrelated-history guard, the fast path, the push and
    // the new commit's parent — so what is published is what was validated.
    // The guard runs before both paths that can push.
    let tip = match resolve_parent(&repo_root) {
        Ok(tip) => tip,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if let Some(refusal) = unpushed_history_blocks(&repo_root, tip.as_deref(), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);

    // HEAD already holds this content (an earlier sync committed it), though
    // `status` is non-empty from a newer edit. There may still be a push to
    // retry.
    if let Some(tip) = tip.as_deref()
        && blob_bytes_at(&repo_root, tip, &relpath).as_deref() == Some(expected_content.as_bytes())
    {
        if ahead_of_upstream(&repo_root, tip) {
            return push(repo_dir, tip);
        }
        return SyncOutcome::Skipped;
    }

    // `expected_content` may be stale by now. Disk must match the newest
    // request git-sync knows about (`latest_requested_hash`); anything else is
    // an outside edit or a deletion, which must not be published, and which a
    // passive reload never requests a sync for.
    match fs::read(file_path) {
        Ok(bytes) if hash_bytes(&bytes) == *lock_hash(latest_requested_hash) => {}
        _ => {
            return SyncOutcome::Failed(
                "the file changed after this sync was queued; toggle again to sync the latest version"
                    .to_string(),
            );
        }
    }

    // Checked after the fast path, which never builds a commit.
    let head_blob = tip
        .as_deref()
        .and_then(|t| blob_at(&repo_root, t, &relpath));
    if staged_target_would_be_lost(&repo_root, &relpath, head_blob.as_deref(), previous_content) {
        return SyncOutcome::Failed(STAGED_TARGET_REFUSAL.to_string());
    }

    let blob = match hash_object(&repo_root, &relpath, expected_content) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };

    // `commit_via_temp_index` re-checks `HEAD == parent` before committing.
    let parent = tip;
    let created_commit =
        match commit_via_temp_index(&repo_root, &parent, &mode, &blob, &relpath, message) {
            Ok(sha) => sha,
            Err(err) => return SyncOutcome::Failed(err),
        };

    push_if_head_unchanged(repo_dir, &repo_root, &created_commit)
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
