//! Building the commit, through a temporary index.
//!
//! The sync never commits the working tree and never touches the real index
//! to do it: a scratch `GIT_INDEX_FILE` is seeded from the parent commit,
//! the one expected blob is staged into it, and an ordinary `git commit`
//! runs against that. Ordinary, so the repository's hooks and
//! `commit.gpgsign` still apply — `commit-tree` would bypass both.
//!
//! The real index is touched exactly once, afterwards, by
//! `align_real_index_entry`, and only for the checklist's own path.

use std::io;
use std::path::Path;
use std::time::Duration;

use super::guards::{repo_sync_blocked, verify_commit_scope};
use super::inspect::{blob_at, current_head, descends_directly_from, git_dir, index_blob};
use super::process::{PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout};
use super::{RacePoint, race_point};

/// Populates a fresh temporary index from `parent` (or leaves it empty for
/// a repository with no commits yet), stages `blob` into it at `mode`/
/// `relpath`, re-confirms `HEAD` hasn't moved since `parent` was read (a
/// residual, narrowed version of the same race a bare `update-ref` had —
/// `git commit` below safely serializes its *own* HEAD update, but the
/// temporary index's *content* for every path other than `relpath` was
/// still only as fresh as `parent`; re-checking here catches the common
/// case rather than silently committing a tree that reverts a
/// concurrently-landed, unrelated commit), and commits it — normal `git
/// commit`, so hooks and `commit.gpgsign` apply, never `commit-tree`. The
/// temporary index file is always removed afterward, success or failure;
/// the real index is never read, and the only write it ever makes is
/// `align_real_index_entry`'s single-path realignment after a successful
/// commit (see its own doc comment for why that's needed) — every other
/// path in the real index, staged or not, is left completely alone.
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
    // What the real index holds for this path *before* any of the work
    // below runs — the baseline `align_real_index_entry` compares against so
    // it never overwrites something the user staged in the meantime.
    let index_before = index_blob(repo_root, relpath);
    let result = (|| {
        if let Some(head) = parent {
            populate_temp_index(repo_root, &temp_index, head)?;
        }
        stage_into_temp_index(repo_root, &temp_index, mode, blob, relpath)?;
        if &current_head(repo_root) != parent {
            return Err("the repository changed while the sync was running".to_string());
        }
        // repo_sync_blocked (in run_sync) is only a snapshot taken before
        // any of the above ran; re-checking here narrows (not eliminates —
        // same residual-race tradeoff as the HEAD check just above) the
        // window in which a merge/rebase/etc. could have started since,
        // which would otherwise let this land as a merge-completing commit
        // instead of refusing outright.
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
            PLUMBING_TIMEOUT,
        )?;
        verify_commit_scope(repo_root, parent, relpath, &created_commit)?;
        race_point(RacePoint::BeforeIndexAlignment);
        align_real_index_entry(repo_root, mode, blob, relpath, index_before.as_deref());
        Ok(created_commit)
    })();
    let _ = std::fs::remove_file(&temp_index);
    // git's index-writing plumbing (`read-tree`/`update-index`/`commit`)
    // stages its write through `<index>.lock` (the full filename with a
    // literal `.lock` suffix appended, not a path-extension replacement —
    // `temp_index`'s own generated name never has an extension to begin
    // with), renaming it into place on success. A subprocess killed
    // mid-write by `run_with_timeout` can leave that lock file behind
    // without ever reaching the rename — the index-file cleanup just above
    // wouldn't catch it, since the target index itself may never have
    // existed at all in that case.
    let mut lock_file = temp_index.into_os_string();
    lock_file.push(".lock");
    let _ = std::fs::remove_file(lock_file);
    result
}
/// `GIT_INDEX_FILE=<temp_index> git read-tree <parent>`: seeds the
/// temporary index with `parent`'s tree, so every path other than the one
/// about to be replaced commits exactly as `parent` had it — never the real
/// index's (possibly unrelated-staged-content-holding) state.
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
/// `GIT_INDEX_FILE=<temp_index> git update-index --add --cacheinfo`: stages
/// `blob` for `relpath` at `mode` in the *temporary* index — `--add` since
/// the path may not already be present (a fresh repo's first commit, or a
/// file staged but never yet committed) — without touching the real index
/// or the working tree at all.
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
/// `git update-index --add --cacheinfo`, run against the *real* index (no
/// `GIT_INDEX_FILE` override) — called once, right after a commit succeeds,
/// to set the real index's entry for `relpath` to the blob/mode just
/// committed. Building the commit through a separate temporary index (see
/// `stage_into_temp_index`) keeps unrelated staged content from riding
/// along, but it also means the real index's own entry for `relpath` is
/// never advanced as a side effect the way a normal `git commit -- <path>`
/// would leave it — external review, round 9: reproduced empirically
/// against a clean repo with no staged content at all. Before any sync,
/// the real index matches `HEAD` for `relpath`; once the temp-index commit
/// moves `HEAD` forward without touching the real index, that path's real
/// entry is left pointing at the *old* blob — permanently, from the very
/// first toggle. From then on `git status` shows the file as both staged
/// (stale index vs. new `HEAD`) and not staged (current working tree vs.
/// that same stale index), recurring on every subsequent toggle, since
/// nothing else ever corrects it. This call is the fix: it realigns only
/// `relpath`'s own entry, mirroring exactly what committing that one path
/// normally leaves behind — every other real-index entry (e.g. an
/// unrelated file the user `git add`ed) is left completely untouched, so
/// `run_sync_never_includes_an_unrelated_staged_file`'s guarantee still
/// holds. Best-effort: the commit and any push it enables have already
/// succeeded by the time this runs, so a failure here (this is local
/// bookkeeping only, not part of the commit's correctness) is not worth
/// reporting as a sync failure.
/// External review, round 10: this write used to be unconditional, which
/// makes it the one place the real index is mutated without first checking
/// that the entry still belongs to markcheck. If the user stages a *newer*
/// version of the checklist while the background sync is running (`git add`
/// from another terminal, or an IDE doing it for them), overwriting the
/// entry silently discards their staged version. The working tree is
/// untouched either way, so this is staging loss rather than content loss —
/// but it is exactly the kind of unrelated state the temporary-index design
/// exists to protect, and the target path shouldn't be the one exception
/// that ignores it. `expected_entry` is the blob the real index held when
/// this sync began: matching means markcheck is still realigning its own
/// entry, and any mismatch means someone else has staged something since
/// and it is theirs to keep.
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
/// `GIT_INDEX_FILE=<temp_index> git commit -m <message>`: commits the
/// temporary index's tree against the repository's real `HEAD`/branch —
/// normal commit machinery (hooks, `commit.gpgsign`, HEAD's own locked
/// read-and-update), just fed from the temporary index instead of the real
/// one. Returns the new commit's SHA (resolved once, immediately after
/// success) — the single source of truth for "the commit markcheck just
/// made" that `verify_commit_scope`/`undo_commit` operate on, rather than
/// each re-resolving `HEAD` independently at a later point in time (which
/// would let a commit landing in between be mistaken for part of this
/// one).
///
/// `timeout` is a parameter (mirroring `run_with_timeout`'s own explicit
/// design) rather than reading `PLUMBING_TIMEOUT` directly, so tests can
/// inject a short one — needed to exercise the reconciliation below
/// deterministically via a hook that sleeps past it.
///
/// External review, round 4: `git commit` can run a `post-commit` hook —
/// which only ever runs *after* the ref update, `git commit`'s own
/// mutation, is already durable — as its last step before the process
/// exits. If that hook (or the commit machinery itself, past the point of
/// no return) is still running when `timeout` expires, `run_with_timeout`
/// kills the whole process group, and a naive caller would report the
/// commit as failed even though it had already succeeded. On a timeout
/// specifically (never on an ordinary nonzero exit, which never leaves
/// `HEAD` in doubt the way a killed process can), `HEAD` is re-resolved: if
/// it moved past `parent`, the commit is treated as having succeeded —
/// returning that new `HEAD` exactly as the success path would, so the
/// normal `verify_commit_scope` pipeline still runs against it (a hook
/// that also misbehaved during that same timed-out run is still caught and
/// undone normally). If `HEAD` didn't move, the commit genuinely never
/// happened and the original timeout is reported.
///
/// External review, round 9: `HEAD` moving past `parent` is not by itself
/// proof that *our* commit is what moved it — another process can
/// legitimately commit something else entirely during the same window
/// (most plausibly while our own commit is genuinely still blocked in a
/// slow hook, not actually done yet). Adopting that unrelated commit as
/// `created_commit` would be worse than reporting a false failure: the
/// scope check below would correctly flag it as out-of-scope (it never
/// touched `relpath`) and undo it — and `undo_commit`'s compare-and-swap
/// would succeed, because we handed it the *right* SHA for the *wrong*
/// reason, rewinding a real, unrelated commit off the branch. `relpath`
/// and `blob` (the exact content this call staged) are threaded through
/// so the timeout branch can positively confirm ownership before trusting
/// it, rather than inferring ownership from ref movement alone.
///
/// Ownership is confirmed by **three** checks, not one, because a content
/// check and a lineage check each catch a case the other misses:
///
/// * `HEAD` moved past `parent` at all — the original round-4 check.
/// * `relpath` at the new `HEAD` is exactly `blob`. Catches an unrelated
///   commit made directly on `parent` (our own commit still blocked in a
///   slow hook): it never touched `relpath`, so the blob there is still
///   the old one.
/// * the new `HEAD`'s parents are exactly `parent` (one parent equal to
///   it, or none at all when `parent` is `None` and this is a root
///   commit). Catches the mirror case the blob check alone cannot: *our*
///   commit landed **and** an unrelated commit landed on top of it without
///   touching `relpath`, which leaves our blob in place at `HEAD` and so
///   passes the content check while still naming the wrong SHA.
///
/// A commit that landed but got raced this way is reported as a failure
/// rather than adopted. That's a false negative — the commit really is
/// there — but it is the safe direction: `run_sync` returns `Failed`, the
/// next request retries from fresh state, and nothing is rewound.
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
mod tests {
    use super::super::inspect::hash_object;
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn commit_via_temp_index_refuses_when_head_moved_since_parent_was_read() {
        // Narrows (doesn't need to eliminate — see run_sync's doc comment)
        // the residual race a bare `update-ref HEAD` used to have with no
        // compare-and-swap at all: if some other commit landed after
        // `parent` was captured but before the temp index is actually
        // committed, the temp index's content for every path *other* than
        // the one being replaced would silently be stale (based on
        // `parent`, not the real current HEAD) — better to refuse and let
        // the next sync attempt retry from fresh state than to commit a
        // tree that quietly reverts the intervening change.
        let work = init_repo_without_remote();
        let stale_parent = current_head(&work);

        // A commit lands that `commit_via_temp_index`'s caller doesn't know
        // about yet — simulating a concurrent `git commit`/pull/rebase
        // racing the background sync thread.
        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &stale_parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("changed while the sync was running")),
            "{result:?}"
        );

        // Nothing committed on top of the concurrent change; no leftover
        // temp index file.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "concurrent change\ninit"
        );
        let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        });
        assert!(!leftover, "temp index file was not cleaned up");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn commit_via_temp_index_refuses_when_repo_becomes_blocked_after_the_head_check() {
        // External review: repo_sync_blocked's check in run_sync is only a
        // snapshot taken before any of the temp-index work below it runs;
        // a merge/rebase/etc. starting afterward but before the actual
        // `git commit` call would otherwise slip through undetected. This
        // exercises commit_via_temp_index's own re-check directly (a
        // marker present at call time), which narrows — not eliminates,
        // same as the HEAD race above — that window.
        let work = init_repo_without_remote();
        let parent = current_head(&work);
        fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();

        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result.as_ref().is_err_and(|e| e.contains("merge")),
            "{result:?}"
        );

        let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        });
        assert!(!leftover, "temp index file was not cleaned up");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn stage_into_temp_index_reports_failure_for_an_invalid_mode() {
        let work = init_repo_without_remote();
        let blob = hash_object(&work, "tracked.md", "content").unwrap();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(
            stage_into_temp_index(&work, &temp_index, "not-a-mode", &blob, "tracked.md").is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn populate_temp_index_reports_failure_for_an_unresolvable_parent() {
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(populate_temp_index(&work, &temp_index, "not-a-commit-id").is_err());
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn commit_temp_index_reports_failure_for_a_tree_unchanged_from_head() {
        // A temp index populated straight from HEAD, with nothing further
        // staged into it, produces the exact same tree HEAD already has —
        // the same "empty commit refused" behavior normal `git commit`
        // always has, exercised here through the temp-index path
        // specifically.
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        let head = current_head(&work).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [ ] one\n").unwrap();
        populate_temp_index(&work, &temp_index, &head).unwrap();
        assert!(
            commit_temp_index(
                &work,
                &temp_index,
                "empty",
                &Some(head.clone()),
                "tracked.md",
                &blob,
                PLUMBING_TIMEOUT
            )
            .is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_reconciles_when_a_post_commit_hook_outlives_the_timeout() {
        // External review, round 4: a `post-commit` hook only ever runs
        // *after* the ref update is already durable. If `run_with_timeout`
        // kills the process group while that hook is still running, the
        // commit itself already succeeded — reporting it as failed would be
        // wrong. A short injected timeout plus a hook that outlives it
        // reproduces this deterministically, without needing a real
        // multi-second wait against `PLUMBING_TIMEOUT` itself.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-timeout");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "reconciled despite timeout",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        let new_head = current_head(&work);
        assert!(
            new_head.is_some() && new_head != Some(parent),
            "the commit itself must have actually landed: {new_head:?}"
        );
        assert_eq!(
            result.as_deref().ok(),
            new_head.as_deref(),
            "a timeout after the ref already moved must report success, not a false failure: {result:?}"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_still_fails_when_the_commit_never_lands() {
        // Companion to the test above: a `pre-commit` hook runs *before*
        // the ref update, so a timeout while it's still sleeping means the
        // commit genuinely never happened — this must still report a
        // failure, not paper over a real one.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-never-lands");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not land",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(parent),
            "the commit must never have happened"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_an_unrelated_commit_after_a_timeout() {
        // External review, round 9: HEAD moving past `parent` after a
        // timeout used to be treated as proof "our commit succeeded" --
        // but HEAD can move because something else entirely committed
        // during the same window, not because our own git commit actually
        // landed. A pre-commit hook that (after unsetting the inherited
        // GIT_INDEX_FILE, standing in for a genuinely separate process
        // rather than our own temp-index machinery) commits something
        // unrelated to the *real* branch, then sleeps past the timeout,
        // means our own temp-index commit never actually happens (blocked
        // in pre-commit) -- HEAD moves, but not because of us.
        //
        // `--no-verify` on the nested commit is load-bearing, not tidiness:
        // without it that commit runs `pre-commit` again, which commits
        // again, recursively, so nothing ever lands, HEAD never moves, and
        // the test neither reproduces the scenario nor exercises the
        // ownership check (it would pass against the pre-fix code purely on
        // the older `HEAD != parent` half). Verified by hand: with
        // `--no-verify` HEAD does move to the unrelated commit while
        // `HEAD:tracked.md` stays the *old* blob -- exactly the state the
        // blob check exists to reject.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated concurrent commit'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-unrelated");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not adopt the unrelated commit",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        // Asserting the *message*, not just `is_err`, so this can't quietly
        // start passing again for the old reason (the commit never landing
        // at all) rather than for the ownership check rejecting it.
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the unrelated commit as its own: {result:?}"
        );
        let head_after = current_head(&work).unwrap();
        assert_ne!(
            head_after, parent,
            "the unrelated commit must still exist, untouched"
        );
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("unrelated concurrent commit"),
            "{}",
            String::from_utf8_lossy(&log.stdout)
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_a_commit_that_landed_on_top_of_its_own() {
        // The mirror of the test above, and the case a content check alone
        // cannot catch: *our* commit lands, and an unrelated commit lands on
        // top of it before HEAD is re-resolved. That unrelated commit never
        // touched `relpath`, so `blob_at(HEAD, relpath)` is still exactly
        // our blob -- the blob check passes -- yet HEAD names the wrong
        // commit. Adopting it would hand `verify_commit_scope` a SHA whose
        // range contains the unrelated commit, and `undo_commit`'s
        // compare-and-swap would then succeed and rewind *both* off the
        // branch. The lineage check (`descends_directly_from`) is what
        // rejects it.
        //
        // A `post-commit` hook is the natural driver: it only ever runs once
        // the ref update is already durable, so our own commit really has
        // landed by the time it commits on top. `--no-verify` on the nested
        // commit avoids the hook recursing into itself, and the `read-tree
        // HEAD` first is what makes this the *mirror* case rather than a
        // second copy of the test above: the real index still holds the
        // pre-sync `tracked.md` (nothing has run `align_real_index_entry`
        // at this point), so committing it as-is would revert our blob and
        // the content check would reject the commit for the wrong reason.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 env -u GIT_INDEX_FILE git read-tree HEAD\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated commit on top'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-on-top");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "tracked.md", "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "ours, then raced",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the commit that landed on top of ours: {result:?}"
        );
        // Both commits are still there, untouched -- refusing must never
        // rewind anything, least of all the unrelated commit.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("unrelated commit on top"), "{subjects}");
        assert!(subjects.contains("ours, then raced"), "{subjects}");
        // The blob check alone would have passed here: our content really is
        // what HEAD holds for the checklist path.
        assert_eq!(
            blob_at(&work, "HEAD", "tracked.md").as_deref(),
            Some(blob.as_str()),
            "test setup: the content check must be the one that would pass"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    // --- Real-index realignment (round 10) ---

    #[test]
    fn align_real_index_entry_leaves_a_newer_staged_version_alone() {
        // External review, round 10: this write was unconditional, so a
        // `git add` of the checklist from another terminal (or an IDE) while
        // the background sync ran had its staged version silently replaced.
        // The working tree is untouched either way, so it is staging loss
        // rather than content loss — but it is exactly the unrelated state
        // the temporary-index design exists to protect.
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");
        assert!(before.is_some(), "test setup: the file is tracked");

        // The user stages a newer version while the sync is in flight.
        fs::write(work.join("tracked.md"), "- [x] staged by the user\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let user_staged = index_blob(&work, "tracked.md").unwrap();
        assert_ne!(Some(&user_staged), before.as_ref(), "test setup");

        // markcheck finishes and would realign to its own committed blob.
        let committed = hash_object(&work, "tracked.md", "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(user_staged),
            "the user's staged version must survive untouched"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn align_real_index_entry_still_realigns_its_own_untouched_entry() {
        // The case the realignment exists for: nothing else staged the path,
        // so the entry is markcheck's to advance. Without this, `git status`
        // shows the file as both staged and unstaged after every toggle.
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");

        let committed = hash_object(&work, "tracked.md", "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(committed),
            "an untouched entry is still advanced to the committed blob"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
