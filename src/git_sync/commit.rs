//! Building the commit through a temporary index seeded from the parent, so
//! unrelated staged content never rides along. An ordinary `git commit`, so
//! hooks and `commit.gpgsign` apply.

use std::io;
use std::path::Path;
use std::time::Duration;

use super::guards::{
    OWN_COMMIT_TRAILER, carries_own_trailer, repo_sync_blocked, verify_commit_scope,
};
use super::inspect::{IDENTITY_FIX, blob_at, current_head, git_dir, index_blob};
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
/// `HEAD` after `git commit` is not trusted to be that commit: a
/// `post-commit` hook, or the user in another terminal, can commit on top of
/// it first, and a timed-out `git commit` may or may not have landed. The
/// commit is found by lineage instead (`own_commit_since`) and returned only
/// while it is still `HEAD`. With something on top, nothing is undone or
/// pushed: rewinding would take the other commit off the branch.
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
    // Refuse an identity git would guess from the account and host name: a
    // pushed author line can't be changed, and git's notice of the guess is
    // output markcheck doesn't show (#224).
    cmd.env("GIT_INDEX_FILE", temp_index).args([
        "-c",
        "user.useConfigOnly=true",
        "commit",
        "-m",
        message,
        "-m",
        OWN_COMMIT_TRAILER,
    ]);
    let timed_out = match run_with_timeout(cmd, timeout) {
        Ok(output) if output.status.success() => None,
        Ok(output) => return Err(commit_error(&output)),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => Some(err),
        Err(err) => return Err(format!("git commit failed: {err}")),
    };
    let head = current_head(repo_root);
    let own = head
        .as_deref()
        .and_then(|head| own_commit_since(repo_root, parent, head, relpath, blob));
    match (head, own, timed_out) {
        (Some(head), Some(own), _) if head == own => Ok(head),
        (Some(_), Some(own), _) => {
            let short: String = own.chars().take(7).collect();
            Err(format!(
                "another commit landed on top of markcheck's commit {short}; \
                 both were left on the branch and not pushed — check them before pushing"
            ))
        }
        (_, _, Some(err)) => Err(format!("git commit failed: {err}")),
        _ => Err(
            "git commit finished, but markcheck's commit could not be found on the branch; \
             the branch was left as it is — check it before pushing"
                .to_string(),
        ),
    }
}
/// A refusal for want of an identity names only the symptom (`empty ident
/// name`, `no email was given`), so the fix is added.
fn commit_error(output: &std::process::Output) -> String {
    let error = command_error("git commit", output);
    if String::from_utf8_lossy(&output.stderr).contains("Please tell me who you are") {
        format!("{error} — {}", IDENTITY_FIX)
    } else {
        error
    }
}
/// The oldest commit on `head`'s first-parent chain since `parent` that
/// holds `blob` at `relpath` and carries markcheck's trailer. A commit made
/// on top of markcheck's comes later on the chain; one that landed between
/// the parent check and `git commit` lacks the blob or the trailer.
fn own_commit_since(
    repo_root: &Path,
    parent: &Option<String>,
    head: &str,
    relpath: &str,
    blob: &str,
) -> Option<String> {
    let range = match parent {
        Some(parent) => format!("{parent}..{head}"),
        None => head.to_string(),
    };
    let mut cmd = git_command(repo_root);
    cmd.args([
        "rev-list",
        "--first-parent",
        "--reverse",
        &format!("--max-count={OWN_COMMIT_SEARCH_LIMIT}"),
        &range,
    ]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|sha| {
            blob_at(repo_root, sha, relpath).as_deref() == Some(blob)
                && carries_own_trailer(repo_root, sha).unwrap_or(false)
        })
        .map(str::to_string)
}
/// Bounds the lineage search; markcheck's commit is normally the first or
/// second entry.
const OWN_COMMIT_SEARCH_LIMIT: usize = 64;

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;
