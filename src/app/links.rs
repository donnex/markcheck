//! Following a card's links (`o`): opening URLs, jumping to in-document
//! anchors, and the jump-back stack (`Ctrl-O`).

use crate::effect::Effect;
use crate::launch::{opener_failure_message, program_name};
use crate::model::{AppState, PendingChord, View};

use super::*;

/// Schemes `o` will hand to the opener. The allowlist doubles as
/// argument-injection cover: a URL starting with one of these can't start with
/// `-`, so the opener can never read it as an option. Matching is
/// case-insensitive — schemes are case-insensitive per RFC 3986.
pub(super) const SAFE_LINK_SCHEMES: [&str; 3] = ["http://", "https://", "mailto:"];

pub(super) fn is_safe_link(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    SAFE_LINK_SCHEMES
        .iter()
        .any(|scheme| lowered.starts_with(scheme))
}

impl AppState {
    /// `o`: open the current card's link. With several links, arm a digit that
    /// picks link `[N]`. The spawn happens in the main loop.
    pub(super) fn open_current_link(&mut self) {
        match self.current_link_urls().as_slice() {
            [url] => self.open_url(url),
            [] => self.set_error("Nothing to open: card has no link".to_string()),
            // Several links: arm a digit. No sticky message, which would hide the list
            // being chosen from.
            _ => self.pending_chord = PendingChord::OpenLink { typed: 0 },
        }
    }

    /// Opens link `[n]` (1-based) after `o` on a multi-link card.
    pub(super) fn open_link_number(&mut self, n: usize) {
        let urls = self.current_link_urls();
        match n.checked_sub(1).and_then(|i| urls.get(i)) {
            Some(url) => self.open_url(url),
            None => self.set_error(format!("No link [{n}] on this card")),
        }
    }

    /// `o` on `[Rollback](#rollback)`: jump to the first task under that
    /// heading, remembering where from for `Ctrl-O`, as in vim (#139).
    pub(super) fn follow_anchor(&mut self, anchor: &str) {
        let Some(target) = self.document.anchor_target(anchor) else {
            self.set_error(if self.document.has_anchor(anchor) {
                format!("#{anchor} has no tasks: nothing to jump to")
            } else {
                format!("No section named #{anchor} in this file")
            });
            return;
        };
        self.jump_back
            .push((self.current_list_index, self.current_item_index));
        self.focus_and_report(
            target,
            Some(format!("Jumped to #{anchor} · Ctrl-O goes back")),
        );
        self.view = View::Checklist;
    }

    /// `Ctrl-O`: back to where the last `#anchor` jump left from.
    pub(super) fn jump_back(&mut self) {
        match self.jump_back.pop() {
            Some(target) => self.focus_and_report(target, Some("Jumped back".to_string())),
            None => self.set_error(
                "Nothing to go back to: Ctrl-O returns from following a #link with o".to_string(),
            ),
        }
    }

    /// Moves to where `markcheck file.md#anchor` or `+N` asked to start
    /// (#114). `Ok` is the confirmation to show; `Err` says why it couldn't,
    /// with the cursor left where [`AppState::new`] put it: the first started
    /// task, or else the first unfinished one. The caller shows
    /// either alongside the other startup messages.
    pub fn open_at(&mut self, start: &crate::model::StartAt) -> Result<String, String> {
        let (target, description) = match start {
            crate::model::StartAt::Line(line) => {
                (self.document.first_task_from(*line), format!("line {line}"))
            }
            crate::model::StartAt::Anchor(anchor) => {
                (self.document.anchor_target(anchor), format!("#{anchor}"))
            }
        };
        let fallback = match self.resumed_label() {
            Some(label) => format!("so resumed at started task: \"{label}\""),
            None => "so opened at the start".to_string(),
        };
        let (list_index, item_index) = target.ok_or_else(|| match start {
            crate::model::StartAt::Anchor(anchor) if self.document.has_anchor(anchor) => {
                format!("#{anchor} has no tasks, {fallback}")
            }
            _ => format!("no list or task at {description}, {fallback}"),
        })?;
        self.focus_item(list_index, item_index, CursorMove::Navigate);
        Ok(format!("Opened at {description}"))
    }

    pub(super) fn current_link_urls(&self) -> Vec<String> {
        self.current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default()
    }

    /// Hands `url` to the main loop's opener, unless its scheme isn't one
    /// markcheck opens. A `#anchor` link is followed within the document.
    pub(super) fn open_url(&mut self, url: &str) {
        if let Some(anchor) = url.strip_prefix('#') {
            self.follow_anchor(anchor);
        } else if is_safe_link(url) {
            self.effects.push(Effect::OpenLink(url.to_string()));
        } else {
            self.set_error(format!("Not opened: unsupported link type ({url})"));
        }
    }

    /// `o`'s opener has started. Worded as an attempt, not a result: a
    /// quick failed exit is reported afterwards (`launch::opener_failure_message`).
    /// `on_this_host` says the opener runs on the machine markcheck runs on,
    /// for an SSH session, where that isn't the user's own.
    pub fn report_link_sent(&mut self, program: &str, url: &str, on_this_host: bool) {
        let name = program_name(program);
        let place = if on_this_host { " on this host" } else { "" };
        self.set_status(format!("Sent to {name}{place}: {url}"));
    }

    /// After a terminal browser (`launch::is_terminal_browser`) that had the
    /// terminal returns: confirms where the link was shown, or reports how
    /// the browser failed, however long it ran.
    pub fn finish_terminal_browser(
        &mut self,
        program: &str,
        url: &str,
        outcome: &std::io::Result<std::process::ExitStatus>,
    ) {
        match opener_failure_message(program, outcome, std::time::Duration::ZERO) {
            Some(message) => self.set_error(message),
            None => self.set_status(format!("Back from {}: {url}", program_name(program))),
        }
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
