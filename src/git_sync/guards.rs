//! The refusals: everything that decides a sync must not proceed. They fail
//! closed — when safety can't be established, the sync refuses.

use std::collections::HashSet;
use std::io;
use std::path::Path;

use super::inspect::{
    blob_sha_for, current_branch_ref, git_dir, index_blob, parse_first_parent, resolve_upstream,
};
use super::process::{PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout};
use super::shared::SyncOutcome;

/// The trailer every commit markcheck makes carries, so its own commit can
/// be found on the branch right after `git commit` (#62, #99, #232).
pub(super) const OWN_COMMIT_TRAILER: &str = "Synced-by: markcheck";
/// The file in the git directory listing the commits git sync made, which
/// is what makes a commit markcheck's own: a trailer is text any patch can
/// carry (#247).
pub(super) const OWN_COMMITS_FILE: &str = "markcheck-own-commits";
/// How many of the latest own commits the record keeps.
const OWN_COMMITS_KEPT: usize = 1024;
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
    let commits = range_commits(repo_root, &format!("{upstream}..{tip}"))?;
    let own_commits = own_commits(repo_root)?;
    // Oldest first, so a refusal names the commit that has waited longest.
    match commits
        .iter()
        .find(|commit| !publishable(commit, &own_commits, relpath, rule))
    {
        Some(commit) => Ok(UnpushedHistory::Blocked(Blocking {
            sha: commit.sha.clone(),
            subject: commit.subject.clone(),
        })),
        None => Ok(UnpushedHistory::Safe(commits.len())),
    }
}
/// What the guards need to know about one commit of a range.
#[derive(Debug)]
struct RangeCommit {
    sha: String,
    is_merge: bool,
    /// Empty for a merge, which is never diffed.
    paths: Vec<Vec<u8>>,
    subject: String,
}
/// Every commit in `range`, oldest first, from one `git log`. The format
/// opens each commit with an empty field, which no path can be, so a path
/// can never be misread as the next commit. Options a user's config could
/// otherwise flip (renames, the root diff, colour, signatures, submodule
/// hiding, the output encoding) are pinned.
fn range_commits(repo_root: &Path, range: &str) -> Result<Vec<RangeCommit>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args([
        "log",
        "--reverse",
        "--root",
        "--no-renames",
        "--no-color",
        "--no-show-signature",
        "--ignore-submodules=none",
        "--encoding=UTF-8",
        "--name-only",
        "-z",
        "--format=%x00%H%x00%P%x00%s",
        range,
        "--",
    ]);
    let output =
        run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| format!("git log failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git log", &output));
    }
    parse_range_commits(&output.stdout)
}
fn parse_range_commits(stdout: &[u8]) -> Result<Vec<RangeCommit>, String> {
    let unreadable = || "could not read git log's list of commits".to_string();
    let mut fields = stdout.split(|&b| b == 0).peekable();
    let mut commits = Vec::new();
    while let Some(marker) = fields.next() {
        if !marker.is_empty() {
            return Err(unreadable());
        }
        let Some(sha) = fields.next() else {
            break;
        };
        let (Some(parents), Some(subject)) = (fields.next(), fields.next()) else {
            return Err(unreadable());
        };
        if sha.is_empty() || !sha.iter().all(u8::is_ascii_hexdigit) {
            return Err(unreadable());
        }
        let mut paths = Vec::new();
        while let Some(path) = fields.next_if(|field| !field.is_empty()) {
            let path = match (paths.is_empty(), path) {
                (true, [b'\n', rest @ ..]) => rest,
                _ => path,
            };
            paths.push(path.to_vec());
        }
        commits.push(RangeCommit {
            sha: String::from_utf8_lossy(sha).into_owned(),
            is_merge: parents.split(|&b| b == b' ').count() > 1,
            paths,
            subject: String::from_utf8_lossy(subject).trim().to_string(),
        });
    }
    Ok(commits)
}
/// Whether one commit may be published under `rule`. A merge never may, nor
/// an empty commit, which git sync never makes.
fn publishable(
    commit: &RangeCommit,
    own_commits: &HashSet<String>,
    relpath: &str,
    rule: Publishable,
) -> bool {
    if commit.is_merge || commit.paths.is_empty() {
        return false;
    }
    let own = own_commits.contains(&commit.sha);
    let this_checklist = commit
        .paths
        .iter()
        .all(|path| path.as_slice() == relpath.as_bytes());
    own || (rule == Publishable::OwnOrThisChecklist && this_checklist)
}
/// The commits git sync recorded as its own in this repository. No record
/// yet is an empty set; one that can't be read is an error, so the guard
/// refuses rather than calling markcheck's commits the user's.
pub(super) fn own_commits(repo_root: &Path) -> Result<HashSet<String>, String> {
    let git_dir = git_dir(repo_root)
        .ok_or_else(|| "could not find the repository's git directory".to_string())?;
    match std::fs::read_to_string(git_dir.join(OWN_COMMITS_FILE)) {
        Ok(record) => Ok(record_entries(&record).map(str::to_string).collect()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(err) => Err(format!(
            "could not read markcheck's record of its commits: {err}"
        )),
    }
}
fn record_entries(record: &str) -> impl Iterator<Item = &str> {
    record
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.bytes().all(|b| b.is_ascii_hexdigit()))
}
/// Adds `sha` to the record of git sync's commits, keeping the latest
/// [`OWN_COMMITS_KEPT`], through a temp file renamed over the record. Best
/// effort: a commit left out is only ever treated as the user's.
pub(super) fn record_own_commit(repo_root: &Path, sha: &str) {
    let Some(git_dir) = git_dir(repo_root) else {
        return;
    };
    let path = git_dir.join(OWN_COMMITS_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut entries: Vec<&str> = record_entries(&existing)
        .filter(|entry| *entry != sha)
        .collect();
    entries.push(sha);
    let kept = &entries[entries.len().saturating_sub(OWN_COMMITS_KEPT)..];
    let contents: String = kept.iter().map(|entry| format!("{entry}\n")).collect();
    let temp = git_dir.join(format!(
        "{OWN_COMMITS_FILE}-{}-{:x}",
        std::process::id(),
        crate::writer::random_suffix()
    ));
    if let Ok(temp) = crate::writer::write_new_temp(&temp, 0o600, &contents, |_| Ok(()))
        && std::fs::rename(&temp, &path).is_err()
    {
        let _ = std::fs::remove_file(temp);
    }
}
pub(super) fn carries_own_trailer(repo_root: &Path, sha: &str) -> Result<bool, String> {
    let format = format!("--format=%(trailers:key={OWN_TRAILER_KEY},valueonly,separator=%x00)");
    let mut cmd = git_command(repo_root);
    cmd.args(["log", "-1", &format, sha]);
    let output =
        run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| format!("git log failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git log", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .any(|value| value.trim() == OWN_TRAILER_VALUE))
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
    Ok(range_commits(repo_root, &range)?.iter().any(|commit| {
        commit.is_merge
            || commit
                .paths
                .iter()
                .any(|path| path.as_slice() != relpath.as_bytes())
    }))
}
pub(super) const OUTSIDE_CHANGES_REFUSAL: &str = "the checklist has changes markcheck didn't make, so nothing was published; \
     commit them yourself, then toggle again";
/// Whether a commit would publish changes markcheck didn't make: `base`,
/// the content markcheck's own changes started from, is neither committed
/// nor staged. Fails closed.
pub(super) fn outside_changes_would_be_published(
    repo_root: &Path,
    relpath: &str,
    head_blob: Option<&str>,
    base: Option<&str>,
) -> bool {
    let Some(sha) = base.and_then(|base| blob_sha_for(repo_root, relpath, base)) else {
        return true;
    };
    head_blob != Some(sha.as_str()) && index_blob(repo_root, relpath).as_deref() != Some(&sha)
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
