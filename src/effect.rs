//! Everything `AppState` asks of the outside world. Handlers push an
//! [`Effect`] instead of touching the clipboard, a process or the terminal;
//! the main loop hands the queue to [`drain`], whose one exhaustive `match`
//! is the only place each kind is carried out.

use crate::clipboard;
use crate::model::{AppState, LineNumber, PendingSync};

/// One outward action requested by `AppState`, in the order it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Copy {
        text: String,
        primary: bool,
        kind: CopyKind,
    },
    /// `e`: suspend the TUI and edit the file, at `line` when known.
    OpenEditor {
        line: Option<LineNumber>,
    },
    /// `Ctrl-Z`: hand the terminal back to the shell until `fg`.
    Suspend,
    OpenLink(String),
    GitSync(PendingSync),
    /// A reload replaced the content a request still waiting in the worker
    /// carries, so that request must not run.
    CancelGitSync,
    Quit,
}

/// What a copy was for, which decides how its outcome is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    /// `--auto-copy` on reaching a card: announces success, otherwise silent
    /// except for a hidden-character refusal.
    Auto,
    /// An explicit `y`, `y` + digit or click on a command.
    Command,
    /// `o` with no browser to send the link to.
    LinkFallback,
}

/// The parts of the outside world `drain` needs from the main loop: the
/// terminal-owning actions and the git-sync worker.
pub trait Host {
    fn open_editor(&mut self, state: &mut AppState, line: Option<LineNumber>)
    -> anyhow::Result<()>;
    fn suspend(&mut self, state: &mut AppState) -> anyhow::Result<()>;
    fn open_link(&mut self, state: &mut AppState, url: &str);
    fn git_sync(&mut self, sync: PendingSync);
    /// Whether a waiting request was dropped.
    fn cancel_git_sync(&mut self) -> bool;
}

/// What a [`drain`] did, for the main loop's redraw and exit decisions.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub ran_any: bool,
    pub quit: bool,
}

/// Carries out every queued effect, including any queued while doing so
/// (a link falling back to a copy, an editor edit queueing a sync), in order.
pub fn drain(state: &mut AppState, host: &mut impl Host) -> anyhow::Result<Drained> {
    let mut drained = Drained::default();
    while !state.effects.is_empty() {
        let effect = state.effects.remove(0);
        drained.ran_any = true;
        match effect {
            Effect::Copy {
                text,
                primary,
                kind,
            } => {
                let outcome = clipboard::copy(&text, primary, state.copy_route);
                state.apply_copy_outcome(kind, &text, outcome);
            }
            Effect::OpenEditor { line } => host.open_editor(state, line)?,
            Effect::Suspend => host.suspend(state)?,
            Effect::OpenLink(url) => host.open_link(state, &url),
            Effect::GitSync(sync) => host.git_sync(sync),
            Effect::CancelGitSync => {
                if host.cancel_git_sync() {
                    state.report_dropped_git_sync();
                }
            }
            Effect::Quit => drained.quit = true,
        }
    }
    Ok(drained)
}

#[cfg(test)]
#[path = "effect_tests.rs"]
mod tests;
