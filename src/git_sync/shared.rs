//! The leaf every other `git_sync` submodule may import: the outcome type,
//! the request-hash lock and the test race points. It imports nothing from
//! its siblings, so the layering stays one-directional.

use std::sync::Mutex;

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
    pub(super) static RACE_HOOKS: std::cell::RefCell<
        std::collections::HashMap<RacePoint, Box<dyn Fn()>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
pub(super) fn race_point(point: RacePoint) {
    // Taken out while it runs, so a hook reaching a sync point can't recurse.
    let hook = RACE_HOOKS.with(|h| h.borrow_mut().remove(&point));
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
#[inline(always)]
pub(super) fn race_point(_point: RacePoint) {}

/// Result of one background commit+push attempt, delivered via
/// [`GitSync::poll`](super::GitSync::poll).
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Synced,
    /// The startup catch-up pushed `pushed` commits an earlier session left
    /// local-only.
    CaughtUp {
        pushed: usize,
    },
    /// The startup catch-up found a commit markcheck didn't make in the way,
    /// so it pushed nothing; the message names it.
    NotCaughtUp(String),
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
    /// The sync stopped: a git command failed (the message is the first line
    /// of its stderr) or a guard refused. Usually nothing was committed, but a
    /// commit whose scope couldn't be verified, or whose undo failed, is left on
    /// the branch and the message names it.
    Failed(String),
    /// A push retry failed outright. Separate from `Failed` because it restarts
    /// the retry's backoff, while a content sync's failure must leave it alone.
    RetryFailed(String),
}

/// Tolerates poisoning: the digest has no invariant to break, and a panic
/// here would crash the TUI from the main thread.
pub(super) fn lock_hash(hash: &Mutex<[u8; 32]>) -> std::sync::MutexGuard<'_, [u8; 32]> {
    hash.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
