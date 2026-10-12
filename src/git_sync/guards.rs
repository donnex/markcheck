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

/// What the unpushed-history check could establish about a tip.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnpushedHistory {
    /// No upstream configured: committing may proceed, and `push` refuses on
    /// its own.
    NoUpstream,
    Safe,
    ContainsUnrelated,
}
/// Whether the unpushed range ending at `tip` touches anything but
/// `relpath`. `None` means the branch has no commits at all.
pub(super) fn unpushed_history(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
) -> Result<UnpushedHistory, String> {
    let Some(tip) = tip else {
        return Ok(UnpushedHistory::Safe);
    };
    let Some(upstream) = resolve_upstream(repo_root)? else {
        return Ok(UnpushedHistory::NoUpstream);
    };
    match range_has_unrelated_commits(repo_root, Some(&upstream), tip, relpath)? {
        true => Ok(UnpushedHistory::ContainsUnrelated),
        false => Ok(UnpushedHistory::Safe),
    }
}
pub(super) const UNRELATED_COMMITS_REFUSAL: &str =
    "this branch has unpushed commits that are not part of this change; push them yourself first";
/// Refuses on an unrelated commit and on an unanswerable check alike.
pub(super) fn unpushed_history_blocks(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
) -> Option<SyncOutcome> {
    match unpushed_history(repo_root, tip, relpath) {
        Ok(UnpushedHistory::Safe) | Ok(UnpushedHistory::NoUpstream) => None,
        Ok(UnpushedHistory::ContainsUnrelated) => {
            Some(SyncOutcome::Failed(UNRELATED_COMMITS_REFUSAL.to_string()))
        }
        Err(err) => Some(SyncOutcome::Failed(format!(
            "could not check whether the branch has other unpushed commits ({err}); \
             not publishing"
        ))),
    }
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
mod tests {
    use super::super::inspect::{blob_at, current_head, hash_object, upstream_parts};
    use super::super::push::push;
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    /// The guard must not refuse when a clean filter normalises the staged
    /// content markcheck loaded.
    #[test]
    fn a_matching_staged_checklist_is_not_refused_under_a_content_filter() {
        let work = init_repo_with_crlf_filter();
        let head_blob = blob_at(&work, &current_head(&work).unwrap(), "tracked.md");

        let loaded = "- [x] one\r\n";
        fs::write(work.join("tracked.md"), loaded).unwrap();
        run(&work, &["add", "tracked.md"]);

        assert_ne!(
            index_blob(&work, "tracked.md"),
            head_blob,
            "test setup: something must actually be staged, or the guard \
             returns early and proves nothing"
        );
        assert!(
            !staged_target_would_be_lost(&work, "tracked.md", head_blob.as_deref(), Some(loaded)),
            "staged content identical to what markcheck loaded loses nothing"
        );

        // The guard still fires when the index holds something else.
        let only_in_index = "- [x] one\r\n- [ ] staged only\r\n";
        fs::write(work.join("tracked.md"), only_in_index).unwrap();
        run(&work, &["add", "tracked.md"]);
        fs::write(work.join("tracked.md"), loaded).unwrap();
        assert!(
            staged_target_would_be_lost(&work, "tracked.md", head_blob.as_deref(), Some(loaded)),
            "a staged snapshot the working tree does not have must still refuse"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn verify_commit_scope_checks_the_captured_commit_not_whatever_head_is_now() {
        // A commit landing after the one being verified must be ignored.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "must never touch the concurrent commit sitting on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn verify_commit_scope_names_the_stray_commit_when_the_undo_itself_fails() {
        // An unrelated commit on top makes the compare-and-swap undo refuse.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        fs::write(work.join("other.md"), "hook-added\n").unwrap();
        run(&work, &["add", "tracked.md", "other.md"]);
        run(&work, &["commit", "-q", "-m", "checklist plus hook spill"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("third.md"), "concurrent\n").unwrap();
        run(&work, &["add", "third.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);

        let err = result.expect_err("an out-of-scope commit must be reported");
        assert!(
            err.contains("undoing the commit failed"),
            "must say the rollback itself failed: {err}"
        );
        assert!(
            err.contains(&created_commit),
            "must name the stray commit for manual cleanup: {err}"
        );
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "nothing may be rewound when the undo refuses"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn undo_commit_refuses_to_rewind_a_commit_that_landed_after_the_one_being_undone() {
        let work = init_repo_without_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "to be undone"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = undo_commit(&work, &created_commit);
        // Requiring `update-ref` in the message pins that the CAS refused.
        let message = result.expect_err("the undo must refuse");
        assert!(
            message.contains("update-ref"),
            "must fail at the compare-and-swap, not before reaching it: {message}"
        );
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "the concurrent commit must not be rewound"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn undo_commit_refuses_to_delete_a_root_commit_that_already_has_a_child() {
        let work = unique_dir("repo-root-commit-cas").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        run(&work, &["commit", "-q", "-m", "root commit to be undone"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = undo_commit(&work, &created_commit);
        let message = result.expect_err("the undo must refuse");
        assert!(
            message.contains("update-ref"),
            "must fail at the compare-and-swap, not before reaching it: {message}"
        );
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "the branch ref must not be deleted out from under the concurrent commit"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn range_has_unrelated_commits_reads_a_whole_batched_range_correctly() {
        // Mixes checklist-only commits, a multi-file commit and a root commit.
        let work = init_repo_without_remote();

        let root = current_head(&work).unwrap();
        for n in 1..=5 {
            fs::write(work.join("tracked.md"), format!("- [x] {n}\n")).unwrap();
            run(&work, &["commit", "-q", "-am", &format!("check {n}")]);
        }
        let checklist_only_tip = current_head(&work).unwrap();
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&root), &checklist_only_tip, "tracked.md"),
            Ok(false),
            "a long run of checklist-only commits is not unrelated work"
        );

        assert_eq!(
            range_has_unrelated_commits(&work, None, &checklist_only_tip, "tracked.md"),
            Ok(false),
            "the root commit must be diffed, and it only touched the checklist"
        );

        fs::write(work.join("tracked.md"), "- [x] six\n").unwrap();
        fs::write(work.join("other.md"), "unrelated\n").unwrap();
        run(&work, &["add", "tracked.md", "other.md"]);
        run(&work, &["commit", "-q", "-m", "two files at once"]);
        let mixed_tip = current_head(&work).unwrap();
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&root), &mixed_tip, "tracked.md"),
            Ok(true),
            "a multi-file commit anywhere in the batch must be caught"
        );

        assert_eq!(
            range_has_unrelated_commits(&work, Some(&mixed_tip), &mixed_tip, "tracked.md"),
            Ok(false),
            "an empty range has nothing to diff"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn verify_commit_scope_catches_a_revert_cancelled_unrelated_change() {
        // A second nested commit reverting the first's unrelated change.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist change"]);

        fs::write(work.join("other.md"), "unrelated\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated nested commit"]);

        run(&work, &["rm", "-q", "other.md"]);
        run(
            &work,
            &["commit", "-q", "-m", "revert unrelated nested commit"],
        );
        let created_commit = current_head(&work).unwrap();

        let result =
            verify_commit_scope(&work, &Some(parent.clone()), "tracked.md", &created_commit);
        assert!(
            result.is_err_and(|e| e.contains("changed files other than the checklist")),
            "the revert-cancelled unrelated commit must still be caught"
        );
        // Only markcheck's own commit is rewound; the hook's nested commit
        // survives.
        let nested = git_stdout(&work, &["rev-parse", &format!("{created_commit}^")]);
        assert_eq!(
            current_head(&work).unwrap(),
            nested,
            "exactly one commit — markcheck's own — is rewound"
        );
        assert_ne!(
            current_head(&work).unwrap(),
            parent,
            "the commits markcheck did not create are deliberately left in place"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_is_none_for_a_clean_repo() {
        let work = init_repo_without_remote();
        assert_eq!(repo_sync_blocked(&work), None);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_a_merge_in_progress() {
        let work = init_repo_without_remote();
        fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("merge")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_a_rebase_in_progress() {
        let work = init_repo_without_remote();
        fs::create_dir_all(work.join(".git").join("rebase-merge")).unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("rebase")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_detached_head() {
        let work = init_repo_without_remote();
        let head = current_head(&work).unwrap();
        run(&work, &["checkout", "-q", &head]);
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("detached")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_a_real_merge_conflict() {
        let work = init_repo_without_remote();
        run(&work, &["checkout", "-q", "-b", "branch-a"]);
        fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
        run(&work, &["commit", "-q", "-am", "a"]);
        run(&work, &["checkout", "-q", "main"]);
        fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
        run(&work, &["commit", "-q", "-am", "b"]);
        let _ = Command::new("git")
            .current_dir(&work)
            .args(["merge", "-q", "branch-a"])
            .output();

        assert!(repo_sync_blocked(&work).is_some());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_unmerged_entries_without_a_merge_marker() {
        // Unmerged entries can exist without MERGE_HEAD, and on a path other than
        // the checklist.
        let work = init_repo_without_remote();
        fs::write(work.join("other.md"), "base\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "add other.md"]);

        let base = hash_object(&work, "tracked.md", "base\n").unwrap();
        let ours = hash_object(&work, "tracked.md", "ours\n").unwrap();
        let theirs = hash_object(&work, "tracked.md", "theirs\n").unwrap();
        let index_info = format!(
            "100644 {base} 1\tother.md\n100644 {ours} 2\tother.md\n100644 {theirs} 3\tother.md\n"
        );
        let mut child = Command::new("git")
            .current_dir(&work)
            .args(["update-index", "--index-info"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(index_info.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(!work.join(".git").join("MERGE_HEAD").exists());

        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("conflict")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_detects_a_multi_commit_cherry_pick_after_a_plain_commit() {
        // `git commit` instead of `--continue` removes CHERRY_PICK_HEAD but
        // leaves the sequence in progress.
        let work = init_repo_without_remote();
        fs::create_dir_all(work.join(".git").join("sequencer")).unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason
                .as_ref()
                .is_some_and(|r| r.contains("cherry-pick or revert is in progress")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn repo_sync_blocked_refuses_when_the_git_dir_cannot_be_found() {
        let dir = unique_dir("no-git-dir");
        fs::create_dir_all(&dir).unwrap();
        let reason = repo_sync_blocked(&dir);
        assert!(
            reason
                .as_ref()
                .is_some_and(|r| r.contains("could not locate the repository's git directory")),
            "{reason:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn repo_sync_blocked_refuses_when_the_conflict_check_fails() {
        let work = init_repo_without_remote();
        fs::write(work.join(".git").join("index"), "not an index").unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason
                .as_ref()
                .is_some_and(|r| r.contains("could not check for unresolved merge conflicts")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn undo_commit_rewinds_one_commit_and_keeps_the_branch() {
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "to be undone"]);
        let created = current_head(&work).unwrap();

        undo_commit(&work, &created).expect("undo should succeed");

        assert_eq!(current_head(&work).as_deref(), Some(parent.as_str()));
        assert!(
            current_branch_ref(&work).is_some(),
            "the branch ref must still exist, not be deleted"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_tipless_branch_has_no_unrelated_unpushed_commits() {
        let work = init_repo_without_remote();
        assert_eq!(
            unpushed_history(&work, None, "x.md"),
            Ok(UnpushedHistory::Safe)
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn staged_target_guard_fails_closed_without_a_pre_write_hash() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [ ] one\n- [ ] staged\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let head = git_stdout(&work, &["rev-parse", "HEAD:tracked.md"]);

        assert!(
            staged_target_would_be_lost(&work, "tracked.md", Some(&head), None),
            "no pre-write hash means the staged version cannot be cleared as safe"
        );
        assert!(!staged_target_would_be_lost(
            &work,
            "other-untouched.md",
            None,
            None
        ));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_unanswerable_history_check_refuses_instead_of_permitting_a_push() {
        let (work, _remote) = init_repo_with_remote();
        let bogus = "0".repeat(40);

        assert!(
            unpushed_history(&work, Some(&bogus), "tracked.md").is_err(),
            "an unresolvable range must surface as an error, not a verdict"
        );
        let refusal = unpushed_history_blocks(&work, Some(&bogus), "tracked.md");
        assert!(
            matches!(&refusal, Some(SyncOutcome::Failed(msg)) if msg.contains("could not check")),
            "the caller must refuse to publish: {refusal:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_head_that_moves_after_the_tip_is_captured_cannot_broaden_the_push() {
        // The validated SHA and the pushed SHA must be the same commit, even when
        // `HEAD` moves in between.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::Safe)
        );

        fs::write(work.join("other.txt"), "unrelated\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);
        let moved = git_stdout(&work, &["rev-parse", "HEAD"]);
        assert_ne!(moved, tip, "test setup: HEAD must have moved");

        assert_eq!(push(&work, &tip), SyncOutcome::Synced);
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "checklist\ninit",
            "the unrelated commit must not reach the remote"
        );

        assert_eq!(
            unpushed_history(&work, Some(&moved), "tracked.md"),
            Ok(UnpushedHistory::ContainsUnrelated)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_branch_without_an_upstream_is_not_a_verification_failure() {
        // A fresh repository has nothing to compare against; committing proceeds.
        let work = init_repo_without_remote();
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::NoUpstream)
        );
        assert!(unpushed_history_blocks(&work, Some(&tip), "tracked.md").is_none());

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_configured_but_unfetched_upstream_still_counts_as_no_upstream() {
        // An upstream set in config whose `@{u}` doesn't resolve is still "no
        // upstream", not a failure.
        let work = init_repo_without_remote();
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        assert!(
            upstream_parts(&work).is_some(),
            "test setup: the config claims an upstream exists"
        );
        assert_eq!(
            resolve_upstream(&work),
            Ok(None),
            "but the tracking ref does not resolve"
        );
        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::NoUpstream)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
