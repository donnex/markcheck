//! Publishing a commit, and the startup catch-up.
//!
//! Every push in this module goes through `push`, which targets an explicit
//! `<sha>:<branch-ref>` refspec rather than running a bare `git push`. That
//! is deliberate: a bare push sends whatever the branch tip has become by
//! the time it runs, so a commit landing in the gap between validation and
//! the push would be published unnoticed.
//!
//! Publishing is also the one place that talks to the network, so it is
//! where the local view of the world stops being trustworthy — hence the
//! remote-state check before publishing, and the `CommittedNotPushed`
//! outcome that keeps work safely local when anything is uncertain.

use std::path::{Path, PathBuf};

use super::guards::{repo_sync_blocked, unpushed_history_blocks};
use super::inspect::{
    RemoteBranch, commits_ahead_of_upstream, current_head, index_entry, remote_branch_state,
    resolve_upstream, upstream_parts,
};
use super::process::{
    PLUMBING_TIMEOUT, PUSH_TIMEOUT, command_error, git_command, run_with_timeout,
};
use super::{RacePoint, SyncOutcome, race_point};

/// Pushes `expected_commit` — explicitly, as `<remote>
/// <expected_commit>:<upstream-branch>` rather than a bare `git push` —
/// translating the result into a `SyncOutcome`. A failure here is always
/// `CommittedNotPushed`, never `Failed`, with one carve-out below: by the
/// time this is called, either a commit was just made or `HEAD` was already
/// confirmed to hold the desired content — either way, a local commit
/// exists and the only thing to retry is the push itself.
///
/// The carve-out is an **empty** `expected_commit`, which is rejected
/// outright as `Failed` (the one case where no local commit can be claimed,
/// since there's no SHA naming one). This is defence in depth, not a
/// reachable path: every caller resolves a real SHA first. It exists because
/// the consequence of getting here with `""` is severe and silent — the
/// refspec below would become `<remote> :<branch>`, git's *delete a remote
/// ref* form, which succeeds and reports `Synced` while deleting the branch.
/// A caller that regressed into passing an unresolved SHA (as `run_sync`'s
/// fast path once did, via `unwrap_or_default()`) must fail loudly rather
/// than delete anything.
///
/// External review, round 5: both of this function's callers
/// (`push_if_head_unchanged`, `retry_commit`) verify `HEAD ==
/// expected_commit` immediately beforehand, but that check and the actual
/// push used to be two separate steps regardless — a bare `git push`
/// sends whatever the local branch tip has become by the time it runs, so
/// a commit landing in the gap between the check and the push would still
/// ride along. Targeting `expected_commit` explicitly closes this rather
/// than merely re-narrowing it: the push can never publish more than that
/// commit's own ancestry, no matter what the local branch has become by
/// push-time.
///
/// When `upstream_parts` can't resolve a remote/branch to target — no
/// upstream tracking configured for this branch — the push is **refused**
/// rather than falling back to a bare `git push`. Deep review, reproduced
/// empirically: that fallback was justified on the premise that a bare push
/// "already reports a clear, git-native error" in this situation, which is
/// false for an ordinary configuration. With a remote added but `-u` never
/// used, and `push.default = current`, a bare push resolves a destination
/// and **succeeds**, publishing the whole branch tip — every unpushed
/// commit, not just `expected_commit`. That is exactly what targeting the
/// commit explicitly exists to prevent, and it composes badly with
/// the unrelated-history guard, which independently fails
/// *closed* in the same configuration (its `@{u}..HEAD` range can't be
/// resolved either, so it declines to refuse): both guards are off at once,
/// and an unrelated local commit gets published by a checklist toggle.
/// Refusing keeps the commit safely local, reuses the existing retry
/// machinery, and gives the user a one-off action to take.
pub(super) fn push(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    race_point(RacePoint::BeforePush);
    if expected_commit.is_empty() {
        return SyncOutcome::Failed("could not work out which commit to push".to_string());
    }
    // `push` and the unpushed-history guard must agree about what "has an
    // upstream" means, and they used to answer it from different sources:
    // this asked the *config* (`upstream_parts`), while `unpushed_history`
    // resolves `@{u}`. Deep rounds 3, round 3, confirmed against real git —
    // those disagree whenever the config names an upstream whose
    // remote-tracking ref does not exist (set by hand, or pruned). The guard
    // then reports `NoUpstream` and stands down, while this built a refspec
    // and published anyway, *creating* the branch on the remote — reopening
    // the very hazard the refuse-without-an-upstream rule closed, since
    // unrelated local commits went out with it.
    //
    // The resolvable ref is the authority for both. A branch whose upstream
    // has never been created still commits locally; only publishing waits
    // for the one-off `git push -u`.
    let no_upstream = SyncOutcome::CommittedNotPushed {
        message: "this branch has no upstream yet; \
                  run `git push -u` once"
            .to_string(),
        commit: expected_commit.to_string(),
    };
    if !matches!(resolve_upstream(repo_dir), Ok(Some(_))) {
        return no_upstream;
    }
    let Some((remote, branch_ref)) = upstream_parts(repo_dir) else {
        return no_upstream;
    };
    // Ask the remote, not the tracking ref — see `remote_branch_state`. An
    // explicit-SHA refspec *creates* a branch that is no longer there, so
    // without this a toggle silently republishes history somebody deleted.
    match remote_branch_state(repo_dir, &remote, &branch_ref) {
        RemoteBranch::Present => {}
        RemoteBranch::Absent => {
            return SyncOutcome::CommittedNotPushed {
                message: format!(
                    "{branch_ref} no longer exists on {remote}; \
                     pushing would recreate it"
                ),
                commit: expected_commit.to_string(),
            };
        }
        // Refusing rather than pushing blind: the hazard above needs only a
        // stale local view to fire, and this is exactly the state where the
        // local view cannot be trusted. Little is given up in practice —
        // `ls-remote` and `push` share a transport and credentials, so a
        // remote that cannot be consulted is one the push would not have
        // reached either, and the retry machinery re-checks on its own.
        RemoteBranch::Unknown => {
            return SyncOutcome::CommittedNotPushed {
                message: format!(
                    "could not reach {remote} to check whether {branch_ref} \
                     still exists"
                ),
                commit: expected_commit.to_string(),
            };
        }
    }
    let mut cmd = git_command(repo_dir);
    cmd.args(["push", &remote, &format!("{expected_commit}:{branch_ref}")]);
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
/// Pushes `expected_commit` (the commit `run_sync` just made and verified),
/// but only if `HEAD` still equals it — external review, round 4:
/// the unrelated-history guard (`unpushed_history`) only ever ran as a snapshot
/// *before* the commit, so a commit landing on the branch after
/// verification but before this point (another markcheck instance, a
/// human, an IDE) would otherwise get pushed right alongside markcheck's
/// own, unnoticed. Unlike `retry_commit`'s silent `Skipped` on the same
/// mismatch (correct there — it's abandoning a *retry* of an
/// already-reported failure), a first attempt at a freshly-made commit
/// must surface something rather than going quiet, so a mismatch here is
/// reported as `CommittedNotPushed` instead: nothing is lost (the commit
/// is safely local either way) and this reuses the existing automatic
/// retry machinery (`GitSync::poll`/`retry_push_if_due` → `retry_commit`,
/// which performs the same check again once due).
pub(super) fn push_if_head_unchanged(
    repo_dir: &Path,
    repo_root: &Path,
    expected_commit: &str,
) -> SyncOutcome {
    if current_head(repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::CommittedNotPushed {
            message: "the repository changed after the commit was made".to_string(),
            commit: expected_commit.to_string(),
        };
    }
    push(repo_dir, expected_commit)
}
/// Re-attempts pushing a specific commit that previously failed to push
/// (`GitSync::retry_push_if_due`), rather than replaying the file content
/// that produced it through the generic commit-or-skip logic in `run_sync`.
/// That distinction is the whole point: if `expected_commit` is no longer
/// `HEAD` (something else — most plausibly another concurrent markcheck/
/// git-sync process — committed on top since the failed push), silently
/// abandoning the retry is strictly safer than `run_sync` would be here,
/// since `run_sync` would build a *new* commit from the stale content,
/// reverting whatever superseded it. Whatever superseded `expected_commit`
/// gets its own sync opportunity through the normal request path, so
/// nothing is lost by giving up on this specific stale retry.
pub(super) fn retry_commit(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };
    if current_head(&repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::RetryAbandoned;
    }
    push(repo_dir, expected_commit)
}
/// Pushes a commit an earlier session left local-only, and **never builds a
/// commit of its own** — the startup counterpart to `retry_commit`'s timed
/// retry, run once by `main.rs` when git-sync activates.
///
/// Deep review, round 3, reproduced end-to-end. Startup used to express this
/// as an ordinary `GitSync::request` carrying the file's *current disk
/// content* as `expected_content`, described as `Catch up a pending push`.
/// That defeats every guard `run_sync` has, by construction: `status` is
/// non-empty, `HEAD`'s blob differs, and the staleness check compares disk
/// against `latest_requested_hash` — which that same request had just set to
/// that same content. So a checklist with ordinary uncommitted edits (made
/// in an editor, no markcheck involvement) was committed *and pushed* purely
/// by opening the file and quitting, under a message describing a push
/// catch-up rather than the change it actually published. Publishing is not
/// undoable the way a local commit is, and nothing about opening a viewer
/// should publish anything.
///
/// So this path only ever pushes. It also fails **closed** throughout —
/// unlike `run_sync`, which fails open in several places because it runs
/// after a change the user just made, where silence would hide a problem.
/// Nothing has happened here, so every unanswerable question means "do
/// nothing, quietly": no upstream, no commits, an unreadable count, all
/// return `Skipped`. The one thing worth reporting is an untracked file,
/// which `run_sync` would have reported at startup before this change and
/// which means git-sync can never do anything at all for this file.
pub(super) fn catch_up_push(repo_dir: &Path, file_path: &Path) -> SyncOutcome {
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
    // Capture the tip **before** validating anything, and push that exact
    // SHA. External review, round 10: this used to validate the range
    // ending at the symbolic `HEAD` and then separately re-resolve `HEAD`
    // for the push, so an unrelated commit landing between those two
    // subprocesses was published without ever having been checked — the
    // same defect as `run_sync`'s fast path, in the one path that runs
    // unprompted at startup. Naming the SHA closes it: `push` targets an
    // explicit commit, so what it can publish is exactly what was validated.
    let Some(head) = current_head(&repo_root) else {
        return SyncOutcome::Skipped;
    };
    // Positively-known unpushed commits only — see `commits_ahead_of_upstream`.
    if commits_ahead_of_upstream(&repo_root, &head).is_none_or(|ahead| ahead == 0) {
        return SyncOutcome::Skipped;
    }
    let relpath = match index_entry(repo_dir, file_path) {
        Ok(Some((_mode, relpath))) => relpath,
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => return SyncOutcome::Failed(err),
    };
    // The same refusal `run_sync` makes: an explicit-SHA push still sends the
    // commit's whole ancestry, so unrelated unpushed work must not ride along.
    if let Some(refusal) = unpushed_history_blocks(&repo_root, Some(&head), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);
    push(repo_dir, &head)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn push_if_head_unchanged_refuses_to_push_a_commit_that_landed_after_verification() {
        // External review, round 4: the unrelated-history guard
        // only ever runs as a snapshot before markcheck's own commit — a
        // commit landing on the branch after that commit was made and
        // verified, but before the push actually runs, would otherwise get
        // pushed right alongside it, unnoticed.
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, commit }
                if message.contains("changed after the commit was made") && commit == &created_commit),
            "{outcome:?}"
        );

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote_log.stdout).trim(),
            "init",
            "neither the markcheck commit nor the concurrent one must reach the remote"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn push_if_head_unchanged_pushes_normally_when_nothing_raced_it() {
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&remote_log.stdout).contains("markcheck commit"),
            "{}",
            String::from_utf8_lossy(&remote_log.stdout)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn push_refuses_an_empty_commit_instead_of_deleting_the_remote_branch() {
        // Deep review, critical finding. `<remote> :<branch>` is git's
        // *delete a remote ref* refspec, so an empty `expected_commit`
        // formatted into `{expected_commit}:{branch_ref}` doesn't fail — it
        // deletes the branch and exits 0, which `push` would then report as
        // `Synced`. Reproduced directly against git before the fix. The
        // reachable route was `run_sync`'s fast path, which used to write
        // `current_head(...).unwrap_or_default()`; this test guards `push`
        // itself, so no future caller can reintroduce it from a different
        // direction.
        let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();

        let outcome = push(&work, "");

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("which commit to push")),
            "{outcome:?}"
        );
        let branches = Command::new("git")
            .current_dir(&remote)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&branches.stdout);
        assert!(
            branches.lines().any(|b| b == branch),
            "the remote branch must still exist: {branches:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn push_never_sends_more_than_the_explicitly_targeted_commit() {
        // External review, round 5: push_if_head_unchanged/retry_commit
        // both verify HEAD == expected_commit immediately before calling
        // push, but that check and the push itself used to be two separate
        // steps regardless -- a commit landing in between would still ride
        // along, since a bare `git push` sends whatever the local branch
        // tip has become by push-time. Proving the fix directly (push an
        // older commit explicitly, with a newer one already sitting on top
        // locally) is far more deterministic than trying to time a race
        // into that now-vanishingly-small window -- the same reasoning
        // round 4's own verify_commit_scope/undo_commit tests already use.
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "older commit"]);
        let older_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "newer\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "newer commit"]);

        let outcome = push(&work, &older_commit);
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            !subjects.contains("newer commit"),
            "must never publish anything beyond the targeted commit: {subjects}"
        );
        assert!(subjects.contains("older commit"), "{subjects}");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn retry_commit_abandons_silently_when_head_has_moved_on() {
        // The actual regression test for the stale-retry bug: a retry must
        // never rebuild a commit from old content when something else has
        // moved HEAD past the commit it was trying to push -- it must give
        // up on this specific retry instead, leaving whatever superseded it
        // alone.
        let work = init_repo_without_remote();
        let file_path = work.join("tracked.md");
        let stale_commit = current_head(&work).unwrap();

        // Something else advances HEAD past the commit this retry is stale
        // for -- standing in for a concurrent markcheck/git-sync process,
        // or any other actor, committing in between.
        fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
        run(&work, &["commit", "-q", "-am", "newer content"]);
        let newer_commit = current_head(&work).unwrap();
        assert_ne!(stale_commit, newer_commit);

        let outcome = retry_commit(&work, &stale_commit);

        assert_eq!(outcome, SyncOutcome::RetryAbandoned);
        assert_eq!(
            current_head(&work).unwrap(),
            newer_commit,
            "the newer commit must be left completely alone"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n- [x] two\n",
            "no stale commit was rebuilt on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn retry_commit_pushes_when_head_still_matches() {
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let commit = current_head(&work).unwrap();

        let outcome = retry_commit(&work, &commit);

        assert_eq!(outcome, SyncOutcome::Synced);
        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn an_unrelated_commit_landing_after_validation_is_not_published_by_catch_up() {
        // Same interleaving through the startup path, which runs unprompted
        // and so is the one where publishing something unchecked is worst.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let validated = git_stdout(&work, &["rev-parse", "HEAD"]);

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
            commit_unrelated(&racer, "other.txt");
        });

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "main"]),
            validated,
            "catch-up must publish only the tip it validated"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    // --- Startup catch-up push (`catch_up_push`) ---

    #[test]
    fn catch_up_push_never_commits_uncommitted_working_tree_changes() {
        // Deep review, round 3, reproduced end-to-end against a real repo
        // and bare remote before this fix. Startup expressed the catch-up
        // as an ordinary content request carrying the file's *current disk
        // bytes*, which satisfies every `run_sync` guard by construction --
        // so merely opening a checklist that had ordinary uncommitted
        // editor changes committed *and pushed* them, under the message
        // `Catch up a pending push`. Opening a viewer must publish nothing.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let edited = "- [x] edited outside markcheck\n";
        fs::write(&file, edited).unwrap();

        let outcome = catch_up_push(&work, &file);

        assert_eq!(
            outcome,
            SyncOutcome::Skipped,
            "nothing was ahead of upstream, so there was nothing to do"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "init",
            "no commit may be created"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "init",
            "nothing may reach the remote"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            edited,
            "the uncommitted edit stays exactly as the user left it"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_pushes_a_local_only_commit_without_committing_a_dirty_tree() {
        // The case the startup hook exists for -- a prior session committed
        // but quit before its push landed -- combined with the case above,
        // to pin both halves at once: the existing commit goes out, and the
        // unrelated uncommitted edit sitting next to it does not.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        fs::write(&file, "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "tracked.md: Check \"one\""]);
        // ... and then the user keeps editing, without committing.
        let dirty = "- [x] one\n- [ ] two\n";
        fs::write(&file, dirty).unwrap();

        let outcome = catch_up_push(&work, &file);

        assert_eq!(outcome, SyncOutcome::Synced);
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "tracked.md: Check \"one\"\ninit",
            "the local-only commit reached the remote"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "tracked.md: Check \"one\"\ninit",
            "and no second commit was created for the dirty tree"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), dirty);

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_is_a_no_op_when_everything_is_already_pushed() {
        let (work, remote) = init_repo_with_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );
        assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_stays_silent_when_no_upstream_is_configured() {
        // Fails closed, unlike `ahead_of_upstream`: nothing has happened
        // here, so an unanswerable question means "do nothing, quietly"
        // rather than nagging about the missing upstream on every launch.
        let work = init_repo_without_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_refuses_when_unrelated_commits_are_unpushed() {
        // An explicit-SHA push still sends the commit's whole ancestry, so
        // the same refusal `run_sync` makes has to apply here too.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg)
                if msg.contains("not part of this change")),
            "{outcome:?}"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "init",
            "the unrelated commit must not be published"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_reports_an_untracked_checklist() {
        // Preserves what the old startup request reported: git-sync being
        // permanently unable to do anything for this file is worth saying.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "ahead"]);
        let untracked = work.join("not-tracked.md");
        fs::write(&untracked, "- [ ] one\n").unwrap();

        assert_eq!(
            catch_up_push(&work, &untracked),
            SyncOutcome::SkippedUntracked
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn catch_up_push_refuses_while_the_repository_is_mid_merge() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "ahead"]);
        let git_dir = work.join(".git");
        fs::write(git_dir.join("MERGE_HEAD"), "deadbeef\n").unwrap();

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("a merge is in progress")),
            "{outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
