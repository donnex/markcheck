mod cards;
mod overview;
mod statusbar;

use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::model::AppState;

/// The scrollbar thumb glyph: a soft shaded block (`▓`) rather than the chunky
/// default full block, with `▲`/`▼` end arrows and the box border showing
/// through as the track. The thumb glyph is also asserted by the tests.
const SCROLLBAR_THUMB: &str = "▓";

/// Draws a vertical scrollbar over the right border of `area` (between the
/// top/bottom borders, so it slides along the existing border with `▲`/`▼`
/// arrows at the ends) when the content overflows the viewport. A no-op
/// when everything fits or the area is too small. Shared by the cards, the
/// overview, and the help overlay.
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
    rows.push(Constraint::Length(1)); // status
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

    if overview_shown {
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
        state.overview_rows.clear();
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

    // The update tag: hidden on narrow terminals where it would
    // clip the left content; the status bar message still announces reloads
    // there. Prefixed with the refresh icon instead of `[ ]` brackets —
    // the ` │ ` separator and the dim-yellow style already set it apart from
    // the persistent info. Lives in the right-hand cluster, not next
    // to the filename — see below.
    let now = SystemTime::now();
    let update_label = update_label(state.last_update_at, now)
        .filter(|_| area.width >= UPDATE_LABEL_MIN_WIDTH)
        .map(|label| format!("{} {label}", state.icons.update));
    // The git-sync section: `{icon} git` is a *persistent*
    // "git-sync is on for this session" indicator, shown the whole time
    // `state.git_sync.active` is set (regardless of whether anything has
    // synced yet) — the literal "git" text is there because the icon alone
    // couldn't be trusted to read as "this is about git" (a prior
    // icon-only attempt didn't land). The relative "Synced Ns ago"
    // text appends after it only while a completed sync is still recent
    // (same mechanism/width gate as the update tag above) and drops away
    // again once it's stale, leaving just `{icon} git`. Absent entirely —
    // no icon, no text, no gap — when git-sync wasn't requested or the file
    // isn't in a repo.
    let sync_section = (state.git_sync.active && area.width >= UPDATE_LABEL_MIN_WIDTH).then(|| {
        match sync_label(state.git_sync.last_at, now) {
            Some(label) => format!("{} git · {label}", state.icons.sync),
            None => format!("{} git", state.icons.sync),
        }
    });

    // Color roles from the palette — accent title, dim filename,
    // done/started progress counter.
    let icons = state.icons;
    let (done, total) = state.document.checkbox_stats();
    // The counter fades dim-gray → yellow → green with completion,
    // resolved per color depth (same ramp the overview counter uses).
    let counter_style = state.palette.progress_color(done, total);
    let counter_text = format!(" {} {done}/{total}", icons.done);

    let separator = Span::styled(" │ ", Style::default().add_modifier(Modifier::DIM));

    // Right-hand cluster: everything *about the file's current
    // state* — recency, git-sync, progress — reads together on the right,
    // rather than the update/sync tags trailing the filename on the left.
    // The counter is still the outermost (rightmost) element, so it never
    // shifts position on screen; the tags before it grow/shrink the
    // cluster's width as they come and go, but that movement stays
    // contained to their left, never touching the counter's own position
    // or the static title/filename group. Built as plain strings first so
    // the combined width (tags + separators + counter) can be measured
    // before laying out the row.
    let mut right_parts: Vec<(String, Style)> = Vec::new();
    if let Some(update_label) = update_label {
        right_parts.push((
            update_label,
            Style::default()
                .fg(state.palette.started)
                .add_modifier(Modifier::DIM),
        ));
    }
    // Green (`palette.done`) rather than the update tag's `palette.started`,
    // so a completed sync reads as a distinct, successful outcome rather
    // than the update tag's neutral "something changed" note.
    if let Some(sync_section) = sync_section {
        right_parts.push((
            sync_section,
            Style::default()
                .fg(state.palette.done)
                .add_modifier(Modifier::DIM),
        ));
    }

    let right_width = counter_text.chars().count() as u16
        + right_parts
            .iter()
            .map(|(text, _)| text.chars().count() as u16 + 3) // + " │ "
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
    // kept as a dim secondary. With no H1, a bold-red placeholder stands in.
    let title_span = match &state.document.title {
        Some(title) => Span::styled(
            format!(" {} {title}", icons.file),
            Style::default()
                .fg(state.palette.current)
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::styled(
            format!(" {} Missing document title", icons.file),
            Style::default()
                .fg(state.palette.error)
                .add_modifier(Modifier::BOLD),
        ),
    };
    let filename_span = Span::styled(file_name, Style::default().add_modifier(Modifier::DIM));

    // Left group: just the static identity — title │ filename.
    // Nothing dynamic lives here any more; the list tabs are their own row
    // on narrow terminals, so they don't either.
    let spans = vec![title_span, separator, filename_span];
    frame.render_widget(Paragraph::new(Line::from(spans)), left_area);
}

/// The list tabs as a full-width strip: `[n] Title` per list, the
/// current one a black-on-accent pill, the rest dim. Rendered in its
/// own row below the title bar on narrow terminals — where the overview is
/// hidden — so it isn't crammed into the title bar. On wide terminals
/// the overview lists the lists instead, so this isn't drawn.
/// Style for the current list's tab pill: black text on the accent
/// background. `Color::Black` here is a deliberate, narrow exception to the
/// "read every color from `state.palette`" rule — it isn't a
/// semantic UI color the way `Palette`'s fields are, just the contrasting
/// text color for a solid-accent pill, and plain black reads fine against
/// `palette.current` at all three color depths (`Palette::basic`/`256`/
/// `truecolor` all pick light/vivid hues for `current`). Not worth adding a
/// dedicated `Palette` field for the one call site that needs it.
fn current_tab_pill_style(state: &AppState) -> Style {
    Style::default().fg(Color::Black).bg(state.palette.current)
}

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

/// Per-tab title character budgets so every list tab is shown.
/// Each tab costs `TAB_DECORATION + title` columns; when the untruncated
/// titles fit in `avail` they're shown full, otherwise the title budget is
/// shared proportionally to title length (never below `MIN_TITLE`). Returns
/// `None` when even minimally-truncated tabs won't fit, signalling the
/// current-tab-only fallback.
fn fit_tab_titles(titles: &[&str], avail: usize) -> Option<Vec<usize>> {
    const TAB_DECORATION: usize = 7; // " [n] " brackets, spaces, separator
    const MIN_TITLE: usize = 3;

    let n = titles.len();
    if n == 0 {
        return Some(vec![]);
    }
    let fixed = TAB_DECORATION * n;
    let title_budget = avail.checked_sub(fixed)?;

    let lengths: Vec<usize> = titles.iter().map(|t| t.chars().count()).collect();
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
    // The `.max(MIN_TITLE)` floor above can push the total over `title_budget`
    // when one title is short enough that its proportional share would
    // otherwise fall under it; claw the excess back from the largest shares
    // (never below MIN_TITLE, which `title_budget >= MIN_TITLE * n` above
    // guarantees is always possible) so the total never exceeds what was
    // actually determined to fit.
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

fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        String::new()
    } else if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars - 1).collect();
        format!("{truncated}…")
    }
}

/// A relative "time since X" label shared by the update tag and the
/// sync tag: `{verb} just now` under 5s, `{verb} Ns ago` under a
/// minute, `{verb} Nm ago` under two, and `None` at/after
/// `UPDATE_LABEL_LIFETIME` (or when `at` is `None`). The seconds are floored
/// to `UPDATE_LABEL_STEP_SECS` so the value changes at most every ~5s rather
/// than every render; the idle event loop redraws several times a
/// second, so it still refreshes without a timer.
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

/// "Update" covers both our own writes and external reloads.
fn update_label(last_update_at: Option<SystemTime>, now: SystemTime) -> Option<String> {
    relative_label("Updated", last_update_at, now)
}

/// A completed background git-sync commit+push.
fn sync_label(last_sync_at: Option<SystemTime>, now: SystemTime) -> Option<String> {
    relative_label("Synced", last_sync_at, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fill_is_proportional_and_safe() {
        assert_eq!(progress_fill(100, 3, 4), 75);
        assert_eq!(progress_fill(100, 0, 4), 0);
        assert_eq!(progress_fill(100, 4, 4), 100);
        assert_eq!(progress_fill(100, 0, 0), 0, "no divide-by-zero");
    }

    #[test]
    fn update_label_none_when_never_updated() {
        assert_eq!(update_label(None, SystemTime::now()), None);
    }

    #[test]
    fn update_label_says_just_now_under_five_seconds() {
        let at = SystemTime::now();
        let now = at + Duration::from_secs(3);
        assert_eq!(
            update_label(Some(at), now).as_deref(),
            Some("Updated just now")
        );
    }

    #[test]
    fn update_label_floors_seconds_to_five_second_steps() {
        // 32s must display as "30s", not "32s", so the tag doesn't tick
        // every second.
        let at = SystemTime::now();
        let now = at + Duration::from_secs(32);
        assert_eq!(
            update_label(Some(at), now).as_deref(),
            Some("Updated 30s ago")
        );
    }

    #[test]
    fn update_label_shows_minutes_over_a_minute() {
        let at = SystemTime::now();
        let now = at + Duration::from_secs(75);
        assert_eq!(
            update_label(Some(at), now).as_deref(),
            Some("Updated 1m ago")
        );
    }

    #[test]
    fn tabs_shown_full_length_when_they_fit() {
        // Plenty of room: both titles kept at their full length.
        let maxes = fit_tab_titles(&["Alpha", "Beta"], 100).unwrap();
        assert_eq!(maxes, vec![5, 4]);
    }

    #[test]
    fn tabs_truncated_proportionally_when_cramped() {
        // Tight budget forces truncation but every tab still appears.
        let maxes = fit_tab_titles(&["LongListTitle", "Short"], 30).unwrap();
        assert_eq!(maxes.len(), 2);
        assert!(maxes.iter().all(|&m| m >= 3), "each tab keeps a minimum");
        assert!(maxes[0] < 16, "longer title is actually truncated");
    }

    #[test]
    fn tabs_proportional_shares_never_exceed_the_title_budget() {
        // Regression: a very short title's proportional share floors up to
        // MIN_TITLE while the rest keep their full proportional (rounded
        // down) share, which used to let the shares sum above what
        // fit_tab_titles itself determined would fit.
        let titles = ["x".repeat(19), "x".repeat(2)];
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let maxes = fit_tab_titles(&titles, 34).unwrap();
        const TAB_DECORATION: usize = 7;
        let title_budget = 34 - TAB_DECORATION * titles.len();
        assert!(
            maxes.iter().sum::<usize>() <= title_budget,
            "shares {maxes:?} must not exceed the {title_budget}-column budget"
        );
        assert!(maxes.iter().all(|&m| m >= 3), "each tab keeps a minimum");
    }

    #[test]
    fn truncate_zero_budget_returns_empty_not_ellipsis_alone() {
        // A zero-width budget (reachable e.g. via section_divider_line's
        // saturating_sub on a very narrow panel) must return nothing, not a
        // single "…" character that itself exceeds the requested budget.
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_nonzero_budget_still_fits_within_it() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn tabs_fallback_when_no_room() {
        // Too little width for even minimal tabs → current-tab-only fallback.
        assert_eq!(fit_tab_titles(&["Alpha", "Beta", "Gamma"], 10), None);
    }

    #[test]
    fn update_label_hidden_after_two_minutes() {
        let at = SystemTime::now();
        let now = at + Duration::from_secs(121);
        assert_eq!(update_label(Some(at), now), None);
    }

    #[test]
    fn update_label_still_shown_just_under_two_minutes() {
        let at = SystemTime::now();
        let now = at + Duration::from_secs(119);
        assert!(update_label(Some(at), now).is_some());
    }

    #[test]
    fn sync_label_says_synced_and_shares_update_labels_lifetime() {
        let at = SystemTime::now();
        assert_eq!(
            sync_label(Some(at), at + Duration::from_secs(3)).as_deref(),
            Some("Synced just now")
        );
        assert_eq!(
            sync_label(Some(at), at + Duration::from_secs(75)).as_deref(),
            Some("Synced 1m ago")
        );
        assert_eq!(sync_label(Some(at), at + Duration::from_secs(121)), None);
        assert_eq!(sync_label(None, at), None);
    }

    // --- Buffer-level rendering tests via ratatui's TestBackend.
    // They assert on content, not styling, so they survive cosmetic
    // tweaks. render() is the public entry that covers cards, overview,
    // statusbar, and the title bar together.

    use crate::model::{
        Document, IconSet, Item, ItemKind, List, OverviewTarget, Screen, SubHeading, TaskState,
    };
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn checkbox(line: usize, text: &str, completed: bool) -> Item {
        Item {
            line_number: line,
            depth: 0,
            section: vec![],
            display_text: text.to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::Checkbox(if completed {
                TaskState::Done
            } else {
                TaskState::NotStarted
            }),
        }
    }

    fn display_only(line: usize, text: &str) -> Item {
        Item {
            line_number: line,
            depth: 0,
            section: vec![],
            display_text: text.to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::DisplayOnly,
        }
    }

    /// Foreground color of the info (`▪`) marker in the overview panel — the
    /// right-hand 30 columns — for the first such cell found. Isolates the
    /// overview marker from any note glyph a card might also draw.
    fn overview_note_marker_fg(state: &mut AppState, width: u16, height: u16) -> Option<Color> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        for y in 0..height {
            for x in (width.saturating_sub(30))..width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "▪" {
                    return Some(cell.fg);
                }
            }
        }
        None
    }

    #[test]
    fn overview_info_parent_marker_reflects_child_progress() {
        // An info parent's overview marker borrows task accents from
        // its sub-list — plain note blue with nothing begun, started, then done.
        let make = |a: TaskState, b: TaskState| {
            let mut parent = display_only(1, "prep");
            parent.depth = 0;
            let mut c1 = checkbox(2, "one", false);
            c1.depth = 1;
            c1.kind = ItemKind::Checkbox(a);
            let mut c2 = checkbox(3, "two", false);
            c2.depth = 1;
            c2.kind = ItemKind::Checkbox(b);
            state_with(vec![List {
                title: "L".to_string(),
                banner: None,
                items: vec![parent, c1, c2],
            }])
        };

        let pal = make(TaskState::NotStarted, TaskState::NotStarted).palette;
        let mut none = make(TaskState::NotStarted, TaskState::NotStarted);
        assert_eq!(overview_note_marker_fg(&mut none, 100, 12), Some(pal.note));
        let mut started = make(TaskState::Done, TaskState::NotStarted);
        assert_eq!(
            overview_note_marker_fg(&mut started, 100, 12),
            Some(pal.started)
        );
        let mut done = make(TaskState::Done, TaskState::Done);
        assert_eq!(overview_note_marker_fg(&mut done, 100, 12), Some(pal.done));
    }

    #[test]
    fn overview_marks_info_items_with_the_note_glyph_not_a_pending_circle() {
        // A display-only row used to fall through to the pending marker
        // (☐), identical to a not-started task. It now shows the note glyph (▪),
        // so information reads apart from a step.
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                checkbox(1, "a real task", false),
                display_only(2, "just a note"),
            ],
        }]);
        let rows = buffer_rows(&mut state, 100, 12);
        let note_row = rows
            .iter()
            .find(|r| r.contains("just a note"))
            .expect("info row is rendered");
        assert!(
            note_row.contains('▪'),
            "info row carries the note glyph: {note_row:?}"
        );
        assert!(
            !note_row.contains('☐'),
            "info row is not marked like a pending task: {note_row:?}"
        );
    }

    fn state_with(lists: Vec<List>) -> AppState {
        let document = Document {
            file_path: PathBuf::from("render-test.md"),
            // A short H1 title keeps width-sensitive tab tests stable (the
            // long "Missing document title" placeholder would eat columns).
            title: Some("Doc".to_string()),
            has_default_list: false,
            lists,
            raw_lines: vec![],
        };
        let mut state = AppState::new(document);
        // Plain Unicode so assertions don't depend on Nerd Font glyphs.
        state.icons = IconSet::unicode();
        state
    }

    fn buffer_text(state: &mut AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }

    #[test]
    fn link_panel_shows_the_url_below_the_card_not_on_it() {
        // The URL is drawn in a panel below the card (not on the
        // card, not in the status bar). Single link → `→ <url>`.
        let mut item = checkbox(1, "see runbook", false);
        item.body = vec![crate::model::BodySpan::Link {
            text: "runbook".to_string(),
            url: "https://example.com/runbook".to_string(),
        }];
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let rows = buffer_rows(&mut state, 100, 24);
        assert!(
            rows.iter()
                .any(|r| r.contains("→ https://example.com/runbook")),
            "panel row shows the URL: {rows:?}"
        );
        // Not inside the card box, and not in the status bar (last row).
        let card = rows
            .iter()
            .filter(|r| r.contains('┃'))
            .cloned()
            .collect::<String>();
        assert!(
            !card.contains("example.com"),
            "URL not on the card: {card:?}"
        );
        assert!(
            !rows.last().unwrap().contains("example.com"),
            "URL not in the status bar: {:?}",
            rows.last()
        );
    }

    #[test]
    fn card_shows_link_hint_when_panel_has_no_room() {
        // On a terminal too short for the below-card link panel, the card
        // itself carries a compact hint so `o` isn't a blind guess.
        let mut item = checkbox(1, "see runbook", false);
        item.body = vec![crate::model::BodySpan::Link {
            text: "runbook".to_string(),
            url: "https://example.com/runbook".to_string(),
        }];
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![item],
        }]);
        // Narrow (single-card layout) and just tall enough for the card to
        // fill the whole main area, leaving no room for the panel below it.
        let rows = buffer_rows(&mut state, 60, 11);
        assert!(
            rows.iter().any(|r| r.contains("→ link")),
            "card shows a link hint: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("example.com")),
            "the panel itself still has no room to show the full URL: {rows:?}"
        );
    }

    #[test]
    fn link_panel_lists_multiple_urls_and_status_prompts_while_arming() {
        // Several links are listed one-per-line in the panel; on
        // `o`, the panel keeps them and the status bar shows the open prompt.
        let mut item = checkbox(1, "links", false);
        item.body = vec![
            crate::model::BodySpan::Link {
                text: "docs".to_string(),
                url: "https://example.com/a".to_string(),
            },
            crate::model::BodySpan::Link {
                text: "wiki".to_string(),
                url: "https://example.com/b".to_string(),
            },
        ];
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![item],
        }]);
        state.handle_key(KeyCode::Char('o'));
        let rows = buffer_rows(&mut state, 120, 26);
        // Both URLs shown on separate panel rows.
        assert!(
            rows.iter().any(|r| r.contains("[1] https://example.com/a")),
            "panel lists link 1: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("[2] https://example.com/b")),
            "panel lists link 2: {rows:?}"
        );
        // The open prompt sits in the status bar.
        let status = rows.last().unwrap();
        assert!(
            status.contains("1") && status.to_lowercase().contains("esc"),
            "status bar shows the open prompt: {status:?}"
        );
    }

    #[test]
    fn overview_pins_the_sub_section_header_when_its_divider_scrolls_off() {
        // Deep inside a long sub-section, its `── divider` scrolls above
        // the panel, so the deepest active sub-heading is pinned on the first
        // visible overview row; unscrolled, nothing is pinned.
        let mut items: Vec<Item> = vec![checkbox(1, "intro task", false)];
        for n in 2..=30 {
            let mut it = checkbox(n, &format!("preflight {n}"), false);
            it.section = vec![crate::model::SubHeading {
                level: 3,
                text: "Pre-flight checks".to_string(),
            }];
            items.push(it);
        }
        let mut state = state_with(vec![List {
            title: "Deploy".to_string(),
            banner: None,
            items,
        }]);
        let (w, h) = (100, 12);

        let overview_top = |rows: &[String]| -> usize {
            rows.iter().position(|r| r.contains("Overview")).unwrap()
        };

        // Cursor at the top: the divider is on-screen, so nothing is pinned —
        // the first content row is the intro item, not the header.
        let unscrolled = buffer_rows(&mut state, w, h);
        let top = overview_top(&unscrolled);
        assert!(
            !unscrolled[top + 1].contains("Pre-flight checks"),
            "unscrolled: header not pinned, got {:?}",
            unscrolled[top + 1]
        );

        // Cursor deep in the group: its divider has scrolled off, so the header
        // is pinned on the first visible overview row.
        state.current_item_index = 28;
        let scrolled = buffer_rows(&mut state, w, h);
        let top = overview_top(&scrolled);
        assert!(
            scrolled[top + 1].contains("Pre-flight checks"),
            "scrolled: header pinned on the top row, got {:?}",
            scrolled[top + 1]
        );
    }

    #[test]
    fn overview_sticky_header_pins_the_full_path_and_list_title() {
        // Full-path pinning — the list title (H2) plus every active H3+
        // level stay pinned, outermost first, when their rows scroll off.
        let outer = crate::model::SubHeading {
            level: 3,
            text: "Outer".to_string(),
        };
        let inner = crate::model::SubHeading {
            level: 4,
            text: "Inner".to_string(),
        };
        let mut deploy_items = vec![checkbox(1, "deploy intro", false)];
        for n in 2..=30 {
            // A distinctive word placed well to the right of a short pinned
            // header, so it survives the overwrite and reveals any bleed-through.
            let mut it = checkbox(n, "hold the line steady REDACTED", false);
            it.line_number = n;
            it.section = vec![outer.clone(), inner.clone()];
            deploy_items.push(it);
        }
        let mut state = state_with(vec![
            List {
                title: "Setup".to_string(),
                banner: None,
                items: vec![checkbox(100, "setup one", false)],
            },
            List {
                title: "Deploy".to_string(),
                banner: None,
                items: deploy_items,
            },
        ]);
        // Deep inside list 2's Inner group, so the list title and both dividers
        // have scrolled above the panel.
        state.current_list_index = 1;
        state.current_item_index = 28;
        let rows = buffer_rows(&mut state, 100, 12);
        let top = rows.iter().position(|r| r.contains("Overview")).unwrap();
        assert!(
            rows[top + 1].contains("Deploy"),
            "list title pinned first: {:?}",
            rows[top + 1]
        );
        assert!(
            rows[top + 2].contains("Outer"),
            "H3 pinned second: {:?}",
            rows[top + 2]
        );
        assert!(
            rows[top + 3].contains("Inner"),
            "H4 pinned third: {:?}",
            rows[top + 3]
        );
        // The short pinned list-title row is cleared first, so the item it
        // covers can't bleed through beside it on the same line (readability).
        let title_chars: Vec<char> = rows[top].chars().collect();
        let ov_col = title_chars
            .windows(8)
            .position(|w| w.iter().collect::<String>() == "Overview")
            .unwrap();
        let pinned_overview: String = rows[top + 1].chars().skip(ov_col).collect();
        assert!(
            !pinned_overview.contains("steady"),
            "no covered item text bleeds beside the pinned title: {pinned_overview:?}"
        );
        // Reserve-space: the list renders *below* the three pinned header
        // rows, so an item sits on the next row down rather than being covered.
        let below: String = rows[top + 4].chars().skip(ov_col).collect();
        assert!(
            below.contains("hold"),
            "list content sits below the reserved headers: {below:?}"
        );
    }

    #[test]
    fn overview_scrollbar_thumb_reaches_the_bottom_on_the_last_item() {
        // Regression: the thumb must sit flush above the ▼ arrow when the
        // cursor is on the last item, not stop short (ratatui's content-length
        // vs. scroll-offset gotcha).
        let items: Vec<Item> = (1..=40)
            .map(|n| checkbox(n, &format!("task {n}"), false))
            .collect();
        let last = items.len() - 1;
        let mut state = state_with(vec![List {
            title: "Big".to_string(),
            banner: None,
            items,
        }]);
        let (w, h) = (100, 12);

        let thumb_rows = |rows: &[String]| -> Vec<usize> {
            (0..rows.len())
                .filter(|&y| rows[y].contains(SCROLLBAR_THUMB))
                .collect()
        };
        let arrow_row = |rows: &[String], glyph: char| -> usize {
            (0..rows.len()).find(|&y| rows[y].contains(glyph)).unwrap()
        };

        // Unscrolled: the thumb sits right below the ▲ arrow.
        let top = buffer_rows(&mut state, w, h);
        let up = arrow_row(&top, '▲');
        assert_eq!(*thumb_rows(&top).iter().min().unwrap(), up + 1);

        // On the last item: the thumb sits right above the ▼ arrow.
        state.current_item_index = last;
        let bottom = buffer_rows(&mut state, w, h);
        let down = arrow_row(&bottom, '▼');
        assert_eq!(
            *thumb_rows(&bottom).iter().max().unwrap(),
            down - 1,
            "thumb is flush against the ▼ on the last item"
        );
    }

    /// Like `buffer_text` but preserves row boundaries, so tests can assert on
    /// column-adjacency (e.g. inner padding after a panel border).
    fn buffer_rows(state: &mut AppState, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn sample_state() -> AppState {
        state_with(vec![
            List {
                title: "First".to_string(),
                banner: None,
                items: vec![checkbox(1, "alpha", true), checkbox(2, "beta", false)],
            },
            List {
                title: "Second".to_string(),
                banner: None,
                items: vec![checkbox(3, "gamma", false)],
            },
        ])
    }

    fn sub(level: u8, text: &str) -> SubHeading {
        SubHeading {
            level,
            text: text.to_string(),
        }
    }

    #[test]
    fn sub_section_divider_shows_once_in_overview() {
        // An item carrying a sub-section path draws a `── Text` divider in
        // the overview, above the first item of the group and not repeated.
        let mut before = checkbox(1, "before", false);
        before.section = vec![];
        let mut a = checkbox(2, "under a", false);
        a.section = vec![sub(3, "Deploy steps")];
        let mut b = checkbox(3, "under b", false);
        b.section = vec![sub(3, "Deploy steps")];
        let mut state = state_with(vec![List {
            title: "Section".to_string(),
            banner: None,
            items: vec![before, a, b],
        }]);
        let rows = buffer_rows(&mut state, 120, 30);
        let divider_rows: Vec<&String> = rows
            .iter()
            .filter(|r| r.contains("── Deploy steps"))
            .collect();
        assert_eq!(
            divider_rows.len(),
            1,
            "divider drawn exactly once for the group: {rows:?}"
        );
    }

    #[test]
    fn sub_section_breadcrumb_shows_above_the_card() {
        // The current item's sub-section path renders as a ` › `-joined
        // dim breadcrumb above the card (nested levels joined).
        let mut item = checkbox(1, "task", false);
        item.section = vec![sub(3, "Outer"), sub(4, "Inner")];
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let text = buffer_text(&mut state, 100, 24);
        assert!(
            text.contains("Outer › Inner"),
            "breadcrumb shows the joined path: {text}"
        );
    }

    #[test]
    fn no_breadcrumb_without_a_sub_section() {
        // An item directly under its H2 has no breadcrumb marker.
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![checkbox(1, "task", false)],
        }]);
        let text = buffer_text(&mut state, 100, 24);
        assert!(!text.contains(" › "), "no breadcrumb separator: {text}");
    }

    #[test]
    fn renders_title_counter_and_current_item() {
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("render-test.md"), "filename in title bar");
        assert!(text.contains("1/3"), "done counter");
        assert!(text.contains("alpha"), "first item visible");
        assert!(text.contains("First"), "list name (shown in the overview)");
    }

    #[test]
    fn title_bar_shows_document_title_and_filename() {
        // The H1 title is primary, the filename is kept as secondary.
        let mut state = sample_state();
        state.document.title = Some("My Runbook".to_string());
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("My Runbook"), "H1 title shown");
        assert!(text.contains("render-test.md"), "filename still shown");
    }

    #[test]
    fn title_bar_shows_missing_title_placeholder_in_red_bold() {
        // With no H1, a bold-red placeholder stands in.
        let mut state = sample_state();
        state.document.title = None;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        // Collect the red+bold cells on the title row (row 0).
        let red_bold: String = (0..100)
            .map(|x| &buffer[(x, 0)])
            .filter(|c| {
                c.style().fg == Some(state.palette.error)
                    && c.style().add_modifier.contains(Modifier::BOLD)
            })
            .map(|c| c.symbol())
            .collect();
        assert!(
            red_bold.contains("Missing document title"),
            "placeholder is red + bold: {red_bold:?}"
        );
    }

    /// Row index of the first bold row, in the given `color`, containing
    /// `needle`, or `None`. Used to locate a heading/banner wherever the
    /// centered block places it.
    fn colored_text_row(
        buffer: &ratatui::buffer::Buffer,
        width: u16,
        height: u16,
        color: Color,
        needle: &str,
    ) -> Option<u16> {
        (0..height).find(|&y| {
            let s: String = (0..width)
                .map(|x| &buffer[(x, y)])
                .filter(|c| {
                    c.style().fg == Some(color) && c.style().add_modifier.contains(Modifier::BOLD)
                })
                .map(|c| c.symbol())
                .collect();
            s.contains(needle)
        })
    }

    #[test]
    fn list_heading_sits_one_line_above_the_card() {
        // The current `## H2` title renders in the overview's current-list
        // color (cyan) + bold, above the card with exactly one blank line
        // between them — mirroring the one-row gap below the card before the
        // link panel — and the pair centered together.
        let mut state = sample_state(); // current list "First", no banner
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Search only the cards columns (0..66); the overview panel on the
        // right also shows the list title in the same cyan+bold style.
        let heading_row = colored_text_row(&buffer, 66, 30, state.palette.current, "First")
            .expect("current-list-colored+bold list heading not found");
        let gap: String = (0..66)
            .map(|x| buffer[(x, heading_row + 1)].symbol())
            .collect();
        assert!(
            gap.trim().is_empty(),
            "one blank line between heading and card: {gap:?}"
        );
        let card_top: String = (0..100)
            .map(|x| buffer[(x, heading_row + 2)].symbol())
            .collect();
        assert!(
            card_top.contains('┏') || card_top.contains('━'),
            "card top border one line below the heading: {card_top:?}"
        );
    }

    #[test]
    fn list_heading_suppressed_for_default_list() {
        // No above-cards heading for the synthesized (Default)
        // list (no real H2).
        let mut state = sample_state();
        state.document.has_default_list = true;
        state.current_list_index = 0;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let row2: String = (0..100).map(|x| buffer[(x, 2)].symbol()).collect();
        assert!(
            !row2.contains("First"),
            "no list heading for the default list: {row2:?}"
        );
    }

    #[test]
    fn list_banner_sits_below_heading_and_one_line_above_card() {
        // The banner (yellow+bold sub-header) sits directly below the
        // H2 title with no blank line between them, then one blank line before
        // the card's top border.
        let mut state = state_with(vec![List {
            title: "First".to_string(),
            banner: Some("Do not run on prod".to_string()),
            items: vec![checkbox(1, "alpha", false)],
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // Restrict to the cards columns (0..66) — the overview also shows both
        // the title and the banner in these colors.
        let heading_row = colored_text_row(&buffer, 66, 30, state.palette.current, "First")
            .expect("current-list-colored+bold list heading not found");
        let banner_row =
            colored_text_row(&buffer, 66, 30, state.palette.warning, "Do not run on prod")
                .expect("warning-colored+bold banner not found");
        assert_eq!(
            banner_row,
            heading_row + 1,
            "banner directly below heading with no blank line between them"
        );
        let gap: String = (0..66)
            .map(|x| buffer[(x, banner_row + 1)].symbol())
            .collect();
        assert!(
            gap.trim().is_empty(),
            "one blank line between banner and card: {gap:?}"
        );
        let card_top: String = (0..100)
            .map(|x| buffer[(x, banner_row + 2)].symbol())
            .collect();
        assert!(
            card_top.contains('┏') || card_top.contains('━'),
            "card top border one line below the banner: {card_top:?}"
        );
    }

    #[test]
    fn list_banner_renders_for_title_less_default_list() {
        // The banner shows even when the list has no H2 title above it.
        let mut state = state_with(vec![List {
            title: "(Default)".to_string(),
            banner: Some("Heads up".to_string()),
            items: vec![checkbox(1, "alpha", false)],
        }]);
        state.document.has_default_list = true;
        state.current_list_index = 0;
        let text = buffer_text(&mut state, 100, 30);
        assert!(
            text.contains("Heads up"),
            "banner shown for the default list: {text:?}"
        );
    }

    /// The first buffer row — the title bar.
    fn title_row(state: &mut AppState, width: u16, height: u16) -> String {
        nth_row(state, width, height, 0)
    }

    /// Row `n` (0-indexed) of the rendered buffer as a string.
    fn nth_row(state: &mut AppState, width: u16, height: u16, n: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let start = (n as usize) * width as usize;
        buffer
            .content()
            .iter()
            .skip(start)
            .take(width as usize)
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn tabs_hidden_when_overview_shown() {
        // With the overview visible (wide), the title bar drops the
        // duplicate list tabs; the overview still lists the lists.
        let mut state = sample_state();
        let title = title_row(&mut state, 100, 30);
        assert!(!title.contains("[1]"), "no tab in title bar: {title:?}");
        let full = buffer_text(&mut state, 100, 30);
        assert!(full.contains("[1] First"), "overview lists the list");
    }

    #[test]
    fn tabs_shown_in_dedicated_row_when_overview_hidden() {
        // On a narrow terminal the overview is gone, so the list
        // tabs appear in their own row (row 1) below the title bar — not in
        // the title bar itself.
        let mut state = sample_state();
        assert!(
            !title_row(&mut state, 50, 20).contains("[1]"),
            "tabs are not in the title bar"
        );
        assert!(
            nth_row(&mut state, 50, 20, 1).contains("[1]"),
            "tabs render in the dedicated row below the title bar"
        );
    }

    #[test]
    fn tab_row_absent_for_single_list() {
        // A single-list doc gets no tab row (no wasted row); row 1 is
        // the progress bar, not tabs.
        let mut state = state_with(vec![List {
            title: "Only".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", false)],
        }]);
        assert!(
            !nth_row(&mut state, 50, 20, 1).contains("[1]"),
            "no tab row for a single-list document"
        );
    }

    #[test]
    fn narrow_tab_row_shows_full_titles() {
        // With the whole row to itself, the tab strip no longer truncates
        // the short list titles the way the crammed title bar did.
        let mut state = sample_state();
        let tabs = nth_row(&mut state, 55, 20, 1);
        assert!(tabs.contains("First"), "full first title: {tabs:?}");
        assert!(tabs.contains("Second"), "full second title: {tabs:?}");
    }

    #[test]
    fn title_counter_is_right_aligned() {
        // The done/total counter is pinned to the right edge, separate
        // from the left-aligned title/filename group.
        let mut state = sample_state();
        let row = title_row(&mut state, 100, 30);
        assert!(
            row.trim_end().ends_with("1/3"),
            "counter pinned to the right: {row:?}"
        );
        assert!(row.contains("render-test.md"), "filename present: {row:?}");
        assert!(
            !row.trim_end().ends_with("render-test.md"),
            "filename is not the right-most element (the counter is): {row:?}"
        );
    }

    #[test]
    fn title_counter_stays_right_aligned_when_update_tag_shows() {
        // The counter is the outermost element and the update tag lives
        // in the left group, so the counter stays flush-right either way.
        let mut without = sample_state();
        let row_without = title_row(&mut without, 100, 30);
        assert!(
            row_without.trim_end().ends_with("1/3"),
            "counter flush right without the tag: {row_without:?}"
        );

        let mut with = sample_state();
        with.last_update_at = Some(SystemTime::now());
        let row_with = title_row(&mut with, 100, 30);
        assert!(
            row_with.contains("Updated"),
            "update tag present: {row_with:?}"
        );
        assert!(
            row_with.trim_end().ends_with("1/3"),
            "counter still flush right with the tag: {row_with:?}"
        );
    }

    #[test]
    fn update_tag_uses_refresh_icon_not_brackets() {
        // The update tag is prefixed with the refresh icon (↻ in the
        // Unicode set used by tests) rather than wrapped in [ ] brackets.
        let mut state = sample_state();
        state.last_update_at = Some(SystemTime::now());
        let row = title_row(&mut state, 100, 30);
        assert!(row.contains("Updated"), "tag present: {row:?}");
        assert!(!row.contains("[Updated"), "no bracket wrapping: {row:?}");
        assert!(
            row.contains(IconSet::unicode().update),
            "refresh icon present: {row:?}"
        );
    }

    #[test]
    fn git_sync_icon_persists_in_title_bar_without_a_recent_sync() {
        // The icon itself is a persistent "git-sync is on this
        // session" indicator — shown as soon as git-sync is active, even
        // before anything has actually synced yet.
        let mut state = sample_state();
        state.git_sync.active = true;
        let row = title_row(&mut state, 100, 30);
        assert!(
            row.contains(&format!("{} git", IconSet::unicode().sync)),
            "icon and the literal \"git\" label present with no sync yet: {row:?}"
        );
        assert!(
            !row.contains("Synced"),
            "no relative-time text without a completed sync: {row:?}"
        );
    }

    #[test]
    fn git_sync_icon_and_synced_label_shown_together_when_recent() {
        // Once a sync has actually completed recently, the relative
        // "Synced Ns ago" text appends after the same icon — same section,
        // same separator convention as the update tag — without crowding
        // either the update tag or the counter.
        let mut state = sample_state();
        state.git_sync.active = true;
        state.last_update_at = Some(SystemTime::now());
        state.git_sync.last_at = Some(SystemTime::now());
        let row = title_row(&mut state, 100, 30);
        assert!(row.contains("Updated"), "update tag present: {row:?}");
        assert!(
            row.contains(&format!("{} git · Synced", IconSet::unicode().sync)),
            "icon, \"git\" label, and sync text all present together: {row:?}"
        );
        assert!(
            row.trim_end().ends_with("1/3"),
            "counter still flush right with both tags: {row:?}"
        );
    }

    #[test]
    fn git_sync_icon_hidden_when_git_sync_inactive() {
        // The icon/section requires `git_sync.active` — the authoritative
        // "is this feature on" flag — not just a `git_sync.last_at` timestamp.
        let mut state = sample_state();
        state.git_sync.last_at = Some(SystemTime::now());
        let row = title_row(&mut state, 100, 30);
        assert!(!state.git_sync.active, "off by default");
        assert!(
            !row.contains(IconSet::unicode().sync) && !row.contains("git"),
            "no git-sync icon or label when inactive: {row:?}"
        );
        assert!(
            !row.contains("Synced"),
            "no sync label when inactive: {row:?}"
        );
    }

    #[test]
    fn dynamic_tags_cluster_with_the_counter_not_the_filename() {
        // The update/sync tags used to trail the filename directly on
        // the left; they now group with the done/total counter on the right
        // instead, leaving the left side purely static (title │ filename).
        let mut state = sample_state();
        state.last_update_at = Some(SystemTime::now());
        state.git_sync.active = true;
        state.git_sync.last_at = Some(SystemTime::now());
        let row = title_row(&mut state, 100, 30);

        assert!(
            !row.contains("render-test.md │"),
            "no tag directly follows the filename any more: {row:?}"
        );
        let filename_end = row.find("render-test.md").unwrap() + "render-test.md".len();
        // The tag's own icon, not the "Updated" text, marks where the
        // right-hand cluster actually starts.
        let tag_start = row.find(state.icons.update).unwrap();
        let counter_pos = row.rfind("1/3").unwrap();
        assert!(
            tag_start > filename_end,
            "update tag comes after the filename: {row:?}"
        );
        assert!(
            counter_pos > tag_start,
            "counter is the rightmost element, after the tags: {row:?}"
        );
        assert!(
            row[filename_end..tag_start].trim().is_empty(),
            "only blank space between the static left group and the dynamic right cluster: {row:?}"
        );
    }

    #[test]
    fn narrow_title_keeps_counter_right_aligned_without_tabs() {
        // On a narrow terminal the title bar keeps the right-aligned
        // counter but no longer carries the tabs (they're a dedicated row).
        let mut state = sample_state();
        let row = title_row(&mut state, 55, 20);
        assert!(!row.contains("[1]"), "no tabs in the title bar: {row:?}");
        assert!(
            row.trim_end().ends_with("1/3"),
            "counter still right-aligned: {row:?}"
        );
    }

    #[test]
    fn renders_without_overview_when_narrow() {
        let mut state = sample_state();
        let text = buffer_text(&mut state, 50, 20);
        assert!(!text.contains("Overview"), "overview hidden below 60 cols");
        // Startup selects the first undone item (beta); it's the current
        // card shown in the single-card narrow layout.
        assert!(text.contains("beta"), "current card still shown");
    }

    #[test]
    fn renders_overview_when_wide() {
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Overview"));
    }

    #[test]
    fn done_total_counter_renders_once_not_in_overview() {
        // The `done/total` counter lives only in the title bar. Even at a
        // wide width where the overview is shown, the count string must appear
        // exactly once (the overview panel no longer carries a copy). sample_state
        // is 1/3 done, and only the counters ever emit that literal string.
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Overview"), "overview is shown at this width");
        assert_eq!(
            text.matches("1/3").count(),
            1,
            "the done/total count renders once (title bar), not also in the overview",
        );
    }

    #[test]
    fn medium_width_still_shows_overview() {
        // Tiers: overview appears from OVERVIEW_MIN_WIDTH up (here the
        // card pane is too narrow to stack, but the overview is present).
        let mut state = sample_state();
        let text = buffer_text(&mut state, 90, 30);
        assert!(text.contains("Overview"), "overview shown at medium width");
    }

    #[test]
    fn single_list_overview_omits_list_row() {
        // With one list the `[1] Title` row is dropped from the
        // overview, but the panel and its items stay.
        let mut state = state_with(vec![List {
            title: "Solo".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", false), checkbox(2, "beta", false)],
        }]);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Overview"), "overview still shown");
        assert!(!text.contains("[1]"), "list-number row dropped");
        assert!(
            text.contains("alpha") && text.contains("beta"),
            "items shown"
        );
    }

    #[test]
    fn multi_list_overview_keeps_list_rows() {
        // Counterpart to the single-list case: >1 list keeps `[n] Title`.
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(
            text.contains("[1]") && text.contains("[2]"),
            "list rows kept"
        );
    }

    #[test]
    fn overview_rows_have_inner_padding() {
        // 1 col of inner padding, so a row's marker sits a space in from
        // the overview's left border ("│ ☐ beta"), not flush against it.
        let mut state = state_with(vec![List {
            title: "Solo".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", false), checkbox(2, "beta", false)],
        }]);
        let rows = buffer_rows(&mut state, 100, 30);
        let beta = rows
            .iter()
            .find(|r| r.contains("beta"))
            .expect("a row with the pending item");
        assert!(
            beta.contains("│ ☐ beta"),
            "pending marker padded a space in from the border, got: {beta:?}"
        );
        assert!(
            !beta.contains("│☐ beta"),
            "marker must not be flush against the border"
        );
    }

    #[test]
    fn help_overlay_lists_keybindings() {
        // The ? overlay shows the cheatsheet.
        let mut state = sample_state();
        state.screen = Screen::Help;
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Keybindings"), "help title");
        assert!(text.contains("toggle the task done"), "a key is listed");
        assert!(text.contains("gg / G"), "gg spelled without a space");
        assert!(text.contains("first / last task"), "gg/G listed");
        assert!(text.contains("} / {"), "sub-section jump listed");
        assert!(
            text.contains("next / previous sub-section"),
            "brace-key jump described"
        );
        assert!(
            text.contains("previous / next unfinished list"),
            "Shift-H/L listed"
        );
        // All four home-row/arrow keys navigate; scrolling moved to the
        // Ctrl viewport keys, which the overlay documents.
        assert!(
            text.contains("h j k l"),
            "all four motion keys listed as navigation"
        );
        assert!(
            text.contains("Ctrl-E / Ctrl-Y") && text.contains("scroll card body"),
            "Ctrl scroll keys listed"
        );
        // Mouse bindings (wheel scroll/nav, click-to-copy, click-to-jump
        // on the overview) are documented in the overlay, not just the README.
        assert!(
            text.contains("mouse wheel"),
            "mouse wheel scroll/nav listed"
        );
        assert!(
            text.contains("click a card"),
            "click-to-copy on a card listed"
        );
        assert!(
            text.contains("click overview row"),
            "click-to-jump on an overview row listed"
        );
    }

    #[test]
    fn status_bar_shows_trimmed_legend() {
        // The always-on legend is trimmed to essentials; the rarely
        // used keys live in the ? overlay, not the status bar.
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("?:help"), "legend points to help");
        assert!(text.contains("y:copy"), "legend keeps the essentials");
        assert!(
            text.contains("hjkl:nav"),
            "legend advertises the motion keys"
        );
        assert!(!text.contains("R:reset"), "rarely-used keys trimmed out");
        assert!(text.contains("/:search"), "legend advertises search");
    }

    #[test]
    fn status_bar_shows_the_search_prompt_while_searching() {
        // Entering search shows the live query vim-style in the status bar,
        // and the checklist cards stay visible behind it.
        let mut state = sample_state();
        state.handle_key(KeyCode::Char('/'));
        state.handle_key(KeyCode::Char('g'));
        state.handle_key(KeyCode::Char('a'));
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("/ga"), "status bar shows the /query prompt");
        // "gamma" is the match; the checklist is still rendered (title visible).
        assert!(
            text.contains("Second"),
            "checklist remains visible under search"
        );
    }

    #[test]
    fn search_status_bar_shows_match_count_and_no_match() {
        // The search prompt reports how many tasks match, and an explicit
        // no-match state when the query hits nothing.
        let mut state = sample_state();
        state.handle_key(KeyCode::Char('/'));
        state.handle_key(KeyCode::Char('a')); // matches alpha, beta, gamma
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("3 matches"), "shows the hit count");
        state.handle_key(KeyCode::Char('z')); // "az" matches nothing
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("no matches"), "shows the no-match state");
    }

    #[test]
    fn list_picker_overlay_lists_tasks_with_count_and_filters() {
        // T opens a filterable "go to task" overlay listing every task,
        // with match-count feedback that updates as the filter is typed.
        let mut state = sample_state();
        state.handle_key(KeyCode::Char('T'));
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Go to task"), "overlay title");
        assert!(
            text.contains("alpha") && text.contains("gamma"),
            "lists tasks"
        );
        state.handle_key(KeyCode::Char('a')); // all three contain 'a'
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("match"), "shows match-count feedback");
    }

    #[test]
    fn list_picker_shows_the_sub_section_path() {
        // The go-to-task overlay surfaces each item's `### H3`+
        // sub-section path as a dim `— …` suffix, so tasks are told apart.
        let item = Item {
            line_number: 1,
            depth: 0,
            section: vec![crate::model::SubHeading {
                level: 3,
                text: "Pre-flight checks".to_string(),
            }],
            display_text: "confirm staging is idle".to_string(),
            body: vec![crate::model::BodySpan::Text(
                "confirm staging is idle".to_string(),
            )],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::Checkbox(TaskState::NotStarted),
        };
        let mut state = state_with(vec![List {
            title: "Deploy".to_string(),
            banner: None,
            items: vec![item],
        }]);
        state.handle_key(KeyCode::Char('T'));
        let text = buffer_text(&mut state, 100, 30);
        assert!(
            text.contains("Pre-flight checks"),
            "picker shows the sub-section path: {text}"
        );
    }

    #[test]
    fn card_breadcrumb_is_clamped_to_the_card_width() {
        // A long / deep sub-section path is truncated with an ellipsis
        // above the card so it can't overflow on a narrow terminal.
        let long_outer = "Outer sub-section with a rather long descriptive name";
        let long_inner = "Inner sub-section that is also quite long indeed";
        let mut item = checkbox(1, "do the thing", false);
        item.section = vec![
            crate::model::SubHeading {
                level: 3,
                text: long_outer.to_string(),
            },
            crate::model::SubHeading {
                level: 4,
                text: long_inner.to_string(),
            },
        ];
        let mut state = state_with(vec![List {
            title: "L".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let text = buffer_text(&mut state, 60, 24);
        assert!(
            text.contains('…'),
            "breadcrumb truncated with an ellipsis: {text}"
        );
        assert!(
            !text.contains(&format!("{long_outer} › {long_inner}")),
            "the full untruncated path must not render"
        );
    }

    #[test]
    fn card_body_has_no_checkbox_prefix() {
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(
            !text.contains("[x]") && !text.contains("[ ]"),
            "no raw checkbox syntax"
        );
    }

    #[test]
    fn render_records_code_region_for_click_to_copy() {
        // Rendering the current card records a per-row code region for a
        // fenced block, so a left-click on that row can copy it.
        let item = Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: "deploy".to_string(),
            body: vec![crate::model::BodySpan::Text("deploy".to_string())],
            header: None,
            code_spans: vec![],
            code_blocks: vec!["kubectl apply".to_string()],
            kind: ItemKind::Checkbox(TaskState::NotStarted),
        };
        let mut state = state_with(vec![List {
            title: "Deploy".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let _ = buffer_text(&mut state, 100, 30);
        assert!(
            state.code_regions.iter().any(|(_, t)| t == "kubectl apply"),
            "code region recorded for the fenced block: {:?}",
            state.code_regions
        );
        assert!(
            state.code_regions.iter().all(|(r, _)| r.height == 1),
            "regions are single rows"
        );
        // Geometry cross-check: the row where the box content actually renders
        // must have a recorded region at that same y (catches vis_row bugs).
        let region_ys: Vec<u16> = state.code_regions.iter().map(|(r, _)| r.y).collect();
        let mut checked = false;
        for y in 0..30u16 {
            if nth_row(&mut state, 100, 30, y).contains("kubectl apply") {
                assert!(
                    region_ys.contains(&y),
                    "region row should match the rendered box row {y}; regions at {region_ys:?}"
                );
                checked = true;
            }
        }
        assert!(checked, "the fenced box rendered somewhere on screen");
    }

    #[test]
    fn card_shows_fenced_code_block() {
        // Fenced blocks (previously copy-only) now render in the card.
        let item = Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: "deploy the service".to_string(),
            body: vec![crate::model::BodySpan::Text(
                "deploy the service".to_string(),
            )],
            header: None,
            code_spans: vec![],
            code_blocks: vec!["kubectl apply".to_string()],
            kind: ItemKind::Checkbox(TaskState::NotStarted),
        };
        let mut state = state_with(vec![List {
            title: "Deploy".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("kubectl apply"), "fenced block shown in card");
    }

    #[test]
    fn started_task_shows_started_icon() {
        // A started task renders the distinct started glyph (◐ in the
        // Unicode icon set used by tests) in the card and the overview.
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, "startme", false), checkbox(2, "other", false)],
        }]);
        state.document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Started);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains('◐'), "started icon shown: {text:?}");
    }

    #[test]
    fn progress_bar_row_is_rendered() {
        // The row under the title bar draws the green filled glyph.
        let mut state = sample_state(); // 1 of 3 done
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains('━'), "progress bar filled segment present");
    }

    #[test]
    fn progress_bar_shows_started_segment_in_yellow() {
        // A started task adds a distinct yellow segment to the bar,
        // alongside the green done segment.
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![
                checkbox(1, "a", true),
                checkbox(2, "b", false),
                checkbox(3, "c", false),
            ],
        }]);
        state.document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Started);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let has = |color| {
            buffer
                .content()
                .iter()
                .any(|c| c.symbol() == "━" && c.style().fg == Some(color))
        };
        assert!(
            has(state.palette.started),
            "started segment rendered in the started color"
        );
        assert!(
            has(state.palette.done),
            "done segment still rendered in the done color"
        );
    }

    #[test]
    fn nested_card_shows_parent_breadcrumb() {
        // A nested item's card shows its parent chain as context. Render
        // narrow (no overview) so the parent's text can only be the breadcrumb.
        let mut child = checkbox(2, "child task", false);
        child.depth = 1;
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, "parent task", false), child],
        }]);
        state.current_item_index = 1; // land on the nested child
        let text = buffer_text(&mut state, 50, 20);
        assert!(text.contains("child task"), "child card shown: {text:?}");
        assert!(
            text.contains("parent task"),
            "parent breadcrumb shown above the nested card: {text:?}"
        );
    }

    #[test]
    fn card_shows_position_dots() {
        // The current card's bottom border shows the position strip.
        let mut state = sample_state();
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains('◉'), "current-position dot present");
    }

    #[test]
    fn overview_shows_list_banner() {
        // A list banner appears as a non-selectable warning row in the
        // overview, below the list title.
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: Some("Heads up".to_string()),
            items: vec![checkbox(1, "alpha", true), checkbox(2, "beta", false)],
        }]);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Heads up"), "banner in overview: {text:?}");
    }

    #[test]
    fn note_card_uses_a_rounded_border() {
        // A display-only note card renders with a rounded border (╭)
        // rather than the task card's thick border (┏).
        let note = Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: "just a note".to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::DisplayOnly,
        };
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![note],
        }]);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains('╭'), "note card has a rounded border");
        assert!(
            !text.contains('┏'),
            "note card is not the thick task border"
        );
    }

    #[test]
    fn overview_shows_depth_colored_guides() {
        // Nested rows get `│` guides (one per depth level), each in that
        // level's hard-coded depth color, so hierarchy reads by which color.
        let parent = checkbox(1, "parent", false);
        let mut child = checkbox(2, "child", false);
        child.depth = 1;
        let mut grandchild = checkbox(3, "grandchild", false);
        grandchild.depth = 2;
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![parent, child, grandchild],
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let guides: Vec<_> = (0..100)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .map(|(x, y)| buffer[(x, y)].clone())
            .filter(|c| c.symbol() == "│")
            .collect();
        // The two nesting levels use different sub-list slots (0 and 1), so
        // distinct colors.
        assert!(
            guides
                .iter()
                .any(|c| c.style().fg == Some(state.palette.depth_color(0))),
            "the level-0 sub-list guide color is present"
        );
        assert!(
            guides
                .iter()
                .any(|c| c.style().fg == Some(state.palette.depth_color(1))),
            "the nested sub-list guide (distinct color) is present"
        );
    }

    #[test]
    fn selected_nested_marker_matches_sublist_color() {
        // A current nested task's `❯` marker takes its own sub-list's
        // depth color, so its REVERSED highlight background matches the depth
        // guides on the row instead of the cyan `palette.current` that clashed.
        let parent = checkbox(1, "parent", false);
        let mut child = checkbox(2, "child", false);
        child.depth = 1;
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![parent, child],
        }]);
        // Move the cursor onto the nested child.
        state.handle_key(KeyCode::Char('j'));
        let marker = state.icons.current;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let markers: Vec<_> = (0..100)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .map(|(x, y)| buffer[(x, y)].clone())
            .filter(|c| c.symbol() == marker)
            .collect();
        assert!(
            !markers.is_empty(),
            "the current `{marker}` marker is drawn"
        );
        // The child sits under sub-list slot 0, so the marker borrows that
        // color; it is never the plain cyan current accent.
        assert!(
            markers
                .iter()
                .all(|c| c.style().fg == Some(state.palette.depth_color(0))),
            "nested current marker uses its sub-list depth color"
        );
        assert_ne!(
            state.palette.depth_color(0),
            state.palette.current,
            "test is only meaningful if the sub-list color differs from current"
        );
    }

    #[test]
    fn depth_guides_differ_between_lists() {
        // Sub-list slots are document-wide, so a second list's sub-list
        // gets a different color and doesn't look like the first list
        // continuing. Two lists, each with a depth-1 child.
        let mut child_a = checkbox(2, "child-a", false);
        child_a.depth = 1;
        let mut child_b = checkbox(4, "child-b", false);
        child_b.depth = 1;
        let mut state = state_with(vec![
            List {
                title: "First".to_string(),
                banner: None,
                items: vec![checkbox(1, "parent-a", false), child_a],
            },
            List {
                title: "Second".to_string(),
                banner: None,
                items: vec![checkbox(3, "parent-b", false), child_b],
            },
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let guide_colors: Vec<_> = (0..100)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .map(|(x, y)| buffer[(x, y)].clone())
            .filter(|c| c.symbol() == "│")
            .filter_map(|c| c.style().fg)
            .collect();
        // List 0's sub-list is slot 0; list 1's sub-list is slot 1 (offset past
        // list 0's one sub-list) — different colors.
        assert_ne!(
            state.palette.depth_color(0),
            state.palette.depth_color(1),
            "the two lists' guide colors differ"
        );
        assert!(
            guide_colors.contains(&state.palette.depth_color(0))
                && guide_colors.contains(&state.palette.depth_color(1)),
            "both lists' distinct guide colors are rendered"
        );
    }

    #[test]
    fn depth_guides_differ_between_sublists_in_one_list() {
        // Two separate sub-lists within the same list get distinct colors
        // (keyed by sublist_slot), so they don't blend into one. parentA/childA
        // then parentB/childB, all in one list.
        let mut child_a = checkbox(2, "child-a", false);
        child_a.depth = 1;
        let mut child_b = checkbox(4, "child-b", false);
        child_b.depth = 1;
        let mut state = state_with(vec![List {
            title: "One".to_string(),
            banner: None,
            items: vec![
                checkbox(1, "parent-a", false),
                child_a,
                checkbox(3, "parent-b", false),
                child_b,
            ],
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let guide_colors: Vec<_> = (0..100)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .map(|(x, y)| buffer[(x, y)].clone())
            .filter(|c| c.symbol() == "│")
            .filter_map(|c| c.style().fg)
            .collect();
        // childA's guide (sublist slot 0) and childB's guide (sublist slot 1) differ.
        assert_ne!(
            state.palette.depth_color(0),
            state.palette.depth_color(1),
            "the two sub-lists' guide colors differ"
        );
        assert!(
            guide_colors.contains(&state.palette.depth_color(0))
                && guide_colors.contains(&state.palette.depth_color(1)),
            "both sub-lists' distinct guide colors are rendered"
        );
    }

    #[test]
    fn nested_example_gives_each_sublist_a_distinct_color() {
        // Regression: examples/nested.md has four sub-lists (three under
        // Rebuild, one under Release); with document-wide slots each gets its
        // own color (no collision across lists).
        let doc =
            crate::parser::parse_document(std::path::PathBuf::from("examples/nested.md")).unwrap();
        let mut state = AppState::new(doc);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let depth_colors: std::collections::HashSet<_> =
            (0..4).map(|s| state.palette.depth_color(s)).collect();
        let seen: std::collections::HashSet<_> = buffer
            .content()
            .iter()
            .filter(|c| c.symbol() == "│")
            .filter_map(|c| c.style().fg)
            .filter(|fg| depth_colors.contains(fg))
            .collect();
        assert_eq!(
            seen.len(),
            4,
            "all four sub-lists render in distinct colors: {seen:?}"
        );
    }

    #[test]
    fn sub_sections_example_renders_dividers_and_drops_empty() {
        // Regression: examples/sub-sections.md groups items under `### H3`+
        // headings. The overview shows each group's `── divider`, and an
        // item-less sub-heading ("Skipped when empty") is never drawn.
        let doc =
            crate::parser::parse_document(std::path::PathBuf::from("examples/sub-sections.md"))
                .unwrap();
        let mut state = AppState::new(doc);
        state.icons = IconSet::unicode();
        let rows = buffer_rows(&mut state, 120, 40);
        let joined = rows.join("\n");
        assert!(
            joined.contains("── Pre-flight checks"),
            "H3 group divider shown: {joined}"
        );
        assert!(
            joined.contains("── Data safety"),
            "nested H4 group divider shown: {joined}"
        );
        assert!(
            !joined.contains("Skipped when empty"),
            "item-less sub-heading is dropped: {joined}"
        );
    }

    #[test]
    fn nested_card_breadcrumb_is_depth_colored() {
        // The parent-chain breadcrumb above a nested card's title takes the
        // depth color for the item's own level. Checked in the card columns
        // (left of the overview, which also draws depth-colored guides).
        let parent = checkbox(1, "parent", false);
        let mut child = checkbox(2, "child", false);
        child.depth = 1;
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![parent, child],
        }]);
        state.current_item_index = 1; // the nested child → breadcrumb shown
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let breadcrumb_colored = (0..55)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .map(|(x, y)| buffer[(x, y)].clone())
            .any(|c| c.symbol() != " " && c.style().fg == Some(state.palette.depth_color(0)));
        assert!(
            breadcrumb_colored,
            "the nested card's breadcrumb is drawn in the depth color"
        );
    }

    #[test]
    fn idle_note_card_border_is_note_colored() {
        // An idle info card's rounded border takes palette.note (blue),
        // matching its note icon, rather than the default color.
        let note = Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: "just a note".to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::DisplayOnly,
        };
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![note],
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rounded = ['╭', '╮', '╰', '╯', '│'];
        let note_border = (0..100)
            .flat_map(|x| (0..30).map(move |y| (x, y)))
            .filter_map(|(x, y)| {
                let cell = &buffer[(x, y)];
                rounded
                    .iter()
                    .any(|g| cell.symbol() == g.to_string())
                    .then(|| cell.style().fg)
            })
            .any(|fg| fg == Some(state.palette.note));
        assert!(note_border, "note card border is drawn in palette.note");
    }

    #[test]
    fn task_card_uses_a_thick_border() {
        // A checkbox task card renders with a thick border (┏) rather than
        // a rounded note border (╭).
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", false)],
        }]);
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains('┏'), "task card has a thick border");
        assert!(
            !text.contains('╭'),
            "task card is not the rounded note border"
        );
    }

    #[test]
    fn renders_list_complete_screen() {
        let mut state = state_with(vec![List {
            title: "Only".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", true)],
        }]);
        state.screen = Screen::ListComplete;
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("List Complete"));
        assert!(text.contains("1 / 1 tasks completed"));
        assert!(
            !text.contains("to reset"),
            "list-complete screen omits the reset hint (whole-document only)"
        );
    }

    #[test]
    fn renders_confirm_reset_screen() {
        let mut state = sample_state();
        state.screen = Screen::ConfirmReset;
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Confirm Reset"));
        assert!(text.contains("cancel"));
    }

    #[test]
    fn renders_confirm_quit_reset_screen() {
        let mut state = sample_state();
        state.screen = Screen::ConfirmQuitReset;
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("All Tasks Complete"));
        assert!(text.contains("reset and quit"));
        assert!(text.contains("quit without resetting"));
    }

    #[test]
    fn status_message_overrides_legend() {
        let mut state = sample_state();
        state.status_message = Some("Copied to clipboard".to_string());
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("Copied to clipboard"));
        assert!(!text.contains("space:toggle"), "legend replaced by message");
    }

    #[test]
    fn error_status_message_is_rendered_in_error_color() {
        // A failure / "nothing happened" message (status_is_error) is
        // painted in palette.error; a passive confirmation is not. Color is the
        // feature here, so we assert on the cell style.
        let width = 100u16;
        let height = 30u16;
        let status_row = height - 1;
        let first_colored_cell = |state: &mut AppState| -> Option<ratatui::style::Color> {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, state)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..width)
                .map(|x| &buffer[(x, status_row)])
                .find(|c| c.symbol() != " ")
                .map(|c| c.style().fg.unwrap_or(ratatui::style::Color::Reset))
        };

        let mut state = sample_state();
        state.set_error("Copy failed: no clipboard available".to_string());
        assert_eq!(
            first_colored_cell(&mut state),
            Some(state.palette.error),
            "error message renders red"
        );

        let mut state = sample_state();
        state.status_message = Some("Copied to clipboard".to_string());
        state.status_is_error = false;
        assert_ne!(
            first_colored_cell(&mut state),
            Some(state.palette.error),
            "a passive confirmation is not red"
        );
    }

    #[test]
    fn renders_tiny_terminal_without_panic() {
        let mut state = sample_state();
        // Degenerate sizes are a classic ratatui panic class.
        let _ = buffer_text(&mut state, 1, 1);
        let _ = buffer_text(&mut state, 3, 2);
    }

    #[test]
    fn completion_screens_render_on_a_short_terminal_without_panic() {
        // Regression test: completion_card used to clamp its height directly
        // against the raw content area, which panics (min > max) once the
        // area drops under MIN_CARD_HEIGHT rows — exactly what a short
        // terminal produces on the very screens shown when a checklist (or
        // the whole document) is finished.
        let mut list_complete = sample_state();
        list_complete.screen = Screen::ListComplete;
        let _ = buffer_text(&mut list_complete, 40, 6);

        let mut all_complete = sample_state();
        all_complete.screen = Screen::AllComplete;
        let _ = buffer_text(&mut all_complete, 40, 6);
    }

    #[test]
    fn wide_layout_shows_prev_and_next_side_cards() {
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![
                checkbox(1, "prevtask", false),
                checkbox(2, "curtask", false),
                checkbox(3, "nexttask", false),
            ],
        }]);
        state.current_item_index = 1; // middle: both neighbors exist
        let text = buffer_text(&mut state, 120, 30);
        assert!(text.contains("curtask"), "center card");
        assert!(text.contains("prevtask"), "prev side card");
        assert!(text.contains("nexttask"), "next side card");
    }

    #[test]
    fn renders_all_complete_screen() {
        let mut state = state_with(vec![List {
            title: "Only".to_string(),
            banner: None,
            items: vec![checkbox(1, "alpha", true)],
        }]);
        state.screen = Screen::AllComplete;
        let text = buffer_text(&mut state, 100, 30);
        assert!(text.contains("All Tasks Complete"));
        assert!(text.contains("Total: 1 / 1 tasks"));
        assert!(text.contains("R  to reset"), "reset hint shown");
    }

    #[test]
    fn overflowing_card_shows_scroll_indicator() {
        let long = (0..300)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, &long, false)],
        }]);
        // Short terminal forces the long body to overflow the card.
        let text = buffer_text(&mut state, 90, 12);
        assert!(state.card_max_scroll > 0, "renderer detected overflow");
        assert!(text.contains('–'), "scroll indicator like 1-6/N present");
        assert!(
            text.contains(SCROLLBAR_THUMB),
            "scrollbar thumb present on overflow"
        );
    }

    #[test]
    fn overflowing_card_with_many_items_never_loses_the_current_dot() {
        // The dots strip and the scroll indicator used to be sized
        // independently, so ratatui's title truncation could silently eat
        // exactly the current-item marker (`◉`) to make room for the
        // indicator — leaving a strip of dots with no "you are here" marker.
        // Now the two are budgeted together, so either both show, or the
        // dots are dropped entirely in favor of the indicator; the marker
        // itself is never the thing that gets clipped.
        let long = (0..300)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut items: Vec<Item> = (1..=46).map(|n| checkbox(n, "task", false)).collect();
        let last = items.len() - 1;
        items[last] = checkbox(46, &long, false);
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items,
        }]);
        state.current_item_index = last;
        let text = buffer_text(&mut state, 50, 10);
        assert!(state.card_max_scroll > 0, "renderer detected overflow");
        let has_dots = text.contains('·') || text.contains('●');
        assert!(
            !has_dots || text.contains('◉'),
            "dots strip must include the current marker whenever any dots are shown: {text:?}"
        );
    }

    #[test]
    fn overflowing_overview_shows_a_scrollbar() {
        // Many items force the overview to overflow a short terminal.
        let items: Vec<Item> = (1..=40)
            .map(|n| checkbox(n, &format!("task {n}"), false))
            .collect();
        let mut state = state_with(vec![List {
            title: "Big".to_string(),
            banner: None,
            items,
        }]);
        let text = buffer_text(&mut state, 100, 12);
        assert!(
            text.contains(SCROLLBAR_THUMB),
            "overview scrollbar thumb present"
        );
    }

    #[test]
    fn overview_records_clickable_row_rects_that_map_to_the_right_task() {
        // After a real render the overview records a Rect per clickable row,
        // and a click on a recorded item Rect focuses that exact task — end to
        // end through the panel's border offset and scroll.
        let mut state = state_with(vec![
            List {
                title: "Alpha".to_string(),
                banner: None,
                items: vec![checkbox(1, "a", false), checkbox(2, "b", false)],
            },
            List {
                title: "Beta".to_string(),
                banner: None,
                items: vec![checkbox(3, "c", false), checkbox(4, "d", false)],
            },
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();

        // A list-title row is recorded for the second list.
        assert!(
            state
                .overview_rows
                .iter()
                .any(|(_, t)| *t == OverviewTarget::List(1)),
            "list-title row recorded"
        );

        // Clicking the recorded rect for list 2's second item focuses it.
        let (rect, _) = state
            .overview_rows
            .iter()
            .find(|(_, t)| *t == OverviewTarget::Item(1, 1))
            .copied()
            .expect("item (1,1) has a recorded row");
        state.handle_left_click(rect.x + rect.width / 2, rect.y);
        assert_eq!(
            (state.current_list_index, state.current_item_index),
            (1, 1),
            "click landed on the clicked task"
        );
    }

    #[test]
    fn overview_sticky_header_click_rects_stay_consistent_near_a_scroll_boundary() {
        // Exercises the sticky-header fixed-point loop (src/ui/overview.rs)
        // under the conditions that could trigger its non-convergence edge
        // case: several stacked ### H3+ sub-heading levels (so the pinned
        // ancestor-row count swings sharply between adjacent rows — 3
        // dropping straight to 0 at the group boundary below) in a short
        // overview panel, scrolled to the bottom so the offset sits right at
        // that boundary. A brute-force sweep over many terminal sizes and
        // selections did not reproduce actual non-convergence (ratatui's
        // real scroll offsets appear to keep the loop settling within its
        // 4-pass budget for realistic content), so this doesn't prove the
        // fixed-point loop can fail to converge — but it does pin the
        // invariant the fix (see safe fallback to `rendered_reserved` in
        // overview.rs) guarantees regardless: every recorded click Rect
        // must map back to the task it visually labels, never a neighboring
        // row shifted by a stale reserved-row count.
        let sub = |level: u8, text: &str| crate::model::SubHeading {
            level,
            text: text.to_string(),
        };
        let with_section = |mut item: Item, section: Vec<crate::model::SubHeading>| {
            item.section = section;
            item
        };
        let deploy = vec![sub(3, "Deploy")];
        let preflight = vec![sub(3, "Deploy"), sub(4, "Pre-flight")];
        let checks = vec![sub(3, "Deploy"), sub(4, "Pre-flight"), sub(5, "Checks")];
        let verify = vec![sub(3, "Verify")];
        let items = vec![
            checkbox(1, "warmup", false),
            with_section(checkbox(2, "deploy-a", false), deploy.clone()),
            with_section(checkbox(3, "deploy-b", false), deploy),
            with_section(checkbox(4, "preflight-a", false), preflight.clone()),
            with_section(checkbox(5, "preflight-b", false), preflight),
            with_section(checkbox(6, "check-a", false), checks.clone()),
            with_section(checkbox(7, "check-b", false), checks.clone()),
            with_section(checkbox(8, "check-c", false), checks),
            with_section(checkbox(9, "verify-a", false), verify.clone()),
            with_section(checkbox(10, "verify-b", false), verify),
        ];
        let mut state = state_with(vec![List {
            title: "Ops".to_string(),
            banner: None,
            items,
        }]);
        // Select the last item so the overview must scroll to the bottom,
        // right past the 3-pinned-rows -> 0-pinned-rows boundary.
        state.current_item_index = 9;
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();

        for (target_list, target_item, label) in [(0, 8, "verify-a"), (0, 9, "verify-b")] {
            let (rect, _) = state
                .overview_rows
                .iter()
                .find(|(_, t)| *t == OverviewTarget::Item(target_list, target_item))
                .copied()
                .unwrap_or_else(|| panic!("{label} has a recorded overview row"));
            state.handle_left_click(rect.x + rect.width / 2, rect.y);
            assert_eq!(
                (state.current_list_index, state.current_item_index),
                (target_list, target_item),
                "clicking {label}'s recorded rect must land on {label}, not a \
                 neighboring row shifted by a stale reserved-row count"
            );
        }
    }

    #[test]
    fn overview_marker_zone_toggles_and_label_zone_navigates() {
        // A rendered checkbox row exposes a marker (toggle) rect to the
        // left of a label (navigate) rect for the same item; the marker toggles
        // in place, the label moves the cursor.
        let mut state = state_with(vec![
            List {
                title: "Alpha".to_string(),
                banner: None,
                items: vec![checkbox(1, "a", false), checkbox(2, "b", false)],
            },
            List {
                title: "Beta".to_string(),
                banner: None,
                items: vec![checkbox(3, "c", false), checkbox(4, "d", false)],
            },
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();

        let rect_for = |state: &AppState, want: OverviewTarget| {
            state
                .overview_rows
                .iter()
                .find(|(_, t)| *t == want)
                .map(|(r, _)| *r)
        };
        let toggle = rect_for(&state, OverviewTarget::Toggle(1, 1)).expect("marker rect");
        let navigate = rect_for(&state, OverviewTarget::Item(1, 1)).expect("label rect");
        assert!(
            toggle.x + toggle.width <= navigate.x,
            "marker zone sits left of the label zone, no overlap"
        );
        assert_eq!(toggle.y, navigate.y, "same row");
        assert_eq!(
            toggle.x,
            navigate.x - toggle.width,
            "the two zones tile the row with no gap"
        );

        // Clicking the label navigates without toggling. (The marker-click →
        // toggle path writes the file, so it's exercised in app.rs against a
        // real temp path; here the render fixture has no backing file.)
        state.handle_left_click(navigate.x + navigate.width / 2, navigate.y);
        assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
        assert!(matches!(
            state.document.lists[1].items[1].kind,
            ItemKind::Checkbox(TaskState::NotStarted)
        ));
    }

    #[test]
    fn overview_rows_cleared_when_overview_is_hidden() {
        // A narrow terminal hides the overview, so any stale click targets
        // from a previous wide frame must be dropped.
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, "a", false)],
        }]);
        state.overview_rows = vec![(Rect::new(0, 0, 1, 1), OverviewTarget::Item(0, 0))];
        // Below OVERVIEW_MIN_WIDTH (60), so the overview isn't drawn.
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        assert!(
            state.overview_rows.is_empty(),
            "stale click targets cleared when the overview is hidden"
        );
    }

    #[test]
    fn overflowing_help_overlay_scrolls_and_shows_a_scrollbar() {
        let mut state = sample_state();
        state.handle_key(KeyCode::Char('?'));
        // A short terminal clips the long help; it becomes scrollable.
        let text = buffer_text(&mut state, 90, 12);
        assert!(
            state.help.max_scroll > 0,
            "help overflows the short viewport"
        );
        assert!(
            text.contains(SCROLLBAR_THUMB),
            "help scrollbar thumb present"
        );
    }

    #[test]
    fn card_title_text_uses_default_fg_for_readability() {
        // The leading bold shows inside the card (not the
        // border). Its text is bold in the terminal's *default* foreground so it
        // stays legible on any background (previously blue, which could
        // wash out); the leading note icon keeps the blue accent.
        let mut item = checkbox(1, "do the thing", false);
        item.header = Some("Reboot".to_string());
        let mut state = state_with(vec![List {
            title: "S".to_string(),
            banner: None,
            items: vec![item],
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| render(f, &mut state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        // The title text is still bold and present…
        let bold_text: String = buffer
            .content()
            .iter()
            .filter(|c| c.style().add_modifier.contains(Modifier::BOLD))
            .map(|c| c.symbol())
            .collect();
        assert!(
            bold_text.contains("Reboot"),
            "title is still bold: {bold_text:?}"
        );
        // …but no longer in the note color (default fg for contrast)…
        let note_bold_text: String = buffer
            .content()
            .iter()
            .filter(|c| {
                c.style().fg == Some(state.palette.note)
                    && c.style().add_modifier.contains(Modifier::BOLD)
            })
            .map(|c| c.symbol())
            .collect();
        assert!(
            !note_bold_text.contains("Reboot"),
            "title text is no longer note-colored: {note_bold_text:?}"
        );
        // …while the note icon keeps the note-colored accent.
        assert!(
            buffer
                .content()
                .iter()
                .any(|c| c.style().fg == Some(state.palette.note)),
            "note icon keeps the note-colored accent"
        );
    }
}
