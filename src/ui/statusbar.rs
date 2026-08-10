use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::model::{AppState, Screen};

// Trimmed to the essentials; the `?` overlay is the full reference.
const KEYBIND_LEGEND: &str = "hjkl:nav  space:toggle  /:search  y:copy  ?:help  q:quit";
const CONFIRM_LEGEND: &str = "y:reset  any other key:cancel";
const CONFIRM_QUIT_LEGEND: &str = "y:reset & quit  n:quit  esc:cancel";
const HELP_LEGEND: &str = "j/k ↑↓: scroll  ·  any other key: close help";
const PICKER_LEGEND: &str =
    "type: filter  ↑↓ Ctrl-N/P: move  Ctrl-D/U: page  enter: go  esc: close";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // Incremental search: show the live query vim-style, taking over the
    // status bar while the Search screen is active. Always give feedback — the
    // current query, the match count, and a distinct "no matches" state.
    if state.screen == Screen::Search {
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

    // A transient status message takes precedence; otherwise show the
    // legend for the current screen.
    let legend = match state.screen {
        Screen::ConfirmReset => CONFIRM_LEGEND,
        Screen::ConfirmQuitReset => CONFIRM_QUIT_LEGEND,
        Screen::Help => HELP_LEGEND,
        Screen::ListPicker => PICKER_LEGEND,
        _ => KEYBIND_LEGEND,
    };
    // While a multi-link `o` is armed, the status bar shows the open prompt.
    // The URLs themselves now live in the panel below the
    // card, so the status bar only carries the "press a number" instruction.
    // Priority: search (handled above) > status message > armed prompt > legend.
    let armed_prompt = if state.status_message.is_none()
        && state.screen == Screen::Checklist
        && state.pending_open_link
    {
        let n = state
            .current_item()
            .map_or(0, |item| item.link_urls().len());
        Some(format!("press 1–{n} to open · esc cancels"))
    } else {
        None
    };
    let text = state
        .status_message
        .as_deref()
        .or(armed_prompt.as_deref())
        .unwrap_or(legend);

    // Failure / "nothing happened" messages are shown in error red so problems
    // stand out from passive confirmations (this supersedes the earlier
    // file-deleted-only case, which now sets an error message like the rest).
    // The flag only applies to a real status message, never the armed prompt or
    // legend, so a message that expired but left the flag set can't mis-color.
    let line = if state.status_is_error && state.status_message.is_some() {
        Line::raw(text).fg(state.palette.error)
    } else {
        Line::raw(text)
    };

    frame.render_widget(Paragraph::new(line), area);
}
