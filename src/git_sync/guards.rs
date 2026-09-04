//! The refusals: everything that decides a sync must not proceed.
//!
//! These are the reason `git_sync` is as large as it is. Each one exists
//! because of a specific way an automatic commit can damage work a human is
//! doing in the same repository — unrelated unpushed history, a staged
//! version of the checklist, a repository mid-merge, a hook that committed
//! more than it was asked to.
//!
//! They share one rule: **fail closed**. When a guard cannot establish that
//! something is safe, it refuses rather than assuming. A false refusal costs
//! a retry; the other direction costs the user their work.

use std::path::Path;

use super::inspect::{
    current_branch_ref, git_dir, index_blob, parse_first_parent, resolve_upstream,
};
use super::process::{
    PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout, run_with_timeout_and_stdin,
};
use super::{SyncOutcome, blob_sha_for};

/// Whether the branch already has unpushed commits ahead of upstream that
/// touch something *other* than `relpath` — checked in `run_sync` before
/// either of its two paths that can push (building a new commit, or the
/// `blob_bytes_at`-already-matches fast path's push of the existing `HEAD`),
/// never inside `retry_commit`'s own timed retry (a stale retry is instead
/// abandoned outright — see its doc comment — rather than re-checked
/// against this). Deliberately not a plain ahead-of-upstream check:
/// multiple markcheck commits can legitimately stack up while
/// offline (a user toggling several tasks before connectivity returns —
/// see `repeated_requests_against_an_unreachable_remote_never_deadlock_or_lose_edits`),
/// and none of those are "unrelated work" to refuse over — only commits
/// touching something else are. `range_has_unrelated_commits` checks each
/// commit in `@{u}..HEAD` individually, which is what tells the two apart
/// correctly — see its own doc comment for why a single net diff isn't
/// enough.
///
/// Deliberately **fails closed** (`false`, i.e. "don't refuse") whenever
/// the check itself can't be answered — no upstream configured, no commits
/// yet (a repo's very first sync), or any other `git` failure — because
/// refusing is the consequential action here, not pushing. Failing open
/// the way `ahead_of_upstream` does would wrongly block every first-ever
/// commit to a fresh repository and every sync where upstream tracking
/// simply isn't set up.
/// What the unpushed-history check could establish about `tip` — three
/// genuinely different answers that a plain `bool` used to collapse into
/// two.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnpushedHistory {
    /// No upstream is configured, so there is no range to compare against.
    /// **Not** a safety failure: the branch simply has no publication target
    /// yet (a fresh repository, or one where `git push -u` was never run),
    /// and `push` refuses on its own with a specific message. Committing may
    /// proceed; publishing can't happen anyway.
    NoUpstream,
    /// Every unpushed commit touches only the checklist path.
    Safe,
    /// At least one unpushed commit touches something else.
    ContainsUnrelated,
}
/// Whether the unpushed range ending at `tip` contains commits touching
/// anything but `relpath` — the guard that keeps a checklist toggle from
/// publishing unrelated local work, since an explicit-SHA push still sends
/// the commit's whole ancestry.
///
/// `tip` is the caller's captured branch tip, never the symbolic `HEAD`, so
/// the range checked is exactly the range that will be published. `None`
/// means the branch has no commits at all, so there is nothing unpushed for
/// anything to be unrelated to.
///
/// External review, round 10: this returned a `bool` built with
/// `.unwrap_or(false)`, so *any* git failure — a `rev-list` timeout on a
/// large history, a transiently unhealthy object database — became
/// indistinguishable from "verified safe" and the caller went on to push.
/// The asymmetry matters: a false refusal costs the user a retry, while a
/// false clearance publishes someone else's commits. The reason it was
/// written that way is real, though, and is why this isn't simply a
/// `Result<bool, _>`: `@{u}` is legitimately unresolvable when no upstream
/// exists, and failing closed on that would block every first-ever commit to
/// a fresh repository. Separating that case out is what lets the genuine
/// failures fail closed without taking the legitimate one down with them.
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
/// The refusal message shared by both push-capable paths.
pub(super) const UNRELATED_COMMITS_REFUSAL: &str =
    "git-sync: branch has unpushed commits unrelated to this change; push them manually first";
/// Maps an `unpushed_history` answer to "may this path continue?", refusing
/// on both an unrelated commit and an unanswerable check.
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
            "git-sync: could not verify the branch has no unrelated unpushed commits ({err}); \
             not publishing"
        ))),
    }
}
/// Whether any commit in `base..tip` — or, when `base` is `None`, in the
/// full ancestry of `tip` (the root-commit case) — touches a path other
/// than `relpath`. Checked **per commit**, not as a single net diff between
/// `base` and `tip`: external review, round 8, and a self-review finding of
/// the same shape in `verify_commit_scope` below — a commit that touches an
/// unrelated path followed by a later commit that reverts that exact change
/// nets out to zero in a `base..tip` tree diff, so a naive check sees
/// nothing wrong, even though both commits are still in the range and still
/// get published (they're ancestors of `tip`, and an explicit-SHA push
/// sends a commit's whole ancestry regardless of what any single commit's
/// net effect on the tree looks like).
///
/// Two subprocesses total, regardless of how many commits are in the range.
/// `git rev-list --parents <range>` gives every commit and its parent count
/// in one call; a commit with more than one parent (a merge) is treated as
/// unrelated unconditionally — refusing outright rather than attempting a
/// combined-diff interpretation of what a merge itself touched, which has
/// genuine edge cases (conflict resolutions, content changed only during the
/// merge) that could misjudge it either way. The remaining (single-parent or
/// root) commits then go to **one** `git diff-tree --root ... --stdin` for
/// the whole range. `--root` is required for a root commit to show anything
/// at all (confirmed empirically: without it, `diff-tree` on a zero-parent
/// commit prints nothing) and is a no-op for an ordinary commit (still diffs
/// against its own first parent).
///
/// Deep review, round 2: this used to spawn one `diff-tree` **per commit**,
/// each through `run_with_timeout`'s subprocess plus two reader threads —
/// on every sync request, over `@{u}..HEAD`, a range that grows without
/// bound while the remote is unreachable (and markcheck's design
/// deliberately lets its own commits stack up while offline). Measured at
/// 200 unpushed commits: 0.503s and ~400 thread spawns per toggle, versus
/// 0.017s for the batched form. Both shapes are "any" predicates over the
/// same range, so only the order of evaluation changes, not the verdict.
///
/// Returns `Err` (rather than failing open or closed itself) on any `git`
/// failure, leaving that decision to each caller: `branch_has_unrelated_
/// unpushed_commits` fails closed (`false`) on `Err`, matching its existing
/// behavior; `verify_commit_scope` propagates it as a hard sync failure,
/// also matching its existing behavior.
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
    // Pass 1: the commit list, plus merge detection from the parent count.
    // A merge short-circuits before any diff is needed at all.
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

    // Pass 2: one `diff-tree` for the whole range, fed the SHA list on
    // stdin, rather than one subprocess per commit.
    //
    // `--no-commit-id` matters beyond tidiness: without it the output
    // interleaves commit ids with paths in the same NUL-separated stream,
    // and telling them apart would mean guessing that a 40-hex field is an
    // id — which a file legitimately named 40 hex characters would defeat.
    // With it, every field is a path, so there is nothing to disambiguate.
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
/// The refusal message for a checklist whose staged version would be lost.
pub(super) const STAGED_TARGET_REFUSAL: &str = "git-sync: the checklist has staged changes that differ from the file on disk; \
     committing would drop them — commit or unstage them first";
/// Whether committing the working tree would discard a staged snapshot of
/// the checklist that exists nowhere else.
///
/// External review, round 11, reproduced end-to-end. The commit is built
/// from the working tree, and `align_real_index_entry` afterwards points the
/// real index at it — so if the index was holding a *different* version of
/// the checklist, that version is neither committed nor still staged. It
/// survives only as a dangling blob, reachable through no ordinary git
/// workflow. Verified: a staged blob's unique content was absent from the
/// resulting commit and unreferenced by anything afterwards.
///
/// This is deliberately **not** "refuse whenever the target is staged".
/// Staged content equal to what markcheck loaded is wholly contained in what
/// is about to be committed — the toggle is the only difference — so nothing
/// is lost, and refusing there would break two ordinary workflows for no
/// safety gain: staging the checklist before running markcheck, and `git
/// add`ing a brand-new checklist that has never been committed. Only a
/// staged snapshot that differs from the pre-write working tree is at risk,
/// and that is exactly what this returns true for.
///
/// Fails **closed**: if the staged bytes can't be read, or the request
/// carries no pre-write hash to compare against, safety can't be
/// established and the sync refuses — the same rule `unpushed_history`
/// follows.
///
/// Both sides of the comparison are blob SHAs that git computed for this
/// path, so content filters apply to each identically. That was a real
/// defect once: the guard compared a digest of the working-tree bytes
/// against the staged blob's bytes, which under `core.autocrlf` differ by
/// exactly the normalisation — measured at 22 bytes on disk against 20 in
/// the blob for the same two lines — so the hashes could never match and a
/// perfectly safe sync was refused, in precisely the two workflows the
/// paragraph above says must keep working. See `hash_object_inner`.
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
    // Both sides are blob SHAs computed by git for *this path*, so any
    // clean filter applies equally to each — see `hash_object_inner`.
    // Comparing raw bytes here instead was the bug: under `core.autocrlf`
    // the staged blob is normalised and the working-tree bytes are not, so
    // they could never match and a safe sync was refused.
    match blob_sha_for(repo_root, relpath, previous) {
        Some(sha) => index.as_deref() != Some(sha.as_str()),
        None => true,
    }
}
/// Runs after `commit_temp_index` succeeds (hooks having already executed
/// against the temporary index) and enforces the invariant a `pre-commit`
/// hook can otherwise silently break: a `git add`/formatter/`lint-staged`/
/// "stage everything" hook inherits the same `GIT_INDEX_FILE` as the rest
/// of this commit, and can stage more into it — defeating the whole point
/// of the temporary-index design (see `commit_via_temp_index`'s doc
/// comment), which exists specifically so an unrelated staged file can
/// never ride along into a markcheck commit. Checks `parent..created_commit`
/// — the exact commit `commit_temp_index` just made, captured once by its
/// caller — rather than re-resolving `HEAD` here: a commit landing after
/// `created_commit` was captured must never be mistaken for (or silently
/// swept into) markcheck's own commit. `range_has_unrelated_commits` (not
/// a single net diff — see its own doc comment for why: a hook that makes
/// two nested commits where a later one reverts an earlier one's unrelated
/// change would net out to zero in a plain diff, even though both extra
/// commits are still in history and still get pushed) decides whether
/// anything beyond `relpath` changed; if so, the commit is undone
/// (`undo_commit`) and an error reported — a hook that adds a nested commit
/// of its own is undone the same way, since resetting the branch ref back
/// to `parent` discards the whole chain regardless of how many commits are
/// in it.
pub(super) fn verify_commit_scope(
    repo_root: &Path,
    parent: &Option<String>,
    relpath: &str,
    created_commit: &str,
) -> Result<(), String> {
    // A verification that *cannot be performed* is not the same as one that
    // passed, and it leaves the commit sitting on the branch. Deep rounds 2,
    // round 3: this used to be a bare `?`, so the user saw only the raw git
    // failure ("git rev-list: fatal: ...") with nothing to say a commit had
    // been made and kept — while the neighbouring undo-failed arm is careful
    // to name exactly that. The commit is deliberately *not* undone here:
    // whether it is in scope is precisely what could not be established, and
    // rewinding a possibly-fine commit is the worse guess.
    let has_unrelated =
        match range_has_unrelated_commits(repo_root, parent.as_deref(), created_commit, relpath) {
            Ok(has_unrelated) => has_unrelated,
            Err(err) => {
                return Err(format!(
                    "git-sync: could not verify what commit {created_commit} changed ({err}); \
                 it was made and left on the branch — check it before pushing"
                ));
            }
        };
    if !has_unrelated {
        return Ok(());
    }
    match undo_commit(repo_root, created_commit) {
        Ok(()) => Err(
            "git-sync: a commit hook modified files beyond the checklist; sync aborted".to_string(),
        ),
        Err(undo_err) => Err(format!(
            "git-sync: a commit hook modified files beyond the checklist, and undoing the \
             commit failed ({undo_err}); commit {created_commit} may need manual cleanup"
        )),
    }
}
/// Moves the branch ref back to `parent` (or deletes it entirely, for a
/// root commit with `parent: None`) — the undo side of
/// `verify_commit_scope`. Never touches the working tree or the real
/// index, same as everything else in the temp-index commit path.
///
/// Both forms are compare-and-swapped against `created_commit` — `git
/// update-ref`'s optional expected-old-value argument — rather than an
/// unconditional move/delete: if the branch has advanced past
/// `created_commit` (another process committed on top before this undo
/// ran), the update-ref itself fails instead of silently rewinding that
/// concurrent commit out of the branch's history.
///
/// **It rewinds exactly one commit — markcheck's own — and never the
/// captured `parent`.** Deep round 3, reproduced with the race harness: this
/// used to reset the branch straight back to `parent`, which discards
/// *everything* between. A commit landing in the window between
/// `commit_via_temp_index`'s `HEAD == parent` re-check and `git commit`
/// itself becomes an **ancestor** of `created_commit`, so the
/// compare-and-swap above still passes — it only guards against the branch
/// moving *past* our commit — and the reset dropped that foreign commit off
/// the branch entirely. Observed directly: a concurrent `unrelated` commit
/// left the branch reading `init` alone, surviving only as a dangling
/// object.
///
/// The graph cannot distinguish that from a `pre-commit` hook making nested
/// commits of its own, and it does not need to: markcheck did not create
/// those either. The module's rule is that markcheck never moves `HEAD`
/// across a commit it did not make, and resetting to `created_commit`'s own
/// first parent is what actually honours it. The cost is that hook-made
/// commits now survive an aborted sync instead of being swept away with it —
/// which is the right trade, and the user is told (`verify_commit_scope`'s
/// error names the commit), and the next sync's unpushed-history guard
/// refuses with a clear message until they deal with it.
pub(super) fn undo_commit(repo_root: &Path, created_commit: &str) -> Result<(), String> {
    // The commit's *own* first parent, not the parent captured before the
    // commit ran — see above.
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
            "git-sync: could not read commit {created_commit}'s parents; \
             refusing to rewind the branch"
        )
    })?;

    let mut cmd = git_command(repo_root);
    match &first_parent {
        Some(sha) => {
            cmd.args(["update-ref", "HEAD", sha, created_commit]);
        }
        // A root commit has no parent to move back to, so the branch ref is
        // deleted outright, returning to the "no commits yet" state.
        None => {
            let branch_ref = current_branch_ref(repo_root).ok_or_else(|| {
                "git-sync: could not resolve branch ref to undo root commit".to_string()
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
/// Refuses to sync while the repository is in a state where an automatic
/// commit would be actively harmful: a merge/cherry-pick/revert/bisect/
/// rebase in progress (each recorded by git as a marker file or directory
/// under the git-dir), a detached `HEAD` (a commit made there is one `git
/// checkout` away from becoming unreachable, with no branch pointing at
/// it), or an unresolved conflict anywhere in the repository (not just the
/// target path — any of these mean `HEAD`/the index currently represent
/// in-progress work a background checklist commit must never interfere
/// with, silently "resolving" a conflict or completing someone else's
/// merge). Checked at the top of `run_sync`, before any other work, and
/// again by `commit_via_temp_index` immediately before the actual `git
/// commit` call — a snapshot check either time, not a held lock, so one of
/// these markers could still appear in the (now much narrower) gap between
/// the second check and `git commit` itself actually running; see
/// `commit_via_temp_index`'s doc comment for why that residual window is
/// accepted rather than eliminated.
pub(super) fn repo_sync_blocked(repo_root: &Path) -> Option<String> {
    let git_dir = git_dir(repo_root)?;
    for (marker, label) in [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("BISECT_LOG", "a bisect"),
    ] {
        if git_dir.join(marker).exists() {
            return Some(format!("git-sync: repository has {label} in progress"));
        }
    }
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        return Some("git-sync: repository has a rebase in progress".to_string());
    }
    let mut symbolic_ref_cmd = git_command(repo_root);
    symbolic_ref_cmd.args(["symbolic-ref", "-q", "HEAD"]);
    let on_a_branch = run_with_timeout(symbolic_ref_cmd, PLUMBING_TIMEOUT)
        .is_ok_and(|output| output.status.success());
    if !on_a_branch {
        return Some("git-sync: repository is in a detached HEAD state".to_string());
    }
    let mut ls_files_cmd = git_command(repo_root);
    ls_files_cmd.args(["ls-files", "-u"]);
    let unmerged = run_with_timeout(ls_files_cmd, PLUMBING_TIMEOUT);
    match unmerged {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some("git-sync: repository has unresolved merge conflicts".to_string())
        }
        _ => None,
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

    /// The false refusal this guard used to produce under a clean filter.
    /// The staged content *is* what markcheck loaded, so nothing would be
    /// lost — but the old comparison held a digest of the working-tree bytes
    /// against the normalised staged blob, which can never match.
    #[test]
    fn a_matching_staged_checklist_is_not_refused_under_a_content_filter() {
        let work = init_repo_with_crlf_filter();
        let head_blob = blob_at(&work, &current_head(&work).unwrap(), "tracked.md");

        // The user stages an edit; markcheck then loads exactly that content.
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

        // And the guard still fires when the index really does hold
        // something else, so the fix did not simply disable it.
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
        // External review, round 4: the old implementation re-resolved
        // `current_head` inside verify_commit_scope itself, so a commit
        // landing after the one being verified (but before this function
        // ran) could be mistaken for part of it. Passing the exact SHA in
        // means a concurrent commit on top must be ignored entirely,
        // whatever it touches.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        // Stands in for markcheck's own commit: touches only tracked.md.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        // A concurrent commit lands on top, touching an unrelated file —
        // if verify_commit_scope re-resolved HEAD instead of using
        // `created_commit`, this would wrongly look like markcheck's own
        // hook had modified `other.md`.
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
        // The most alarming message in this module -- an out-of-scope commit
        // that could *not* be rolled back, so it's still sitting on the
        // branch and named for manual cleanup -- and it had no test. Forced
        // deterministically by making `undo_commit`'s compare-and-swap fail:
        // an unrelated commit lands on top of the one being verified, so
        // update-ref refuses (correctly -- rewinding would take the
        // concurrent commit with it), and verify_commit_scope has to report
        // a violation it couldn't undo rather than one it did.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        // Stands in for a commit a hook expanded beyond the checklist.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        fs::write(work.join("other.md"), "hook-added\n").unwrap();
        run(&work, &["add", "tracked.md", "other.md"]);
        run(&work, &["commit", "-q", "-m", "checklist plus hook spill"]);
        let created_commit = current_head(&work).unwrap();

        // Something else commits on top, so the CAS-guarded undo must refuse.
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
        // External review, round 4: the old undo_commit did a bare
        // `update-ref HEAD <parent>` with no compare-and-swap, so a commit
        // landing on top of the one being undone (between the commit and
        // the undo) would be silently rewound along with it. The
        // CAS-guarded update-ref must refuse instead.
        let work = init_repo_without_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "to be undone"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = undo_commit(&work, &created_commit);
        // Naming the *reason*, not just `is_err`: a bare `is_err` is satisfied
        // by any failure at all, so it still passes against an `undo_commit`
        // that never reaches the compare-and-swap — verified by mutation, an
        // unconditional `Err` at the top of the function passes a bare
        // `is_err` here. Requiring the message to come from `update-ref` pins
        // that the CAS ran and that *it* is what refused.
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
        // Same CAS guarantee as the test above, for the root-commit
        // (`parent: None`) undo path, which deletes the branch ref outright
        // instead of moving it.
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
        // Same reasoning as the test above: pin that the refusal came from the
        // compare-and-swap rather than from anything earlier.
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
        // The per-commit `diff-tree` calls are now one batched `--stdin`
        // call, so the parser sees every commit's paths in a single NUL
        // stream instead of one clean response per commit. Exercise it over
        // a range deliberately mixing shapes: checklist-only commits, a
        // multi-file commit, and a root commit (via the `base: None` call
        // shape `verify_commit_scope` uses).
        let work = init_repo_without_remote();

        // A run of checklist-only commits -- nothing unrelated yet.
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

        // The root commit itself only touched tracked.md, so the full
        // ancestry (base: None, which needs `--root` to show anything) is
        // still clean.
        assert_eq!(
            range_has_unrelated_commits(&work, None, &checklist_only_tip, "tracked.md"),
            Ok(false),
            "the root commit must be diffed, and it only touched the checklist"
        );

        // One commit touching two paths anywhere in the range flips it.
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

        // And an empty range short-circuits before the batched call.
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&mixed_tip), &mixed_tip, "tracked.md"),
            Ok(false),
            "an empty range has nothing to diff"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn verify_commit_scope_catches_a_revert_cancelled_unrelated_change() {
        // The same net-diff blind spot as branch_has_unrelated_unpushed_
        // commits, but for verify_commit_scope's own parent..created_commit
        // range -- a hook that made two nested commits where the second
        // reverts the first's unrelated change would net out to zero in a
        // plain diff, even though both extra commits are still in history
        // and still get pushed. Simulated directly here rather than through
        // an actual hook, since the mechanism being tested is
        // range_has_unrelated_commits itself, not hook plumbing.
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
            result.is_err_and(|e| e.contains("hook modified files beyond the checklist")),
            "the revert-cancelled unrelated commit must still be caught"
        );
        // Deep round 3 changed what "undo" means here. It used to reset
        // straight back to `parent`, sweeping away every commit in between —
        // but markcheck did not create those either, and the same reset is
        // what discarded a genuinely concurrent commit that landed in the
        // window before `git commit` (see `undo_commit`). Only markcheck's
        // own commit is rewound now; the hook's nested commits survive, are
        // named in the reported error, and the next sync's unpushed-history
        // guard refuses until the user deals with them.
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
        // MERGE_HEAD's presence alone is what's checked — no need to drive
        // an actual conflicted merge to exercise this specific marker file.
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
        // End-to-end: a genuine conflicted merge must be blocked — this is
        // the exact scenario a prior version of run_sync would have
        // silently "resolved" (see the regression test on run_sync below
        // for the full empirically-reproduced failure mode this replaces).
        // MERGE_HEAD's presence is what actually catches it here (checked
        // before the unmerged-index scan), which is fine: any of the gate's
        // checks blocking is the invariant that matters.
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
        // Isolates the ls-files -u check specifically: unmerged (stage
        // 1/2/3) index entries can exist without MERGE_HEAD too (e.g. after
        // `git checkout -m`, or a conflicted `stash pop`) — inject one
        // directly via update-index --index-info rather than relying on a
        // specific porcelain command to produce it. The conflicted path
        // (other.md) is deliberately *not* the file markcheck is syncing
        // (tracked.md) — the gate must look at the whole repository, not
        // just the target path, per the review's own recommendation.
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
    fn undo_commit_rewinds_one_commit_and_keeps_the_branch() {
        // The ordinary case the guard above must not disturb: a commit with
        // a parent moves the branch back exactly one commit, and the branch
        // itself survives.
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
        // `None` means the branch has no commits at all, so there is nothing
        // unpushed for anything to be unrelated to. Previously this question
        // was asked about the symbolic `HEAD`, which simply errored on an
        // empty repository and fell through to the same answer by accident.
        let work = init_repo_without_remote();
        assert_eq!(
            unpushed_history(&work, None, "x.md"),
            Ok(UnpushedHistory::Safe)
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn staged_target_guard_fails_closed_without_a_pre_write_hash() {
        // Safety can't be established without something to compare the
        // staged bytes against, so the guard refuses — the same rule
        // `unpushed_history` follows for an unanswerable check.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [ ] one\n- [ ] staged\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let head = git_stdout(&work, &["rev-parse", "HEAD:tracked.md"]);

        assert!(
            staged_target_would_be_lost(&work, "tracked.md", Some(&head), None),
            "no pre-write hash means the staged version cannot be cleared as safe"
        );
        // ...and an unstaged path is never at risk regardless.
        assert!(!staged_target_would_be_lost(
            &work,
            "other-untouched.md",
            None,
            None
        ));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    // --- Unpushed-history check: three answers, not two (round 10) ---

    #[test]
    fn an_unanswerable_history_check_refuses_instead_of_permitting_a_push() {
        // External review, round 10: this check was `.unwrap_or(false)`, so
        // any git failure — a `rev-list` timeout on a large history, a
        // transiently unhealthy object database — read exactly like
        // "verified safe" and the caller went on to push. A false refusal
        // costs a retry; a false clearance publishes someone else's commits.
        let (work, _remote) = init_repo_with_remote();
        // A tip that doesn't exist: `rev-list` fails, so the question
        // genuinely cannot be answered.
        let bogus = "0".repeat(40);

        assert!(
            unpushed_history(&work, Some(&bogus), "tracked.md").is_err(),
            "an unresolvable range must surface as an error, not a verdict"
        );
        let refusal = unpushed_history_blocks(&work, Some(&bogus), "tracked.md");
        assert!(
            matches!(&refusal, Some(SyncOutcome::Failed(msg)) if msg.contains("could not verify")),
            "the caller must refuse to publish: {refusal:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    // --- Captured-tip invariant (round 10) ---

    #[test]
    fn a_head_that_moves_after_the_tip_is_captured_cannot_broaden_the_push() {
        // External review, round 10. `run_sync`'s fast path and
        // `catch_up_push` both used to validate the unpushed range ending at
        // the *symbolic* `HEAD`, then separately re-resolve `HEAD` to get a
        // SHA for the push. Those are two different subprocesses, so a
        // commit landing between them was published having never been
        // checked. Both now capture the tip once and name that SHA at every
        // step.
        //
        // The interleaving is reproduced deterministically by moving `HEAD`
        // inside the window rather than by racing a real thread: what the
        // fix guarantees is that the *validated* SHA and the *pushed* SHA
        // are the same commit, which is exactly what this asserts.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        // The guard passes for that tip: its unpushed range is checklist-only.
        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::Safe)
        );

        // Now an unrelated commit lands — exactly what could happen between
        // the two subprocesses in the old code.
        fs::write(work.join("other.txt"), "unrelated\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);
        let moved = git_stdout(&work, &["rev-parse", "HEAD"]);
        assert_ne!(moved, tip, "test setup: HEAD must have moved");

        // Pushing the captured tip publishes exactly the validated range.
        assert_eq!(push(&work, &tip), SyncOutcome::Synced);
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "checklist\ninit",
            "the unrelated commit must not reach the remote"
        );

        // And had the moved `HEAD` been what the guard was asked about, it
        // would have refused — which is what makes checking one commit and
        // pushing another unsafe in the first place.
        assert_eq!(
            unpushed_history(&work, Some(&moved), "tracked.md"),
            Ok(UnpushedHistory::ContainsUnrelated)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_branch_without_an_upstream_is_not_a_verification_failure() {
        // The legitimate case that stops this failing closed everywhere: a
        // fresh repository has nothing to compare against, and refusing
        // there would block every first-ever commit. Committing proceeds;
        // `push` declines separately, with its own specific message.
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
        // Regression for a real mistake made while fixing the above: the
        // no-upstream case was first detected via `upstream_parts`, which
        // reads `branch.<name>.remote`/`.merge` *config*. Config can say an
        // upstream exists while `@{u}` still doesn't resolve — a remote
        // added but never fetched, or those keys set by hand — and treating
        // that as an unanswerable check turned three passing tests into hard
        // sync failures. Resolving `@{u}` itself is what tells them apart.
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
