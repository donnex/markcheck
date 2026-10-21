mod commit;
mod guards;
mod inspect;
mod process;
mod push;
mod shared;
mod sync;
#[cfg(test)]
mod test_support;

pub use shared::SyncOutcome;
use shared::lock_hash;

use process::{
    Children, PLUMBING_TIMEOUT, git_command, run_with_timeout, track_children_on_this_thread,
};
use push::{catch_up_push, retry_commit};
use sync::{Publish, batch_message, run_sync};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::model::{PendingSync, SyncBase, hash_bytes};

/// Backoff before the first automatic push retry after `CommittedNotPushed`.
/// Each failed retry of the same commit doubles it, up to
/// `PUSH_RETRY_MAX_INTERVAL`, so an offline laptop isn't polling the remote
/// every 30 seconds for hours.
const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const PUSH_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The sticky status-bar message for an outcome polled while the TUI runs;
/// `None` for the ones not worth interrupting the user for: a landed sync
/// (the title bar's "Synced … ago" tag shows it), an ordinary no-op, and a
/// retry giving up on a commit something else has already superseded.
pub fn status_message(outcome: &SyncOutcome) -> Option<String> {
    match outcome {
        SyncOutcome::Synced | SyncOutcome::Skipped | SyncOutcome::RetryAbandoned => None,
        SyncOutcome::CaughtUp { pushed: 1 } => {
            Some("Pushed 1 earlier markcheck commit".to_string())
        }
        SyncOutcome::CaughtUp { pushed } => {
            Some(format!("Pushed {pushed} earlier markcheck commits"))
        }
        SyncOutcome::NotCaughtUp(message) => Some(format!("Git sync: {message}")),
        SyncOutcome::SkippedUntracked => {
            Some("Git sync skipped: file is not tracked in git (run `git add` on it)".to_string())
        }
        SyncOutcome::CommittedNotPushed { message, .. } => Some(format!(
            "Committed, not pushed: {message} (will keep retrying)"
        )),
        SyncOutcome::Failed(message) => Some(format!("Git sync failed: {message}")),
        // The commit is still safely local and still armed, so say so rather
        // than implying the change was lost.
        SyncOutcome::RetryFailed(message) => {
            Some(format!("Push retry failed: {message} (will keep retrying)"))
        }
    }
}

/// The line to print on stderr after quitting, for a sync problem still
/// standing: `last` is [`GitSync::unresolved`], and `still_running` says the wait gave up before the worker finished. The TUI
/// is already gone by then, so this is the only place the user can learn it.
pub fn exit_report(last: Option<&SyncOutcome>, still_running: bool) -> Option<String> {
    exit_report_text(last, still_running).map(|report| crate::printable::printable(&report))
}

fn exit_report_text(last: Option<&SyncOutcome>, still_running: bool) -> Option<String> {
    if still_running {
        return Some(
            "markcheck: git sync was still running when markcheck exited; \
             the last change may not be committed or pushed"
                .to_string(),
        );
    }
    match last? {
        SyncOutcome::Synced
        | SyncOutcome::CaughtUp { .. }
        | SyncOutcome::NotCaughtUp(_)
        | SyncOutcome::Skipped
        | SyncOutcome::RetryAbandoned => None,
        SyncOutcome::SkippedUntracked => Some(
            "markcheck: git sync skipped: file is not tracked in git (run `git add` on it)"
                .to_string(),
        ),
        SyncOutcome::CommittedNotPushed { message, .. } | SyncOutcome::RetryFailed(message) => {
            Some(format!(
                "markcheck: committed but not pushed: {message}; \
                 markcheck pushes it the next time it opens this file with git sync on"
            ))
        }
        SyncOutcome::Failed(message) => Some(format!(
            "markcheck: git sync failed: {message}; the file is saved, but not committed"
        )),
    }
}

/// Drives commit+push for one file on a background thread, so a slow
/// `git push` never blocks the UI.
pub struct GitSync {
    repo_root: PathBuf,
    file_path: PathBuf,
    sender: mpsc::Sender<SyncOutcome>,
    receiver: mpsc::Receiver<SyncOutcome>,
    /// A request arriving while busy waits in `queued`: two concurrent
    /// commits on one repository could race on the index.
    busy: bool,
    /// The request waiting for its quiet period or for the running sync.
    /// Requests are folded together: the latest content, every description.
    queued: Option<Queued>,
    /// The quiet period: how long after the last request the sync starts.
    delay: Duration,
    /// The commit SHA to retry pushing and when the last attempt finished.
    retry: Option<(String, Instant)>,
    /// How long after the last attempt the next retry is due.
    retry_interval: Duration,
    /// Hash of the latest request, read live by the worker so `run_sync` can
    /// tell a superseded request from an outside edit.
    latest_requested_hash: Arc<Mutex<[u8; 32]>>,
    /// The git processes the workers have running, for `abandon`.
    children: Arc<Children>,
    /// The base an `e` edit the user saved was made on: later toggles on it
    /// may publish that edit too.
    edited_epoch: Option<u64>,
    /// The base the last landed content sync was made on, and the content it
    /// committed. A later request on that base is checked against this
    /// content, since the commit moved `HEAD` past the base itself.
    landed: Option<(u64, String)>,
    /// The base and content of the running content sync, for `landed`.
    in_flight: Option<(u64, String)>,
    /// The latest problem no later outcome has resolved, for the exit report.
    unresolved: Option<SyncOutcome>,
}

/// Requests folded together while they wait (see `GitSync::queued`).
struct Queued {
    sync: PendingSync,
    descriptions: Vec<String>,
    /// `None` when the delay is too long to add to now: due only on a flush.
    due: Option<Instant>,
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

/// Why `git` itself couldn't be run at startup.
fn detect_spawn_error(err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        "git not found on PATH".to_string()
    } else {
        format!("could not run git: {err}")
    }
}

/// Why `git rev-parse` refused, from its stderr. Kept whole rather than cut
/// to the first line: for a repository owned by another user, git follows its
/// `fatal:` line with the exact `safe.directory` command that fixes it.
fn detect_git_error(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    if stderr.contains("not a git repository") {
        return "the file is not in a git repository".to_string();
    }
    let text = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        "git could not inspect the file's directory".to_string()
    } else {
        text
    }
}

/// The oldest git with every format atom git-sync reads: `%(push:remotename)`
/// (2.16) and `%(trailers:key=…,valueonly,separator=…)` (2.22).
const MIN_GIT_VERSION: (u32, u32) = (2, 22);

/// The `(major, minor)` of `git version` output such as `git version 2.39.2`
/// or `git version 2.40.0.windows.1`, with the version text as printed.
fn parse_git_version(stdout: &str) -> Option<((u32, u32), &str)> {
    let text = stdout
        .trim()
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some(((major, minor), text))
}

/// Refuses a git too old for git-sync. Output that can't be parsed is let
/// through: a wrapper may print its own banner, and the commands themselves
/// still report any failure.
fn check_git_version(stdout: &str) -> Result<(), String> {
    match parse_git_version(stdout) {
        Some((version, text)) if version < MIN_GIT_VERSION => Err(format!(
            "git {}.{} or newer is needed (found {text})",
            MIN_GIT_VERSION.0, MIN_GIT_VERSION.1
        )),
        _ => Ok(()),
    }
}

impl GitSync {
    /// `Err` with the reason, for the startup message, when the file isn't
    /// in a git work tree or `git` is missing or too old; git-sync is
    /// optional, so the caller carries on without it.
    pub fn detect(file_path: &Path) -> Result<GitSync, String> {
        let file_dir = file_path
            .parent()
            .ok_or_else(|| "the file has no parent directory".to_string())?;
        let mut cmd = git_command(file_dir);
        cmd.arg("version");
        let output =
            run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| detect_spawn_error(&err))?;
        check_git_version(&String::from_utf8_lossy(&output.stdout))?;
        let mut cmd = git_command(file_dir);
        cmd.args(["rev-parse", "--is-inside-work-tree"]);
        let output =
            run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| detect_spawn_error(&err))?;
        if !output.status.success() {
            return Err(detect_git_error(&output.stderr));
        }
        if String::from_utf8_lossy(&output.stdout).trim() != "true" {
            return Err("the file is not in a git work tree".to_string());
        }
        // Resolved once: every git-sync command runs from the root, so none
        // can resolve a path or `@{u}` differently from another (#141).
        let mut cmd = git_command(file_dir);
        cmd.args(["rev-parse", "--show-toplevel"]);
        let output =
            run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| detect_spawn_error(&err))?;
        if !output.status.success() {
            return Err(detect_git_error(&output.stderr));
        }
        let repo_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let (sender, receiver) = mpsc::channel();
        Ok(GitSync {
            repo_root,
            file_path: file_path.to_path_buf(),
            sender,
            receiver,
            busy: false,
            queued: None,
            delay: Duration::ZERO,
            retry: None,
            retry_interval: PUSH_RETRY_INTERVAL,
            latest_requested_hash: Arc::new(Mutex::new(hash_bytes(b""))),
            children: Arc::default(),
            edited_epoch: None,
            landed: None,
            in_flight: None,
            unresolved: None,
        })
    }

    /// Waits `delay` after each request before syncing, so a burst of
    /// changes becomes one commit (#56). Zero syncs every request at once.
    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Records the request's hash before coalescing, so a sync already in
    /// flight sees that a newer request exists. The request then waits out the
    /// quiet period, restarted by every later one, unless it is `immediate`.
    pub fn request(&mut self, sync: PendingSync) {
        *lock_hash(&self.latest_requested_hash) = sync.content_hash;
        if let SyncBase::Edited { epoch } = sync.base {
            self.edited_epoch = Some(epoch);
        }
        let now = Instant::now();
        let due = if sync.immediate {
            Some(now)
        } else {
            now.checked_add(self.delay)
        };
        self.queued = Some(match self.queued.take() {
            // The earliest pre-write content is what the batch replaces.
            Some(mut queued) => {
                queued.descriptions.push(sync.description.clone());
                queued.sync = PendingSync {
                    previous_content: queued.sync.previous_content,
                    ..sync
                };
                queued.due = due;
                queued
            }
            None => Queued {
                descriptions: vec![sync.description.clone()],
                sync,
                due,
            },
        });
        self.spawn_if_due(now);
    }

    /// Drops the waiting request, for a reload that replaced its content,
    /// returning whether there was one. A running sync and an armed push
    /// retry are left alone.
    pub fn cancel_queued(&mut self) -> bool {
        self.queued.take().is_some()
    }

    /// Starts the waiting request now, for a quit that shouldn't lose it.
    pub fn flush(&mut self) {
        let now = Instant::now();
        if let Some(queued) = &mut self.queued {
            queued.due = Some(now);
        }
        self.spawn_if_due(now);
    }

    /// Whether a request is waiting out the quiet period (not just behind a
    /// running sync), for the title bar.
    pub fn is_waiting(&self) -> bool {
        self.queued.is_some() && !self.busy
    }

    /// Starts a waiting request whose quiet period has passed, then any push
    /// retry that has come due. Call once per frame.
    pub fn tick(&mut self, now: Instant) {
        self.spawn_if_due(now);
        self.retry_push_if_due(now);
    }

    fn spawn_if_due(&mut self, now: Instant) {
        if self.busy
            || !self
                .queued
                .as_ref()
                .is_some_and(|queued| queued.due.is_some_and(|due| due <= now))
        {
            return;
        }
        if let Some(queued) = self.queued.take() {
            self.spawn(queued.sync, queued.descriptions);
        }
    }

    /// The content a request on `base` must find committed or staged, given
    /// what has landed since; `None` when it may publish everything.
    fn publish_scope(&self, base: SyncBase) -> Option<Option<String>> {
        match base {
            SyncBase::Own { epoch, content } if self.edited_epoch != Some(epoch) => {
                Some(match &self.landed {
                    Some((landed_epoch, landed)) if *landed_epoch == epoch => Some(landed.clone()),
                    _ => content,
                })
            }
            _ => None,
        }
    }

    fn spawn(&mut self, sync: PendingSync, descriptions: Vec<String>) {
        self.busy = true;
        self.in_flight = Some((sync.base.epoch(), sync.content.clone()));
        let scope = self.publish_scope(sync.base);
        let repo_root = self.repo_root.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        let latest_requested_hash = Arc::clone(&self.latest_requested_hash);
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
            let outcome = run_reporting_panics(|| {
                let message = batch_message(&file_path, &descriptions);
                let publish = match &scope {
                    None => Publish::Everything,
                    Some(base) => Publish::OwnChangesSince(base.as_deref()),
                };
                run_sync(
                    &repo_root,
                    &file_path,
                    &sync.content,
                    &message,
                    &latest_requested_hash,
                    sync.previous_content.as_deref(),
                    publish,
                )
            });
            let _ = sender.send(outcome);
        });
    }

    fn spawn_retry(&mut self, commit: String) {
        self.busy = true;
        self.in_flight = None;
        let repo_root = self.repo_root.clone();
        let sender = self.sender.clone();
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
            // Every failure here is the retry's own.
            let outcome = match run_reporting_panics(|| retry_commit(&repo_root, &commit)) {
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
        self.in_flight = None;
        let repo_root = self.repo_root.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
            let outcome = run_reporting_panics(|| catch_up_push(&repo_root, &file_path));
            let _ = sender.send(outcome);
        });
    }

    /// Drains the channel non-blockingly; call once per frame.
    pub fn poll(&mut self) -> Option<SyncOutcome> {
        let outcome = self.receiver.try_recv().ok();
        if let Some(outcome) = &outcome {
            self.busy = false;
            if let Some(synced) = self.in_flight.take()
                && matches!(
                    outcome,
                    SyncOutcome::Synced
                        | SyncOutcome::Skipped
                        | SyncOutcome::CommittedNotPushed { .. }
                )
            {
                self.landed = Some(synced);
            }
            self.apply_outcome(outcome);
            self.track_unresolved(outcome);
            self.spawn_if_due(Instant::now());
        }
        outcome
    }

    /// Polls until no sync is running, the requests a finishing one starts
    /// included, or `timeout` passes, and returns every outcome seen in
    /// order; [`GitSync::is_busy`] afterwards says whether it gave up.
    #[cfg(test)]
    pub fn wait_for_outcomes(&mut self, timeout: Duration) -> Vec<SyncOutcome> {
        self.wait_for_outcomes_unless(timeout, || false)
    }

    /// Polls until no sync is running, the requests a finishing one starts
    /// included, `timeout` passes or `give_up` returns true, and returns
    /// every outcome seen in order; [`GitSync::is_busy`] afterwards says
    /// whether it gave up.
    pub fn wait_for_outcomes_unless(
        &mut self,
        timeout: Duration,
        give_up: impl Fn() -> bool,
    ) -> Vec<SyncOutcome> {
        let deadline = Instant::now() + timeout;
        let mut outcomes = Vec::new();
        while self.busy && Instant::now() < deadline && !give_up() {
            outcomes.extend(self.poll());
            if self.busy {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        outcomes
    }

    /// The single writer of `self.retry`, as a total `match` so a new
    /// `SyncOutcome` variant can't silently leave a retry firing every frame.
    fn apply_outcome(&mut self, outcome: &SyncOutcome) {
        match outcome {
            SyncOutcome::Synced | SyncOutcome::CaughtUp { .. } | SyncOutcome::RetryAbandoned => {
                self.retry = None;
                self.retry_interval = PUSH_RETRY_INTERVAL;
            }
            SyncOutcome::CommittedNotPushed { commit, .. } => {
                let same_commit = self
                    .retry
                    .as_ref()
                    .is_some_and(|(armed, _)| armed == commit);
                self.retry_interval = if same_commit {
                    self.grown_retry_interval()
                } else {
                    PUSH_RETRY_INTERVAL
                };
                self.retry = Some((commit.clone(), Instant::now()));
            }
            // Keep it armed but restart the backoff.
            SyncOutcome::RetryFailed(_) => {
                if let Some((_, last_attempt)) = &mut self.retry {
                    *last_attempt = Instant::now();
                    self.retry_interval = self.grown_retry_interval();
                }
            }
            // A content sync's failure says nothing about an earlier commit's retry.
            SyncOutcome::Failed(_) => {}
            SyncOutcome::Skipped | SyncOutcome::SkippedUntracked | SyncOutcome::NotCaughtUp(_) => {}
        }
    }

    /// A push outcome settles only an earlier push problem: a content sync
    /// that failed is still uncommitted whatever happens to an older commit.
    fn track_unresolved(&mut self, outcome: &SyncOutcome) {
        let push_problem = matches!(
            self.unresolved,
            None | Some(SyncOutcome::CommittedNotPushed { .. } | SyncOutcome::RetryFailed(_))
        );
        match outcome {
            SyncOutcome::Synced | SyncOutcome::Skipped => self.unresolved = None,
            SyncOutcome::CaughtUp { .. } | SyncOutcome::RetryAbandoned if push_problem => {
                self.unresolved = None;
            }
            SyncOutcome::Failed(_)
            | SyncOutcome::SkippedUntracked
            | SyncOutcome::CommittedNotPushed { .. } => self.unresolved = Some(outcome.clone()),
            SyncOutcome::RetryFailed(_) if push_problem => {
                self.unresolved = Some(outcome.clone());
            }
            SyncOutcome::CaughtUp { .. }
            | SyncOutcome::RetryAbandoned
            | SyncOutcome::RetryFailed(_)
            | SyncOutcome::NotCaughtUp(_) => {}
        }
    }

    /// The latest sync problem nothing since has resolved: what the user
    /// still needs to hear about when markcheck exits.
    pub fn unresolved(&self) -> Option<&SyncOutcome> {
        self.unresolved.as_ref()
    }

    fn grown_retry_interval(&self) -> Duration {
        (self.retry_interval * 2).min(PUSH_RETRY_MAX_INTERVAL)
    }

    /// Re-attempts a failed push once `retry_interval` has passed, since
    /// content that's already committed never triggers another request. `now`
    /// is a parameter so tests can skip the wait.
    pub fn retry_push_if_due(&mut self, now: Instant) {
        // A waiting request's sync pushes the armed commit along with its own.
        if self.busy || self.queued.is_some() {
            return;
        }
        let Some((commit, last_attempt)) = &self.retry else {
            return;
        };
        if now.saturating_duration_since(*last_attempt) < self.retry_interval {
            return;
        }
        let commit = commit.clone();
        self.spawn_retry(commit);
    }

    /// The repository's top level, which every git-sync command runs from.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// The sticky startup warning when no git identity is configured, since
    /// every commit is then refused rather than made under one git guessed
    /// from the account and host name.
    pub fn identity_warning(&self) -> Option<String> {
        inspect::identity_missing(&self.repo_root).then(|| {
            format!(
                "Git sync: no git identity set — commits will be refused; {}",
                inspect::IDENTITY_FIX
            )
        })
    }

    /// Drives the title bar's "syncing" state, and the wait when quitting.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Removes the temporary indexes (`markcheck-index-…`) a markcheck killed
    /// mid-sync left in the repository's git directory. Returns how many.
    pub fn sweep_interrupted_syncs(&self) -> usize {
        inspect::git_dir(&self.repo_root).map_or(0, |git_dir| {
            crate::writer::remove_leftovers(&git_dir, "markcheck-index-")
        })
    }

    /// Gives up on the running sync, for a quit that won't wait for it: the
    /// running git process group gets `SIGTERM`, or `SIGKILL` with `force`,
    /// and no further git process starts, so the worker unwinds through its
    /// own cleanup. Returns whether a git process was running.
    pub fn abandon(&mut self, force: bool) -> bool {
        self.queued = None;
        self.retry = None;
        self.in_flight = None;
        self.children
            .abandon(if force { libc::SIGKILL } else { libc::SIGTERM })
    }
}

#[cfg(test)]
#[path = "git_sync_tests.rs"]
mod tests;
