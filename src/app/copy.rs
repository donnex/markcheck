//! Copying a card's command to the clipboard (`y`, `--auto-copy`) and
//! reporting how the copy went.

use crate::clipboard::{self, CopyOutcome};
use crate::effect::{CopyKind, Effect};
use crate::model::{AppState, PendingChord};

pub(super) const HIDDEN_CHARACTERS_MESSAGE: &str =
    "Not copied: command contains hidden control characters";

impl AppState {
    /// With `--auto-copy`, copy the current card's code when it has exactly
    /// one candidate. Silent for no-code, ambiguous or failure, except a
    /// refusal over hidden characters, which is always shown.
    pub(super) fn maybe_auto_copy(&mut self) {
        if !self.auto_copy {
            return;
        }
        if let Some([text]) = self.current_item().map(clipboard::candidates) {
            let text = text.clone();
            self.request_copy(text, CopyKind::Auto);
        }
    }

    pub(super) fn request_copy(&mut self, text: String, kind: CopyKind) {
        let primary = self.clipboard_primary;
        self.effects.push(Effect::Copy {
            text,
            primary,
            kind,
        });
    }

    /// Reports a finished copy the way its `kind` calls for. `text` is what
    /// was copied, named in the message only when a link had nowhere else
    /// to go.
    pub fn apply_copy_outcome(&mut self, kind: CopyKind, text: &str, outcome: CopyOutcome) {
        const COPIED: &str = "Copied to clipboard";
        const SENT: &str = "Sent to clipboard (OSC 52)";
        const BOTH: &str = "Copied (system clipboard + OSC 52)";
        const SYSTEM_ONLY: &str =
            "Copied to the system clipboard only — too large to send through the terminal (OSC 52)";
        match (kind, outcome) {
            (CopyKind::Auto, CopyOutcome::CopiedSystem) => self.report_auto_copy(COPIED),
            (CopyKind::Auto, CopyOutcome::CopiedOsc52) => self.report_auto_copy(SENT),
            (CopyKind::Auto, CopyOutcome::CopiedBoth) => self.report_auto_copy(BOTH),
            (CopyKind::Command, CopyOutcome::CopiedSystem) => self.set_status(COPIED.to_string()),
            (CopyKind::Command, CopyOutcome::CopiedOsc52) => self.set_status(SENT.to_string()),
            (CopyKind::Command, CopyOutcome::CopiedBoth) => self.set_status(BOTH.to_string()),
            (CopyKind::Auto, CopyOutcome::CopiedSystemOnlyTooLarge) => {
                self.report_auto_copy_problem(SYSTEM_ONLY)
            }
            (CopyKind::Command, CopyOutcome::CopiedSystemOnlyTooLarge) => {
                self.set_error(SYSTEM_ONLY.to_string())
            }
            (CopyKind::Auto | CopyKind::Command, CopyOutcome::HiddenCharacters) => {
                self.set_error(HIDDEN_CHARACTERS_MESSAGE.to_string())
            }
            (CopyKind::Auto, _) => {}
            (CopyKind::Command, CopyOutcome::TooLarge) => self.set_error(
                "Not copied: this command is too large for the terminal clipboard".to_string(),
            ),
            (CopyKind::Command, CopyOutcome::Failed) => {
                self.set_error("Copy failed: no clipboard available".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedSystem) => {
                self.set_status("No browser here — link copied to clipboard".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedOsc52) => {
                self.set_status("No browser here — link sent to clipboard (OSC 52)".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedBoth) => self.set_status(
                "No browser here — link copied (system clipboard + OSC 52)".to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::CopiedSystemOnlyTooLarge) => self.set_error(
                "No browser here — link copied to the system clipboard only, too large to send through the terminal (OSC 52)"
                    .to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::TooLarge) => self.set_error(
                "No browser here, and the link is too large for the terminal clipboard".to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::HiddenCharacters) => self.set_error(
                "No browser here, and the link contains hidden control characters — not copied"
                    .to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::Failed) => {
                self.set_error(format!("No browser or clipboard here — link: {text}"))
            }
            (CopyKind::LinkAlso, CopyOutcome::CopiedSystem) => {
                self.report_link_also("link also copied to clipboard", false)
            }
            (CopyKind::LinkAlso, CopyOutcome::CopiedOsc52) => {
                self.report_link_also("link also sent to clipboard (OSC 52)", false)
            }
            (CopyKind::LinkAlso, CopyOutcome::CopiedBoth) => {
                self.report_link_also("link also copied (system clipboard + OSC 52)", false)
            }
            (CopyKind::LinkAlso, CopyOutcome::CopiedSystemOnlyTooLarge) => self.report_link_also(
                "link also copied, but only to the system clipboard: too large to send through the terminal (OSC 52)",
                true,
            ),
            (CopyKind::LinkAlso, CopyOutcome::TooLarge) => self.report_link_also(
                "link not copied: too large for the terminal clipboard",
                true,
            ),
            (CopyKind::LinkAlso, CopyOutcome::HiddenCharacters) => self.report_link_also(
                "link not copied: it contains hidden control characters",
                true,
            ),
            (CopyKind::LinkAlso, CopyOutcome::Failed) => {
                self.report_link_also("link not copied: no clipboard available", true)
            }
        }
    }

    /// Copies an exact code fragment (a specific clicked row), bypassing
    /// the single-candidate rule; reported the same way as `y`.
    pub(super) fn copy_specific(&mut self, text: &str) {
        self.request_copy(text.to_string(), CopyKind::Command);
    }

    /// An auto-copy confirmation goes after whatever the motion reported
    /// (`Match 2/5 · Copied to clipboard`), and never hides an error.
    pub(super) fn report_auto_copy(&mut self, copied: &str) {
        if self.status_is_error {
            return;
        }
        let message = match self.status_message.take() {
            Some(reported) => format!("{reported} · {copied}"),
            None => copied.to_string(),
        };
        self.set_status(message);
    }

    /// Like [`Self::report_auto_copy`], but for a copy that needs reading:
    /// it goes after whatever the motion reported, error or not, and stays.
    pub(super) fn report_auto_copy_problem(&mut self, problem: &str) {
        let message = match self.status_message.take() {
            Some(reported) => format!("{reported} · {problem}"),
            None => problem.to_string(),
        };
        self.set_error(message);
    }

    /// Adds how `o`'s extra copy went after the opener's report
    /// (`Sent to xdg-open on this host: … · link also copied (OSC 52)`),
    /// keeping the message sticky when either part is a problem.
    fn report_link_also(&mut self, copied: &str, problem: bool) {
        let was_error = self.status_is_error;
        let message = match self.status_message.take() {
            Some(reported) => format!("{reported} · {copied}"),
            None => copied.to_string(),
        };
        if problem || was_error {
            self.set_error(message);
        } else {
            self.set_status(message);
        }
    }

    /// `o` over SSH, after handing the link to an opener on this machine:
    /// copy it too, which reaches the user's own terminal via OSC 52.
    pub fn copy_link_too(&mut self, url: &str) {
        self.request_copy(url.to_string(), CopyKind::LinkAlso);
    }

    /// `o` with nowhere to send the link (`launch::LinkRoute::CopyInstead`):
    /// copy it instead, which over SSH reaches the local terminal via OSC 52.
    pub fn copy_link_instead(&mut self, url: &str) {
        self.request_copy(url.to_string(), CopyKind::LinkFallback);
    }

    pub(super) fn copy_current_code(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        match clipboard::candidates(current) {
            [] => self.set_error("Nothing to copy: item has no code".to_string()),
            [text] => {
                let text = text.clone();
                self.request_copy(text, CopyKind::Command);
            }
            // Several commands: arm a digit that picks command `[N]`. No sticky
            // message, so the status bar shows the prompt instead.
            _ => self.pending_chord = PendingChord::CopyCode { typed: 0 },
        }
    }

    /// Copies command `[n]` (1-based) after `y` on a card with several.
    pub(super) fn copy_code_number(&mut self, n: usize) {
        let text = self
            .current_item()
            .and_then(|item| clipboard::candidates(item).get(n - 1).cloned());
        match text {
            Some(text) => self.copy_specific(&text),
            None => self.set_error(format!("No command [{n}] on this card")),
        }
    }
}

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;
