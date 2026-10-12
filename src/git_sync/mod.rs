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

/// Backoff between automatic push retries after `CommittedNotPushed`.
const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// A named point inside a sync where a test can interleave a real
/// concurrent git process. Compiles to nothing outside tests. Hooks are
/// thread-local so parallel tests can't fire each other's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RacePoint {
    AfterHistoryValidation,
    BeforeCommit,
    BeforeIndexAlignment,
    BeforePush,
}

#[cfg(test)]
thread_local! {
    static RACE_HOOKS: std::cell::RefCell<
        std::collections::HashMap<RacePoint, Box<dyn Fn()>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
fn race_point(point: RacePoint) {
    // Taken out while it runs, so a hook reaching a sync point can't recurse.
    let hook = RACE_HOOKS.with(|h| h.borrow_mut().remove(&point));
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
#[inline(always)]
fn race_point(_point: RacePoint) {}

/// Result of one background commit+push attempt, delivered via
/// [`GitSync::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Synced,
    /// Nothing to do: already committed and pushed. Not reported.
    Skipped,
    /// The file isn't tracked. Reported, but never `git add`ed automatically.
    SkippedUntracked,
    /// Committed locally, but the push failed. `commit` is retried, never the
    /// content that produced it, so a retry can't build a new commit from stale
    /// content.
    CommittedNotPushed {
        message: String,
        commit: String,
    },
    /// A push retry gave up because its commit is no longer `HEAD`. Unlike
    /// `Skipped`, it clears the armed retry.
    RetryAbandoned,
    /// Nothing was committed; the message is the first line of git's stderr.
    Failed(String),
    /// A push retry failed outright. Separate from `Failed` because it restarts
    /// the retry's backoff, while a content sync's failure must leave it alone.
    RetryFailed(String),
}

/// Drives commit+push for one file on a background thread, so a slow
/// `git push` never blocks the UI.
pub struct GitSync {
    repo_dir: PathBuf,
    file_path: PathBuf,
    sender: mpsc::Sender<SyncOutcome>,
    receiver: mpsc::Receiver<SyncOutcome>,
    /// A request arriving while busy is coalesced into `pending`: two
    /// concurrent commits on one repository could race on the index.
    busy: bool,
    /// Only the latest coalesced request matters.
    pending: Option<PendingSync>,
    /// The commit SHA to retry pushing and when the last attempt finished.
    retry: Option<(String, Instant)>,
    /// Hash of the latest request, read live by the worker so `run_sync` can
    /// tell a superseded request from an outside edit.
    latest_requested_hash: Arc<Mutex<[u8; 32]>>,
}

/// Tolerates poisoning: the digest has no invariant to break, and a panic
/// here would crash the TUI from the main thread.
fn lock_hash(hash: &Mutex<[u8; 32]>) -> std::sync::MutexGuard<'_, [u8; 32]> {
    hash.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Turns a panic into `Failed`: a worker that dies without sending leaves
/// `busy` stuck and git-sync silently dead. A backstop with no known path,
/// so untested.
fn run_reporting_panics(op: impl FnOnce() -> SyncOutcome) -> SyncOutcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)).unwrap_or(SyncOutcome::Failed(
        "something went wrong inside markcheck; your change is saved but was not synced"
            .to_string(),
    ))
}

impl GitSync {
    /// `None` outside a git work tree; git-sync is optional, so this fails open.
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

    /// Records the request's hash before coalescing, so a sync already in
    /// flight sees that a newer request exists.
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

    fn spawn_retry(&mut self, commit: String) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            // Every failure here is the retry's own.
            let outcome = match run_reporting_panics(|| retry_commit(&repo_dir, &commit)) {
                SyncOutcome::Failed(msg) => SyncOutcome::RetryFailed(msg),
                other => other,
            };
            let _ = sender.send(outcome);
        });
    }

    /// Pushes a commit an earlier session left local-only. Never a `request`,
    /// because this must not be able to create a commit.
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

    /// Drains the channel non-blockingly; call once per frame.
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

    /// The single writer of `self.retry`, as a total `match` so a new
    /// `SyncOutcome` variant can't silently leave a retry firing every frame.
    fn apply_outcome(&mut self, outcome: &SyncOutcome) {
        match outcome {
            SyncOutcome::Synced => self.retry = None,
            SyncOutcome::CommittedNotPushed { commit, .. } => {
                self.retry = Some((commit.clone(), Instant::now()));
            }
            SyncOutcome::RetryAbandoned => self.retry = None,
            // Keep it armed but restart the backoff.
            SyncOutcome::RetryFailed(_) => {
                if let Some((_, last_attempt)) = &mut self.retry {
                    *last_attempt = Instant::now();
                }
            }
            // A content sync's failure says nothing about an earlier commit's retry.
            SyncOutcome::Failed(_) => {}
            SyncOutcome::Skipped | SyncOutcome::SkippedUntracked => {}
        }
    }

    /// Re-attempts a failed push once `PUSH_RETRY_INTERVAL` has passed, since
    /// content that's already committed never triggers another request. `now`
    /// is a parameter so tests can skip the wait.
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

    /// Used only when quitting.
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
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let hash = Arc::clone(&sync.latest_requested_hash);
        let poisoner = std::thread::spawn(move || {
            let _guard = hash.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(poisoner.join().is_err(), "the poisoning thread must panic");
        assert!(sync.latest_requested_hash.is_poisoned());

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
        // A nonexistent remote makes `git push` fail fast.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

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

        // "second" is always overwritten by "third" before it runs.
        assert!(
            outcomes.len() <= 2,
            "at most 2 sync attempts, never one per request: {outcomes:?}"
        );
        assert!(!sync.busy, "must settle back to idle, not stuck busy");
        assert!(sync.pending.is_none(), "no request left dangling");
        assert!(
            matches!(
                outcomes.first(),
                Some(SyncOutcome::CommittedNotPushed { .. })
            ),
            "first attempt has a real, unpushed change: {outcomes:?}"
        );

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

        sync.retry_push_if_due(Instant::now());

        assert!(sync.busy);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_retries_once_the_backoff_interval_elapses() {
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        // Committed directly rather than through `run_sync`.
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
        // An abandoned retry must be cleared, or it fires again every call.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let stale_commit = current_head(&work).unwrap();

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

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 2);
        assert!(!sync.busy, "nothing left to retry");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn apply_outcome_covers_every_variant_s_effect_on_the_retry_state() {
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let armed = |sync: &mut GitSync, at: Instant| sync.retry = Some((commit.clone(), at));

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::Synced);
        assert!(sync.retry.is_none(), "Synced clears the retry");

        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::RetryAbandoned);
        assert!(sync.retry.is_none(), "an abandoned retry clears it");

        let old = Instant::now() - PUSH_RETRY_INTERVAL * 4;
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::CommittedNotPushed {
            message: "nope".to_string(),
            commit: commit.clone(),
        });
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > old, "CommittedNotPushed restarts the backoff");

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

        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::Failed("boom".to_string()));
        let (_, at) = sync.retry.clone().expect("Failed keeps the retry armed");
        assert_eq!(at, old, "Failed must leave the retry's backoff alone");

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
        // A repeatedly failing content sync must not postpone a pushable commit.
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

        sync.retry = Some((commit, due));
        sync.apply_outcome(&SyncOutcome::RetryFailed("retry blew up".to_string()));
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > due, "a failed retry restarts its own backoff");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_failed_retry_does_not_fire_again_on_the_very_next_frame() {
        // A permanently failing retry must not fire on every frame.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now() - PUSH_RETRY_INTERVAL * 2;
        sync.retry = Some((commit, last_attempt));

        sync.retry_push_if_due(Instant::now());
        assert!(sync.busy, "a retry should have spawned");
        sync.busy = false;
        sync.apply_outcome(&SyncOutcome::RetryFailed(
            "git rev-parse: not a git repository".to_string(),
        ));

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
        // The change is written before either request, so "first" has something
        // to commit and "second" finds nothing left.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        sync.request(pending_sync("- [x] one\n", "first"));
        assert!(sync.busy, "first request should mark the worker busy");
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
        // A failed scope check must say a commit was made and kept.
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
