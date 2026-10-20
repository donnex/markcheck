mod cards;
#[cfg(test)]
pub(crate) use cards::HELP_SECTIONS;
mod overview;
mod statusbar;

use std::borrow::Cow;
use std::time::{Duration, SystemTime};

use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::model::{AppState, IconSet, ItemKind, Overlay, Palette, TaskState};

/// The scrollbar thumb glyph: a soft shaded block (`▓`) rather than the chunky
/// default full block, with `▲`/`▼` end arrows and the box border showing
/// through as the track. The thumb glyph is also asserted by the tests.
const SCROLLBAR_THUMB: &str = "▓";

/// Draws a vertical scrollbar over the right border of `area` when the
/// content overflows. Shared by the cards, the overview and the help
/// overlay.
fn render_scrollbar(frame: &mut Frame, area: Rect, total: usize, viewport: usize, position: usize) {
    if total <= viewport || area.height < 3 {
        return;
    }
    // ratatui puts the thumb at the bottom only when `position == content_length
    // - 1`, but a scroll offset maxes out at `total - viewport`. So the content
    // length passed here is the number of scroll *positions* (`total - viewport +
    // 1`), which makes the last item land the thumb flush at the bottom.
    let mut sb = ScrollbarState::new(total - viewport + 1)
        .viewport_content_length(viewport)
        .position(position);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(None)
            .thumb_symbol(SCROLLBAR_THUMB),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut sb,
    );
}

const OVERVIEW_MIN_WIDTH: u16 = 60;
const UPDATE_LABEL_MIN_WIDTH: u16 = 80;
/// The columns the title bar guarantees the filename (or its full length,
/// if shorter) before the document title gets the rest.
const FILENAME_MIN_WIDTH: usize = 24;
const UPDATE_LABEL_LIFETIME: Duration = Duration::from_secs(120);
/// The update tag's relative time is floored to this granularity so it
/// changes at most this often, instead of ticking every render.
const UPDATE_LABEL_STEP_SECS: u64 = 5;
/// Cap the whole UI at this width and center it, so on very wide terminals
/// the cards and overview don't stretch into unreadably long lines.
const MAX_CONTENT_WIDTH: u16 = 120;
/// Blank columns between the cards area and the overview list.
const OVERVIEW_GAP: u16 = 2;

pub fn render(frame: &mut Frame, state: &mut AppState) {
    // Cap and center the working area on wide terminals; narrower
    // terminals use their full width unchanged.
    let full = frame.area();
    let [area] = Layout::horizontal([Constraint::Max(MAX_CONTENT_WIDTH)])
        .flex(Flex::Center)
        .areas(full);

    // The overview already shows every list, so on wide terminals the list
    // tabs would just duplicate it; the tabs get their own row below the title
    // bar only when the overview is hidden and there's more than one list.
    let overview_shown = area.width >= OVERVIEW_MIN_WIDTH;
    let show_tab_row = !overview_shown && state.document.lists.len() > 1;

    let mut rows = vec![Constraint::Length(1)]; // title
    if show_tab_row {
        rows.push(Constraint::Length(1)); // list tabs
    }
    rows.push(Constraint::Length(1)); // progress bar
    rows.push(Constraint::Min(0)); // main
    rows.push(Constraint::Length(statusbar::height(
        state,
        area.width,
        area.height,
    ))); // status
    let chunks = Layout::vertical(rows).split(area);

    let mut next = 0;
    let title_area = chunks[next];
    next += 1;
    let tab_area = show_tab_row.then(|| {
        let a = chunks[next];
        next += 1;
        a
    });
    let progress_area = chunks[next];
    next += 1;
    let main_area = chunks[next];
    next += 1;
    let status_area = chunks[next];

    render_title_bar(frame, title_area, state);
    if let Some(tab_area) = tab_area {
        render_list_tabs(frame, tab_area, state);
    }
    render_progress_bar(frame, progress_area, state);

    if overview_shown && state.overlay != Some(Overlay::Help) {
        // A fixed-width gap sits between the cards and the overview so the
        // "next" side card doesn't butt up against the list.
        let [cards_area, _gap, overview_area] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(OVERVIEW_GAP),
            Constraint::Length(30),
        ])
        .areas(main_area);
        overview::render(frame, overview_area, state);
        cards::render(frame, cards_area, state);
    } else {
        // No overview panel this frame, so drop any stale click targets.
        state.layout.overview_rows.clear();
        cards::render(frame, main_area, state);
    }

    statusbar::render(frame, status_area, state);
}

/// Filled cell count for a progress bar of `width`, proportional to
/// `done/total` (0 when there's nothing to complete).
fn progress_fill(width: usize, done: usize, total: usize) -> usize {
    (width * done).checked_div(total).unwrap_or(0)
}

/// A one-row completion bar under the title bar: green heavy line for
/// the done portion, a yellow heavy line for started/in-progress tasks,
/// then a dim light line for what remains.
fn render_progress_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let (done, started, total) = state.document.checkbox_progress();
    let width = area.width as usize;
    let done_w = progress_fill(width, done, total);
    let started_w = progress_fill(width, started, total).min(width.saturating_sub(done_w));
    let remaining = width.saturating_sub(done_w).saturating_sub(started_w);
    let line = Line::from(vec![
        Span::styled("━".repeat(done_w), Style::default().fg(state.palette.done)),
        Span::styled(
            "━".repeat(started_w),
            Style::default().fg(state.palette.started),
        ),
        Span::styled(
            "─".repeat(remaining),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_title_bar(frame: &mut Frame, area: Rect, state: &AppState) {
    let file_name = state
        .document
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".to_string());

    // Hidden on narrow terminals, where the status bar still announces reloads.
    let now = SystemTime::now();
    let update_label = update_label(state.last_update_at, now)
        .filter(|_| area.width >= UPDATE_LABEL_MIN_WIDTH)
        .map(|label| format!("{} {label}", state.icons.update));
    // `{icon} git` shows for the whole session while git-sync is active; the
    // "Synced Ns ago" text follows only while recent. The literal "git" is
    // there because the icon alone didn't read as git.
    let sync_section = (state.git_sync.active && area.width >= UPDATE_LABEL_MIN_WIDTH)
        .then(|| sync_section(state, now));
    // Color roles from the palette — accent title, dim filename,
    // done/started progress counter.
    let icons = state.icons;
    let (done, total) = state.document.checkbox_stats();
    // The counter fades yellow → green with completion, resolved per
    // color depth.
    let counter_style = state.palette.progress_color(done, total);
    // Skipped tasks are outside done/total, so say how many were set aside.
    let skipped = state.document.skipped_count();
    let counter_text = if skipped > 0 {
        format!(" {} {done}/{total} (+{skipped} skipped)", icons.done)
    } else {
        format!(" {} {done}/{total}", icons.done)
    };

    let separator = Span::styled(" │ ", Style::default().add_modifier(Modifier::DIM));

    // Right-hand cluster: tags, then the counter outermost so it never moves.
    // Built as strings first so the width can be measured.
    let mut right_parts: Vec<(String, Style)> = Vec::new();
    if let Some(update_label) = update_label {
        right_parts.push((
            update_label,
            Style::default()
                .fg(state.palette.started)
                .add_modifier(Modifier::DIM),
        ));
    }
    if let Some(sync_section) = sync_section {
        right_parts.push(sync_section);
    }

    let right_width = display_width(&counter_text) as u16
        + right_parts
            .iter()
            .map(|(text, _)| display_width(text) as u16 + 3) // + " │ "
            .sum::<u16>();
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);

    let mut right_spans = Vec::new();
    for (text, style) in right_parts {
        right_spans.push(Span::styled(text, style));
        right_spans.push(separator.clone());
    }
    right_spans.push(Span::styled(counter_text, counter_style));
    frame.render_widget(Paragraph::new(Line::from(right_spans)), right_area);

    // Document title: the file's first `# H1`, primary; the filename is
    // kept as a dim secondary. With no H1, a title derived from the file
    // stem stands in, styled the same, since a missing H1 is not an error.
    let title = match &state.document.title {
        Some(title) => Cow::Borrowed(title.as_str()),
        None => Cow::Owned(crate::scaffold::derive_title(&state.document.file_path)),
    };
    let title_span = Span::styled(
        format!(" {} {title}", icons.file),
        Style::default()
            .fg(state.palette.current)
            .add_modifier(Modifier::BOLD),
    );
    // The filename keeps up to `FILENAME_MIN_WIDTH` columns however long the
    // title is, since it is the only thing naming the open file; the title
    // takes the rest and ends in `…` when cut. One column stays free so the
    // filename never runs into the right-hand cluster.
    let available = (left_area.width as usize).saturating_sub(separator.width() + 1);
    let (title_budget, file_budget) = split_width(
        display_width(&title_span.content),
        display_width(&file_name),
        available,
        FILENAME_MIN_WIDTH,
    );
    let title_span = Span::styled(
        truncate(&title_span.content, title_budget),
        title_span.style,
    );
    let filename_span = Span::styled(
        truncate(&file_name, file_budget),
        Style::default().add_modifier(Modifier::DIM),
    );

    // Left group: just the static identity — title │ filename.
    let spans = vec![title_span, separator, filename_span];
    frame.render_widget(Paragraph::new(Line::from(spans)), left_area);
}

/// Black text on the accent background. `Color::Black` is a deliberate
/// exception to reading colors from the palette: it reads fine on
/// `palette.current` at every color depth. Without color, reverse video.
fn current_tab_pill_style(state: &AppState) -> Style {
    if state.palette.is_mono() {
        return Style::default().add_modifier(Modifier::REVERSED);
    }
    Style::default().fg(Color::Black).bg(state.palette.current)
}

/// The `[N] title` tab row, the current list highlighted as a pill.
fn render_list_tabs(frame: &mut Frame, area: Rect, state: &AppState) {
    let titles: Vec<&str> = state
        .document
        .lists
        .iter()
        .map(|s| s.title.as_str())
        .collect();
    let avail = area.width as usize;

    let mut spans = Vec::new();
    // Show every tab, full-length when there's room, truncating only what must
    // be truncated to fit. If even truncated tabs won't fit, fall back
    // to just the current one.
    if let Some(maxes) = fit_tab_titles(&titles, avail) {
        for (index, list) in state.document.lists.iter().enumerate() {
            let label = format!(" [{}] {} ", index + 1, truncate(&list.title, maxes[index]));
            let style = if index == state.current_list_index {
                current_tab_pill_style(state)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
        }
    } else if let Some(list) = state.document.lists.get(state.current_list_index) {
        spans.push(Span::styled(
            format!(
                " [{}] {} ",
                state.current_list_index + 1,
                truncate(&list.title, 20)
            ),
            current_tab_pill_style(state),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Per-tab title budgets so every list tab fits in `avail`: full titles
/// when they fit, otherwise shared proportionally (never below
/// `MIN_TITLE`). `None` falls back to the current tab alone.
fn fit_tab_titles(titles: &[&str], avail: usize) -> Option<Vec<usize>> {
    const TAB_DECORATION: usize = 7; // " [n] " brackets, spaces, separator
    const MIN_TITLE: usize = 3;

    let n = titles.len();
    let fixed = TAB_DECORATION * n;
    let title_budget = avail.checked_sub(fixed)?;

    let lengths: Vec<usize> = titles.iter().map(|t| display_width(t)).collect();
    let total_len: usize = lengths.iter().sum();
    if total_len <= title_budget {
        return Some(lengths); // everything fits at full length
    }
    if title_budget < MIN_TITLE * n {
        return None; // not enough room for meaningful truncated tabs
    }
    let mut shares: Vec<usize> = lengths
        .iter()
        .map(|&len| (title_budget * len / total_len).max(MIN_TITLE))
        .collect();
    // The `MIN_TITLE` floor can push the total over budget; take the excess
    // back from the largest shares.
    let mut overflow = shares.iter().sum::<usize>().saturating_sub(title_budget);
    while overflow > 0 {
        let (largest, _) = shares
            .iter()
            .enumerate()
            .filter(|&(_, &share)| share > MIN_TITLE)
            .max_by_key(|&(_, &share)| share)
            .expect("overflow implies at least one share above MIN_TITLE");
        shares[largest] -= 1;
        overflow -= 1;
    }
    Some(shares)
}

/// Splits `total` columns between two texts `first` and `second` cells wide
/// sharing one row. Both keep their full length when they fit;
/// otherwise `second` keeps at least `second_floor` columns (or its own
/// length, if shorter) and `first` takes the rest, so neither text can push
/// the other off the row.
fn split_width(first: usize, second: usize, total: usize, second_floor: usize) -> (usize, usize) {
    if first + second <= total {
        return (first, second);
    }
    let second_budget = second.min(second_floor.max(total.saturating_sub(first)).min(total));
    (total - second_budget, second_budget)
}

/// Cuts `text` to at most `max_width` display cells, ending in `…` when cut.
fn truncate(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        String::new()
    } else if display_width(text) <= max_width {
        text.to_string()
    } else {
        let mut used = 0;
        let end = text
            .char_indices()
            .find_map(|(i, ch)| {
                used += char_width(ch);
                (used > max_width - 1).then_some(i)
            })
            .unwrap_or(text.len());
        format!("{}…", &text[..end])
    }
}

/// Display width in terminal cells. A wide (e.g. East Asian) glyph takes two
/// cells, so anything that wraps, pads or truncates to fit a row measures
/// with this rather than a character count.
fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// Display width of a single character, without allocating —
/// `char::encode_utf8` writes into a stack buffer.
fn char_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    display_width(ch.encode_utf8(&mut buf))
}

/// The column interval a tab in a command advances to on a card.
const TAB_WIDTH: usize = 4;

/// `text` with every tab expanded to spaces up to the next `TAB_WIDTH` stop,
/// counted from the start of `text`. The terminal never draws a tab (ratatui
/// drops control characters), so commands are expanded for display only;
/// the copied text keeps the real tabs.
fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + TAB_WIDTH);
    let mut col = 0;
    for ch in text.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += char_width(ch);
        }
    }
    Cow::Owned(out)
}

/// The byte index splitting off the longest prefix of `text` that fits in
/// `width` cells — but always at least one character, so a glyph wider than
/// the whole budget still makes progress instead of looping forever.
fn split_at_width(text: &str, width: usize) -> usize {
    let mut used = 0;
    for (i, ch) in text.char_indices() {
        used += char_width(ch);
        if used > width && i > 0 {
            return i;
        }
    }
    text.len()
}

/// A relative "time since X" label for the update and sync tags, `None`
/// once `UPDATE_LABEL_LIFETIME` passes. Floored to `UPDATE_LABEL_STEP_SECS`
/// so it changes at most every few seconds.
fn relative_label(verb: &str, at: Option<SystemTime>, now: SystemTime) -> Option<String> {
    let at = at?;
    let elapsed = now.duration_since(at).ok()?;
    if elapsed >= UPDATE_LABEL_LIFETIME {
        return None;
    }
    let secs = (elapsed.as_secs() / UPDATE_LABEL_STEP_SECS) * UPDATE_LABEL_STEP_SECS;
    if secs < UPDATE_LABEL_STEP_SECS {
        Some(format!("{verb} just now"))
    } else if secs < 60 {
        Some(format!("{verb} {secs}s ago"))
    } else {
        Some(format!("{verb} {}m ago", secs / 60))
    }
}

/// When `relative_label` for `at` next reads differently: the next
/// `UPDATE_LABEL_STEP_SECS` boundary, or its disappearance at
/// `UPDATE_LABEL_LIFETIME`. `None` once it's gone for good.
fn next_relative_label_change(at: Option<SystemTime>, now: SystemTime) -> Option<SystemTime> {
    let at = at?;
    let Ok(elapsed) = now.duration_since(at) else {
        return Some(at);
    };
    if elapsed >= UPDATE_LABEL_LIFETIME {
        return None;
    }
    let step = UPDATE_LABEL_STEP_SECS;
    let next = Duration::from_secs((elapsed.as_secs() / step + 1) * step);
    Some(at + next.min(UPDATE_LABEL_LIFETIME))
}

/// When the title bar's time-based labels next change on their own, so the
/// event loop can skip redrawing an unchanged screen until then. `None` when
/// nothing on screen is counting.
pub fn next_label_change(state: &AppState, now: SystemTime) -> Option<SystemTime> {
    let update = next_relative_label_change(state.last_update_at, now);
    let sync = next_relative_label_change(state.git_sync.last_at, now);
    update.into_iter().chain(sync).min()
}

/// Whether a terminal event can change what's on screen. Pointer motion
/// (reported while mouse capture is on) and key releases do nothing, so the
/// event loop skips the redraw for them.
pub fn event_needs_redraw(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind == KeyEventKind::Press,
        Event::Mouse(mouse) => mouse.kind != MouseEventKind::Moved,
        _ => true,
    }
}

/// "Update" covers both our own writes and external reloads.
fn update_label(last_update_at: Option<SystemTime>, now: SystemTime) -> Option<String> {
    relative_label("Updated", last_update_at, now)
}

/// The title bar's git-sync section and its style, in order of precedence:
/// unsynced (error-colored, until a later sync lands), syncing, recently
/// synced, or just the bare indicator.
fn sync_section(state: &AppState, now: SystemTime) -> (String, Style) {
    let prefix = format!("{} git", state.icons.sync);
    let palette = &state.palette;
    if let Some(problem) = state.git_sync.problem {
        return (
            format!("{prefix} · {problem}"),
            Style::default().fg(palette.error),
        );
    }
    if state.git_sync.busy {
        return (
            format!("{prefix} · syncing…"),
            Style::default()
                .fg(palette.started)
                .add_modifier(Modifier::DIM),
        );
    }
    if state.git_sync.waiting {
        return (
            format!("{prefix} · pending"),
            Style::default()
                .fg(palette.started)
                .add_modifier(Modifier::DIM),
        );
    }
    // Green (`palette.done`) rather than the update tag's `palette.started`,
    // so a completed sync reads as a distinct, successful outcome.
    let healthy = Style::default()
        .fg(palette.done)
        .add_modifier(Modifier::DIM);
    match sync_label(state.git_sync.last_at, now) {
        Some(label) => (format!("{prefix} · {label}"), healthy),
        None => (prefix, healthy),
    }
}

/// A completed background git-sync commit+push.
fn sync_label(last_sync_at: Option<SystemTime>, now: SystemTime) -> Option<String> {
    relative_label("Synced", last_sync_at, now)
}

/// An item's state glyph and color, the one mapping every view draws from:
/// done, started, pending (no color of its own), or for a note the note glyph
/// tinted by `info_state`, its sub-list's aggregate state, green when all
/// done, yellow when under way, otherwise note blue.
fn state_marker(
    kind: ItemKind,
    info_state: Option<TaskState>,
    icons: IconSet,
    palette: Palette,
) -> (&'static str, Option<Color>) {
    let glyph = match kind {
        ItemKind::Checkbox(TaskState::Done) => icons.done,
        ItemKind::Checkbox(TaskState::Started) => icons.started,
        ItemKind::Checkbox(TaskState::NotStarted) => icons.pending,
        ItemKind::Checkbox(TaskState::Skipped) => icons.skipped,
        ItemKind::DisplayOnly => icons.note,
    };
    (glyph, marker_color(kind, info_state, palette))
}

/// [`state_marker`]'s color alone.
fn marker_color(kind: ItemKind, info_state: Option<TaskState>, palette: Palette) -> Option<Color> {
    match kind {
        ItemKind::Checkbox(TaskState::Done) => Some(palette.done),
        ItemKind::Checkbox(TaskState::Started) => Some(palette.started),
        ItemKind::Checkbox(TaskState::NotStarted | TaskState::Skipped) => None,
        ItemKind::DisplayOnly => Some(match info_state {
            Some(TaskState::Done) => palette.done,
            Some(TaskState::Started) => palette.started,
            _ => palette.note,
        }),
    }
}

/// `color` as a foreground, or the plain style for none.
fn fg(color: Option<Color>) -> Style {
    color.map_or_else(Style::default, |color| Style::default().fg(color))
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "golden_tests.rs"]
mod golden_tests;
