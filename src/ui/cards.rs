use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List as ListWidget, ListItem, ListState, Padding, Paragraph,
};

use crate::model::{
    AppState, BodySpan, IconSet, Item, ItemKind, List, Palette, Screen, SubHeading, TaskState,
};

/// Prose that wraps to at most this many lines is centered; longer prose is
/// left-aligned, which reads better than ragged-both-edges centering.
const PROSE_CENTER_MAX_LINES: usize = 2;

/// Horizontal padding (columns each side) inside a card body so text isn't
/// flush against the border.
const BODY_PAD: u16 = 1;

/// Blank lines to prepend so `len` body lines sit vertically centered in
/// `height` rows. Shared by the center and side cards so all three align
/// their content the same way.
fn top_pad(len: usize, height: usize) -> usize {
    height.saturating_sub(len) / 2
}

/// Convert a line count to `u16`, clamping instead of wrapping. No real
/// terminal is anywhere near `u16::MAX` rows, so this only guards a
/// pathological body/fenced block against silently truncating to a small
/// wrapped value.
fn clamp_to_u16(n: usize) -> u16 {
    n.min(u16::MAX as usize) as u16
}

/// The accent color for a task's state: the palette's `done` when done,
/// `started` when started, none otherwise. Drives borders and the
/// position status.
/// `info_state` is the display-only parent's aggregate child state
/// (`List::info_parent_state`); `None` for a checkbox item, which
/// carries its own state. A display-only parent thus borrows the same accent a
/// task would — green all-done, yellow in-progress — while its rounded border
/// keeps saying "information": shape encodes kind, color encodes state.
fn state_color(item: &Item, info_state: Option<TaskState>, palette: Palette) -> Option<Color> {
    let state = match item.kind {
        ItemKind::Checkbox(state) => Some(state),
        ItemKind::DisplayOnly => info_state,
    };
    match state {
        Some(TaskState::Done) => Some(palette.done),
        Some(TaskState::Started) => Some(palette.started),
        _ => None,
    }
}

/// The accent color for a card's frame decorations — the border and the ` n/m `
/// position status. It's `state_color`, but a **display-only note** with no
/// active sub-list state falls back to `palette.note` (blue) so an idle info
/// card's frame matches its blue note icon and inside-title. Tasks are
/// unaffected: a pending task still returns `None` (default), done/started keep
/// their state color. When a note's sub-list becomes active the border still
/// takes the green/yellow state accent.
fn accent_color(item: &Item, info_state: Option<TaskState>, palette: Palette) -> Option<Color> {
    match state_color(item, info_state, palette) {
        Some(color) => Some(color),
        None if matches!(item.kind, ItemKind::DisplayOnly) => Some(palette.note),
        None => None,
    }
}

/// Responsive tiers are driven by the *card-area* width (the space
/// left after the overview split, if any): at/above `NARROW_THRESHOLD` the
/// three-card stack is shown; below it, a single current card. Combined
/// with `OVERVIEW_MIN_WIDTH` (in `mod.rs`), this yields three tiers —
/// narrow (single card, no overview), medium (single card + overview), and
/// wide (stack + overview).
const NARROW_THRESHOLD: u16 = 80;
/// Below this card-row width the side cards don't have room to tuck.
const MIN_STACK_WIDTH: u16 = 44;
const MIN_CARD_HEIGHT: u16 = 8;
/// The center (current) card's share of the card-row width in the stack
/// layout. Wider than the side cards so long commands fit; the side
/// cards split the remainder. Used both to measure card height
/// (`current_card_width`) and to place the rendered Rect (`center_width`),
/// which must stay in sync.
const CENTER_CARD_PCT: u16 = 72;
/// How far the side cards extend underneath the center card.
const STACK_OVERLAP: u16 = 4;
/// How far the side cards sit down from the center card's top edge.
const STACK_TUCK: u16 = 1;

pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    match state.screen {
        // Search keeps the checklist visible; the query shows in the status
        // bar and the cursor jumps live to the match.
        Screen::Checklist | Screen::Search => render_checklist(frame, area, state),
        Screen::ListComplete => render_list_complete(frame, area, state),
        Screen::AllComplete => render_all_complete(frame, area, state),
        Screen::ConfirmReset => render_confirm_reset(frame, area, state),
        Screen::ConfirmQuitReset => render_confirm_quit_reset(frame, area, state),
        Screen::Help => render_help(frame, area, state),
        Screen::ListPicker => render_list_picker(frame, area, state),
    }
}

/// The `T` "go to task" overlay: a filterable list of every task, with
/// its state marker and (when there are several lists) its list name. Typing
/// filters via `AppState::picker_matches`; the highlighted row is `Enter`ed to
/// jump. Always gives feedback — the filter text and a match count / no-match
/// state (per the UI Feedback rule).
fn render_list_picker(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let icons = state.icons;
    let palette = state.palette;
    let matches = state.picker_matches();
    let multi_list = state.document.lists.len() > 1;

    let rows: Vec<ListItem> = matches
        .iter()
        .map(|&(li, ii)| {
            let item = &state.document.lists[li].items[ii];
            let (marker, marker_style) = match item.kind {
                ItemKind::Checkbox(TaskState::Done) => {
                    (icons.done, Style::default().fg(palette.done))
                }
                ItemKind::Checkbox(TaskState::Started) => {
                    (icons.started, Style::default().fg(palette.started))
                }
                ItemKind::Checkbox(TaskState::NotStarted) => {
                    (icons.pending, Style::default().add_modifier(Modifier::DIM))
                }
                ItemKind::DisplayOnly => (icons.note, Style::default().fg(palette.note)),
            };
            let label = item.header.as_deref().unwrap_or(&item.display_text);
            let mut spans = vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::raw(label.to_string()),
            ];
            // A dim `— {list} › {sub-section}` context suffix: the
            // list title (only when several lists exist) followed by the item's
            // `### H3`+ sub-section path, so the same visible text can be told
            // apart and the sub-section is discoverable in the picker.
            let mut context: Vec<String> = Vec::new();
            if multi_list {
                context.push(state.document.lists[li].title.clone());
            }
            context.extend(item.section.iter().map(|h| h.text.clone()));
            if !context.is_empty() {
                spans.push(Span::styled(
                    format!("  — {}", context.join(" › ")),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    // Feedback in the title: total when unfiltered, match count / no-match otherwise.
    let count = matches.len();
    let count_label = if state.picker.query.is_empty() {
        format!(" {count} items ")
    } else if count == 0 {
        " no matches ".to_string()
    } else {
        let noun = if count == 1 { "match" } else { "matches" };
        format!(" {count} {noun} ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.current))
        .padding(Padding::horizontal(1))
        .title(" Go to task ")
        .title_top(
            Line::from(Span::styled(
                count_label,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .right_aligned(),
        );

    frame.render_widget(Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Filter prompt on the first row, the (scrolling) list below it.
    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    // Expose the list viewport height for the Ctrl-D/Ctrl-U half-page jumps.
    state.picker.viewport_height = list_area.height;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Filter: ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw(state.picker.query.clone()),
        ])),
        filter_area,
    );

    if matches.is_empty() {
        let msg = if state.picker.query.is_empty() {
            "No items"
        } else {
            "No matching items"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(palette.error),
            )))
            .centered(),
            list_area,
        );
        return;
    }

    let list =
        ListWidget::new(rows).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state =
        ListState::default().with_selected(Some(state.picker.selection.min(count - 1)));
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

/// The `?` keybinding cheatsheet overlay. Scrolls when it doesn't fit a
/// short terminal; any non-scroll key dismisses it.
fn render_help(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let palette = state.palette;
    let key = |k: &str, desc: &str| {
        Line::from(vec![
            Span::styled(
                // Wide enough for the longest key label ("Shift-H / Shift-L")
                // so the description column always clears it.
                format!("  {k:<19}"),
                Style::default()
                    .fg(palette.current)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(desc.to_string()),
        ])
    };
    let lines = vec![
        Line::from(""),
        key("h j k l  ← → ↑ ↓", "prev / next task (crosses lists)"),
        key("gg / G", "first / last task in the document"),
        key("} / {", "next / previous sub-section"),
        key("Ctrl-E / Ctrl-Y", "scroll card body one line"),
        key("Ctrl-D / Ctrl-U", "scroll card body half a page"),
        key("PageDn / PageUp", "scroll card body one page"),
        key(
            "mouse wheel",
            "scroll an overflowing card, else prev/next task",
        ),
        key("space / enter", "toggle the task done"),
        key("s", "mark the task started / in progress"),
        key("u / Ctrl-R", "undo / redo the last change"),
        key("Tab", "jump to the next unfinished task"),
        key("Shift-H / Shift-L", "previous / next unfinished list"),
        key("/", "search tasks by text"),
        key("n / N", "next / previous search match"),
        key("T", "go to task (filterable list)"),
        key(
            "click overview row",
            "toggle (icon) or jump to (label) a task or list",
        ),
        key("y", "copy the task's code to the clipboard"),
        key(
            "click a card",
            "copy the clicked command, or the card's sole one",
        ),
        key("o", "open the card's link in your browser"),
        key("o then 1-9", "open link [N] when a card has several"),
        key("e", "edit the file in $EDITOR"),
        key("R", "reset all tasks (asks first)"),
        key("1–9", "jump to list N"),
        key("?", "toggle this help"),
        key("q / Esc", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "j / k · ↑ / ↓ scroll   ·   any other key closes",
            Style::default().add_modifier(Modifier::DIM),
        ))
        .centered(),
    ];

    // Size the overlay to its content (plus borders), centered; on a short
    // terminal it fills the height and the body scrolls.
    let total = lines.len();
    let height = (total as u16 + 2).min(area.height);
    let row = card_row(area, height);
    let inner_h = row.height.saturating_sub(2) as usize; // visible content rows
    state.help.max_scroll = total.saturating_sub(inner_h) as u16;
    state.help.viewport_height = inner_h as u16;
    state.help.scroll = state.help.scroll.min(state.help.max_scroll);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.current))
        .title(" Keybindings ");

    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((state.help.scroll, 0));
    frame.render_widget(paragraph, row);
    super::render_scrollbar(frame, row, total, inner_h, state.help.scroll as usize);
}

/// Renders the current card's link URLs in the empty space **below the card**,
/// one per line and horizontally centered, with a one-row blank spacer
/// between the card and the list. Single link → `→ url`; multiple → `[N] url`
/// keyed to the card's `[N]` markers and the `o`-then-digit open. The card
/// itself is positioned as if this panel didn't exist (see `render_checklist`),
/// so the card never moves when links come and go — no layout jitter. The full
/// URL always lives in `Item.body`, so this is display-only; a URL wider than
/// the content area is clipped (rare) rather than wrapped.
/// Whether there's room to draw at least one row of the link panel below
/// `card` within `area` — the one blank spacer row plus at least one URL
/// row. Shared with `render_current_card`, which falls back to an in-border
/// hint when this is false so a card with a link never renders with no
/// visual cue at all.
fn link_panel_fits(area: Rect, card: Rect) -> bool {
    card.bottom().saturating_add(1) < area.bottom()
}

fn render_link_panel_below(frame: &mut Frame, area: Rect, card: Rect, state: &AppState) {
    let urls: Vec<String> = state
        .current_item()
        .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
        .unwrap_or_default();
    if urls.is_empty() {
        return;
    }
    // One blank spacer row below the card, then the URLs; clamp to the room
    // left above the status bar.
    let start_y = card.bottom().saturating_add(1);
    if !link_panel_fits(area, card) {
        return;
    }
    let height = (area.bottom() - start_y).min(urls.len() as u16);
    let panel = Rect {
        x: area.x,
        y: start_y,
        width: area.width,
        height,
    };
    let single = urls.len() == 1;
    let lines: Vec<Line<'static>> = urls
        .iter()
        .enumerate()
        .map(|(i, url)| {
            let prefix = if single {
                "→ ".to_string()
            } else {
                format!("[{}] ", i + 1)
            };
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(state.palette.note)),
                Span::raw(url.clone()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).centered(), panel);
}

/// The card row of the given height, vertically centered in the main area
/// with the rest left empty. The height is chosen by
/// `desired_card_height`.
fn card_row(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    row
}

/// Fixed height for the completion/confirmation modal cards — their content
/// is fixed, so they keep the original ~40%-of-height sizing.
fn fixed_card_height(area: Rect) -> u16 {
    ((area.height as u32 * 2 / 5) as u16).max(MIN_CARD_HEIGHT)
}

/// The width the current card will actually render at, so height can be
/// measured against the same wrapping: the centered `CENTER_CARD_PCT` share
/// in the stack layout, or the full width in the narrow single-card layout.
fn current_card_width(area: Rect) -> u16 {
    if area.width < NARROW_THRESHOLD {
        area.width
    } else {
        (area.width * CENTER_CARD_PCT / 100).max(20)
    }
}

/// Card height sized to the current item's content: wrapped body
/// lines + top/bottom borders, clamped between `MIN_CARD_HEIGHT` and ~60%
/// of the main area. Short tasks get a compact card (which also removes the
/// empty-body void); long tasks get more room before scrolling.
fn desired_card_height(item: &Item, area: Rect, code_bg: Color) -> u16 {
    let inner_w = current_card_width(area).saturating_sub(2 + 2 * BODY_PAD);
    let body = clamp_to_u16(body_lines(item, inner_w.max(1), code_bg).len());
    // The inside-card title adds a title line + a blank separator.
    let title = if item.header.is_some() { 2 } else { 0 };
    // A nested item adds a one-line parent breadcrumb above the title.
    let breadcrumb = u16::from(item.depth > 0);
    let content = body
        .saturating_add(title)
        .saturating_add(breadcrumb)
        .saturating_add(2);
    let max_h = ((area.height as u32 * 3 / 5) as u16).max(MIN_CARD_HEIGHT);
    content.clamp(MIN_CARD_HEIGHT, max_h).min(area.height)
}

fn render_checklist(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // List header: show the current list's `## H2` title above the
    // cards, unless this is the synthesized `(Default)` list (no real H2).
    // A leading bold-only bullet renders as a warning banner just below the
    // title (or at the top for a title-less default list). Each of the
    // title and banner is preceded by a blank spacer row for breathing room.
    let has_h2 = !(state.current_list_index == 0 && state.document.has_default_list);
    let banner = state.current_list().banner.clone();
    // The current item's `### H3`+ sub-section path, shown as a dim breadcrumb
    // directly above the card so the single-card view still says which
    // sub-section you're in — the overview's divider isn't visible on narrow
    // terminals.
    let breadcrumb = state
        .current_item()
        .and_then(|item| section_breadcrumb(&item.section));
    // The heading rows sit directly above the card: the `## H2` title and the
    // banner (sub-header) are adjacent with no blank between them, then the
    // sub-section breadcrumb (nearest the card), then a single blank spacer
    // separates the block from the card — mirroring the one-row gap below the
    // card before the link panel, so the card sits symmetrically.
    let heading_rows = (has_h2 as u16) + (banner.is_some() as u16) + (breadcrumb.is_some() as u16);
    let reserved = if heading_rows > 0 {
        heading_rows + 1
    } else {
        0
    };

    let content_h = if let Some(item) = state.current_item() {
        desired_card_height(item, area, state.palette.code_bg)
    } else {
        MIN_CARD_HEIGHT.min(area.height)
    };

    // Center the heading rows and the card *together* as a single block
    // so the heading stays tied to the card instead of floating at the top of
    // the area with the card centered below it. When there isn't room for
    // both, fall back to just centering the card (heading dropped, as before).
    let row = if reserved > 0 && area.height > content_h + reserved {
        let [block] = Layout::vertical([Constraint::Length(reserved + content_h)])
            .flex(Flex::Center)
            .areas(area);
        let [top, card] =
            Layout::vertical([Constraint::Length(reserved), Constraint::Length(content_h)])
                .areas(block);
        let rows = Layout::vertical(vec![Constraint::Length(1); reserved as usize]).split(top);
        let mut i = 0;
        if has_h2 {
            render_list_heading(
                frame,
                rows[i],
                &state.current_list().title,
                state.icons,
                state.palette,
            );
            i += 1;
        }
        if let Some(banner) = &banner {
            render_list_banner(frame, rows[i], banner, state.icons, state.palette);
            i += 1;
        }
        if let Some(crumb) = &breadcrumb {
            render_section_breadcrumb(frame, rows[i], crumb, current_card_width(area));
        }
        // The final reserved row (`rows[reserved - 1]`) is left blank: the
        // one-line gap above the card that mirrors the gap below it.
        card
    } else {
        card_row(area, content_h)
    };

    if area.width < NARROW_THRESHOLD || row.width < MIN_STACK_WIDTH {
        render_current_card(frame, row, area, state);
        render_link_panel_below(frame, area, row, state);
        return;
    }

    // Stack geometry: the center card is full height and painted
    // last; the side cards are tucked down and shortened, their inner
    // columns extending underneath the center card.
    let center_width = (row.width * CENTER_CARD_PCT / 100).max(20);
    let center_x = row.x + (row.width - center_width) / 2;
    let center_right = center_x + center_width;
    let side_height = row.height.saturating_sub(2 * STACK_TUCK);

    // Anchor both side cards to the center card's edges (not the row's), each
    // tucked STACK_OVERLAP columns under it, so an odd leftover width can't
    // leave a 1-column gap between the center card and the next (right) card.
    let prev_rect = Rect {
        x: row.x,
        y: row.y + STACK_TUCK,
        width: (center_x + STACK_OVERLAP).saturating_sub(row.x),
        height: side_height,
    };
    let next_x = center_right.saturating_sub(STACK_OVERLAP);
    let next_rect = Rect {
        x: next_x,
        y: row.y + STACK_TUCK,
        width: row.right().saturating_sub(next_x),
        height: side_height,
    };
    let center_rect = Rect {
        x: center_x,
        y: row.y,
        width: center_width,
        height: row.height,
    };

    let list = state.current_list();
    let items = &list.items;
    let index = state.current_item_index;
    let prev_idx = index.checked_sub(1).filter(|&i| i < items.len());
    let next_idx = Some(index + 1).filter(|&i| i < items.len());
    let prev_item = prev_idx.map(|i| &items[i]);
    let next_item = next_idx.map(|i| &items[i]);
    let prev_info = prev_idx.and_then(|i| list.info_parent_state(i));
    let next_info = next_idx.and_then(|i| list.info_parent_state(i));

    render_side_card(
        frame,
        prev_rect,
        prev_item,
        prev_info,
        state.icons,
        state.palette,
    );
    render_side_card(
        frame,
        next_rect,
        next_item,
        next_info,
        state.icons,
        state.palette,
    );
    // ratatui doesn't blank cells under later widgets, so the center card
    // must explicitly clear the area it covers.
    frame.render_widget(Clear, center_rect);
    render_current_card(frame, center_rect, area, state);
    // The link URLs sit in the empty space below the (centered) card.
    render_link_panel_below(frame, area, center_rect, state);
}

/// The current list's `## H2` title, centered above the cards, in the same
/// color as the overview's current-list row (`palette.current` bold).
fn render_list_heading(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    icons: IconSet,
    palette: Palette,
) {
    let heading = Line::from(Span::styled(
        format!("{} {title}", icons.list),
        Style::default()
            .fg(palette.current)
            .add_modifier(Modifier::BOLD),
    ))
    .centered();
    frame.render_widget(Paragraph::new(heading), area);
}

/// A list's banner: a leading bold-only bullet, shown below the list
/// title as a non-navigable warning line in `palette.warning`, prefixed with
/// the info icon.
fn render_list_banner(
    frame: &mut Frame,
    area: Rect,
    banner: &str,
    icons: IconSet,
    palette: Palette,
) {
    let line = Line::from(Span::styled(
        format!("{} {banner}", icons.note),
        Style::default()
            .fg(palette.warning)
            .add_modifier(Modifier::BOLD),
    ))
    .centered();
    frame.render_widget(Paragraph::new(line), area);
}

/// The item's `### H3`+ sub-section path as a ` › `-joined breadcrumb, or
/// `None` when the item sits directly under its `## H2`.
fn section_breadcrumb(section: &[SubHeading]) -> Option<String> {
    (!section.is_empty()).then(|| {
        section
            .iter()
            .map(|h| h.text.as_str())
            .collect::<Vec<_>>()
            .join(" › ")
    })
}

/// The sub-section breadcrumb: a dim, centered line just above the card
/// naming the `### H3`+ path the current item lives under. The path is clamped
/// to the card's width (with an ellipsis) so a deep or long path can't overflow
/// the card on a narrow terminal, matching the list tabs' truncation.
fn render_section_breadcrumb(frame: &mut Frame, area: Rect, crumb: &str, max_width: u16) {
    let line = Line::from(Span::styled(
        super::truncate(crumb, max_width as usize),
        Style::default().add_modifier(Modifier::DIM),
    ))
    .centered();
    frame.render_widget(Paragraph::new(line), area);
}

/// State icon + bold title (when the item has one) for the top border.
/// The top-border line: just the task-state icon (`☑`/`◐`/`☐`/note) in its
/// state color. The card title (`Item.header`) is no longer shown here — it
/// moves inside the card body (`card_title_lines`).
fn header_line(
    item: &Item,
    info_state: Option<TaskState>,
    icons: IconSet,
    palette: Palette,
) -> Line<'static> {
    let (icon, icon_style) = match item.kind {
        ItemKind::Checkbox(TaskState::Done) => (icons.done, Style::default().fg(palette.done)),
        ItemKind::Checkbox(TaskState::Started) => {
            (icons.started, Style::default().fg(palette.started))
        }
        ItemKind::Checkbox(TaskState::NotStarted) => (icons.pending, Style::default()),
        // The note glyph is unchanged (shape still says "information"); only
        // its color reflects the sub-list's aggregate state, staying the
        // plain note blue when nothing is under way.
        ItemKind::DisplayOnly => {
            let color = match info_state {
                Some(TaskState::Done) => palette.done,
                Some(TaskState::Started) => palette.started,
                _ => palette.note,
            };
            (icons.note, Style::default().fg(color))
        }
    };
    Line::from(Span::styled(format!(" {icon} "), icon_style))
}

/// The card title shown inside the card: the item's leading
/// `**bold**` (`Item.header`), prefixed with the info icon like a list banner
/// but in `palette.note` (blue) rather than the banner's amber. Returns
/// the title line plus a blank separator, or nothing when the item has no
/// leading bold.
/// A dim "you are here" breadcrumb of the current item's parent chain, shown
/// above the card title for a nested item — the sub-list header.
/// `None` for a top-level item. Labels are each parent's card title (`header`)
/// or `display_text`, joined with ` › ` and terminated with ` ›`; truncated
/// from the front (keeping the nearest parent) when it doesn't fit.
fn breadcrumb_line(
    list: &List,
    sublist_base: usize,
    index: usize,
    width: u16,
    palette: Palette,
) -> Option<Line<'static>> {
    let chain = list.parent_chain(index);
    if chain.is_empty() {
        return None;
    }
    let labels: Vec<&str> = chain
        .iter()
        .map(|&i| {
            let it = &list.items[i];
            it.header.as_deref().unwrap_or(it.display_text.as_str())
        })
        .collect();
    let text = format!("{} ›", labels.join(" › "));

    let width = width.max(1) as usize;
    let n = text.chars().count();
    let text = if n > width {
        let skip = n - width.saturating_sub(1);
        format!("…{}", text.chars().skip(skip).collect::<String>())
    } else {
        text
    };
    // The breadcrumb is the card-side depth cue, so it takes the same color as
    // the overview's guide for this item's own sub-list — the document-wide slot
    // of its immediate parent — no dim, so it stays legible.
    let slot = sublist_base + chain.last().map_or(0, |&parent| list.sublist_slot(parent));
    Some(
        Line::from(Span::styled(
            text,
            Style::default().fg(palette.depth_color(slot)),
        ))
        .centered(),
    )
}

fn card_title_lines(item: &Item, icons: IconSet, palette: Palette) -> Vec<Line<'static>> {
    match &item.header {
        // The title text uses the terminal's **default** foreground (bold) so it
        // always contrasts with the background and stays legible (a
        // colored title could wash out on some backgrounds); the leading note
        // icon keeps `palette.note` (blue) as a small accent.
        Some(header) => vec![
            Line::from(vec![
                Span::styled(
                    format!("{} ", icons.note),
                    Style::default().fg(palette.note),
                ),
                Span::styled(
                    header.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])
            .centered(),
            Line::from(""),
        ],
        None => Vec::new(),
    }
}

fn item_block(
    item: &Item,
    info_state: Option<TaskState>,
    icons: IconSet,
    palette: Palette,
    position: Option<(usize, usize)>,
) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title_top(header_line(item, info_state, icons, palette).left_aligned());

    if let Some((n, m)) = position {
        let style = match accent_color(item, info_state, palette) {
            Some(color) => Style::default().fg(color),
            None => Style::default(),
        };
        block =
            block.title_top(Line::from(Span::styled(format!(" {n}/{m} "), style)).right_aligned());
    }

    block
}

/// A compact pilot-style dot strip for the current list's items:
/// current `◉` in the accent color, done/started in their state colors,
/// pending `·` dim.
fn position_dots(list: &List, current: usize, palette: Palette) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, item) in list.items.iter().enumerate() {
        let (glyph, style) = if i == current {
            ("◉", Style::default().fg(palette.current))
        } else {
            match item.kind {
                ItemKind::Checkbox(TaskState::Done) => ("●", Style::default().fg(palette.done)),
                ItemKind::Checkbox(TaskState::Started) => {
                    ("●", Style::default().fg(palette.started))
                }
                // Info items keep the small `·` (not a task, no progress of
                // their own) but in note blue, so the strip stays a faithful
                // item map while reading as information, not a pending task.
                ItemKind::DisplayOnly => ("·", Style::default().fg(palette.note)),
                ItemKind::Checkbox(TaskState::NotStarted) => {
                    ("·", Style::default().add_modifier(Modifier::DIM))
                }
            }
        };
        spans.push(Span::styled(glyph, style));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn render_side_card(
    frame: &mut Frame,
    area: Rect,
    item: Option<&Item>,
    info_state: Option<TaskState>,
    icons: IconSet,
    palette: Palette,
) {
    let Some(item) = item else {
        return; // no neighbor: leave the space empty
    };

    let border_style = match accent_color(item, info_state, palette) {
        Some(color) => Style::default().fg(color).add_modifier(Modifier::DIM),
        None => Style::default().add_modifier(Modifier::DIM),
    };

    let block = item_block(item, info_state, icons, palette, None)
        .border_style(border_style)
        .padding(Padding::horizontal(BODY_PAD));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = wrap_text(&item.display_text, inner.width);
    // Vertically center like the current card, prepending blank lines
    // rather than top-anchoring under the header border.
    let mut padded = vec![String::new(); top_pad(lines.len(), inner.height as usize)];
    padded.extend(lines);
    let paragraph = Paragraph::new(padded.join("\n"))
        .style(Style::default().add_modifier(Modifier::DIM))
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, inner);
}

fn render_current_card(frame: &mut Frame, area: Rect, screen_area: Rect, state: &mut AppState) {
    // Record the card's on-screen area for click-to-copy hit-testing.
    state.card_rect = Some(area);
    let icons = state.icons;
    let palette = state.palette;
    let list_title = state.current_list().title.clone();
    let position = (
        state.current_item_index + 1,
        state.current_list().items.len(),
    );

    let Some(item) = state.current_item() else {
        // Defensive: lists always have items, but AppState can still be
        // constructed with empty lists.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick);
        let paragraph = Paragraph::new(format!("No checklist items in \"{list_title}\"."))
            .block(block)
            .alignment(Alignment::Center);
        frame.render_widget(paragraph, area);
        return;
    };
    let has_links = !item.link_urls().is_empty();

    let info_state = state
        .current_list()
        .info_parent_state(state.current_item_index);
    let done = matches!(item.kind, ItemKind::Checkbox(TaskState::Done));
    let border_style = match accent_color(item, info_state, palette) {
        Some(color) => Style::default().fg(color).add_modifier(Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::BOLD),
    };

    // Display-only notes get a rounded border so they read as information,
    // not an actionable step; tasks get a thick border so an actionable step
    // reads as a bold, solid frame (thick replaced the old double).
    let border_type = if matches!(item.kind, ItemKind::DisplayOnly) {
        BorderType::Rounded
    } else {
        BorderType::Thick
    };
    let mut block = item_block(item, info_state, icons, palette, Some(position))
        .border_type(border_type)
        .border_style(border_style)
        .padding(Padding::horizontal(BODY_PAD));

    // Body layout: checkbox syntax is gone; the body wraps with exact line
    // counts so scrolling can be clamped precisely. Inline code and fenced
    // blocks are styled distinctly. Width comes from the padded inner area
    // so wrapping matches what's rendered.
    let inner = block.inner(area);
    // For a nested item, a dim parent-chain breadcrumb sits above the title as
    // "you are here" context — the sub-list header. Then the card title
    // inside the card, then the body.
    let mut lines: Vec<Line> = Vec::new();
    if let Some(crumb) = breadcrumb_line(
        state.current_list(),
        state.document.sublist_base(state.current_list_index),
        state.current_item_index,
        inner.width,
        palette,
    ) {
        lines.push(crumb);
    }
    lines.extend(card_title_lines(item, icons, palette));
    let n_before = lines.len();
    let (body, code_rows) = body_layout(item, inner.width, palette.code_bg);
    lines.extend(body);
    let visible = inner.height as usize;
    let total_lines = lines.len();
    let max_scroll = clamp_to_u16(total_lines.saturating_sub(visible));
    state.card_max_scroll = max_scroll;
    state.card_viewport_height = inner.height;
    state.card_scroll = state.card_scroll.min(max_scroll);

    // Row-based click-to-copy: map each code row to its on-screen row,
    // accounting for the vertical-centering pad (body fits) or the scroll
    // offset (body overflows), and clip to the visible body area.
    state.code_regions.clear();
    let scroll = state.card_scroll as usize;
    let top = if max_scroll == 0 {
        top_pad(lines.len(), visible)
    } else {
        0
    };
    for (k, code) in code_rows.iter().enumerate() {
        let Some(text) = code else { continue };
        let line_idx = n_before + k;
        let vis_row = if max_scroll == 0 {
            top + line_idx
        } else if line_idx >= scroll {
            line_idx - scroll
        } else {
            continue;
        };
        if vis_row >= visible {
            continue;
        }
        state.code_regions.push((
            Rect::new(inner.x, inner.y + vis_row as u16, inner.width, 1),
            text.clone(),
        ));
    }

    // Pilot-style position strip in the bottom border, and the
    // right-aligned ` first–last/total ` scroll indicator when the body
    // overflows. Budgeted together against the card width — the two
    // used to be decided independently, so ratatui's title truncation could
    // silently eat the dots' current-item marker (`◉`) to make room for the
    // indicator. The indicator is the harder functional need once the body
    // overflows, so it wins and the dots are dropped instead of truncated.
    let list = state.current_list();
    let scroll_title = (max_scroll > 0).then(|| {
        let first = state.card_scroll as usize + 1;
        let last = (state.card_scroll as usize + visible).min(lines.len());
        format!(" {first}–{last}/{} ", lines.len())
    });
    let scroll_width = scroll_title
        .as_ref()
        .map_or(0, |s| s.chars().count() as u16);
    let dots_width = list.items.len() as u16 + 2;
    if !list.items.is_empty() && dots_width + scroll_width <= area.width {
        block = block
            .title_bottom(position_dots(list, state.current_item_index, palette).left_aligned());
    }
    if let Some(scroll_title) = &scroll_title {
        block = block.title_bottom(
            Line::from(Span::styled(
                scroll_title.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ))
            .right_aligned(),
        );
    } else if has_links && !link_panel_fits(screen_area, area) {
        // No room to draw the link panel below the card (short terminal): a
        // border hint so `o` isn't a blind guess. Loses to the scroll
        // indicator on the rare card that's both link-bearing and
        // overflowing, matching the dots-vs-scroll priority above.
        block = block.title_bottom(
            Line::from(Span::styled(" → link ", Style::default().fg(palette.note))).right_aligned(),
        );
    }

    let text: Vec<Line> = if max_scroll == 0 {
        // Fits: vertically center within the card.
        let mut padded = vec![Line::from(""); top_pad(lines.len(), visible)];
        padded.extend(lines);
        padded
    } else {
        // Overflows: top-anchored (the scroll indicator title is added above).
        lines
    };

    let body_style = if done {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };

    let paragraph = Paragraph::new(text)
        .block(block)
        .style(body_style)
        .alignment(Alignment::Center)
        .scroll((state.card_scroll, 0));
    frame.render_widget(paragraph, area);
    // A scrollbar on the right border when the body overflows, alongside
    // the numeric ` first–last/total ` indicator in the bottom border.
    if max_scroll > 0 {
        super::render_scrollbar(
            frame,
            area,
            total_lines,
            visible,
            state.card_scroll as usize,
        );
    }
}

/// Splits an item's body into flowed prose tokens `(text, is_code)` and, when
/// the last *meaningful* span is inline code, that trailing command — which
/// renders on its own line. Trailing whitespace-only text is ignored
/// when deciding whether code is last, and only the final span is split off
/// (earlier inline spans stay in the flow).
/// How a body word-token should be styled. `code` renders the
/// padded chip on `code_bg` (and is what click-to-copy keys off); the rest are
/// inline text modifiers. `link`/`url` mark a link's text (underlined) and its
/// trailing `(url)` (dim).
#[derive(Clone, Copy, Default)]
struct SpanStyle {
    code: bool,
    emphasis: bool,
    strong: bool,
    strikethrough: bool,
    link: bool,
    url: bool,
}

/// One flowed body token: text, its style, and `glue` — when true it abuts the
/// previous token with **no** separating space (so punctuation right after a
/// styled/code/link span stays attached, e.g. `emphasis,` not `emphasis ,`).
type Token = (String, SpanStyle, bool);

/// Splits `text` into word tokens under `style`, appending them to `tokens`.
/// The first word is glued to the previous token unless there was whitespace at
/// the boundary (the previous run ended with space, per `prev_ws`, or this run
/// starts with one). `prev_ws` is updated to this run's trailing-whitespace
/// state for the next call.
fn push_words(tokens: &mut Vec<Token>, prev_ws: &mut bool, text: &str, style: SpanStyle) {
    let leading_ws = text.starts_with(char::is_whitespace);
    let mut first = true;
    for word in text.split_whitespace() {
        let glue = first && !*prev_ws && !leading_ws;
        tokens.push((word.to_string(), style, glue));
        first = false;
    }
    // All-whitespace (or empty) runs leave a space boundary for the next run.
    *prev_ws = text.split_whitespace().next().is_none() || text.ends_with(char::is_whitespace);
}

fn body_tokens(item: &Item) -> (Vec<Token>, Option<String>) {
    if item.body.is_empty() {
        let tokens = item
            .display_text
            .split_whitespace()
            .map(|w| (w.to_string(), SpanStyle::default(), false))
            .collect();
        return (tokens, None);
    }

    let last_meaningful = item.body.iter().rposition(|span| match span {
        BodySpan::Text(t) => !t.trim().is_empty(),
        BodySpan::Styled { text, .. } => !text.trim().is_empty(),
        BodySpan::Code(_) | BodySpan::Link { .. } => true,
    });
    let trailing_idx = match last_meaningful {
        Some(i) if matches!(item.body[i], BodySpan::Code(_)) => Some(i),
        _ => None,
    };

    // The URL is no longer shown inline on the card (it wrapped ugly on
    // narrow cards); it's surfaced in the status bar instead. When a card has
    // several links, each link text is tagged with a dim `[N]` marker so it
    // maps to the numbered status-bar list.
    let link_count = item.link_urls().len();
    let mut link_idx = 0usize;

    let mut tokens = Vec::new();
    let mut trailing = None;
    let mut prev_ws = true; // start of body: nothing to separate from
    for (i, span) in item.body.iter().enumerate() {
        if Some(i) == trailing_idx {
            if let BodySpan::Code(c) = span {
                trailing = Some(c.clone());
            }
            continue;
        }
        match span {
            BodySpan::Text(t) => push_words(&mut tokens, &mut prev_ws, t, SpanStyle::default()),
            BodySpan::Styled { text, style } => push_words(
                &mut tokens,
                &mut prev_ws,
                text,
                SpanStyle {
                    emphasis: style.emphasis,
                    strong: style.strong,
                    strikethrough: style.strikethrough,
                    ..Default::default()
                },
            ),
            // The inline chip is space-padded (` c `), so it manages its own
            // spacing: never glued, and it leaves a space boundary after it.
            BodySpan::Code(c) => {
                tokens.push((
                    format!(" {c} "),
                    SpanStyle {
                        code: true,
                        ..Default::default()
                    },
                    false,
                ));
                prev_ws = true;
            }
            // A link renders as just its underlined text; the URL lives
            // in the status bar, not inline. With several links on one card, a
            // dim `[N]` marker after the text keys it to the status-bar list.
            BodySpan::Link { text, url: _ } => {
                push_words(
                    &mut tokens,
                    &mut prev_ws,
                    text,
                    SpanStyle {
                        link: true,
                        ..Default::default()
                    },
                );
                link_idx += 1;
                if link_count >= 2 {
                    tokens.push((
                        format!("[{link_idx}]"),
                        SpanStyle {
                            url: true,
                            ..Default::default()
                        },
                        false,
                    ));
                    prev_ws = false;
                }
            }
        }
    }
    (tokens, trailing)
}

/// Builds the current card's body as styled, word-wrapped lines:
/// prose in the base style, inline code and fenced blocks in `code_bg`.
/// Char-count based like `wrap_text`, so line counts stay exact for
/// scroll clamping. Fenced `code_blocks` are appended below the flowed body,
/// preserving their own line breaks (hard-wrapped, not word-wrapped).
fn body_lines(item: &Item, width: u16, code_bg: Color) -> Vec<Line<'static>> {
    body_layout(item, width, code_bg).0
}

/// The inline code on a flowed prose line, recovered from the first
/// code-background span (its text is the padded chip ` cmd `), trimmed to the
/// clean command — the row's copyable text for click-to-copy. `None`
/// when the line carries no inline code. Fenced-block and trailing-command
/// rows carry their clean text directly, so this is only used for prose.
fn line_inline_code(line: &Line, code_bg: Color) -> Option<String> {
    line.spans
        .iter()
        .find(|s| s.style.bg == Some(code_bg))
        .map(|s| s.content.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Builds the current card's body as styled, word-wrapped lines (see
/// `body_lines`) **and**, in lockstep, the copyable code text per output line
/// for row-based click-to-copy: `Some(clean code)` for an inline-code
/// row, the trailing command, or a fenced-block row; `None` otherwise. The two
/// vecs are the same length and index-aligned.
fn body_layout(
    item: &Item,
    width: u16,
    code_bg: Color,
) -> (Vec<Line<'static>>, Vec<Option<String>>) {
    let width = width.max(1) as usize;

    // Flow prose + inline code as styled word tokens; a trailing inline code
    // span is split off to render on its own line. Inline code is kept
    // as a single padded chip so a short command stays contiguous.
    let (tokens, trailing_code) = body_tokens(item);

    // Center short prose, left-align it once it wraps past a couple of
    // lines (centered multi-line prose is tiring to read). The trailing
    // command line, if any, shares the prose alignment.
    let prose = wrap_tokens(&tokens, width, code_bg);
    let center_prose = prose.len() <= PROSE_CENTER_MAX_LINES;
    let align = |line: Line<'static>| {
        if center_prose {
            line.centered()
        } else {
            line.left_aligned()
        }
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut code_rows: Vec<Option<String>> = Vec::new();

    for line in prose {
        let code = line_inline_code(&line, code_bg);
        lines.push(align(line));
        code_rows.push(code);
    }

    // A trailing command drops onto its own line, with a blank line
    // separating it from any prose above (nothing to separate when the body
    // is only the command).
    if let Some(code) = trailing_code {
        if !lines.is_empty() {
            lines.push(Line::from(""));
            code_rows.push(None);
        }
        let chip = SpanStyle {
            code: true,
            ..Default::default()
        };
        for line in wrap_tokens(&[(format!(" {code} "), chip, false)], width, code_bg) {
            lines.push(align(line));
            code_rows.push(Some(code.clone()));
        }
    }

    // Fenced blocks below, left-aligned in a light box, with a blank
    // separator when there's flowed prose above them. Every box row maps to
    // the block's clean text so a click anywhere on the box copies it.
    for block in &item.code_blocks {
        if !lines.is_empty() {
            lines.push(Line::from(""));
            code_rows.push(None);
        }
        for line in code_block_lines(block, width, code_bg) {
            lines.push(line);
            code_rows.push(Some(block.clone()));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
        code_rows.push(None);
    }
    (lines, code_rows)
}

/// Renders one fenced block as left-aligned, code-colored lines inside a
/// light box. Preserves the block's own line breaks (hard-wrapped to
/// fit); falls back to plain left-aligned lines when the card is too narrow
/// for a box. Deterministic line count, so scroll clamping stays exact.
fn code_block_lines(block: &str, width: usize, code_bg: Color) -> Vec<Line<'static>> {
    let style = Style::default().bg(code_bg);

    // Not enough room for "│ x │": drop the box, keep left-aligned code.
    if width < 6 {
        return block
            .lines()
            .flat_map(|raw| hard_wrap(raw, width))
            .map(|c| Line::styled(c, style).left_aligned())
            .collect();
    }

    let content_w = width - 4; // "│ " + content + " │"
    let chunks: Vec<String> = block
        .lines()
        .flat_map(|raw| hard_wrap(raw, content_w))
        .collect();
    // The box always spans the full card-body width — the gray
    // background reaches both edges even for a short one-line block — so code
    // blocks align regardless of content length, matching a rendered Markdown
    // block. (Previously `box_w` shrank to the longest content line.)
    let box_w = content_w;

    // A blank gray row just inside the top and bottom borders gives the code
    // vertical breathing room, like a rendered Markdown block.
    let blank = || Line::styled(format!("│ {} │", " ".repeat(box_w)), style).left_aligned();

    let mut out = Vec::with_capacity(chunks.len() + 4);
    out.push(Line::styled(format!("┌{}┐", "─".repeat(box_w + 2)), style).left_aligned());
    out.push(blank());
    for c in &chunks {
        let pad = box_w.saturating_sub(c.chars().count());
        out.push(Line::styled(format!("│ {c}{} │", " ".repeat(pad)), style).left_aligned());
    }
    out.push(blank());
    out.push(Line::styled(format!("└{}┘", "─".repeat(box_w + 2)), style).left_aligned());
    out
}

/// Greedy word wrap over styled tokens `(text, is_code)`, char-count based
/// and hard-splitting overlong tokens. Same wrapping contract as
/// `wrap_text` but emits styled `Line`s with code fragments in `code_bg`.
fn wrap_tokens(tokens: &[Token], width: usize, code_bg: Color) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut cur_len = 0usize;

    let span_for = |text: String, style: SpanStyle| {
        if style.code {
            // Inline code keeps its background chip; it's also how
            // click-to-copy recognises a code row (line_inline_code).
            return Span::styled(text, Style::default().bg(code_bg));
        }
        let mut modifier = Modifier::empty();
        if style.emphasis {
            modifier |= Modifier::ITALIC;
        }
        if style.strong {
            modifier |= Modifier::BOLD;
        }
        if style.strikethrough {
            modifier |= Modifier::CROSSED_OUT;
        }
        if style.link {
            modifier |= Modifier::UNDERLINED;
        }
        if style.url {
            modifier |= Modifier::DIM;
        }
        Span::styled(text, Style::default().add_modifier(modifier))
    };

    for (word, style, glue) in tokens {
        let mut rest = word.as_str();
        // Glued tokens abut the previous one with no separating space (unless
        // this is the first token on the line). Only the first physical piece
        // of a hard-split token inherits the glue.
        let glued = *glue;
        loop {
            let sep = usize::from(!current.is_empty() && !glued);
            let word_len = rest.chars().count();
            if cur_len + sep + word_len <= width {
                if sep == 1 {
                    current.push(Span::raw(" "));
                }
                current.push(span_for(rest.to_string(), *style));
                cur_len += sep + word_len;
                break;
            }
            if current.is_empty() {
                // Hard-split a token longer than the whole line.
                let split: String = rest.chars().take(width).collect();
                let rest_start = split.len();
                lines.push(Line::from(span_for(split, *style)));
                rest = &rest[rest_start..];
                if rest.is_empty() {
                    break;
                }
            } else {
                // Flush the current line and retry the word on a fresh one.
                lines.push(Line::from(std::mem::take(&mut current)));
                cur_len = 0;
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Hard-wraps a single line to `width` by character count (no word
/// breaking), used for fenced code where original layout matters.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// Greedy word wrap on character counts, hard-splitting overlong words.
/// Used instead of ratatui's internal wrapping so line counts are exact
/// for scroll clamping. Known limitation: counts chars, not display
/// width, so wide glyphs may wrap a little early.
fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let fits =
                current.chars().count() + word.chars().count() + usize::from(!current.is_empty());
            if fits <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if current.is_empty() {
                // Hard-split a word longer than the line.
                let split: String = word.chars().take(width).collect();
                let rest_start = split.len();
                lines.push(split);
                word = &word[rest_start..];
                if word.is_empty() {
                    break;
                }
            } else {
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn completion_card(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    border_color: Color,
    lines: Vec<Line>,
) {
    // Size the card to its content (+ borders) so no line is clipped on
    // shorter terminals; centered by `card_row`.
    let height = (lines.len() as u16 + 2).clamp(MIN_CARD_HEIGHT, area.height);
    let row = card_row(area, height);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(format!(" {title} "));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, row);
}

fn render_list_complete(frame: &mut Frame, area: Rect, state: &AppState) {
    let icons = state.icons;
    let list = state.current_list();
    let (done, total) = list.checkbox_stats();

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} List Complete", icons.done),
            Style::default()
                .fg(state.palette.done)
                .add_modifier(Modifier::BOLD),
        )),
        completion_rule(),
        Line::from(""),
        Line::from(list.title.clone()),
        Line::from(format!("{done} / {total} tasks completed")),
        Line::from(""),
        Line::from("Press  l / Enter  to go to next list"),
        Line::from("Press  h  to review tasks"),
    ];
    completion_card(frame, area, "List Complete", state.palette.done, lines);
}

fn render_all_complete(frame: &mut Frame, area: Rect, state: &AppState) {
    let icons = state.icons;
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} All Tasks Complete", icons.done),
            Style::default()
                .fg(state.palette.done)
                .add_modifier(Modifier::BOLD),
        )),
        completion_rule(),
        Line::from(""),
    ];

    let (total_done, total_all) = state.document.checkbox_stats();
    for list in &state.document.lists {
        let (done, total) = list_stats_line(list);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", icons.done),
                Style::default().fg(state.palette.done),
            ),
            Span::raw(format!(" {}  {done} / {total}", list.title)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "Total: {total_done} / {total_all} tasks"
    )));
    lines.push(Line::from(""));
    lines.push(Line::from("Press  R  to reset · h to review · q to quit"));

    completion_card(frame, area, "All Tasks Complete", state.palette.done, lines);
}

fn list_stats_line(list: &List) -> (usize, usize) {
    list.checkbox_stats()
}

/// A dim rule line used as a divider on the completion screens.
fn completion_rule() -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(20),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

fn render_confirm_reset(frame: &mut Frame, area: Rect, state: &AppState) {
    let (done, total) = state.document.checkbox_stats();
    let file_name = state
        .document
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "the file".to_string());

    let row = card_row(area, fixed_card_height(area));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.palette.started))
        .title(" Confirm Reset ");

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Reset checklist?",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "This marks all {done} completed tasks ({done}/{total}) as not done"
        )),
        Line::from(format!("and rewrites {file_name}.")),
        Line::from(""),
        Line::from("Press  y  to reset"),
        Line::from("Press any other key to cancel"),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, row);
}

fn render_confirm_quit_reset(frame: &mut Frame, area: Rect, state: &AppState) {
    let (_, total) = state.document.checkbox_stats();

    let row = card_row(area, fixed_card_height(area));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(state.palette.done))
        .title(" All Tasks Complete ");

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "All tasks are done.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Reset all {total} tasks to not done before quitting,"
        )),
        Line::from("so the checklist is ready to run again?"),
        Line::from(""),
        Line::from("Press  y  to reset and quit"),
        Line::from("Press  n  to quit without resetting"),
        Line::from("Press  Esc  to keep working"),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TextStyle;

    #[test]
    fn wrap_exact_fit_stays_one_line() {
        assert_eq!(wrap_text("ab cd", 5), vec!["ab cd".to_string()]);
    }

    #[test]
    fn wrap_splits_on_word_boundaries() {
        assert_eq!(
            wrap_text("alpha beta gamma", 11),
            vec!["alpha beta".to_string(), "gamma".to_string()]
        );
    }

    #[test]
    fn wrap_hard_splits_overlong_word() {
        assert_eq!(
            wrap_text("abcdefghij", 4),
            vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string()]
        );
    }

    #[test]
    fn wrap_empty_text_yields_single_empty_line() {
        assert_eq!(wrap_text("", 10), vec![String::new()]);
    }

    fn body_item(body: Vec<BodySpan>, blocks: Vec<&str>, display: &str) -> Item {
        Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: display.to_string(),
            body,
            header: None,
            code_spans: vec![],
            code_blocks: blocks.into_iter().map(str::to_string).collect(),
            kind: ItemKind::Checkbox(TaskState::NotStarted),
        }
    }

    fn kind_item(kind: ItemKind) -> Item {
        let mut item = body_item(vec![], vec![], "item");
        item.kind = kind;
        item
    }

    #[test]
    fn state_color_gives_an_info_parent_its_aggregate_accent() {
        let pal = Palette::truecolor();
        let info = kind_item(ItemKind::DisplayOnly);
        // A display-only parent borrows the task accents from its sub-list's
        // aggregate state, and stays unaccented when nothing's begun.
        assert_eq!(
            state_color(&info, Some(TaskState::Done), pal),
            Some(pal.done)
        );
        assert_eq!(
            state_color(&info, Some(TaskState::Started), pal),
            Some(pal.started)
        );
        assert_eq!(state_color(&info, None, pal), None);
        // A checkbox carries its own state and ignores the info aggregate.
        let task = kind_item(ItemKind::Checkbox(TaskState::Done));
        assert_eq!(state_color(&task, None, pal), Some(pal.done));
    }

    #[test]
    fn accent_color_falls_back_to_note_blue_for_idle_info_cards() {
        let pal = Palette::truecolor();
        let info = kind_item(ItemKind::DisplayOnly);
        // An idle info card's frame takes the note blue (matching its icon)
        // instead of the default, while an active sub-list keeps the state accent.
        assert_eq!(accent_color(&info, None, pal), Some(pal.note));
        assert_eq!(
            accent_color(&info, Some(TaskState::Started), pal),
            Some(pal.started)
        );
        assert_eq!(
            accent_color(&info, Some(TaskState::Done), pal),
            Some(pal.done)
        );
        // Tasks are unaffected: a pending task frame stays default (None).
        let pending = kind_item(ItemKind::Checkbox(TaskState::NotStarted));
        assert_eq!(accent_color(&pending, None, pal), None);
        let done = kind_item(ItemKind::Checkbox(TaskState::Done));
        assert_eq!(accent_color(&done, None, pal), Some(pal.done));
    }

    #[test]
    fn position_dots_tint_info_items_apart_from_pending_tasks() {
        let pal = Palette::truecolor();
        let list = List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                kind_item(ItemKind::DisplayOnly),
                kind_item(ItemKind::Checkbox(TaskState::NotStarted)),
            ],
        };
        // current = the pending task (index 1), so neither dot is the ◉ marker.
        let dots = position_dots(&list, 1, pal);
        let info = &dots.spans[1]; // spans[0] is the leading pad space
        let pending = &dots.spans[2];
        assert_eq!(info.content, "·");
        assert_eq!(info.style.fg, Some(pal.note), "info dot is note-blue");
        assert_eq!(pending.content, "◉", "the pending task is current here");
        // Make the info item non-current so the pending dot shows its dim ·.
        let dots = position_dots(&list, 0, pal);
        let pending = &dots.spans[2];
        assert_eq!(
            pending.style.fg, None,
            "a pending task stays a dim ·, not blue"
        );
    }

    /// Every (text, background) pair across all lines, for content+style
    /// assertions (code is styled with a background).
    fn span_pairs(lines: &[Line]) -> Vec<(String, Option<Color>)> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| (s.content.to_string(), s.style.bg)))
            .collect()
    }

    #[test]
    fn body_lines_style_inline_code_distinctly() {
        let item = body_item(
            vec![
                BodySpan::Text("run".to_string()),
                BodySpan::Code("cmd".to_string()),
            ],
            vec![],
            "run cmd",
        );
        let pairs = span_pairs(&body_lines(&item, 40, Color::Magenta));
        assert!(
            pairs
                .iter()
                .any(|(t, bg)| t.contains("cmd") && *bg == Some(Color::Magenta)),
            "inline code sits on the code background (padded chip)"
        );
        assert!(
            pairs.iter().any(|(t, bg)| t == "run" && bg.is_none()),
            "prose keeps the base style (no code background)"
        );
    }

    /// Every (text, modifiers) pair across all lines, for inline-style
    /// assertions (emphasis/strong/strikethrough/link).
    fn span_mods(lines: &[Line]) -> Vec<(String, Modifier)> {
        lines
            .iter()
            .flat_map(|l| {
                l.spans
                    .iter()
                    .map(|s| (s.content.to_string(), s.style.add_modifier))
            })
            .collect()
    }

    #[test]
    fn body_lines_style_emphasis_strong_and_strikethrough() {
        let item = body_item(
            vec![
                BodySpan::Styled {
                    text: "italicword".to_string(),
                    style: TextStyle {
                        emphasis: true,
                        ..Default::default()
                    },
                },
                BodySpan::Text(" plain ".to_string()),
                BodySpan::Styled {
                    text: "boldword".to_string(),
                    style: TextStyle {
                        strong: true,
                        ..Default::default()
                    },
                },
                BodySpan::Text(" ".to_string()),
                BodySpan::Styled {
                    text: "crossed".to_string(),
                    style: TextStyle {
                        strikethrough: true,
                        ..Default::default()
                    },
                },
            ],
            vec![],
            "italicword plain boldword crossed",
        );
        let mods = span_mods(&body_lines(&item, 60, Color::Magenta));
        assert!(
            mods.iter()
                .any(|(t, m)| t == "italicword" && m.contains(Modifier::ITALIC))
        );
        assert!(
            mods.iter()
                .any(|(t, m)| t == "boldword" && m.contains(Modifier::BOLD))
        );
        assert!(
            mods.iter()
                .any(|(t, m)| t == "crossed" && m.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn body_lines_render_link_as_underlined_text_without_inline_url() {
        // A single link shows only its underlined text; the URL is not
        // rendered inline (it's surfaced in the status bar), and a lone link
        // gets no `[N]` marker.
        let item = body_item(
            vec![
                BodySpan::Text("see ".to_string()),
                BodySpan::Link {
                    text: "runbook".to_string(),
                    url: "https://ex.com/rb".to_string(),
                },
            ],
            vec![],
            "see runbook",
        );
        let lines = body_lines(&item, 60, Color::Magenta);
        let whole: String = line_texts(&lines).join(" ");
        assert!(whole.contains("runbook"), "link text shown: {whole:?}");
        assert!(!whole.contains("ex.com"), "URL not shown inline: {whole:?}");
        assert!(!whole.contains('['), "no marker for a lone link: {whole:?}");
        let mods = span_mods(&lines);
        assert!(
            mods.iter()
                .any(|(t, m)| t == "runbook" && m.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn body_lines_tag_multiple_links_with_dim_numbered_markers() {
        // With 2+ links, each link text is followed by a dim `[N]`
        // marker (document order) so it maps to the numbered status-bar list.
        let item = body_item(
            vec![
                BodySpan::Text("see ".to_string()),
                BodySpan::Link {
                    text: "runbook".to_string(),
                    url: "https://ex.com/rb".to_string(),
                },
                BodySpan::Text(" and ".to_string()),
                BodySpan::Link {
                    text: "wiki".to_string(),
                    url: "https://ex.com/wiki".to_string(),
                },
            ],
            vec![],
            "see runbook and wiki",
        );
        let lines = body_lines(&item, 60, Color::Magenta);
        let whole: String = line_texts(&lines).join(" ");
        assert!(whole.contains("runbook [1]"), "first marker: {whole:?}");
        assert!(whole.contains("wiki [2]"), "second marker: {whole:?}");
        assert!(!whole.contains("ex.com"), "no inline URL: {whole:?}");
        let mods = span_mods(&lines);
        assert!(
            mods.iter()
                .any(|(t, m)| t == "[1]" && m.contains(Modifier::DIM)),
            "marker is dim: {mods:?}"
        );
    }

    #[test]
    fn punctuation_after_a_styled_span_stays_attached() {
        // A comma right after an emphasized word must not get a spurious
        // space (the styled span is a separate run from the following text).
        let item = body_item(
            vec![
                BodySpan::Text("say ".to_string()),
                BodySpan::Styled {
                    text: "hi".to_string(),
                    style: TextStyle {
                        emphasis: true,
                        ..Default::default()
                    },
                },
                BodySpan::Text(", ok".to_string()),
            ],
            vec![],
            "say hi, ok",
        );
        let whole = line_texts(&body_lines(&item, 40, Color::Magenta)).join(" ");
        assert!(
            whole.contains("hi,"),
            "comma attaches to the word: {whole:?}"
        );
        assert!(!whole.contains("hi ,"), "no spurious space: {whole:?}");
    }

    /// Content of each line, concatenated per line.
    fn line_texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn trailing_inline_command_breaks_to_its_own_line() {
        // A trailing inline code span drops onto its own line, with a
        // blank line between it and the prose above.
        let item = body_item(
            vec![
                BodySpan::Text("run this".to_string()),
                BodySpan::Code("cmd".to_string()),
            ],
            vec![],
            "run this cmd",
        );
        let lines = body_lines(&item, 40, Color::Magenta);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 3, "prose, blank, command: {texts:?}");
        assert_eq!(texts[0], "run this");
        assert!(texts[1].trim().is_empty(), "blank separator");
        assert!(texts[2].contains("cmd"), "command on its own line");
        // The command line is the styled code chip.
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|s| s.style.bg == Some(Color::Magenta)),
            "command line is a code chip"
        );
    }

    #[test]
    fn inline_code_followed_by_text_stays_inline() {
        // Code that is not the last span keeps flowing — no break.
        let item = body_item(
            vec![
                BodySpan::Text("run".to_string()),
                BodySpan::Code("x".to_string()),
                BodySpan::Text("and wait".to_string()),
            ],
            vec![],
            "run x and wait",
        );
        let lines = body_lines(&item, 40, Color::Magenta);
        let texts = line_texts(&lines);
        assert!(
            !texts.iter().any(|t| t.trim().is_empty()),
            "no blank separator; code stays inline: {texts:?}"
        );
        let code_line: String = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.style.bg == Some(Color::Magenta)))
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .unwrap();
        assert!(
            code_line.contains("run") || code_line.contains("wait"),
            "code shares a line with prose: {code_line:?}"
        );
    }

    #[test]
    fn body_of_only_a_command_has_no_leading_blank() {
        // With nothing above it, the command needs no separator.
        let item = body_item(
            vec![BodySpan::Code("refresh-cache".to_string())],
            vec![],
            "refresh-cache",
        );
        let lines = body_lines(&item, 40, Color::Magenta);
        let texts = line_texts(&lines);
        assert_eq!(texts.len(), 1, "just the command line: {texts:?}");
        assert!(texts[0].contains("refresh-cache"));
    }

    #[test]
    fn body_lines_box_fenced_code_left_aligned_with_code_background() {
        let item = body_item(
            vec![BodySpan::Text("do this".to_string())],
            vec!["first line\nsecond line"],
            "do this",
        );
        let lines = body_lines(&item, 40, Color::Magenta);
        // Each code line's content appears inside a boxed, code-background,
        // left-aligned line.
        for expected in ["first line", "second line"] {
            let found = lines.iter().any(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let is_code_bg = line.style.bg == Some(Color::Magenta)
                    || (!line.spans.is_empty()
                        && line
                            .spans
                            .iter()
                            .all(|s| s.style.bg == Some(Color::Magenta)));
                text.contains(expected) && line.alignment == Some(Alignment::Left) && is_code_bg
            });
            assert!(found, "boxed code line for {expected:?} not found");
        }
        // The box borders are present.
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all.contains('┌') && all.contains('└'),
            "box drawn around code"
        );
    }

    #[test]
    fn code_block_box_spans_full_width_even_when_short() {
        // A short one-line block still fills the full card-body width, so
        // every box row (borders, blanks, content) is exactly `width` wide.
        let width = 30usize;
        let lines = code_block_lines("hi", width, Color::Magenta);
        assert!(lines.len() >= 5, "top + blank + content + blank + bottom");
        for line in &lines {
            let w: usize = line.spans.iter().flat_map(|s| s.content.chars()).count();
            assert_eq!(w, width, "every box row spans the full width");
        }
        // The whole box is painted on the code background.
        assert!(
            lines.iter().all(|l| l.style.bg == Some(Color::Magenta)),
            "background reaches both edges"
        );
    }

    #[test]
    fn narrow_code_block_falls_back_without_a_box() {
        // Below the box minimum the fallback is still plain left-aligned
        // code lines (no border), unchanged by the full-width change.
        let lines = code_block_lines("hello world", 4, Color::Magenta);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!all.contains('┌'), "no box at tiny widths");
    }

    #[test]
    fn short_prose_centered_long_prose_left_aligned() {
        let short = body_item(vec![BodySpan::Text("do it".to_string())], vec![], "do it");
        for line in body_lines(&short, 40, Color::Magenta) {
            if !line.spans.is_empty() {
                assert_eq!(
                    line.alignment,
                    Some(Alignment::Center),
                    "short prose centered"
                );
            }
        }
        let long_text = "word ".repeat(60);
        let long = body_item(vec![BodySpan::Text(long_text.clone())], vec![], &long_text);
        let long_lines = body_lines(&long, 20, Color::Magenta);
        assert!(long_lines.len() > PROSE_CENTER_MAX_LINES);
        for line in long_lines {
            assert_eq!(
                line.alignment,
                Some(Alignment::Left),
                "long prose left-aligned"
            );
        }
    }

    #[test]
    fn body_lines_fall_back_to_display_text_when_body_empty() {
        let item = body_item(vec![], vec![], "plain text");
        let pairs = span_pairs(&body_lines(&item, 40, Color::Magenta));
        let text: String = pairs
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("plain"));
        assert!(text.contains("text"));
    }

    #[test]
    fn current_card_width_is_center_pct_in_stack_and_full_when_narrow() {
        // In the stack tier the center card takes CENTER_CARD_PCT of
        // the width; below NARROW_THRESHOLD it's the full width; floored at 20.
        assert_eq!(current_card_width(Rect::new(0, 0, 100, 40)), 72); // 100 * 72%
        assert_eq!(current_card_width(Rect::new(0, 0, 120, 40)), 86); // 120 * 72%
        assert_eq!(
            current_card_width(Rect::new(0, 0, 50, 40)),
            50,
            "narrow tier (< NARROW_THRESHOLD) uses the full width"
        );
        assert_eq!(current_card_width(Rect::new(0, 0, 81, 40)), 58); // 81 * 72% = 58
    }

    #[test]
    fn desired_card_height_grows_with_content_and_clamps() {
        let area = Rect::new(0, 0, 100, 40); // max ≈ 24 rows (60%)
        let short = body_item(vec![BodySpan::Text("do it".to_string())], vec![], "do it");
        assert_eq!(
            desired_card_height(&short, area, Color::Magenta),
            MIN_CARD_HEIGHT,
            "short task uses the minimum height"
        );

        let long_text = "word ".repeat(400);
        let long = body_item(vec![BodySpan::Text(long_text.clone())], vec![], &long_text);
        assert_eq!(
            desired_card_height(&long, area, Color::Magenta),
            (area.height * 3 / 5).max(MIN_CARD_HEIGHT),
            "long task clamps to the max height"
        );
    }

    #[test]
    fn top_pad_centers_and_saturates() {
        assert_eq!(top_pad(2, 10), 4); // (10-2)/2
        assert_eq!(top_pad(3, 10), 3); // floor of 3.5
        assert_eq!(top_pad(10, 10), 0); // exact fit
        assert_eq!(top_pad(12, 10), 0); // overflow: no negative pad
    }

    #[test]
    fn hard_wrap_splits_by_char_width() {
        assert_eq!(
            hard_wrap("abcdef", 4),
            vec!["abcd".to_string(), "ef".to_string()]
        );
        assert_eq!(hard_wrap("", 4), vec![String::new()]);
    }

    // --- Row-based click-to-copy code mapping. `body_layout` returns a
    // per-output-line code text (index-aligned with the lines).

    #[test]
    fn body_layout_lines_and_code_rows_are_aligned() {
        let item = body_item(vec![], vec!["kubectl apply"], "");
        let (lines, code_rows) = body_layout(&item, 40, Color::Magenta);
        assert_eq!(lines.len(), code_rows.len());
    }

    #[test]
    fn body_layout_maps_fenced_block_rows_to_block_text() {
        let item = body_item(vec![], vec!["kubectl apply"], "");
        let (_, code_rows) = body_layout(&item, 40, Color::Magenta);
        assert!(
            code_rows
                .iter()
                .any(|c| c.as_deref() == Some("kubectl apply")),
            "a fenced-block row copies the clean block text: {code_rows:?}"
        );
    }

    #[test]
    fn body_layout_maps_trailing_command_row() {
        // A trailing inline command drops onto its own row.
        let item = body_item(
            vec![
                BodySpan::Text("run this".to_string()),
                BodySpan::Code("do-it".to_string()),
            ],
            vec![],
            "run this do-it",
        );
        let (_, code_rows) = body_layout(&item, 40, Color::Magenta);
        assert!(
            code_rows.iter().any(|c| c.as_deref() == Some("do-it")),
            "trailing command row copies the command: {code_rows:?}"
        );
    }

    #[test]
    fn body_layout_maps_inline_code_in_prose() {
        // Code with text after it stays inline (not trailing), so it rides a
        // flowed prose row; that row still copies the span.
        let item = body_item(
            vec![
                BodySpan::Text("first".to_string()),
                BodySpan::Code("cmd".to_string()),
                BodySpan::Text("then done".to_string()),
            ],
            vec![],
            "first cmd then done",
        );
        let (_, code_rows) = body_layout(&item, 40, Color::Magenta);
        assert!(
            code_rows.iter().any(|c| c.as_deref() == Some("cmd")),
            "inline code row copies the span: {code_rows:?}"
        );
    }

    #[test]
    fn body_layout_prose_rows_carry_no_code() {
        let item = body_item(
            vec![BodySpan::Text("just prose here".to_string())],
            vec![],
            "just prose here",
        );
        let (_, code_rows) = body_layout(&item, 40, Color::Magenta);
        assert!(
            code_rows.iter().all(|c| c.is_none()),
            "prose-only body has no code rows: {code_rows:?}"
        );
    }
}
