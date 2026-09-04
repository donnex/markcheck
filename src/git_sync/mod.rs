mod commit;
mod guards;
mod inspect;
mod process;
mod push;
mod sync;
#[cfg(test)]
mod test_support;

use inspect::blob_sha_for;
use process::{PLUMBING_TIMEOUT, git_command, run_with_timeout};
use push::{catch_up_push, retry_commit};
use sync::{commit_message, run_sync};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::model::{PendingSync, hash_bytes};

/// How long to wait between automatic push retries after a
/// `CommittedNotPushed` outcome, so a still-down network doesn't get
/// hammered with retries but connectivity returning is still noticed
/// without requiring another checklist edit.
const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// A named point inside a sync where a test can interleave a real
/// concurrent git process.
///
/// Every review of this module has made the same observation: its race tests
/// construct the repository state *before* invoking the function, which pins
/// the resulting invariant but never exercises the actual check → race → use
/// ordering. These points close that gap. In production every one of them
/// compiles to nothing (`race_point` has an empty `cfg(not(test))` body), so
/// this costs the shipped binary literally nothing.
///
/// Hooks are **thread-local** on purpose. Tests run in parallel in one
/// process, and they drive `run_sync`/`catch_up_push` directly on their own
/// thread, so a thread-local registry gives each test an isolated view; a
/// global one would have tests firing each other's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RacePoint {
    /// After the unpushed-history guard has cleared the captured tip, and
    /// before anything acts on that verdict.
    AfterHistoryValidation,
    /// Immediately before a commit is built from the temporary index.
    BeforeCommit,
    /// Immediately before the real index is realigned to the new commit.
    BeforeIndexAlignment,
    /// Immediately before `git push` runs.
    BeforePush,
}

#[cfg(test)]
thread_local! {
    static RACE_HOOKS: std::cell::RefCell<
        std::collections::HashMap<RacePoint, Box<dyn Fn()>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Runs whatever this thread has installed at `point`, if anything.
#[cfg(test)]
fn race_point(point: RacePoint) {
    // The hook is taken out of the map while it runs, so a hook that itself
    // reaches a sync point can't recurse into itself.
    let hook = RACE_HOOKS.with(|h| h.borrow_mut().remove(&point));
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
#[inline(always)]
fn race_point(_point: RacePoint) {}

/// Result of one background commit+push attempt, delivered to the
/// main loop via [`GitSync::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Committed and pushed.
    Synced,
    /// Nothing to do: the file is already tracked, identical to what's
    /// already committed, *and* that commit has already reached upstream
    /// (see `ahead_of_upstream`) — not just committed locally. Not reported
    /// to the user — nothing meaningful happened, so silence here doesn't
    /// read as broken.
    Skipped,
    /// The file isn't tracked by git at all. Unlike `Skipped`, this *is*
    /// reported to the user (git-sync being silently unable to do anything,
    /// forever, looks exactly like a bug) — but it still never gets
    /// `git add`ed automatically; that stays the user's call.
    SkippedUntracked,
    /// The commit succeeded but `git push` failed (offline, auth, no
    /// upstream, etc.) — distinct from `Failed` because the recovery action
    /// differs: nothing was lost, and the fix is to retry the *push*, never
    /// to create another commit. `GitSync` retries this automatically (see
    /// `retry_push_if_due`) rather than waiting for the next checklist edit,
    /// since a local commit that already matches the desired content would
    /// otherwise never trigger another sync attempt on its own. `commit` is
    /// the specific commit SHA that failed to push — `retry_push_if_due`
    /// retries *that commit*, not the file content that produced it, so a
    /// retry can never turn into a brand new commit built from stale
    /// content if something else has moved `HEAD` in the meantime (see
    /// `retry_commit`).
    CommittedNotPushed { message: String, commit: String },
    /// A push retry (`retry_commit`) gave up because the commit it was
    /// retrying is no longer `HEAD` — something else committed on top since
    /// the failed push, so replaying this specific retry would be pointless
    /// at best. Whatever superseded it gets its own sync opportunity through
    /// the normal request path, so nothing is lost.
    ///
    /// A dedicated variant rather than reusing `Skipped`: the two mean
    /// genuinely different things to the retry state (`Skipped` must leave a
    /// still-valid retry armed; this one must clear it), and disambiguating
    /// them used to need an out-of-band `last_spawn_was_retry` flag (long since
    /// removed) read at
    /// `poll` time. Carrying it in the outcome is what lets `apply_outcome`
    /// be a total `match` — which is the point, since the missing arm in the
    /// old one is exactly how the `Failed` retry storm survived.
    RetryAbandoned,
    /// `git status`/`commit` failed (nothing was committed); the message is
    /// the first line of the failing command's stderr.
    Failed(String),
    /// A **push retry** (`retry_commit`) failed outright — as opposed to
    /// `Failed`, which is a *content* sync failing. The two are separated
    /// because they say opposite things about the armed retry, and
    /// `apply_outcome` needs to treat them differently: a retry's own
    /// failure must restart its backoff (or a permanently failing retry
    /// fires on every frame), while a content sync's failure must leave the
    /// backoff alone (or repeated unrelated failures push the retry's turn
    /// out indefinitely and a pushable commit never goes out).
    ///
    /// Produced by mapping `retry_commit`'s `Failed` in `spawn_retry`, so
    /// there is one place the distinction is made rather than a flag read
    /// back at `poll` time — the same reasoning that made `RetryAbandoned`
    /// its own variant instead of an out-of-band `last_spawn_was_retry`.
    RetryFailed(String),
}

/// Drives commit+push for one file on a background thread, so a slow or
/// offline `git push` never blocks the UI. Mirrors `FileWatcher`:
/// a background worker feeds an `mpsc` channel that the main loop drains
/// non-blockingly once per frame via [`poll`](GitSync::poll).
pub struct GitSync {
    repo_dir: PathBuf,
    file_path: PathBuf,
    sender: mpsc::Sender<SyncOutcome>,
    receiver: mpsc::Receiver<SyncOutcome>,
    /// `true` while a background sync is running. A `request` that arrives
    /// while busy is coalesced into `pending` rather than spawning a second
    /// thread — two concurrent `git commit`/`push` runs on the same repo
    /// could race on the index/HEAD.
    busy: bool,
    /// The most recent request received while `busy`; only the latest
    /// matters, since a coalesced-over request's content already includes
    /// whatever the dropped one would have committed.
    pending: Option<PendingSync>,
    /// Set when the most recently completed attempt committed locally but
    /// failed to push; cleared on `Synced`. Carries the *commit SHA* to
    /// retry pushing plus when that attempt finished, so `retry_push_if_due`
    /// can back off between attempts instead of hammering a still-down
    /// remote. Deliberately not the file content that produced the commit
    /// (see `SyncOutcome::CommittedNotPushed`/`retry_commit`) — replaying
    /// content through the generic commit-or-skip machinery could build a
    /// *new* commit from stale content if `HEAD` had moved on in the
    /// meantime, silently reverting whatever superseded it.
    retry: Option<(String, Instant)>,
    /// The content hash of the *latest* `PendingSync` this `GitSync` has
    /// ever been asked to sync — updated on every `request()` call,
    /// including ones that coalesce into `pending` rather than spawning
    /// immediately. Shared with the background thread (`run_sync` reads it
    /// live, not a snapshot taken at spawn time) so a sync in flight can
    /// tell "the file changed because a newer request already supersedes
    /// me, which will correct things right after I finish" apart from "the
    /// file changed because of something outside git-sync entirely, which
    /// nothing will ever correct" — see `run_sync`'s staleness check.
    latest_requested_hash: Arc<Mutex<[u8; 32]>>,
}

/// Locks `latest_requested_hash`, tolerating a poisoned mutex rather than
/// propagating the panic. The guarded value is a plain `[u8; 32]` — a
/// content digest, overwritten wholesale on every write — so there is no
/// multi-field invariant a panicking holder could have left half-updated,
/// which is the only thing poisoning is there to warn about. Tolerating it
/// matters because one of the two lock sites is on the **main** thread
/// (`GitSync::request`): an `unwrap()` there turns a background worker's
/// panic into a TUI crash, several frames after the fact and with no
/// connection to what the user was doing.
fn lock_hash(hash: &Mutex<[u8; 32]>) -> std::sync::MutexGuard<'_, [u8; 32]> {
    hash.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs a sync operation, converting a panic into a reportable
/// `SyncOutcome::Failed` instead of letting the worker thread die silently.
///
/// `GitSync.busy` is cleared **only** when `poll` receives an outcome, and
/// the receiver never reports `Disconnected` (the struct holds its own
/// sender clone), so a worker that panics before sending leaves `busy` stuck
/// `true` forever: every later `request` coalesces into `pending` and never
/// spawns, `retry_push_if_due` returns immediately, and quitting burns the
/// full `wait_for_git_sync` budget — git-sync silently, permanently dead
/// with nothing on screen to say so. Guaranteeing an outcome is always sent
/// keeps that failure loud and recoverable.
///
/// Not reachable by any known path — `run_sync` and `retry_commit` return
/// `Result`-shaped failures throughout, and the one `unwrap` they relied on
/// (a poisoned `latest_requested_hash`) is handled by `lock_hash` above.
/// This is the backstop for the unknown ones, so it has no test of its own;
/// forcing a panic through it would mean adding a panic to production code
/// purely to observe it.
fn run_reporting_panics(op: impl FnOnce() -> SyncOutcome) -> SyncOutcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)).unwrap_or(SyncOutcome::Failed(
        "git-sync: internal error (sync worker panicked)".to_string(),
    ))
}

impl GitSync {
    /// Confirms `file_path`'s directory is inside a git work tree and, if
    /// so, returns a `GitSync` ready to accept requests. `None` when it
    /// isn't (or `git` itself can't be run) — git-sync is a convenience
    /// feature, so this fails open rather than erroring out.
    pub fn detect(file_path: &Path) -> Option<GitSync> {
        let repo_dir = file_path.parent()?.to_path_buf();
        let mut cmd = git_command(&repo_dir);
        cmd.args(["rev-parse", "--is-inside-work-tree"]);
        let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        Some(GitSync {
            repo_dir,
            file_path: file_path.to_path_buf(),
            sender,
            receiver,
            busy: false,
            pending: None,
            retry: None,
            latest_requested_hash: Arc::new(Mutex::new(hash_bytes(b""))),
        })
    }

    /// Requests a commit+push for `sync.content` (the exact file content
    /// expected once the underlying change lands), labeled with
    /// `sync.description` (e.g. `Check "Restart service"`); the full commit
    /// message is built from the file name plus this description. Coalesced
    /// with any already-running sync per the `pending` rule above.
    ///
    /// Always records `sync.content_hash` as the latest-known request
    /// first — unconditionally, before deciding whether to coalesce or
    /// spawn — so a sync already in flight for an *older* request can see
    /// that a newer one now exists, even though that newer one won't
    /// itself start running until the current one finishes.
    pub fn request(&mut self, sync: PendingSync) {
        *lock_hash(&self.latest_requested_hash) = sync.content_hash;
        if self.busy {
            self.pending = Some(sync);
            return;
        }
        self.spawn(sync);
    }

    fn spawn(&mut self, sync: PendingSync) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        let latest_requested_hash = Arc::clone(&self.latest_requested_hash);
        std::thread::spawn(move || {
            let outcome = run_reporting_panics(|| {
                let message = commit_message(&file_path, &sync.description);
                run_sync(
                    &repo_dir,
                    &file_path,
                    &sync.content,
                    &message,
                    &latest_requested_hash,
                    sync.previous_content.as_deref(),
                )
            });
            let _ = sender.send(outcome);
        });
    }

    /// Like `spawn`, but for retrying a push of an already-made commit
    /// (`retry_push_if_due`) rather than a fresh content-based sync request
    /// — see `retry_commit`.
    fn spawn_retry(&mut self, commit: String) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            // Everything this path can report as an outright failure is a
            // *retry's* failure, including a panic backstop — mapped once,
            // here, so `apply_outcome` never has to ask where it came from.
            let outcome = match run_reporting_panics(|| retry_commit(&repo_dir, &commit)) {
                SyncOutcome::Failed(msg) => SyncOutcome::RetryFailed(msg),
                other => other,
            };
            let _ = sender.send(outcome);
        });
    }

    /// Asks the background worker to push a commit an earlier session left
    /// local-only — see `catch_up_push`, which this is the only caller of.
    /// Called once by `main.rs` when git-sync activates, so a leftover
    /// unpushed commit gets a chance to go out without waiting for the user
    /// to make another checklist edit (a commit that already matches the
    /// desired content never triggers a `request` on its own).
    ///
    /// Deliberately **not** a `request`: this must never be able to create a
    /// commit, which is exactly what expressing it as one used to do.
    pub fn request_catch_up_push(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let outcome = run_reporting_panics(|| catch_up_push(&repo_dir, &file_path));
            let _ = sender.send(outcome);
        });
    }

    /// Drains the channel non-blockingly; call once per frame regardless of
    /// input, like `FileWatcher::poll_changed`. Returns the outcome of a
    /// completed sync, if one just finished, updates the push-retry state
    /// from it (`apply_outcome`), and kicks off a queued request that
    /// arrived while busy.
    pub fn poll(&mut self) -> Option<SyncOutcome> {
        let outcome = self.receiver.try_recv().ok();
        if let Some(outcome) = &outcome {
            self.busy = false;
            self.apply_outcome(outcome);
            if let Some(sync) = self.pending.take() {
                self.spawn(sync);
            }
        }
        outcome
    }

    /// Folds one completed outcome into the push-retry state — the single
    /// place `self.retry` is written, and deliberately a **total** `match`
    /// with no catch-all arm, so adding a `SyncOutcome` variant is a compile
    /// error rather than a silent no-op.
    ///
    /// That totality is the point. Deep review, round 2: `Failed` used to
    /// fall into a catch-all that did nothing, which left an armed retry
    /// holding its *original* timestamp. `retry_push_if_due`'s backoff check
    /// then stayed permanently elapsed, so a retry spawned on every ~100ms
    /// frame forever — measured at 98 `git` subprocesses in 10 seconds, with
    /// no way out, since nothing in that state can produce a different
    /// outcome. This is the same defect round 7 fixed for the abandoned-retry
    /// case; it survived here because that fix patched one arm instead of
    /// closing the hole. Every arm below now either clears the retry or
    /// restarts its backoff.
    fn apply_outcome(&mut self, outcome: &SyncOutcome) {
        match outcome {
            // Pushed successfully — nothing left to retry.
            SyncOutcome::Synced => self.retry = None,
            // The commit is local-only; arm (or re-arm) the retry, with the
            // backoff starting from now.
            SyncOutcome::CommittedNotPushed { commit, .. } => {
                self.retry = Some((commit.clone(), Instant::now()));
            }
            // The retried commit is no longer HEAD — that specific retry is
            // pointless now, and whatever superseded it syncs on its own.
            SyncOutcome::RetryAbandoned => self.retry = None,
            // The retry itself failed: still unpushed, so keep it armed —
            // but restart the backoff, or a persistently failing retry fires
            // every frame.
            SyncOutcome::RetryFailed(_) => {
                if let Some((_, last_attempt)) = &mut self.retry {
                    *last_attempt = Instant::now();
                }
            }
            // A *content* sync failed. That says nothing about an armed
            // retry for an earlier commit, so its backoff is left exactly as
            // it is — deep round 1: restarting it here let repeated
            // unrelated failures (an unrelated-commits refusal on every
            // toggle, say) postpone a perfectly pushable commit forever.
            SyncOutcome::Failed(_) => {}
            // Ordinary no-ops from a content sync. They say nothing about an
            // unrelated earlier commit's retry, so leave it exactly as it is
            // — clearing here would strand a commit that still needs pushing.
            SyncOutcome::Skipped | SyncOutcome::SkippedUntracked => {}
        }
    }

    /// Re-attempts a previously failed push, if one is due: call once per
    /// frame alongside `poll`, passing the current time (a parameter
    /// instead of reading `Instant::now()` internally, matching
    /// `AppState::expire_status`'s pattern, so tests can simulate the
    /// backoff elapsing without an actual multi-second sleep). A commit
    /// that already matches the desired content never triggers `request`
    /// again on its own (there's nothing new to commit), so without this a
    /// push failure — network down, auth expired — could otherwise leave a
    /// commit sitting local-only indefinitely if the user makes no further
    /// checklist edits after connectivity returns. No-ops while `busy` or
    /// before `PUSH_RETRY_INTERVAL` has elapsed since the last attempt.
    pub fn retry_push_if_due(&mut self, now: Instant) {
        if self.busy {
            return;
        }
        let Some((commit, last_attempt)) = &self.retry else {
            return;
        };
        if now.saturating_duration_since(*last_attempt) < PUSH_RETRY_INTERVAL {
            return;
        }
        let commit = commit.clone();
        self.spawn_retry(commit);
    }

    /// Whether a sync is currently running or queued behind one that is.
    /// Used only when quitting, to decide whether it's worth waiting at all.
    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::guards::verify_commit_scope;
    use super::inspect::current_head;
    use super::test_support::*;
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn detect_finds_a_repo() {
        let work = init_repo_without_remote();
        assert!(GitSync::detect(&work.join("tracked.md")).is_some());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_poisoned_latest_hash_does_not_take_down_the_main_thread() {
        // Deep review: `request` runs on the main (TUI) thread and takes the
        // same lock a background worker holds. An `unwrap()` there turns any
        // worker panic into a TUI crash, several frames later and with no
        // connection to what the user was doing. The guarded value is a
        // plain digest with no invariant to protect, so a poisoned lock is
        // recovered rather than propagated.
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let hash = Arc::clone(&sync.latest_requested_hash);
        let poisoner = std::thread::spawn(move || {
            let _guard = hash.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(poisoner.join().is_err(), "the poisoning thread must panic");
        assert!(sync.latest_requested_hash.is_poisoned());

        // Must not panic, and must still record the request.
        sync.request(pending_sync("- [x] one\n", "after poisoning"));
        assert_eq!(
            *lock_hash(&sync.latest_requested_hash),
            hash_bytes(b"- [x] one\n")
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn detect_none_outside_a_repo() {
        let dir = unique_dir("not-a-repo");
        fs::create_dir_all(&dir).unwrap();
        assert!(GitSync::detect(&dir.join("file.md")).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repeated_requests_against_an_unreachable_remote_never_deadlock_or_lose_edits() {
        // Simulates several toggles happening in quick succession while the
        // network is down. Pointing at a nonexistent path makes `git push`
        // fail fast and deterministically, in place of a real (slow,
        // OS-timeout-dependent) network hang.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        // Three edits fired in a row while `sync` is still busy with the
        // first. Each `request` call is itself synchronous and instant (it
        // only ever spawns a thread or sets `pending` — never runs `git`
        // inline), so none of this blocks regardless of how slow the
        // background attempt turns out to be.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        sync.request(pending_sync("- [x] one\n", "first"));
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        sync.request(pending_sync("- [x] one\n- [x] two\n", "second"));
        fs::write(
            work.join("tracked.md"),
            "- [x] one\n- [x] two\n- [x] three\n",
        )
        .unwrap();
        sync.request(pending_sync("- [x] one\n- [x] two\n- [x] three\n", "third"));
        assert!(sync.busy, "still mid-flight on the first attempt");

        // Only ever one attempt in flight: "second"/"third" coalesce into a
        // single `pending` slot rather than queuing three separate threads.
        // Poll until it settles back to idle (busy clears once a completed
        // attempt has no queued follow-up) or the deadline passes.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcomes = Vec::new();
        loop {
            if let Some(outcome) = sync.poll() {
                outcomes.push(outcome);
            }
            if !sync.busy && !outcomes.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // At most two attempts ever ran (never three — "second" was always
        // overwritten by "third" in `pending` before it got its own turn),
        // and it settles back to idle rather than getting stuck busy.
        assert!(
            outcomes.len() <= 2,
            "at most 2 sync attempts, never one per request: {outcomes:?}"
        );
        assert!(!sync.busy, "must settle back to idle, not stuck busy");
        assert!(sync.pending.is_none(), "no request left dangling");
        // The very first attempt already had a real change committed, so
        // pushing against the unreachable remote always fails — but the
        // commit itself always succeeds, hence `CommittedNotPushed` rather
        // than `Failed` (never Skipped either).
        assert!(
            matches!(
                outcomes.first(),
                Some(SyncOutcome::CommittedNotPushed { .. })
            ),
            "first attempt has a real, unpushed change: {outcomes:?}"
        );

        // Nothing was lost: every edit landed as a *local* commit regardless
        // of the push failing — `commit --only` always commits whatever is
        // on disk when it runs, so the accumulated edits end up in one or
        // two local commits (never zero), just not yet pushed.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(
            subjects.contains("tracked.md: first"),
            "first edit committed locally: {subjects}"
        );
        let final_contents = fs::read_to_string(work.join("tracked.md")).unwrap();
        assert!(
            final_contents.contains("three"),
            "the latest edit is on disk regardless of push failing: {final_contents}"
        );

        // The network "comes back": a plain `git push` from here (not via
        // `GitSync` — standing in for the *next* successful sync once
        // connectivity returns) must send every commit accumulated while it
        // was down, not just the latest, since `push` always sends the whole
        // unpushed range.
        let remote = init_repo_without_remote();
        run(
            &work,
            &["remote", "set-url", "origin", remote.to_str().unwrap()],
        );
        run(&work, &["push", "-q", "origin", "main:recovered"]);
        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "recovered"])
            .output()
            .unwrap();
        let remote_subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            remote_subjects.contains("tracked.md: first"),
            "catch-up push carries every commit made while offline: {remote_subjects}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
        fs::remove_dir_all(remote.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_when_nothing_is_pending() {
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.retry_push_if_due(Instant::now());
        assert!(!sync.busy, "nothing pending, nothing to retry");
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_before_the_backoff_interval_elapses() {
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((commit, last_attempt));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL - Duration::from_millis(1));

        assert!(!sync.busy, "backoff interval hasn't elapsed yet");
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_while_busy() {
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.busy = true;
        sync.retry = Some((commit, Instant::now() - PUSH_RETRY_INTERVAL));

        // A no-op here just means "doesn't spawn a second concurrent
        // attempt while one's already running" -- there's nothing else to
        // observe from outside, so this mainly guards against a panic or a
        // stray thread spawn while `busy` is set for an unrelated reason.
        sync.retry_push_if_due(Instant::now());

        assert!(sync.busy);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_retries_once_the_backoff_interval_elapses() {
        // External review: the whole point of automatic push retry is
        // closing the gap where a user makes no further checklist edits
        // after a push failure -- this drives that path directly, passing
        // a synthetic `now` (matching AppState::expire_status's pattern)
        // instead of a real 30-second sleep.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        // A prior sync attempt already committed locally but failed to push
        // -- simulate that directly (a plain commit, not via run_sync) to
        // keep this test focused on retry_push_if_due's own timing logic,
        // not commit creation.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let commit = current_head(&work).unwrap();

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((commit, last_attempt));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL);
        assert!(
            sync.busy,
            "the backoff interval elapsed, a retry should have spawned"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = None;
        while Instant::now() < deadline {
            if let Some(o) = sync.poll() {
                outcome = Some(o);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(outcome, Some(SyncOutcome::CommittedNotPushed { .. })),
            "still no remote to push to, so this retry fails the same way: {outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_clears_the_retry_when_head_has_moved_on() {
        // External review, round 7: retry_commit correctly abandons a
        // stale retry (Skipped) when HEAD has moved past the commit being
        // retried, but poll() used to leave `retry` armed regardless --
        // its Instant never refreshed, so the backoff check stays
        // permanently elapsed and a fresh (equally-abandoned) attempt
        // would otherwise fire on every subsequent call, not just every
        // PUSH_RETRY_INTERVAL. Confirms both halves: the retry state is
        // actually cleared, and a follow-up call is a genuine no-op.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let stale_commit = current_head(&work).unwrap();

        // Something else supersedes it before the retry fires.
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        run(&work, &["commit", "-q", "-am", "newer commit"]);

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((stale_commit, last_attempt));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL);
        assert!(
            sync.busy,
            "the backoff interval elapsed, a retry should have spawned"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = None;
        while Instant::now() < deadline {
            if let Some(o) = sync.poll() {
                outcome = Some(o);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(outcome, Some(SyncOutcome::RetryAbandoned), "{outcome:?}");
        assert!(
            sync.retry.is_none(),
            "the stale retry must be cleared, not left armed forever"
        );

        // And retry_push_if_due is now a genuine no-op -- no second spawn.
        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 2);
        assert!(!sync.busy, "nothing left to retry");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn apply_outcome_covers_every_variant_s_effect_on_the_retry_state() {
        // `apply_outcome` is the single writer of `self.retry` and is a
        // total match on purpose. This is the matrix that pins each arm --
        // `Failed` being the cell that was missing, and the reason a
        // permanently-failing retry used to fire on every frame.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let armed = |sync: &mut GitSync, at: Instant| sync.retry = Some((commit.clone(), at));

        // Synced: nothing left to push.
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::Synced);
        assert!(sync.retry.is_none(), "Synced clears the retry");

        // RetryAbandoned: that specific commit is superseded.
        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::RetryAbandoned);
        assert!(sync.retry.is_none(), "an abandoned retry clears it");

        // CommittedNotPushed: (re)arms, with the backoff restarted.
        let old = Instant::now() - PUSH_RETRY_INTERVAL * 4;
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::CommittedNotPushed {
            message: "nope".to_string(),
            commit: commit.clone(),
        });
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > old, "CommittedNotPushed restarts the backoff");

        // RetryFailed: the retry's *own* failure. Stays armed (the commit is
        // still unpushed) and restarts the backoff, so it can't fire again on
        // the very next frame.
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::RetryFailed("boom".to_string()));
        let (_, at) = sync
            .retry
            .clone()
            .expect("RetryFailed keeps the retry armed");
        assert!(
            at > old,
            "RetryFailed must restart the backoff, not leave it stale"
        );

        // Failed: a *content* sync's failure. It says nothing about an armed
        // retry for an earlier commit, so the backoff must be left alone --
        // restarting it here let repeated unrelated failures starve a
        // perfectly pushable commit.
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::Failed("boom".to_string()));
        let (_, at) = sync.retry.clone().expect("Failed keeps the retry armed");
        assert_eq!(at, old, "Failed must leave the retry's backoff alone");

        // Ordinary no-ops say nothing about an unrelated armed retry.
        for outcome in [SyncOutcome::Skipped, SyncOutcome::SkippedUntracked] {
            armed(&mut sync, old);
            sync.apply_outcome(&outcome);
            let (_, at) = sync.retry.clone().expect("left armed");
            assert_eq!(at, old, "{outcome:?} must leave an unrelated retry alone");
        }

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_unrelated_content_failure_does_not_starve_an_armed_retry() {
        // Deep round 1. `apply_outcome`'s `Failed` arm restarted the backoff
        // for *any* failure, but only a failure of the retry itself says
        // anything about the retry. A content sync that keeps failing for
        // its own reasons — an unrelated-commits refusal on every toggle,
        // say — pushed the armed retry's clock forward each time, so a
        // commit that was perfectly pushable never got its turn.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let due = Instant::now() - PUSH_RETRY_INTERVAL * 4;
        sync.retry = Some((commit.clone(), due));
        sync.apply_outcome(&SyncOutcome::Failed("unrelated content sync".to_string()));

        let (_, at) = sync.retry.clone().expect("still armed");
        assert_eq!(
            at, due,
            "a content sync's failure must leave the retry's backoff alone"
        );

        // The retry's *own* failure is the one that must restart it, or a
        // permanently failing retry fires on every frame.
        sync.retry = Some((commit, due));
        sync.apply_outcome(&SyncOutcome::RetryFailed("retry blew up".to_string()));
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > due, "a failed retry restarts its own backoff");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_failed_retry_does_not_fire_again_on_the_very_next_frame() {
        // Deep review, round 2, reproduced against the real binary: once the
        // repository becomes unresolvable, `retry_commit` returns `Failed`.
        // That arm used to leave the retry's timestamp untouched, so the
        // backoff check stayed permanently elapsed and a retry spawned on
        // every ~100ms event-loop frame -- 98 `git` subprocesses in 10
        // seconds, indefinitely, with no outcome able to break the loop.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now() - PUSH_RETRY_INTERVAL * 2;
        sync.retry = Some((commit, last_attempt));

        // A retry comes due and fails (the repo is gone).
        sync.retry_push_if_due(Instant::now());
        assert!(sync.busy, "a retry should have spawned");
        sync.busy = false;
        // `spawn_retry` maps a retry's failure to `RetryFailed`, which is
        // the arm that owns the backoff restart.
        sync.apply_outcome(&SyncOutcome::RetryFailed(
            "git rev-parse: not a git repository".to_string(),
        ));

        // The whole point: the next frame must be a no-op, not another spawn.
        sync.retry_push_if_due(Instant::now());
        assert!(
            !sync.busy,
            "a failed retry must respect the backoff instead of firing every frame"
        );
        assert!(
            sync.retry.is_some(),
            "the commit is still unpushed, so the retry stays armed for later"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn request_and_poll_roundtrip_reports_synced() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.request(pending_sync("- [x] one\n", "Check \"one\""));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut result = None;
        while Instant::now() < deadline {
            if let Some(outcome) = sync.poll() {
                result = Some(outcome);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(result, Some(SyncOutcome::Synced));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_request_while_busy_is_coalesced_and_runs_after() {
        // The change is written *before* either request, so there's no race
        // between the background thread's `git status` check and a
        // concurrent write on this thread: "first" deterministically has
        // something to commit, and "second" (queued behind it) deterministically
        // finds nothing left to do once "first" has already committed it.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        sync.request(pending_sync("- [x] one\n", "first"));
        assert!(sync.busy, "first request should mark the worker busy");
        // A second `request` while busy doesn't spawn a second concurrent
        // thread (two `git commit`/`push` runs on the same repo could race
        // on the index/HEAD) — it queues instead.
        sync.request(pending_sync("- [x] one\n", "second"));
        assert_eq!(
            sync.pending.as_ref().map(|p| p.description.as_str()),
            Some("second")
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcomes = Vec::new();
        while Instant::now() < deadline && outcomes.len() < 2 {
            if let Some(outcome) = sync.poll() {
                outcomes.push(outcome);
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert_eq!(outcomes, vec![SyncOutcome::Synced, SyncOutcome::Skipped]);

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "tracked.md: first"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_unverifiable_commit_scope_says_the_commit_was_left_behind() {
        // Deep rounds 2, round 3. When the scope check itself fails, the
        // commit has already been made and stays on the branch. The old bare
        // `?` reported only the raw git error, so the user was told the sync
        // failed with no hint that a commit now exists — unlike the
        // undo-failed path, which names it.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let bogus = "0".repeat(40);

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &bogus);

        let err = result.expect_err("an unresolvable commit cannot be verified");
        assert!(
            err.contains(&bogus) && err.contains("left on the branch"),
            "the message must name the commit and say it survived: {err}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
