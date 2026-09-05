//! The sync itself: one request, start to finish.
//!
//! `run_sync` is the sequence every other module in `git_sync` exists to
//! serve — classify the file, run the guards, build the commit, verify what
//! landed, publish it. Reading it top to bottom is the shortest description
//! of what the feature actually does, and the order of its steps is load
//! bearing: each guard runs before the work it protects, never after.
//!
//! Also `commit_message`, which turns a change description into the one-line
//! message that commit carries.

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
use super::process::{PLUMBING_TIMEOUT, command_error, git_command, run_with_timeout};
use super::push::{push, push_if_head_unchanged};
use super::{RacePoint, SyncOutcome, lock_hash, race_point};

/// Commit messages are kept to one line and capped here so a long task
/// title (the usual source of `change_desc`) can't produce an unwieldy
/// `git log` entry; the description is what gets cut, with a trailing `…`
/// marking the cut, and the file-name prefix is kept intact — unless the
/// file name is itself long enough to blow the cap on its own, in which
/// case it's cut too (see `MAX_FILE_NAME_LEN`).
const MAX_COMMIT_MESSAGE_LEN: usize = 80;
/// The smallest description budget worth leaving room for, so a truncated
/// message still says *something* about the change instead of degenerating
/// to a bare ellipsis.
const MIN_DESC_LEN: usize = 8;
/// How much of the file name the prefix may use. Without this bound,
/// `budget` below saturates to 0 for a file name of ~79 characters or more
/// and the result is `<prefix>…` — *longer* than `MAX_COMMIT_MESSAGE_LEN`,
/// which is the one property that constant exists to guarantee. Accounts
/// for the prefix's own `": "` plus a minimum description and its ellipsis.
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
    // change_desc is usually `Verb "item text"`; if the cut lands inside the
    // quoted item text, the opening `"` would otherwise never be closed.
    if truncated.matches('"').count() % 2 == 1 {
        let shorter: String = change_desc.chars().take(budget.saturating_sub(1)).collect();
        format!("{prefix}{shorter}\u{2026}\"")
    } else {
        format!("{prefix}{truncated}\u{2026}")
    }
}
/// Runs the commit+push sequence synchronously; called from the background
/// thread spawned by `spawn`, kept as a free function so tests can drive it
/// directly without waiting on a thread.
///
/// The commit is built entirely from `expected_content` rather than
/// re-reading the working tree at commit time — despite appearances, `git
/// commit --only`/a plain pathspec always re-reads the file's *current
/// working-tree content* for the named path (it stages it fresh, ignoring
/// whatever's already in the index for that path), never a manually staged
/// index entry. That's the hazard the exact-content approach closes: if
/// some other change (another markcheck write, an external editor,
/// anything) lands on disk between a request being queued and this function
/// actually running, a working-tree-reading commit would silently absorb it
/// under a message that only describes the *original* request. A later,
/// unrelated change still gets synced — as its own, separately labeled
/// commit — *whenever something actually requests one*, which it just can
/// never bleed into this one. External review, round 5: that qualifier
/// matters — a passive file-watcher reload never requests one (see
/// `AppState::request_external_edit_sync`'s doc comment), so a request that
/// goes stale that way would otherwise never be corrected. See the
/// staleness check below (`latest_requested_hash`) for how this function
/// refuses rather than publishing a request once that's happened, without
/// weakening the guarantee in this paragraph.
///
/// The commit itself is built via a **temporary index** (`GIT_INDEX_FILE`)
/// populated from `HEAD` plus exactly one replaced path, committed with a
/// normal `git commit` — not by hand-assembling tree/commit objects and
/// moving `HEAD` with `update-ref`, which an earlier version of this
/// function did. That manual-plumbing approach used the *real* repo index
/// for `write-tree`, which silently pulled in anything else the user had
/// staged (`git add`ed but not yet committed) into markcheck's commit, and
/// used a bare `update-ref HEAD <new>` with no compare-and-swap against a
/// concurrently-moving `HEAD`. A temporary index sidesteps the first
/// problem entirely (the real index is never read or written), and a
/// normal `git commit` sidesteps the second (it does its own HEAD read as
/// part of one locked operation, not a separate read-then-blindly-write
/// step on markcheck's side) while also running the repo's normal commit
/// hooks and honoring `commit.gpgsign`, neither of which `commit-tree`
/// ever did. `repo_sync_blocked` (called first, below) covers the one
/// thing a normal commit *doesn't* refuse on its own: committing while the
/// repository is mid-merge/rebase/cherry-pick/etc., which would otherwise
/// silently resolve a conflict or advance past unfinished work.
pub(super) fn run_sync(
    repo_dir: &Path,
    file_path: &Path,
    expected_content: &str,
    message: &str,
    latest_requested_hash: &Mutex<[u8; 32]>,
    previous_content: Option<&str>,
) -> SyncOutcome {
    // All plumbing commands run from the repo root with a root-relative
    // path, sidestepping any ambiguity between CWD-relative and
    // repo-root-relative pathspec handling. Resolved first (before even
    // `status`) since every other check below needs it.
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };

    // Refuse outright while the repository is in a state where an automatic
    // commit could do real damage (see repo_sync_blocked's own doc comment)
    // — checked before anything else, so a mid-merge repository never even
    // reaches the status check below.
    if let Some(reason) = repo_sync_blocked(&repo_root) {
        return SyncOutcome::Failed(reason);
    }

    // Scoped to exactly this one file (`--`), so there's at most one
    // porcelain line to interpret: absent, `?? ` (untracked), or any other
    // two-letter code (a real change to commit). A leading `??` is taken as
    // untracked here because it is unambiguous, but **empty output is not
    // taken as "tracked and unchanged"** — see the index lookup below, which
    // is what actually settles trackedness. `status` is only trusted for the
    // question it can always answer: has anything changed.
    let mut cmd = git_command(repo_dir);
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

    // The path relative to the repo root (required by the plumbing commands
    // below, several of which don't share `status`/`commit`/`push`'s
    // CWD-relative pathspec handling) plus the file's tracked mode, in one
    // call. Safe to read from the real index now that repo_sync_blocked has
    // already confirmed there's no unmerged (conflicted) entry for it.
    //
    // Deliberately resolved **before** the empty-status short-circuit below.
    // Deep round 2: `git status` is not a reliable untrackedness oracle, and
    // is silent about an untracked file in two ordinary configurations —
    // `status.showUntrackedFiles=no`, and the checklist matching a
    // `.gitignore` rule (both confirmed). Either one produced empty output,
    // which the short-circuit read as "nothing to do" and reported as
    // `Skipped` — silently, forever, which is exactly the "git-sync can
    // never do anything and looks like a bug" case `SkippedUntracked` exists
    // to surface. The index is the authority on whether a path is tracked,
    // so trackedness is settled here first and `status` is left to answer
    // only the question it can: has anything changed.
    let (mode, relpath) = match index_entry(repo_dir, file_path) {
        Ok(Some(entry)) => entry,
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if status.stdout.is_empty() {
        return SyncOutcome::Skipped;
    }

    // Refuse if the branch already has unpushed history that touches
    // something other than this file, rather than publishing it as a side
    // effect — checked before *both* branches below that can push (see
    // `unpushed_history`'s doc comment for why plain
    // ahead-of-upstream isn't the right test here — several of markcheck's
    // own commits can legitimately stack up while offline). External
    // review, round 7: this used to run only right before building a new
    // commit, on the assumption that the `blob_bytes_at`-matches fast path
    // below is always "markcheck's own prior commit, always legitimate to
    // push" — false whenever an unrelated commit lands on top of that
    // prior commit without touching the checklist file at all: the
    // checklist's blob at `HEAD` still matches `expected_content` (the
    // unrelated commit never touched it), so the fast path would push
    // `HEAD` — and the unrelated commit riding along with it — without
    // this check ever running. Reachable whenever a request queued
    // earlier gets processed after both a newer, not-yet-committed edit
    // has landed on disk (so `status` above is non-empty, reaching this
    // far) *and* an unrelated commit has landed on the branch — see the
    // fast path's own comment below for why `status` can be non-empty
    // even once this older request is already satisfied at `HEAD`.
    //
    // Resolve the branch tip **once**, and ask every question below about
    // that exact commit — this guard, the already-committed fast path, the
    // push, and the parent a new commit is built on.
    //
    // External review, round 10: these used to re-resolve `HEAD`
    // independently at each step, so this guard validated the range ending
    // at one commit while the fast path pushed whatever `HEAD` had become by
    // the time it ran, a few subprocesses later. A commit landing in that
    // window was published without ever having been checked. Capturing the
    // SHA closes it outright rather than narrowing it: `push` targets an
    // explicit SHA, so the ancestry it can publish is exactly the range that
    // was validated, no matter what `HEAD` does afterwards — no re-check and
    // no retry path needed. `resolve_parent` rather than `current_head`
    // because only it tells "no commits yet" apart from a failed lookup,
    // which the commit path below depends on.
    let tip = match resolve_parent(&repo_root) {
        Ok(tip) => tip,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if let Some(refusal) = unpushed_history_blocks(&repo_root, tip.as_deref(), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);

    // If HEAD already holds exactly this content, the commit half of this
    // request was already satisfied by an earlier sync (e.g. it sat
    // coalesced behind one that committed the same or newer content) —
    // even though `status` above is non-empty because of someone else's
    // still-uncommitted change to the file. That's not the same as the
    // request being *fully* satisfied, though: if the commit hasn't reached
    // upstream yet (a prior push failed), there's still a push worth
    // retrying — see `ahead_of_upstream`'s doc comment for why this matters.
    //
    // Both the content check and the push name `tip` — the same SHA the
    // guard above validated — so this path can only ever publish history
    // that was actually checked. It used to read the blob at the symbolic
    // `HEAD` and then separately re-resolve `HEAD` for the push, which are
    // not guaranteed to be the same commit.
    //
    // A `tip` of `None` (no commits yet) simply isn't this path: there is no
    // committed blob to match, so it falls through to build the first
    // commit. That also subsumes the old unresolvable-`HEAD` failure here,
    // which existed because `current_head(...).unwrap_or_default()` produced
    // `""`, and `push` formats that into the refspec `<remote> :<branch>` —
    // git's *delete a remote ref* form, which succeeds and reports `Synced`
    // while the branch is gone. `push` still rejects an empty SHA outright.
    if let Some(tip) = tip.as_deref()
        && blob_bytes_at(&repo_root, tip, &relpath).as_deref() == Some(expected_content.as_bytes())
    {
        if ahead_of_upstream(&repo_root, tip) {
            return push(repo_dir, tip);
        }
        return SyncOutcome::Skipped;
    }

    // `expected_content` is a snapshot captured whenever the request was
    // queued — possibly well before this background thread actually gets
    // to it (coalesced behind a slow push, a busy worker). External
    // review, round 5: nothing above re-confirms that snapshot is still
    // accounted for by the file's *current* revision before committing it.
    //
    // A naive "disk must equal expected_content" check is too strict,
    // though: several of markcheck's own toggles can legitimately land on
    // disk in quick succession (each one queuing its own request) before
    // the *first* request's background thread ever gets scheduled — by
    // the time it runs, disk already reflects a *later*, already-queued
    // request, not an unrelated external change. That's fine (the later
    // request's own sync will commit that content momentarily); it's not
    // the case this check needs to catch.
    //
    // What actually matters: does the file's current content correspond
    // to *some* request git-sync knows about — either this one, or a newer
    // one already queued to run next — or is it something entirely
    // outside that system (an edit not made through markcheck's own `e`,
    // or a deletion)? `latest_requested_hash` is updated on every
    // `request()` call, including coalesced ones, so it always reflects
    // the newest content git-sync has ever been asked to sync. If disk
    // matches that, whatever's there is already accounted for (either by
    // this commit or a following one); if it doesn't, the change is
    // unaccounted for and must not be silently published. The passive
    // file-watcher reload deliberately never queues a request of its own
    // (see "Isolated commits" below), so without this check, nothing would
    // ever correct the record for that case. `fs::read` failing (the file
    // was deleted) is always unaccounted for, regardless of
    // `latest_requested_hash` — committing `expected_content` regardless
    // would otherwise silently resurrect a file the user just deleted.
    match fs::read(file_path) {
        Ok(bytes) if hash_bytes(&bytes) == *lock_hash(latest_requested_hash) => {}
        _ => {
            return SyncOutcome::Failed(
                "the file changed after this sync was queued; toggle again to sync the latest version"
                    .to_string(),
            );
        }
    }

    // Refuse if committing the working tree would drop a staged version of
    // the checklist that exists nowhere else. Checked here, after the fast
    // path (which never builds a commit, so it can't lose anything) and
    // before any commit is constructed.
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

    // The same tip captured before the guards ran — not a fresh resolution,
    // so the commit is built on exactly the history that was validated.
    // `commit_via_temp_index` re-checks `HEAD == parent` immediately before
    // committing and refuses if anything moved in between.
    let parent = tip;
    let created_commit =
        match commit_via_temp_index(&repo_root, &parent, &mode, &blob, &relpath, message) {
            Ok(sha) => sha,
            Err(err) => return SyncOutcome::Failed(err),
        };

    push_if_head_unchanged(repo_dir, &repo_root, &created_commit)
}

#[cfg(test)]
mod tests {
    use super::super::inspect::{
        current_branch_ref, current_head, index_blob, resolve_upstream, upstream_parts,
    };
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;

    /// A repository with a clean filter configured, where the working tree
    /// and the blob git stores differ by exactly that filter.
    /// External review of `8a405dd`. The local remote-tracking ref is a
    /// cache, so after somebody deletes the branch upstream it still points
    /// at the last commit this clone saw and `@{u}` still resolves. An
    /// explicit `<sha>:refs/heads/<branch>` refspec then *creates* the
    /// branch again rather than being rejected — confirmed against a real
    /// remote, which reports `* [new branch]`. Ticking a checkbox must not
    /// republish history somebody deliberately removed.
    #[test]
    fn a_branch_deleted_on_the_remote_is_not_recreated_by_a_sync() {
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        fs::write(&file, content).unwrap();

        // The remote drops the branch; nothing fetches, so the local view
        // goes stale exactly as it would in practice.
        run(&remote, &["update-ref", "-d", "refs/heads/main"]);
        assert!(
            !git_stdout(&work, &["rev-parse", "--verify", "-q", "origin/main"]).is_empty(),
            "test setup: the stale tracking ref must still be present, or \
             this exercises the no-upstream path instead"
        );

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("no longer exists")),
            "the sync must commit locally and refuse to publish: {outcome:?}"
        );
        assert_eq!(
            git_stdout(
                &remote,
                &["for-each-ref", "--format=%(refname)", "refs/heads/"]
            ),
            "",
            "the deleted branch must still be absent from the remote"
        );
        // The work is not lost — it is committed locally, waiting for the
        // user to decide where it should go.
        assert!(
            git_stdout(&work, &["log", "--format=%s", "-1", "main"]).contains("Check"),
            "the commit itself must have been made locally"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_reports_an_untracked_file_without_adding_it() {
        let work = init_repo_without_remote();
        let untracked = work.join("untracked.md");
        fs::write(&untracked, "- [ ] new\n").unwrap();

        assert_eq!(
            run_sync(
                &work,
                &untracked,
                "- [ ] new\n",
                "should not commit",
                &no_race("- [ ] new\n"),
                None,
            ),
            SyncOutcome::SkippedUntracked
        );
        // Confirm it really never got added.
        let status = Command::new("git")
            .current_dir(&work)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("?? untracked.md"));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_skips_when_tracked_file_is_unchanged() {
        let work = init_repo_without_remote();
        assert_eq!(
            run_sync(
                &work,
                &work.join("tracked.md"),
                "- [ ] one\n",
                "no changes",
                &no_race("- [ ] one\n"),
                None,
            ),
            SyncOutcome::Skipped
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_commits_and_pushes_a_tracked_change() {
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "tracked.md: Check \"one\""
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_leaves_the_real_index_matching_head_after_a_commit() {
        // External review, round 9: reproduced empirically against a
        // freshly cloned repo with no staged content at all -- before this
        // fix, the temp-index commit advanced HEAD for `tracked.md` without
        // ever advancing the real index's own entry for it, so `git
        // status` permanently showed the file as both staged (stale index
        // vs. new HEAD) and not staged (current working tree vs. that same
        // stale index), starting from the very first sync.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let status = Command::new("git")
            .current_dir(&work)
            .args(["status", "--porcelain", "--", "tracked.md"])
            .output()
            .unwrap();
        assert!(
            status.stdout.is_empty(),
            "real index must match HEAD and the working tree after a commit: {:?}",
            String::from_utf8_lossy(&status.stdout)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_reports_failure_when_push_has_no_remote() {
        let work = init_repo_without_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

        let outcome = run_sync(
            &work,
            &work.join("tracked.md"),
            "- [x] one\n",
            "Check \"one\"",
            &no_race("- [x] one\n"),
            None,
        );
        // The commit itself succeeded (nothing was lost); only the push
        // failed, which is why this is `CommittedNotPushed` rather than
        // `Failed` — see the `SyncOutcome` doc comments.
        assert!(matches!(outcome, SyncOutcome::CommittedNotPushed { .. }));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_commits_and_pushes_on_a_non_default_branch() {
        // Every other git-sync test runs on `main`, which a bare remote
        // refuses to delete out from under itself — masking any refspec
        // mistake. A topic branch has no such protection and is the shape
        // of every real branch this project's own workflow uses.
        let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", &format!("{branch}:tracked.md")])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&show.stdout),
            "- [x] one\n",
            "the topic branch must carry the change, not be deleted or left behind"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_never_publishes_unrelated_work_when_no_upstream_is_configured() {
        // Deep review, reproduced empirically. Two fail-safe decisions that
        // are each locally reasonable compose into the exact hazard
        // `unpushed_history` exists to prevent:
        //
        //   * that guard resolves `@{u}..HEAD`, which *fails* with no
        //     upstream configured, and it deliberately fails closed
        //     ("don't refuse") so a fresh repo's first commit isn't blocked;
        //   * `push` used to fall back to a bare `git push` for the same
        //     "no upstream" case, justified on the premise that a bare push
        //     reports a clear error.
        //
        // With `push.default = current` a bare push resolves a destination
        // and succeeds, so both guards were off at once and an unrelated
        // local commit rode out with the checklist change. `push` now
        // refuses instead.
        let root = unique_dir("repo-no-upstream");
        let remote = root.join("remote.git");
        let work = root.join("work");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&work).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);
        run(&work, &["config", "push.default", "current"]);
        fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        run(&work, &["commit", "-q", "-m", "init"]);
        // A remote, but deliberately no `-u`: `branch.main.remote` and
        // `branch.main.merge` stay unset, so `upstream_parts` is `None` and
        // `@{u}` can't be resolved.
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        assert!(
            upstream_parts(&work).is_none(),
            "test setup: no upstream must be configured"
        );

        // The user's own unrelated work-in-progress commit.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("no upstream yet")),
            "{outcome:?}"
        );
        // Nothing reached the remote at all -- it has no branches yet.
        let branches = Command::new("git")
            .current_dir(&remote)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the unrelated commit must never reach the remote: {:?}",
            String::from_utf8_lossy(&branches.stdout)
        );

        fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn run_sync_refuses_to_commit_content_that_went_stale_before_the_worker_ran() {
        // External review, round 5: an earlier version of this test asserted
        // the opposite of what's checked here — that committing exactly the
        // queued snapshot, even once the working tree has more written to
        // it, was correct, on the theory that "a later sync picks up the
        // rest." That's false whenever nothing ever triggers a later sync —
        // a plain file-watcher reload (unlike the explicit `e`-editor
        // return path) never queues one, on purpose (see "Isolated
        // commits" below). A request that's gone stale must now be
        // refused rather than published, without ever absorbing B into
        // this commit either.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"A\"");

        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();

        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] A\n",
            &message,
            &no_race("- [x] A\n"),
            None,
        );
        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed after this sync was queued")),
            "{outcome:?}"
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "init",
            "nothing must be committed at all -- neither the stale snapshot nor a mix with B"
        );

        // B is untouched on disk -- refusing never resurrects, reverts, or
        // otherwise rewrites the working tree.
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] A\n- [x] B\n"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_to_recommit_a_file_deleted_after_the_request_was_queued() {
        // External review, round 5: the same staleness gap, but for a
        // deletion -- git status still sees a deleted-in-worktree tracked
        // file (not `??`, not empty) as something to proceed on, so without
        // this check the stale queued content would be committed via the
        // temp index regardless, resurrecting a file the user just deleted
        // locally.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        fs::remove_file(&file_path).unwrap();

        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );
        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed after this sync was queued")),
            "{outcome:?}"
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "init",
            "the deleted file must not be resurrected in a commit"
        );
        assert!(!file_path.exists(), "the deletion must be left alone");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_recovers_on_the_next_request_after_a_stale_refusal() {
        // A refusal must be a one-shot skip of this specific stale request,
        // not a stuck state -- the very next sync, carrying the file's
        // actual current content, must succeed normally.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");

        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
        let stale_message = commit_message(&file_path, "Check \"A\"");
        assert!(matches!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n",
                &stale_message,
                &no_race("- [x] A\n"),
                None,
            ),
            SyncOutcome::Failed(_)
        ));

        let fresh_message = commit_message(&file_path, "Edited in vim");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] A\n- [x] B\n",
            &fresh_message,
            &no_race("- [x] A\n- [x] B\n"),
            None,
        );
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_never_includes_an_unrelated_staged_file() {
        // External review, empirically reproduced against the prior
        // implementation before this fix: `write-tree` against the *real*
        // index serialized everything in it, not just the checklist path —
        // so a file the user had `git add`ed but not yet committed
        // silently rode along inside markcheck's commit, under a message
        // describing only the checklist change, then got pushed. The
        // temp-index rewrite must never see the real index's staged
        // content at all.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        fs::write(work.join("secret.env"), "SECRET_API_KEY=xyz123\n").unwrap();
        run(&work, &["add", "secret.env"]);

        let message = commit_message(&file_path, "Check \"one\"");
        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "--stat", "HEAD"])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&show.stdout);
        assert!(
            !stat.contains("secret.env"),
            "the unrelated staged file must never appear in the pushed commit: {stat}"
        );

        // The real index must be completely untouched — secret.env is
        // still exactly as the user staged it, ready for their own commit.
        let index = Command::new("git")
            .current_dir(&work)
            .args(["ls-files", "--stage", "secret.env"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&index.stdout).is_empty(),
            "secret.env must remain staged in the real index"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[cfg(unix)]
    #[test]
    fn run_sync_undoes_the_commit_when_a_hook_stages_extra_files() {
        // External review: the temp-index rewrite closes the *staged
        // file* contamination path (the test above), but a normal `git
        // commit` still runs the repository's commit hooks, and a hook
        // that itself runs `git add` inherits the same `GIT_INDEX_FILE` --
        // so it can stage extra files into the *temporary* index instead,
        // defeating the same guarantee through a different door. This is
        // that door: a real, executable `pre-commit` hook that stages an
        // unrelated file, verifying markcheck detects it and undoes the
        // whole commit rather than letting it stand (let alone push it).
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed files other than the checklist")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            parent,
            "the commit must be fully undone, not just refused going forward"
        );
        let head_has_other = Command::new("git")
            .current_dir(&work)
            .args(["cat-file", "-e", "HEAD:other.md"])
            .status()
            .unwrap()
            .success();
        assert!(
            !head_has_other,
            "the hook-staged file must never reach a reachable commit"
        );
        let real_index = Command::new("git")
            .current_dir(&work)
            .args(["ls-files", "other.md"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&real_index.stdout).is_empty(),
            "the real index must stay untouched by the hook too"
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
    #[cfg(unix)]
    #[test]
    fn run_sync_undoes_a_hook_violated_root_commit_by_deleting_the_branch_ref() {
        // The root-commit case (parent: None) needs its own undo path --
        // there's no prior SHA to move the branch ref back to, so
        // undo_commit must resolve the branch's symref and delete it
        // entirely instead, returning to the pre-commit "no commits yet"
        // state.
        use std::os::unix::fs::PermissionsExt;

        let work = unique_dir("repo-root-commit-hook").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        // Staged (so index_entry can read it) but never committed -- a
        // fresh repo's first sync, with no HEAD yet at all.
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);

        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            staged_matches("- [x] one\n"),
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("changed files other than the checklist")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work),
            None,
            "the repository must be back to having no commits at all"
        );
        assert_eq!(
            current_branch_ref(&work).as_deref(),
            Some("refs/heads/main"),
            "HEAD must still symbolically resolve to the branch, just with no commit on it yet"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_during_an_unresolved_merge_conflict() {
        // External review, empirically reproduced against the prior
        // implementation before this fix: with a genuine unresolved merge
        // conflict on the checklist file, the plumbing sequence silently
        // collapsed the 3-stage conflict entry to markcheck's own expected
        // content, wrote a normal (non-merge) commit, and advanced HEAD
        // past the in-progress merge — while `MERGE_HEAD` and the
        // conflict-marked working tree were left behind, exactly as if the
        // merge had never been dealt with. run_sync must now refuse
        // outright instead, leaving the merge exactly as the user left it.
        let work = init_repo_without_remote();
        run(&work, &["checkout", "-q", "-b", "branch-a"]);
        fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
        run(&work, &["commit", "-q", "-am", "a"]);
        run(&work, &["checkout", "-q", "main"]);
        fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
        run(&work, &["commit", "-q", "-am", "b"]);
        let head_before_merge_attempt = current_head(&work).unwrap();
        let _ = Command::new("git")
            .current_dir(&work)
            .args(["merge", "-q", "branch-a"])
            .output();
        assert!(
            work.join(".git").join("MERGE_HEAD").exists(),
            "test setup: real conflict expected"
        );

        let file_path = work.join("tracked.md");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] resolved\n",
            "should not commit",
            &no_race("- [x] resolved\n"),
            None,
        );
        assert!(matches!(outcome, SyncOutcome::Failed(_)), "{outcome:?}");

        // Nothing about the in-progress merge was touched: HEAD never
        // moved, MERGE_HEAD still marks the merge as unfinished, and the
        // conflict markers are still in the working tree.
        assert_eq!(current_head(&work).unwrap(), head_before_merge_attempt);
        assert!(work.join(".git").join("MERGE_HEAD").exists());
        let contents = fs::read_to_string(&file_path).unwrap();
        assert!(
            contents.contains("<<<<<<<"),
            "conflict markers must still be present, untouched: {contents:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_later_request_commits_the_concurrent_change_separately() {
        // The disk already holds "A\nB\n" before either sync runs, standing
        // in for a rapid double-toggle: the first request (content "A") was
        // queued, then a second toggle wrote "A\nB\n" and queued its own
        // request before the first one's worker got scheduled. That second
        // request is why `latest` is set to "A\nB\n" for both calls here --
        // git-sync already knows about it (it's `pending`, about to be
        // spawned the moment the first finishes), so the first request's
        // own staleness check must not refuse just because disk has moved
        // past its own content.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
        let latest = no_race("- [x] A\n- [x] B\n");

        let first_message = commit_message(&file_path, "Check \"A\"");
        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n",
                &first_message,
                &latest,
                None
            ),
            SyncOutcome::Synced
        );

        let second_message = commit_message(&file_path, "Check \"B\"");
        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n- [x] B\n",
                &second_message,
                &latest,
                None,
            ),
            SyncOutcome::Synced
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("tracked.md: Check \"A\""));
        assert!(subjects.contains("tracked.md: Check \"B\""));

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_skips_when_expected_content_already_matches_head_and_upstream() {
        // A remote is required here (unlike most `run_sync` tests): a
        // truly `Skipped` result now requires HEAD to both match the
        // expected content *and* already be pushed — see `ahead_of_upstream`.
        let (work, _remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        // Working tree has drifted further (an unrelated concurrent change)
        // so `git status` is non-empty, but this specific request's
        // expected content is already what's committed at HEAD *and*
        // already pushed (by `init_repo_with_remote`'s own setup) — e.g. it
        // sat coalesced behind an earlier sync that already committed and
        // pushed it.
        fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [ ] one\n",
                "already committed",
                &no_race("- [ ] one\n"),
                None,
            ),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_retries_the_push_when_expected_content_matches_head_but_is_unpushed() {
        // External review: HEAD matching the expected content used to be
        // treated as proof the request was fully satisfied, even when the
        // matching commit had never actually reached upstream (e.g. a prior
        // push failed). That silently stranded the commit local-only
        // forever unless another checklist edit happened to come along.
        // `run_sync` must instead retry the push in that case.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

        let outcome = run_sync(
            &work,
            &file_path,
            "- [ ] one\n",
            "already committed",
            &no_race("- [ ] one\n"),
            None,
        );
        assert!(
            matches!(outcome, SyncOutcome::CommittedNotPushed { .. }),
            "must attempt (and report failure of) the push, not silently skip: {outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_when_branch_has_unrelated_unpushed_commits() {
        // External review: markcheck's own commit scope is carefully
        // restricted (temp index, hook-scope enforcement), but `git push`
        // sends the whole branch -- so an unrelated local commit made
        // outside markcheck would get published right alongside the
        // checklist change unless sync refuses outright first.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits that are not part of this change")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "no checklist commit should have been created on top"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n",
            "the on-disk edit itself is untouched, just not synced yet"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_the_fast_path_push_when_branch_has_unrelated_unpushed_commits() {
        // External review, round 7: the unrelated-unpushed-commits guard
        // used to run only right before building a *new* commit, on the
        // assumption that the blob_bytes_at-matches fast path is always
        // "markcheck's own prior commit, always legitimate to push." That's
        // false whenever an unrelated commit lands on top without touching
        // the checklist file at all: the checklist's blob at HEAD still
        // matches an older, already-satisfied request's expected_content,
        // so the fast path would push HEAD -- and the unrelated commit
        // riding along with it.
        //
        // Reaching the fast path (rather than the earlier `git status`
        // empty-check, or the new-commit path) needs `expected_content` to
        // match HEAD while the *working tree* currently holds something
        // else -- exactly the fast path's own doc comment scenario ("status
        // is non-empty because of someone else's still-uncommitted
        // change"): a request queued earlier (content "one", now already
        // satisfied at HEAD) executes after both a newer, not-yet-committed
        // edit lands on disk *and* an unrelated commit lands on the branch.
        let (work, remote) = init_repo_with_remote(); // origin/main = A, pushed.

        // A1: a checklist commit that's ahead of upstream (standing in for
        // one whose push failed -- the fast path doesn't care why it's
        // unpushed, only that it is).
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);

        // A2: an unrelated commit on top that never touches the checklist.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
        let head_before = current_head(&work).unwrap();

        // A newer edit sits on disk, uncommitted -- makes `git status`
        // non-empty, so the stale "one"-content request below reaches the
        // fast path instead of the earlier empty-status Skip.
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();

        let file_path = work.join("tracked.md");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            "Catch up a pending push",
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits that are not part of this change")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "nothing should have been committed or pushed"
        );

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            !subjects.contains("unrelated local commit"),
            "the unrelated commit must never reach the remote: {subjects}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_when_an_unrelated_commit_is_reverted_before_the_checklist_commit() {
        // External review, round 8, empirically confirmed: a plain net
        // tree diff between upstream and HEAD nets an unrelated commit and
        // its own revert out to zero, even though both commits are still
        // in the unpushed range and still get published (they're ancestors
        // of the checklist commit, and an explicit-SHA push sends a
        // commit's whole ancestry regardless of any single commit's net
        // effect on the tree). Reproduced directly: `git diff --name-only`
        // between the pre-unrelated-commit state and HEAD here shows only
        // tracked.md, despite `other.md` genuinely appearing and
        // disappearing in between.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

        run(&work, &["rm", "-q", "other.md"]);
        run(
            &work,
            &["commit", "-q", "-m", "revert unrelated local commit"],
        );
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits that are not part of this change")),
            "the reverted commit must still count as unrelated history: {outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "no checklist commit should have been created on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_refuses_when_a_merge_commit_is_in_the_unpushed_range() {
        // A merge commit's own diff-tree needs a combined-diff
        // interpretation that's genuinely ambiguous to check safely against
        // a single path (conflict resolutions, content changed only during
        // the merge) -- treated as unrelated unconditionally instead, even
        // when the merge itself is trivial and touches nothing but the
        // checklist.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

        run(&work, &["checkout", "-q", "-b", "side"]);
        fs::write(work.join("other.md"), "side\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "side change"]);
        run(&work, &["checkout", "-q", "main"]);
        run(
            &work,
            &["merge", "-q", "--no-ff", "side", "-m", "merge side"],
        );
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            "already committed",
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits that are not part of this change")),
            "a merge commit in range must refuse, even a clean one: {outcome:?}"
        );
        assert_eq!(current_head(&work).unwrap(), head_before);

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_does_not_refuse_for_markchecks_own_stacked_unpushed_commits() {
        // The refusal above must not fire just because markcheck's own
        // earlier commits (from toggling while offline) are what's ahead of
        // upstream -- only genuinely unrelated history should be refused.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let file_path = work.join("tracked.md");

        // First edit: commits locally (via run_sync's own temp-index path),
        // push fails against the unreachable remote -- exactly the
        // "markcheck's own commit is what's ahead" state this must allow.
        fs::write(&file_path, "- [x] one\n").unwrap();
        let first = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &commit_message(&file_path, "Check \"one\""),
            &no_race("- [x] one\n"),
            None,
        );
        assert!(matches!(first, SyncOutcome::CommittedNotPushed { .. }));

        // Second, different edit: must still be allowed to commit, not
        // refused just because the branch is now ahead of upstream by
        // markcheck's own first commit.
        fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
        let second = run_sync(
            &work,
            &file_path,
            "- [x] one\n- [x] two\n",
            &commit_message(&file_path, "Check \"two\""),
            &no_race("- [x] one\n- [x] two\n"),
            None,
        );
        assert!(
            matches!(second, SyncOutcome::CommittedNotPushed { .. }),
            "must still commit (and only then fail to push), not refuse: {second:?}"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n- [x] two\n"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_works_when_the_file_is_in_a_repo_subdirectory() {
        // `repo_dir` (the file's own parent) isn't the repo root here,
        // exercising the plumbing commands' repo-root-relative path
        // handling (`index_entry`/`blob_bytes_at`/`stage_blob`) rather than the
        // CWD-relative handling `status`/`push` rely on.
        let root = unique_dir("repo-nested");
        let remote = root.join("remote.git");
        let repo = root.join("repo");
        let sub = repo.join("checklists");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&sub).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "test"]);
        fs::write(sub.join("tracked.md"), "- [ ] one\n").unwrap();
        run(&repo, &["add", "checklists/tracked.md"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo, &["push", "-q", "-u", "origin", "main"]);

        let file_path = sub.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        assert_eq!(
            run_sync(
                &sub,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:checklists/tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

        fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn run_sync_handles_filenames_with_spaces_dashes_colons_and_unicode() {
        // External review: the git plumbing path (`index_entry`/
        // `blob_bytes_at`/`stage_into_temp_index`) was never exercised against
        // anything but a plain `tracked.md`. None of these need `--`
        // protection to matter in practice (every command that takes a
        // pathspec already uses it — see `run_sync`'s own doc comment) but
        // are exactly the filenames most likely to break naive text
        // parsing of `git` output, like the `ls-files --stage` line this
        // module parses in `index_entry`. Confirmed against the pre-fix,
        // newline-delimited version of `index_entry` locally: it actually
        // failed on the *unicode* case first (not embedded-tab/newline as
        // expected) — `git ls-files --stage` C-quotes/escapes non-ASCII
        // bytes by default without `-z`, so `relpath` came back as that
        // quoted-and-escaped literal string rather than the real path,
        // silently committing to the wrong index entry instead of erroring
        // (this sync test's mismatch was `HEAD:<real name>` still showing
        // the old content). The embedded-tab and embedded-newline cases
        // are still worth keeping per the review's own callout: `-z` is
        // what avoids the quoting *and* is what makes a raw newline in the
        // record safe to split on, even though neither case reaches the
        // ambiguous `split_once('\t')` call in a way that would actually
        // break it — see `index_entry`'s doc comment for why a tab in the
        // path is harmless there regardless.
        for file_name in [
            "with space.md",
            "-leading-dash.md",
            "colon:name.md",
            "unicode-\u{2705}\u{65e5}\u{672c}.md",
            "tab\tname.md",
            "newline\nname.md",
        ] {
            let (work, remote) = init_repo_with_remote_named(file_name);
            let file_path = work.join(file_name);
            fs::write(&file_path, "- [x] one\n").unwrap();
            let message = commit_message(&file_path, "Check \"one\"");

            assert_eq!(
                run_sync(
                    &work,
                    &file_path,
                    "- [x] one\n",
                    &message,
                    &no_race("- [x] one\n"),
                    None,
                ),
                SyncOutcome::Synced,
                "file name: {file_name:?}"
            );

            let show = Command::new("git")
                .current_dir(&remote)
                .args(["show", &format!("HEAD:{file_name}")])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&show.stdout),
                "- [x] one\n",
                "file name: {file_name:?}"
            );

            fs::remove_dir_all(work.parent().unwrap()).ok();
        }
    }
    #[test]
    fn commit_message_prefixes_the_file_name() {
        assert_eq!(
            commit_message(Path::new("/a/b/checklist.md"), "Check \"x\""),
            "checklist.md: Check \"x\""
        );
    }
    #[test]
    fn commit_message_truncates_a_long_description() {
        let long_item = "x".repeat(100);
        let message = commit_message(
            Path::new("/a/b/checklist.md"),
            &format!("Check \"{long_item}\""),
        );
        assert_eq!(message.chars().count(), 80);
        assert!(message.starts_with("checklist.md: Check \"xxx"));
        assert!(
            message.ends_with("\u{2026}\""),
            "closes the quote opened by the item text: {message:?}"
        );
        assert_eq!(message.matches('"').count(), 2);
    }
    #[test]
    fn commit_message_truncation_without_a_quote_gets_no_closing_quote() {
        let message = commit_message(
            Path::new("/a/b/checklist.md"),
            &format!(
                "Reset all tasks to not done and then some more {}",
                "x".repeat(50)
            ),
        );
        assert_eq!(message.chars().count(), 80);
        assert!(message.ends_with('\u{2026}'));
        assert!(!message.ends_with("\u{2026}\""));
    }
    #[test]
    fn commit_message_stays_within_the_cap_for_a_very_long_file_name() {
        // Deep review: the file-name prefix used to be kept whole
        // unconditionally, so a long enough name saturated the description
        // budget to 0 and produced `<prefix>…` -- longer than
        // MAX_COMMIT_MESSAGE_LEN, the one thing the constant guarantees.
        for name_len in [MAX_FILE_NAME_LEN - 1, MAX_FILE_NAME_LEN, 79, 100, 200] {
            let name = format!("{}.md", "x".repeat(name_len));
            let message = commit_message(
                Path::new(&format!("/a/b/{name}")),
                "Check \"a task with a reasonably long title\"",
            );
            assert!(
                message.chars().count() <= MAX_COMMIT_MESSAGE_LEN,
                "name_len {name_len}: {} chars: {message:?}",
                message.chars().count()
            );
            assert!(
                message.contains(": "),
                "the prefix separator survives: {message:?}"
            );
        }
    }
    #[test]
    fn commit_message_keeps_an_ordinary_file_name_intact() {
        // The clamp above must only ever engage for pathological names --
        // a normal checklist file name is untouched.
        let message = commit_message(Path::new("/a/b/deployment-runbook.md"), "Check \"x\"");
        assert!(message.starts_with("deployment-runbook.md: "), "{message}");
    }
    #[test]
    fn commit_message_leaves_a_short_description_untruncated() {
        let message = commit_message(Path::new("/a/b/checklist.md"), "Check \"short\"");
        assert_eq!(message, "checklist.md: Check \"short\"");
        assert!(!message.contains('\u{2026}'));
    }
    #[test]
    fn an_untracked_checklist_is_reported_even_when_git_status_stays_silent() {
        // Deep round 2. `git status --porcelain -- <file>` prints nothing for
        // an untracked file in two ordinary configurations, so the
        // empty-output short-circuit reported `Skipped` and git-sync sat
        // silent forever — the exact "looks like a bug" case
        // `SkippedUntracked` was introduced to surface. Both confirmed
        // against real git before this test was written.
        for (label, configure) in [
            (
                "status.showUntrackedFiles=no",
                &["config", "status.showUntrackedFiles", "no"][..],
            ),
            (
                "gitignored",
                &["config", "core.excludesFile", ".ignore"][..],
            ),
        ] {
            let (work, _remote) = init_repo_with_remote();
            run(&work, configure);
            if label == "gitignored" {
                fs::write(work.join(".ignore"), "untracked.md\n").unwrap();
            }
            let untracked = work.join("untracked.md");
            fs::write(&untracked, "- [ ] new\n").unwrap();

            // Precondition: status really is silent about it here.
            assert_eq!(
                git_stdout(&work, &["status", "--porcelain", "--", "untracked.md"]),
                "",
                "{label}: test setup expects git status to say nothing"
            );

            assert_eq!(
                run_sync(
                    &work,
                    &untracked,
                    "- [ ] new\n",
                    "should not commit",
                    &no_race("- [ ] new\n"),
                    None,
                ),
                SyncOutcome::SkippedUntracked,
                "{label}: an untracked checklist must still be reported"
            );

            fs::remove_dir_all(work.parent().unwrap()).ok();
        }
    }
    // --- Genuine check-then-race-then-use interleavings (round 3) ---

    #[test]
    fn an_unrelated_commit_landing_after_validation_is_not_published_by_the_fast_path() {
        // The ordering every previous review asked for and none of the tests
        // actually exercised: the guard clears the branch, *then* an
        // unrelated commit lands, *then* the push runs. Before the
        // captured-tip work this published the unrelated commit; the hook
        // makes that ordering deterministic instead of hoping for a race.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        // Commit the content but leave it unpushed, so the branch is ahead
        // of upstream and `HEAD` already holds exactly what is being synced.
        fs::write(&file, content).unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let validated = git_stdout(&work, &["rev-parse", "HEAD"]);
        // A further uncommitted edit keeps `git status` non-empty, which is
        // what carries the request past the nothing-to-do short-circuit and
        // into the already-committed fast path.
        fs::write(&file, "- [x] one\n- [ ] two\n").unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
            commit_unrelated(&racer, "other.txt");
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "main"]),
            validated,
            "the remote must hold exactly the validated tip, not the commit that raced in"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_commit_landing_before_ours_is_detected_rather_than_built_on_stale_state() {
        // HEAD moves between the guards and the commit itself. The temp
        // index was seeded from the old parent, so committing anyway would
        // produce a tree that silently reverts the commit that raced in.
        let (work, _remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        fs::write(&file, content).unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
            let sha = commit_unrelated(&racer, "other.txt");
            fs::write(racer.join(".racer-sha"), &sha).unwrap();
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        // The commit is aborted either way; what matters is that the commit
        // markcheck did *not* make is still on the branch afterwards.
        assert!(
            matches!(&outcome, SyncOutcome::Failed(_)),
            "the sync must not succeed on a superseded parent: {outcome:?}"
        );
        let unrelated = racer_sha_probe(&work);
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "unrelated\ninit",
            "the commit that raced in must still be on the branch"
        );
        assert_eq!(
            git_stdout(&work, &["rev-parse", "HEAD"]),
            unrelated,
            "HEAD must be the concurrent commit, not rewound past it"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_version_staged_during_the_sync_survives_index_alignment() {
        // Round 10's guard, finally tested as the race it defends against
        // rather than by pre-staging before the call.
        let (work, _remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        fs::write(&file, content).unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::BeforeIndexAlignment, move || {
            fs::write(racer.join("tracked.md"), "- [ ] staged mid-sync\n").unwrap();
            run(&racer, &["add", "tracked.md"]);
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        assert!(
            matches!(
                outcome,
                SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            staged_bytes(&work, "tracked.md").as_deref(),
            Some(&b"- [ ] staged mid-sync\n"[..]),
            "a version staged during the sync must not be overwritten by the realignment"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    // --- Two concurrent syncs in one repository (deep rounds 2, round 4) ---

    #[test]
    fn a_second_sync_committing_mid_flight_cannot_revert_the_first() {
        // The realistic multi-process shape: two markcheck instances open on
        // *different* checklists in the *same* repository. The write lock
        // serialises writes per file, and `GitSync` serialises syncs within
        // one process — neither does anything across processes, so two
        // `run_sync` calls really can interleave here.
        //
        // The danger is that both seed a temp index from the same tip: the
        // loser's tree then reverts the winner's file. The racer here is a
        // complete second `run_sync`, not a bare `git commit`, so the whole
        // pipeline participates.
        let (work, _remote) = init_repo_with_remote();
        let a = work.join("a.md");
        let b = work.join("b.md");
        fs::write(&a, "- [ ] a\n").unwrap();
        fs::write(&b, "- [ ] b\n").unwrap();
        run(&work, &["add", "a.md", "b.md"]);
        run(&work, &["commit", "-q", "-m", "add both checklists"]);
        run(&work, &["push", "-q", "origin", "main"]);

        // The other instance finishes its whole sync while ours is mid-commit.
        let racer_dir = work.clone();
        let racer_a = a.clone();
        let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
            fs::write(&racer_a, "- [x] a\n").unwrap();
            let outcome = run_sync(
                &racer_dir,
                &racer_a,
                "- [x] a\n",
                "a.md: Check \"a\"",
                &no_race("- [x] a\n"),
                None,
            );
            assert!(
                matches!(
                    outcome,
                    SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
                ),
                "the other instance's sync should land: {outcome:?}"
            );
        });

        fs::write(&b, "- [x] b\n").unwrap();
        let outcome = run_sync(
            &work,
            &b,
            "- [x] b\n",
            "b.md: Check \"b\"",
            &no_race("- [x] b\n"),
            None,
        );

        // Ours must not succeed on a tree seeded from the superseded tip.
        assert!(
            matches!(&outcome, SyncOutcome::Failed(_)),
            "the losing sync must refuse, not commit a reverting tree: {outcome:?}"
        );
        // And the winner's work must be intact, both in history and on disk.
        assert_eq!(
            git_stdout(&work, &["show", "HEAD:a.md"]),
            "- [x] a",
            "the other instance's commit must survive untouched"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "-1", "main"]),
            "a.md: Check \"a\"",
            "and must still be the branch tip"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn a_configured_but_unresolvable_upstream_does_not_bypass_the_unrelated_guard() {
        // Deep rounds 3, round 3. Two functions answered "is there an
        // upstream?" differently: `push` asks the *config*
        // (`branch.<n>.remote`/`.merge` via `upstream_parts`), while
        // `unpushed_history` resolves `@{u}`. Confirmed against real git that
        // those disagree when the config names an upstream whose
        // remote-tracking ref does not exist — set by hand, or pruned.
        //
        // The guard then answers `NoUpstream` and is skipped, while `push`
        // happily builds a refspec and publishes, creating the branch on the
        // remote. That reopens exactly the hazard the "refuse without an
        // upstream" work closed: unrelated local commits get published by a
        // checklist toggle.
        let root = unique_dir("upstream-disagreement");
        let remote = root.join("remote.git");
        let work = root.join("work");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&work).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);
        fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        run(&work, &["commit", "-q", "-m", "init"]);
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        // Config only — never `push -u`, so no remote-tracking ref exists.
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        assert!(
            upstream_parts(&work).is_some(),
            "test setup: the config claims an upstream"
        );
        assert_eq!(
            resolve_upstream(&work),
            Ok(None),
            "test setup: but @{{u}} does not resolve"
        );

        // Unrelated local work that must not be published.
        fs::write(work.join("secret.txt"), "unrelated\n").unwrap();
        run(&work, &["add", "secret.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated work"]);

        let file = work.join("tracked.md");
        let expected = "- [x] one\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &work,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert!(
            !matches!(outcome, SyncOutcome::Synced),
            "must not publish when the unrelated-commits guard could not run: {outcome:?}"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--oneline", "-1", "main"]),
            "",
            "the unrelated commit must not have reached the remote"
        );

        fs::remove_dir_all(&root).ok();
    }
    // --- Randomised operation sequences (deep rounds 4, round 1) ---

    /// A tiny deterministic PRNG. Seeded per case so a failure reproduces
    /// exactly, and written inline rather than pulling in a dependency for
    /// what is two lines of arithmetic.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    #[test]
    fn randomised_sequences_never_lose_a_commit_markcheck_did_not_make() {
        // Deep rounds 4, round 1. Every previous round reasoned about one
        // interleaving at a time. This drives *sequences* of them against
        // the strongest invariant the module has — markcheck never moves the
        // branch across a commit it did not create — which is the one that
        // produced the worst bug found so far (the rollback that rewound a
        // concurrent commit off the branch).
        //
        // Deterministic: every case is seeded, so a failure names the exact
        // sequence that produced it.
        for seed in 0..10u64 {
            let (work, remote) = init_repo_with_remote();
            let file = work.join("tracked.md");
            let broken = work.parent().unwrap().join("gone.git");
            let good = remote.clone();

            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut foreign: Vec<String> = Vec::new();
            let mut content = "- [ ] one\n".to_string();
            fs::write(&file, &content).unwrap();

            for step in 0..10 {
                match rng.below(5) {
                    // A markcheck toggle and the sync it queues.
                    0 => {
                        let previous = content.clone();
                        content = if content.contains("- [ ] one") {
                            content.replace("- [ ] one", "- [x] one")
                        } else {
                            content.replace("- [x] one", "- [ ] one")
                        };
                        fs::write(&file, &content).unwrap();
                        let _ = run_sync(
                            &work,
                            &file,
                            &content,
                            &commit_message(&file, "Toggle"),
                            &no_race(&content),
                            Some(previous.as_str()),
                        );
                    }
                    // Somebody edits the checklist outside markcheck.
                    1 => {
                        content = format!("{content}- [ ] edit{step}\n");
                        fs::write(&file, &content).unwrap();
                    }
                    // A commit markcheck did not make.
                    2 => {
                        let name = format!("foreign{step}.txt");
                        fs::write(work.join(&name), "not the checklist\n").unwrap();
                        run(&work, &["add", &name]);
                        run(&work, &["commit", "-q", "-m", &format!("foreign {step}")]);
                        foreign.push(git_stdout(&work, &["rev-parse", "HEAD"]));
                    }
                    // The user stages the checklist themselves.
                    3 => {
                        run(&work, &["add", "tracked.md"]);
                    }
                    // The remote comes and goes.
                    _ => {
                        let url = if rng.below(2) == 0 {
                            broken.to_str().unwrap()
                        } else {
                            good.to_str().unwrap()
                        };
                        run(&work, &["remote", "set-url", "origin", url]);
                    }
                }

                // The invariant, checked after every single step.
                for sha in &foreign {
                    assert!(
                        still_on_branch(&work, sha),
                        "seed {seed}, step {step}: commit {sha} was made outside markcheck \
                         and must still be on the branch"
                    );
                }
                // And the checklist must never be destroyed.
                assert!(
                    fs::read_to_string(&file).is_ok_and(|on_disk| !on_disk.is_empty()),
                    "seed {seed}, step {step}: the checklist must survive every sequence"
                );
                // The publication promise: a commit markcheck did not make
                // must never be pushed. The unrelated-history guard is what
                // enforces this, so any sequence that gets one onto the
                // remote is a hole in it.
                for sha in &foreign {
                    assert!(
                        !reached_the_remote(&remote, sha),
                        "seed {seed}, step {step}: commit {sha} is not the checklist's \
                         and must never have been published"
                    );
                }
            }

            fs::remove_dir_all(work.parent().unwrap()).ok();
        }
    }
    // --- Hostile repository shapes (deep rounds 2, round 2) ---

    #[test]
    fn run_sync_works_when_the_checklist_lives_in_a_subdirectory() {
        // `repo_dir` is the checklist's own parent, so for anything but a
        // root-level checklist it differs from `repo_root`. Several plumbing
        // commands run in one and take root-relative paths from the other.
        let (work, remote) = init_repo_with_remote();
        let sub = work.join("docs").join("runbooks");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("list.md");
        fs::write(&file, "- [ ] one\n").unwrap();
        run(&work, &["add", "docs/runbooks/list.md"]);
        run(&work, &["commit", "-q", "-m", "add nested checklist"]);
        run(&work, &["push", "-q", "origin", "main"]);

        let expected = "- [x] one\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &sub,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "HEAD:docs/runbooks/list.md"]),
            expected.trim_end(),
            "the nested path must be committed at its repo-root-relative location"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_works_inside_a_linked_worktree() {
        // A linked worktree's `.git` is a *file*, and `rev-parse --git-dir`
        // points at `.git/worktrees/<name>` — which is where its MERGE_HEAD
        // and the temp index have to live, not the main repo's git dir.
        let (work, remote) = init_repo_with_remote();
        let linked = work.parent().unwrap().join("linked");
        run(
            &work,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                linked.to_str().unwrap(),
                "main",
            ],
        );
        run(&linked, &["config", "user.email", "test@example.com"]);
        run(&linked, &["config", "user.name", "test"]);
        run(&linked, &["push", "-q", "-u", "origin", "wt"]);
        assert!(
            linked.join(".git").is_file(),
            "test setup: a linked worktree's .git is a file"
        );

        let file = linked.join("tracked.md");
        let expected = "- [x] one\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &linked,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "wt:tracked.md"]),
            expected.trim_end()
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn detect_and_sync_cope_with_a_shallow_clone() {
        // A shallow clone has grafted history: `rev-list` over a range that
        // reaches the graft boundary behaves differently, and the
        // unrelated-history guard walks exactly such a range.
        let (origin, remote) = init_repo_with_remote();
        for n in 1..=3 {
            fs::write(origin.join("tracked.md"), format!("- [ ] {n}\n")).unwrap();
            run(&origin, &["commit", "-q", "-am", &format!("edit {n}")]);
        }
        run(&origin, &["push", "-q", "origin", "main"]);

        let shallow = origin.parent().unwrap().join("shallow");
        run(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                remote.to_str().unwrap(),
                shallow.to_str().unwrap(),
            ],
        );
        run(&shallow, &["config", "user.email", "test@example.com"]);
        run(&shallow, &["config", "user.name", "test"]);

        let file = shallow.join("tracked.md");
        let expected = "- [x] shallow\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &shallow,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(
            outcome,
            SyncOutcome::Synced,
            "a shallow clone must still sync: {outcome:?}"
        );

        fs::remove_dir_all(origin.parent().unwrap()).ok();
    }
    // --- Staged-target guard (round 11) ---

    #[test]
    fn run_sync_refuses_when_a_staged_checklist_version_would_be_dropped() {
        // External review, round 11, reproduced end-to-end before this
        // guard: the commit is built from the working tree and the real
        // index is realigned to it afterwards, so a staged snapshot that
        // differs from the working tree is neither committed nor still
        // staged — it survives only as a dangling blob, reachable through no
        // ordinary git workflow.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");

        // The user stages a version carrying work that exists only in the index...
        fs::write(&file, "- [ ] one\n- [ ] IMPORTANT\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let staged = index_blob(&work, "tracked.md").unwrap();

        // ...then keeps editing, so the working tree no longer has it.
        let pre_write = "- [ ] one\n- [ ] later\n";
        fs::write(&file, pre_write).unwrap();

        // markcheck toggles a task, writing the post-toggle content to disk.
        let expected = "- [x] one\n- [ ] later\n";
        fs::write(&file, expected).unwrap();

        let outcome = run_sync(
            &work,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            staged_matches(pre_write),
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("staged changes")),
            "{outcome:?}"
        );
        assert_eq!(
            index_blob(&work, "tracked.md").as_deref(),
            Some(staged.as_str()),
            "the staged version must still be staged"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "init",
            "no commit may be built"
        );
        assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn run_sync_proceeds_when_the_staged_checklist_matches_the_working_tree() {
        // The deliberately permitted case: staging the checklist and *then*
        // toggling loses nothing, because everything staged is already
        // contained in the content about to be committed. Refusing here
        // would break an ordinary workflow for no safety gain — which is why
        // this guard is not "refuse whenever the target is staged".
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");

        let pre_write = "- [ ] one\n- [ ] extra\n";
        fs::write(&file, pre_write).unwrap();
        run(&work, &["add", "tracked.md"]);

        // An unrelated staged file must survive this untouched, as always.
        fs::write(work.join("other.txt"), "unrelated staged\n").unwrap();
        run(&work, &["add", "other.txt"]);
        let other_staged = index_blob(&work, "other.txt");

        let expected = "- [x] one\n- [ ] extra\n";
        fs::write(&file, expected).unwrap();

        let outcome = run_sync(
            &work,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            staged_matches(pre_write),
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "HEAD:tracked.md"]),
            expected.trim_end(),
            "the commit carries exactly the toggled content, staged work included"
        );
        assert_eq!(
            index_blob(&work, "other.txt"),
            other_staged,
            "an unrelated staged file is still untouched"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
