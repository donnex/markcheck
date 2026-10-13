//! Building the commit through a temporary index seeded from the parent, so
//! unrelated staged content never rides along. An ordinary `git commit`, so
//! hooks and `commit.gpgsign` apply.

use std::io;
use std::path::Path;
use std::time::Duration;

use super::guards::{repo_sync_blocked, verify_commit_scope};
use super::inspect::{blob_at, current_head, descends_directly_from, git_dir, index_blob};
use super::process::{
    COMMIT_TIMEOUT, PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout,
};
use super::shared::{RacePoint, race_point};

/// Re-checks that `HEAD` is still `parent` before committing, so a tree
/// based on a stale parent can't revert a concurrent commit. The real index
/// is only touched afterwards, for `relpath` alone.
pub(super) fn commit_via_temp_index(
    repo_root: &Path,
    parent: &Option<String>,
    mode: &str,
    blob: &str,
    relpath: &str,
    message: &str,
) -> Result<String, String> {
    let git_dir = git_dir(repo_root)
        .ok_or_else(|| "could not find the repository's .git directory".to_string())?;
    let temp_index = git_dir.join(format!(
        "markcheck-index-{}-{:x}",
        std::process::id(),
        crate::writer::random_suffix()
    ));
    // The baseline `align_real_index_entry` compares against, so a version the
    // user stages meanwhile is never overwritten.
    let index_before = index_blob(repo_root, relpath);
    let result = (|| {
        if let Some(head) = parent {
            populate_temp_index(repo_root, &temp_index, head)?;
        }
        stage_into_temp_index(repo_root, &temp_index, mode, blob, relpath)?;
        if &current_head(repo_root) != parent {
            return Err("the repository changed while the sync was running".to_string());
        }
        // Re-checked here because the earlier check in `run_sync` is only a
        // snapshot.
        if let Some(reason) = repo_sync_blocked(repo_root) {
            return Err(reason);
        }
        race_point(RacePoint::BeforeCommit);
        let created_commit = commit_temp_index(
            repo_root,
            &temp_index,
            message,
            parent,
            relpath,
            blob,
            COMMIT_TIMEOUT,
        )?;
        verify_commit_scope(repo_root, parent, relpath, &created_commit)?;
        race_point(RacePoint::BeforeIndexAlignment);
        align_real_index_entry(repo_root, mode, blob, relpath, index_before.as_deref());
        Ok(created_commit)
    })();
    let _ = std::fs::remove_file(&temp_index);
    // A subprocess killed mid-write can leave `<index>.lock` behind.
    let mut lock_file = temp_index.into_os_string();
    lock_file.push(".lock");
    let _ = std::fs::remove_file(lock_file);
    result
}
fn populate_temp_index(repo_root: &Path, temp_index: &Path, parent: &str) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["read-tree", parent]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git read-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git read-tree", &output));
    }
    Ok(())
}
fn stage_into_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index).args([
        "update-index",
        "--add",
        "--cacheinfo",
        mode,
        blob,
        relpath,
    ]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git update-index failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-index", &output));
    }
    Ok(())
}
/// Points the real index's entry for `relpath` at the committed blob, as a
/// normal `git commit -- <path>` would; otherwise `git status` shows the file
/// as both staged and unstaged after every sync. Skipped when the entry no
/// longer matches `expected_entry`, since the user has staged something of
/// their own. Best-effort: the commit has already succeeded.
fn align_real_index_entry(
    repo_root: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
    expected_entry: Option<&str>,
) {
    if index_blob(repo_root, relpath).as_deref() != expected_entry {
        return;
    }
    let mut cmd = git_command(repo_root);
    cmd.args(["update-index", "--add", "--cacheinfo", mode, blob, relpath]);
    let _ = run_with_timeout(cmd, PLUMBING_TIMEOUT);
}
/// Returns the new commit's SHA, captured once for `verify_commit_scope` and
/// `undo_commit`.
///
/// On a timeout, a `post-commit` hook may still be running after the commit
/// became durable, so the commit is adopted if the new `HEAD` holds `blob`
/// at `relpath` and its only parent is `parent`. Both checks are needed: a
/// concurrent commit on `parent` fails the first, one on top of ours the
/// second. Anything else is reported as a failure, never rewound.
fn commit_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    message: &str,
    parent: &Option<String>,
    relpath: &str,
    blob: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["commit", "-m", message]);
    match run_with_timeout(cmd, timeout) {
        Ok(output) if output.status.success() => current_head(repo_root)
            .ok_or_else(|| "could not read the repository's state after committing".to_string()),
        Ok(output) => Err(command_error("git commit", &output)),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => match current_head(repo_root) {
            Some(head)
                if Some(&head) != parent.as_ref()
                    && blob_at(repo_root, &head, relpath).as_deref() == Some(blob)
                    && descends_directly_from(repo_root, &head, parent) =>
            {
                Ok(head)
            }
            _ => Err(format!("git commit failed: {err}")),
        },
        Err(err) => Err(format!("git commit failed: {err}")),
    }
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
