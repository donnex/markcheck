//! How a session ends, or stops for `Ctrl-Z`, with a git sync still running:
//! waiting for it, abandoning it, and the line `git_sync::exit_report` leaves
//! for after the TUI closes. The worker sits behind [`SyncWorker`], so the
//! sequence runs against a fake in tests; `main.rs` only routes events here.

use std::time::Duration;

use crate::git_sync::{self, GitSync, SyncOutcome};
use crate::model::AppState;

/// Shown while `q` waits for a sync still running.
pub const WAITING_FOR_SYNC: &str = "Waiting for git sync… press q again to abandon";

/// How long a signal lets a running sync finish before it is abandoned, and
/// `Ctrl-Z` waits for one before stopping anyway.
pub const QUIT_WAIT: Duration = Duration::from_secs(5);

/// How long an abandoned sync's worker gets to unwind through its cleanup
/// after `SIGTERM`, and then after `SIGKILL`.
pub const ABANDON_GRACE: Duration = Duration::from_secs(2);

/// What the quit sequence needs from the git-sync worker.
pub trait SyncWorker {
    /// Starts a change still in its quiet period now.
    fn flush(&mut self);
    fn is_busy(&self) -> bool;
    /// Stops the running git processes with `SIGTERM`, or `SIGKILL` with
    /// `force`.
    fn abandon(&mut self, force: bool);
    /// Waits up to `timeout` for the running sync to land, giving up early
    /// once `give_up` returns true, and returns the outcomes seen.
    fn wait_unless(&mut self, timeout: Duration, give_up: &dyn Fn() -> bool) -> Vec<SyncOutcome>;
    /// The latest sync problem nothing since has resolved.
    fn unresolved(&self) -> Option<&SyncOutcome>;
}

impl SyncWorker for GitSync {
    fn flush(&mut self) {
        GitSync::flush(self);
    }

    fn is_busy(&self) -> bool {
        GitSync::is_busy(self)
    }

    fn abandon(&mut self, force: bool) {
        GitSync::abandon(self, force);
    }

    fn wait_unless(&mut self, timeout: Duration, give_up: &dyn Fn() -> bool) -> Vec<SyncOutcome> {
        self.wait_for_outcomes_unless(timeout, give_up)
    }

    fn unresolved(&self) -> Option<&SyncOutcome> {
        GitSync::unresolved(self)
    }
}

/// Stops a running sync's git processes, then waits for the worker to remove
/// its temporary index and scratch files. A `hurried` quit (a second signal)
/// skips straight to `SIGKILL`.
pub fn abandon(sync: &mut impl SyncWorker, hurried: &dyn Fn() -> bool) {
    if !hurried() {
        sync.abandon(false);
        sync.wait_unless(ABANDON_GRACE, hurried);
    }
    if sync.is_busy() {
        sync.abandon(true);
        sync.wait_unless(ABANDON_GRACE, &|| false);
    }
}

/// Abandons whatever is still running, then gives the line to print after
/// the TUI closes for a sync problem still standing, however long before the
/// quit it was reported.
pub fn finish(sync: &mut impl SyncWorker, hurried: &dyn Fn() -> bool) -> Option<String> {
    if sync.is_busy() {
        abandon(sync, hurried);
    }
    git_sync::exit_report(sync.unresolved(), sync.is_busy())
}

/// Gives a running sync up to [`QUIT_WAIT`] to land, unless `hurried` cuts
/// the wait short, then [`finish`]es it.
pub fn settle(sync: &mut impl SyncWorker, hurried: &dyn Fn() -> bool) -> Option<String> {
    sync.wait_unless(QUIT_WAIT, hurried);
    finish(sync, hurried)
}

/// A quit with no TUI left to wait in (a signal, or a terminal error): a
/// change still in its quiet period goes out now, `on_wait` runs when there
/// is a sync to wait for, and the sync is [`settle`]d.
pub fn stop_without_tui(
    sync: &mut impl SyncWorker,
    hurried: &dyn Fn() -> bool,
    on_wait: impl FnOnce(),
) -> Option<String> {
    sync.flush();
    if sync.is_busy() {
        on_wait();
    }
    settle(sync, hurried)
}

/// `q` from the TUI: once it finds a sync still running, it waits for it
/// with [`WAITING_FOR_SYNC`] on screen until the sync lands or a second `q`
/// abandons it.
#[derive(Debug, Default)]
pub struct Quit {
    waiting: bool,
}

impl Quit {
    /// Whether `q` is waiting for a sync, and the checklist is done with.
    pub fn is_waiting(&self) -> bool {
        self.waiting
    }

    /// `q` was pressed. A change still in its quiet period goes out now, not
    /// never. Returns whether to stop at once; otherwise a sync is running
    /// and the wait has begun.
    pub fn begin(&mut self, sync: Option<&mut impl SyncWorker>, state: &mut AppState) -> bool {
        let Some(sync) = sync else {
            return true;
        };
        sync.flush();
        if !sync.is_busy() {
            return true;
        }
        self.waiting = true;
        state.set_notice(WAITING_FOR_SYNC.to_string());
        false
    }

    /// Whether the wait is over because the sync has landed.
    pub fn landed(&self, busy: bool) -> bool {
        self.waiting && !busy
    }

    /// A key pressed while waiting: `q` again abandons the sync (returns
    /// true), anything else is answered with the notice again, so no edit
    /// queues another sync.
    pub fn key(&self, code: crossterm::event::KeyCode, state: &mut AppState) -> bool {
        if code == crossterm::event::KeyCode::Char('q') {
            return true;
        }
        state.set_notice(WAITING_FOR_SYNC.to_string());
        false
    }
}

/// A stopped process commits and pushes nothing, so before `Ctrl-Z` stops it
/// a change still in its quiet period starts now and a running sync gets up
/// to [`QUIT_WAIT`] to land, or until `stopped` (a signal asking markcheck
/// to quit) returns true. `draw` shows the wait first. Returns whether a
/// sync is still running.
pub fn settle_before_suspend(
    state: &mut AppState,
    sync: &mut impl SyncWorker,
    stopped: &dyn Fn() -> bool,
    draw: impl FnOnce(&mut AppState) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    sync.flush();
    if sync.is_busy() {
        state.wait_for_sync_before_suspend();
        state.git_sync.busy = true;
        state.git_sync.waiting = false;
        draw(state)?;
        for outcome in sync.wait_unless(QUIT_WAIT, stopped) {
            state.apply_git_sync_outcome(&outcome);
        }
        state.finish_waiting_to_suspend();
    }
    Ok(sync.is_busy())
}

#[cfg(test)]
#[path = "quit_tests.rs"]
mod tests;
