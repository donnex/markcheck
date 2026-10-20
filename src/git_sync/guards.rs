//! The refusals: everything that decides a sync must not proceed. They fail
//! closed — when safety can't be established, the sync refuses.

use std::path::Path;

use super::inspect::{
    blob_sha_for, current_branch_ref, git_dir, index_blob, parse_first_parent, resolve_upstream,
};
use super::process::{
    PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout, run_with_timeout_and_stdin,
};
use super::shared::SyncOutcome;

/// The trailer every commit markcheck makes carries, so a later sync can
/// tell its own stranded commits from the user's (#62, #99).
pub(super) const OWN_COMMIT_TRAILER: &str = "Synced-by: markcheck";
const OWN_TRAILER_KEY: &str = "Synced-by";
const OWN_TRAILER_VALUE: &str = "markcheck";

/// Which unpushed commits a sync may publish along with its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Publishable {
    /// A toggle's sync: markcheck's own commits to any one checklist, and any
    /// commit touching only this checklist.
    OwnOrThisChecklist,
    /// The startup catch-up, which the user did nothing to start: markcheck's
    /// own commits only.
    OwnOnly,
}
/// An unpushed commit a sync must not publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Blocking {
    pub(super) sha: String,
    pub(super) subject: String,
}
impl Blocking {
    /// `abc1234 "WIP: reorder"`, for a message naming it.
    pub(super) fn describe(&self) -> String {
        let short: String = self.sha.chars().take(7).collect();
        format!("commit {short} \"{}\"", self.subject)
    }
}
/// What the unpushed-history check could establish about a tip.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnpushedHistory {
    /// No upstream configured: committing may proceed, and `push` refuses on
    /// its own.
    NoUpstream,
    /// Every unpushed commit may be published; holds how many there are.
    Safe(usize),
    Blocked(Blocking),
}
/// Whether the unpushed range ending at `tip` holds a commit `rule` doesn't
/// allow publishing. `None` means the branch has no commits at all.
pub(super) fn unpushed_history(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
    rule: Publishable,
) -> Result<UnpushedHistory, String> {
    let Some(tip) = tip else {
        return Ok(UnpushedHistory::Safe(0));
    };
    let Some(upstream) = resolve_upstream(repo_root)? else {
        return Ok(UnpushedHistory::NoUpstream);
    };
    let range = format!("{upstream}..{tip}");
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--reverse", "--parents", &range]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git rev-list failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git rev-list", &output));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut commits = 0;
    // Oldest first, so a refusal names the commit that has waited longest.
    for line in listing.lines() {
        let mut tokens = line.split_whitespace();
        let Some(sha) = tokens.next() else { continue };
        commits += 1;
        let is_merge = tokens.count() > 1;
        if is_merge || !publishable(repo_root, sha, relpath, rule)? {
            return Ok(UnpushedHistory::Blocked(Blocking {
                sha: sha.to_string(),
                subject: commit_subject(repo_root, sha)?,
            }));
        }
    }
    Ok(UnpushedHistory::Safe(commits))
}
/// Whether one non-merge commit may be published under `rule`.
fn publishable(
    repo_root: &Path,
    sha: &str,
    relpath: &str,
    rule: Publishable,
) -> Result<bool, String> {
    let paths = commit_paths(repo_root, sha)?;
    let own = matches!(paths.as_slice(), [path] if is_checklist_path(path))
        && carries_own_trailer(repo_root, sha)?;
    let this_checklist = paths
        .iter()
        .all(|path| path.as_slice() == relpath.as_bytes());
    Ok(own || (rule == Publishable::OwnOrThisChecklist && this_checklist))
}
fn is_checklist_path(path: &[u8]) -> bool {
    path.len() > 3 && path[path.len() - 3..].eq_ignore_ascii_case(b".md")
}
/// The paths one commit changed. `--no-commit-id` makes every field a path.
fn commit_paths(repo_root: &Path, sha: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args([
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        sha,
    ]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git diff-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git diff-tree", &output));
    }
    Ok(output
        .stdout
        .split(|&b| b == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}
pub(super) fn carries_own_trailer(repo_root: &Path, sha: &str) -> Result<bool, String> {
    let format = format!("--format=%(trailers:key={OWN_TRAILER_KEY},valueonly,separator=%x00)");
    let output = commit_log(repo_root, sha, &format)?;
    Ok(output
        .split('\0')
        .any(|value| value.trim() == OWN_TRAILER_VALUE))
}
fn commit_subject(repo_root: &Path, sha: &str) -> Result<String, String> {
    Ok(commit_log(repo_root, sha, "--format=%s")?
        .trim()
        .to_string())
}
fn commit_log(repo_root: &Path, sha: &str, format: &str) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["log", "-1", format, sha]);
    let output =
        run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| format!("git log failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git log", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
/// Refuses on an unpublishable commit and on an unanswerable check alike.
pub(super) fn unpushed_history_blocks(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
) -> Option<SyncOutcome> {
    match unpushed_history(repo_root, tip, relpath, Publishable::OwnOrThisChecklist) {
        Ok(UnpushedHistory::Safe(_)) | Ok(UnpushedHistory::NoUpstream) => None,
        Ok(UnpushedHistory::Blocked(blocking)) => Some(SyncOutcome::Failed(format!(
            "{} is not pushed yet and isn't markcheck's; push or drop it first",
            blocking.describe()
        ))),
        Err(err) => Some(SyncOutcome::Failed(unpushed_check_failed(&err))),
    }
}
pub(super) fn unpushed_check_failed(err: &str) -> String {
    format!("could not check whether the branch has other unpushed commits ({err}); not publishing")
}
/// Whether any commit in `base..tip` (or all of `tip`'s ancestry when `base`
/// is `None`) touches a path other than `relpath`. Checked per commit, since a
/// later commit reverting an unrelated change nets out in a plain diff but
/// both still get published. Merges count as unrelated. `Err` on any git
/// failure, leaving the decision to the caller.
pub(super) fn range_has_unrelated_commits(
    repo_root: &Path,
    base: Option<&str>,
    tip: &str,
    relpath: &str,
) -> Result<bool, String> {
    let range = match base {
        Some(base) => format!("{base}..{tip}"),
        None => tip.to_string(),
    };
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", &range]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git rev-list failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git rev-list", &output));
    }
    // A merge short-circuits before any diff is needed.
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut shas = String::new();
    for line in listing.lines() {
        let mut tokens = line.split_whitespace();
        let Some(sha) = tokens.next() else { continue };
        if tokens.count() > 1 {
            return Ok(true);
        }
        shas.push_str(sha);
        shas.push('\n');
    }
    if shas.is_empty() {
        return Ok(false);
    }

    // One `diff-tree --stdin` for the whole range. `--no-commit-id` makes every
    // field a path, so a file named like a 40-hex SHA can't be misread.
    let mut diff_cmd = git_command(repo_root);
    diff_cmd.args([
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        "--stdin",
    ]);
    let diff_output = run_with_timeout_and_stdin(diff_cmd, PLUMBING_TIMEOUT, &shas)
        .map_err(|err| format!("git diff-tree failed: {err}"))?;
    if !diff_output.status.success() {
        return Err(command_error("git diff-tree", &diff_output));
    }
    Ok(diff_output
        .stdout
        .split(|&b| b == 0)
        .any(|path| !path.is_empty() && path != relpath.as_bytes()))
}
pub(super) const STAGED_TARGET_REFUSAL: &str = "the checklist has staged changes that differ from the file on disk; \
     committing would drop them — commit or unstage them first";
/// Whether committing the working tree would discard a staged snapshot of
/// the checklist that differs from what markcheck loaded. Staged content
/// equal to the loaded content is not at risk. Fails closed.
pub(super) fn staged_target_would_be_lost(
    repo_root: &Path,
    relpath: &str,
    head_blob: Option<&str>,
    previous_content: Option<&str>,
) -> bool {
    let index = index_blob(repo_root, relpath);
    if index.as_deref() == head_blob {
        return false; // nothing staged for this path
    }
    let Some(previous) = previous_content else {
        return true;
    };
    // Both sides are blob SHAs git computed for this path, so clean filters
    // apply equally.
    match blob_sha_for(repo_root, relpath, previous) {
        Some(sha) => index.as_deref() != Some(sha.as_str()),
        None => true,
    }
}
/// Refuses and undoes a commit that touched more than `relpath`, e.g.
/// because a `pre-commit` hook staged extra files into the temporary index.
/// Checks the captured `created_commit`, never a re-resolved `HEAD`.
pub(super) fn verify_commit_scope(
    repo_root: &Path,
    parent: &Option<String>,
    relpath: &str,
    created_commit: &str,
) -> Result<(), String> {
    // A verification that cannot be performed is reported with the commit it
    // left behind, not undone: whether it is in scope is unknown.
    let has_unrelated =
        match range_has_unrelated_commits(repo_root, parent.as_deref(), created_commit, relpath) {
            Ok(has_unrelated) => has_unrelated,
            Err(err) => {
                return Err(format!(
                    "could not check what commit {created_commit} changed ({err}); \
                 it was made and left on the branch — check it before pushing"
                ));
            }
        };
    if !has_unrelated {
        return Ok(());
    }
    match undo_commit(repo_root, created_commit) {
        Ok(()) => Err(
            "a commit hook changed files other than the checklist, so the commit was undone"
                .to_string(),
        ),
        Err(undo_err) => Err(format!(
            "a commit hook changed files other than the checklist, and undoing the \
             commit failed ({undo_err}); commit {created_commit} may need manual cleanup"
        )),
    }
}
/// Rewinds exactly markcheck's own commit to its own first parent (or
/// deletes the branch for a root commit), compare-and-swapped against
/// `created_commit`. Never resets to the captured parent: a concurrent
/// commit can land between that capture and `git commit`.
pub(super) fn undo_commit(repo_root: &Path, created_commit: &str) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", "-n", "1", created_commit]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git rev-list failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git rev-list", &output));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let first_parent = parse_first_parent(&listing).ok_or_else(|| {
        format!(
            "could not read commit {created_commit}'s history; \
             refusing to rewind the branch"
        )
    })?;

    let mut cmd = git_command(repo_root);
    match &first_parent {
        Some(sha) => {
            cmd.args(["update-ref", "HEAD", sha, created_commit]);
        }
        None => {
            let branch_ref = current_branch_ref(repo_root).ok_or_else(|| {
                "could not undo the commit: the branch reference could not be read".to_string()
            })?;
            cmd.args(["update-ref", "-d", &branch_ref, created_commit]);
        }
    }
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(command_error("git update-ref", &output)),
        Err(err) => Err(format!("git update-ref failed: {err}")),
    }
}
/// Refuses while a merge, cherry-pick, revert, bisect, sequencer run or
/// rebase is in progress, `HEAD` is detached, or any path is conflicted — and
/// when any of that can't be checked.
pub(super) fn repo_sync_blocked(repo_root: &Path) -> Option<String> {
    let Some(git_dir) = git_dir(repo_root) else {
        return Some(
            "could not locate the repository's git directory to check for a merge or rebase \
             in progress; not syncing"
                .to_string(),
        );
    };
    for (marker, label) in [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("BISECT_LOG", "a bisect"),
        ("sequencer", "a multi-commit cherry-pick or revert"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
    ] {
        match git_dir.join(marker).try_exists() {
            Ok(false) => {}
            Ok(true) => return Some(format!("{label} is in progress")),
            Err(err) => {
                return Some(format!(
                    "could not check for {label} in progress ({err}); not syncing"
                ));
            }
        }
    }
    let mut symbolic_ref_cmd = git_command(repo_root);
    symbolic_ref_cmd.args(["symbolic-ref", "-q", "HEAD"]);
    match run_with_timeout(symbolic_ref_cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {}
        Ok(output) if output.status.code() == Some(1) => {
            return Some(
                "the repository is not on a branch (detached HEAD); check one out to sync"
                    .to_string(),
            );
        }
        Ok(output) => {
            return Some(format!(
                "could not check whether the repository is on a branch ({}); not syncing",
                command_error("git symbolic-ref", &output)
            ));
        }
        Err(err) => {
            return Some(format!(
                "could not check whether the repository is on a branch ({err}); not syncing"
            ));
        }
    }
    let mut ls_files_cmd = git_command(repo_root);
    ls_files_cmd.args(["ls-files", "-u"]);
    match run_with_timeout(ls_files_cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() && output.stdout.is_empty() => None,
        Ok(output) if output.status.success() => {
            Some("the repository has unresolved merge conflicts".to_string())
        }
        Ok(output) => Some(format!(
            "could not check for unresolved merge conflicts ({}); not syncing",
            command_error("git ls-files", &output)
        )),
        Err(err) => Some(format!(
            "could not check for unresolved merge conflicts ({err}); not syncing"
        )),
    }
}

#[cfg(test)]
#[path = "guards_tests.rs"]
mod tests;
