use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::cards::wrap_text;
use crate::clipboard;
use crate::model::{AppState, Overlay, View};

/// Most rows a long status message may grow the status bar to.
const MAX_STATUS_ROWS: u16 = 6;

// Trimmed to the essentials; the `?` overlay is the full reference.
const KEYBIND_LEGEND: &str = "hjkl:nav  space:toggle  /:search  y:copy  ?:help  q:quit";
const LIST_COMPLETE_LEGEND: &str = "l/enter:next list  h:review  ?:help  q:quit";
const ALL_COMPLETE_LEGEND: &str = "h:review  R:reset  ?:help  q:quit";
const CONFIRM_LEGEND: &str = "y:reset  any other key:cancel";
const CONFIRM_QUIT_LEGEND: &str = "y:reset & quit  n:quit  esc:cancel";
const HELP_LEGEND: &str = "j/k ↑↓: scroll  ·  any other key: close help";
const PICKER_LEGEND: &str =
    "type: filter  ↑↓ Ctrl-N/P: move  Ctrl-D/U: page  enter: go  esc: close";

/// The status message wrapped to `width`, or `None` when the bar shows the
/// search prompt, an armed prompt or a legend instead, which stay on one row.
fn wrapped_message(state: &AppState, width: u16) -> Option<Vec<String>> {
    if state.overlay == Some(Overlay::Search) {
        return None;
    }
    Some(wrap_text(state.status_message.as_deref()?, width))
}

/// Rows the status bar needs at `width`: one, unless a status message is too
/// long for a single row. Long messages wrap instead of being clipped, since
/// their actionable part (a lock file to remove, the reason a sync failed) is
/// often at the end. Capped at `MAX_STATUS_ROWS` and a quarter of `height`, so
/// the checklist always keeps most of the screen.
pub fn height(state: &AppState, width: u16, height: u16) -> u16 {
    let rows = wrapped_message(state, width).map_or(1, |lines| lines.len());
    let cap = MAX_STATUS_ROWS.min(height / 4).max(1);
    u16::try_from(rows).unwrap_or(u16::MAX).clamp(1, cap)
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // Incremental search: show the live query vim-style, taking over the
    // status bar while the Search overlay is open. Always give feedback — the
    // current query, the match count, and a distinct "no matches" state.
    if state.overlay == Some(Overlay::Search) {
        let query = &state.search.query;
        let line = if query.is_empty() {
            Line::raw("/".to_string())
        } else {
            let count = state.find_matches(query).len();
            if count == 0 {
                Line::raw(format!("/{query}  (no matches)")).fg(state.palette.error)
            } else {
                let noun = if count == 1 { "match" } else { "matches" };
                Line::raw(format!("/{query}  ({count} {noun})"))
            }
        };
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // A status message takes precedence; otherwise show the armed prompt or
    // the legend for the current screen.
    if let Some(lines) = wrapped_message(state, area.width) {
        // The error color applies only to a real message, never the prompt or
        // legend.
        let lines: Vec<Line> = lines
            .into_iter()
            .map(|line| {
                if state.status_is_error {
                    Line::raw(line).fg(state.palette.error)
                } else {
                    Line::raw(line)
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let legend = match (state.overlay, state.view) {
        (Some(Overlay::ConfirmReset), _) => CONFIRM_LEGEND,
        (Some(Overlay::ConfirmQuitReset), _) => CONFIRM_QUIT_LEGEND,
        (Some(Overlay::Help), _) => HELP_LEGEND,
        (Some(Overlay::ListPicker), _) => PICKER_LEGEND,
        (Some(Overlay::Search) | None, View::ListComplete) => LIST_COMPLETE_LEGEND,
        (Some(Overlay::Search) | None, View::AllComplete) => ALL_COMPLETE_LEGEND,
        (Some(Overlay::Search) | None, View::Checklist) => KEYBIND_LEGEND,
    };
    // While a multi-link `o` or multi-command `y` is armed, the status bar shows
    // its digit prompt; the URLs themselves live in the panel below the card.
    // Priority: search > status message > armed prompt > legend.
    let armed_prompt = if state.overlay.is_none() && state.view == View::Checklist {
        let item = state.current_item();
        if state.pending_open_link {
            let n = item.map_or(0, |item| item.link_urls().len());
            Some(number_prompt("open", n, state.pending_number))
        } else if state.pending_copy_code {
            let n = item.map_or(0, |item| clipboard::candidates(item).len());
            Some(number_prompt("copy", n, state.pending_number))
        } else {
            None
        }
    } else {
        None
    };
    let text = armed_prompt.as_deref().unwrap_or(legend);
    frame.render_widget(Paragraph::new(Line::raw(text)), area);
}

/// The armed `o`/`y` prompt: the range to choose from, then, once a digit that
/// could start a longer number is typed, that number and how to finish it.
fn number_prompt(verb: &str, count: usize, typed: usize) -> String {
    if typed == 0 {
        format!("press 1–{count} to {verb} · esc cancels")
    } else {
        format!("{verb} [{typed}…] of 1–{count} · next digit, or enter for [{typed}] · esc cancels")
    }
}
