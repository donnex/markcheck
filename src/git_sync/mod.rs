mod commit;
mod guards;
mod inspect;
mod process;
mod push;
mod shared;
mod sync;
#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(crate) use process::REDIRECTING_GIT_VARS;
pub use shared::SyncOutcome;
use shared::lock_hash;

use process::{
    Children, PLUMBING_TIMEOUT, git_command, run_with_timeout, track_children_on_this_thread,
};
use push::{catch_up_push, retry_commit};
use sync::{commit_message, run_sync};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::model::{PendingSync, hash_bytes};

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

/// The line to print on stderr after quitting, for a final sync that didn't
/// land cleanly: `last` is the last outcome seen while waiting, and
/// `still_running` says the wait gave up before the worker finished. The TUI
/// is already gone by then, so this is the only place the user can learn it.
pub fn exit_report(last: Option<&SyncOutcome>, still_running: bool) -> Option<String> {
    if still_running {
        return Some(
            "markcheck: git sync was still running when markcheck exited; \
             the last change may not be committed or pushed"
                .to_string(),
        );
    }
    match last? {
        SyncOutcome::Synced | SyncOutcome::Skipped | SyncOutcome::RetryAbandoned => None,
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
    /// How long after the last attempt the next retry is due.
    retry_interval: Duration,
    /// Hash of the latest request, read live by the worker so `run_sync` can
    /// tell a superseded request from an outside edit.
    latest_requested_hash: Arc<Mutex<[u8; 32]>>,
    /// The git processes the workers have running, for `abandon`.
    children: Arc<Children>,
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

impl GitSync {
    /// `Err` with the reason, for the startup message, when the file isn't
    /// in a git work tree or `git` can't be run; git-sync is optional, so the
    /// caller carries on without it.
    pub fn detect(file_path: &Path) -> Result<GitSync, String> {
        let repo_dir = file_path
            .parent()
            .ok_or_else(|| "the file has no parent directory".to_string())?
            .to_path_buf();
        let mut cmd = git_command(&repo_dir);
        cmd.args(["rev-parse", "--is-inside-work-tree"]);
        let output =
            run_with_timeout(cmd, PLUMBING_TIMEOUT).map_err(|err| detect_spawn_error(&err))?;
        if !output.status.success() {
            return Err(detect_git_error(&output.stderr));
        }
        if String::from_utf8_lossy(&output.stdout).trim() != "true" {
            return Err("the file is not in a git work tree".to_string());
        }
        let (sender, receiver) = mpsc::channel();
        Ok(GitSync {
            repo_dir,
            file_path: file_path.to_path_buf(),
            sender,
            receiver,
            busy: false,
            pending: None,
            retry: None,
            retry_interval: PUSH_RETRY_INTERVAL,
            latest_requested_hash: Arc::new(Mutex::new(hash_bytes(b""))),
            children: Arc::default(),
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
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
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
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
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
        let children = Arc::clone(&self.children);
        std::thread::spawn(move || {
            track_children_on_this_thread(children);
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
            SyncOutcome::Synced | SyncOutcome::RetryAbandoned => {
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
            SyncOutcome::Skipped | SyncOutcome::SkippedUntracked => {}
        }
    }

    fn grown_retry_interval(&self) -> Duration {
        (self.retry_interval * 2).min(PUSH_RETRY_MAX_INTERVAL)
    }

    /// Re-attempts a failed push once `retry_interval` has passed, since
    /// content that's already committed never triggers another request. `now`
    /// is a parameter so tests can skip the wait.
    pub fn retry_push_if_due(&mut self, now: Instant) {
        if self.busy {
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

    /// Drives the title bar's "syncing" state, and the wait when quitting.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Removes the temporary indexes (`markcheck-index-…`) a markcheck killed
    /// mid-sync left in the repository's git directory. Returns how many.
    pub fn sweep_interrupted_syncs(&self) -> usize {
        inspect::git_dir(&self.repo_dir).map_or(0, |git_dir| {
            crate::writer::remove_leftovers(&git_dir, "markcheck-index-")
        })
    }

    /// Gives up on the running sync, for a quit that won't wait for it: the
    /// running git process group gets `SIGTERM`, or `SIGKILL` with `force`,
    /// and no further git process starts, so the worker unwinds through its
    /// own cleanup. Returns whether a git process was running.
    pub fn abandon(&mut self, force: bool) -> bool {
        self.pending = None;
        self.retry = None;
        self.children.abandon(if force { "-KILL" } else { "-TERM" })
    }
}

#[cfg(test)]
#[path = "git_sync_tests.rs"]
mod tests;
