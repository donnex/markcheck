use std::borrow::Cow;
use std::ops::Range;
use std::rc::Rc;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List as ListWidget, ListItem, ListState, Padding, Paragraph,
};

use super::{
    char_width, display_width, expand_tabs, fg, marker_color, split_at_width, state_marker,
};
use crate::model::{
    AppState, BodySpan, IconSet, Item, ItemKind, List, Overlay, Palette, PreambleBlock,
    PreambleKind, ResetScope, SubHeading, TaskState, View,
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

/// A card's border accent, the same color as its state icon: `info_state`
/// is a display-only parent's aggregate child state, and an idle note is
/// note blue.
fn accent_color(item: &Item, info_state: Option<TaskState>, palette: Palette) -> Option<Color> {
    marker_color(item.kind, info_state, palette)
}

/// Below this card-area width a single card is shown instead of the
/// three-card stack.
const NARROW_THRESHOLD: u16 = 80;
const MIN_CARD_HEIGHT: u16 = 8;
/// The center card's share of the stack width. Used both to measure card
/// height and to place the card, which must agree.
const CENTER_CARD_PCT: u16 = 72;
/// How far the side cards extend underneath the center card.
const STACK_OVERLAP: u16 = 4;
/// How far the side cards sit down from the center card's top edge.
const STACK_TUCK: u16 = 1;

pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    match state.overlay {
        Some(Overlay::ConfirmReset) => render_confirm_reset(frame, area, state),
        Some(Overlay::ConfirmQuitReset) => render_confirm_quit_reset(frame, area, state),
        Some(Overlay::Help) => render_help(frame, area, state),
        Some(Overlay::ListPicker) => render_list_picker(frame, area, state),
        // Search keeps the view visible; the query shows in the status bar
        // and the cursor jumps live to the match.
        Some(Overlay::Search) | None => match state.view {
            View::Checklist => render_checklist(frame, area, state),
            View::ListComplete => render_list_complete(frame, area, state),
            View::AllComplete => render_all_complete(frame, area, state),
        },
    }
}

/// The `T` "go to task" overlay: a filterable list of every task.
fn render_list_picker(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let icons = state.icons;
    let palette = state.palette;
    let matches = state.picker_matches();
    let multi_list = state.document.lists.len() > 1;
    // A row's text width inside the borders and padding, less the marker.
    let text_width = area.width.saturating_sub(4 + 2) as usize;

    let rows: Vec<ListItem> = matches
        .iter()
        .map(|&(li, ii)| {
            let item = &state.document.lists[li].items[ii];
            let (marker, color) = state_marker(item.kind, None, icons, palette);
            let marker_style = match color {
                Some(color) => Style::default().fg(color),
                None => Style::default().add_modifier(Modifier::DIM),
            };
            let label = item.label();
            // A dim `— {list} › {sub-section}` context suffix: the
            // list title (only when several lists exist) followed by the item's
            // `### H3`+ sub-section path, so the same visible text can be told
            // apart and the sub-section is discoverable in the picker.
            let mut context: Vec<String> = Vec::new();
            if multi_list {
                context.push(state.document.lists[li].title.clone());
            }
            context.extend(item.section.iter().map(|h| h.text.clone()));
            let suffix = if context.is_empty() {
                String::new()
            } else {
                format!("  — {}", context.join(" › "))
            };
            // A long label must not push the suffix off the row, since the
            // suffix is what tells same-named tasks apart: both keep a share
            // and are cut with `…`.
            let (label_budget, suffix_budget) = super::split_width(
                display_width(&label),
                display_width(&suffix),
                text_width,
                text_width / 2,
            );
            let mut spans = vec![
                Span::styled(format!("{marker} "), marker_style),
                Span::raw(super::truncate(&label, label_budget)),
            ];
            if !suffix.is_empty() {
                spans.push(Span::styled(
                    super::truncate(&suffix, suffix_budget),
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
    state.layout.picker_viewport_height = list_area.height;
    state.layout.picker_rows_area = None;
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
    state.layout.picker_rows_area = Some(list_area);
    state.layout.picker_scroll_offset = list_state.offset();
}

/// One titled group of `?` overlay rows: `(key column, description)`.
pub(crate) type HelpSection = (&'static str, &'static [(&'static str, &'static str)]);

/// Every row of the `?` overlay, grouped by the screen or mode it applies in.
/// The first group (untitled) is the checklist and global keys. The key
/// column's whitespace-separated words are the key names a test matches every
/// handled key against, so name keys consistently (`Enter`, `Esc`, `Ctrl-D`).
pub(crate) const HELP_SECTIONS: &[HelpSection] = &[
    (
        "",
        &[
            ("h j k l  ← → ↑ ↓", "prev / next task (crosses lists)"),
            ("gg / G", "first / last task in the document"),
            ("} / {", "next / previous sub-section"),
            ("Ctrl-E / Ctrl-Y", "scroll card body one line"),
            ("Ctrl-D / Ctrl-U", "scroll card body half a page"),
            ("PageDn / PageUp", "scroll card body one page"),
            (
                "mouse wheel",
                "scroll an overflowing card, else prev/next task",
            ),
            ("wheel in ? / T", "scroll this help / move the T highlight"),
            ("space / Enter", "toggle the task done, then next open task"),
            ("s", "mark the task started / in progress"),
            ("-", "skip the task (doesn't apply this run), or unskip"),
            ("critical step", "space or - twice, pausing between"),
            ("u / Ctrl-R", "undo / redo the last change"),
            ("Ctrl-O", "go back after o followed a #section link"),
            ("Tab", "jump to the next unfinished task"),
            ("Shift-H / Shift-L", "previous / next unfinished list"),
            ("/", "search tasks by text"),
            ("n / N", "next / previous search match"),
            ("T", "go to task (filterable list)"),
            (
                "click T picker row",
                "highlight it; click it again (or Enter) to jump",
            ),
            (
                "click overview row",
                "toggle (icon) or jump to (label) a task or list",
            ),
            ("y", "copy the task's code to the clipboard"),
            ("y then N", "copy command [N] when a card has several"),
            (
                "click a card",
                "copy the clicked command, or the card's sole one",
            ),
            ("o", "open the card's link in your browser"),
            ("o then N", "open link [N] when a card has several"),
            ("e", "edit the file in $EDITOR"),
            ("Ctrl-Z", "suspend to the shell (fg to come back)"),
            ("Ctrl-C", "cancel like Esc; elsewhere hints how to quit"),
            ("R", "reset all, the list or the sub-list (asks first)"),
            ("1-9", "jump to list N"),
            ("?", "toggle this help"),
            (
                "q",
                "quit; during a git sync, wait for it (q again abandons)",
            ),
            (
                "Esc",
                "doesn't quit: hints how to (cancels everywhere else)",
            ),
        ],
    ),
    (
        "Waiting for a number after o / y",
        &[
            ("1-9", "open link [N] / copy command [N]"),
            (
                "10+",
                "type each digit; acts once no more digits can follow",
            ),
            ("Enter", "take the number typed so far (e.g. [1] of 12)"),
            ("Backspace", "delete the last digit typed"),
            ("Esc", "cancel; any other key cancels and acts"),
        ],
    ),
    (
        "On a list-complete screen",
        &[
            ("l j → ↓ / Enter", "go on to the next unfinished list"),
            ("h k ← ↑", "go back and review (all done: last task)"),
        ],
    ),
    (
        "In / search",
        &[
            ("type", "edit the query; jumps to the next match"),
            ("Backspace", "delete the last character"),
            ("Ctrl-W", "delete the last word"),
            ("Ctrl-U", "clear the query"),
            ("Enter", "keep the match (then n / N cycle)"),
            ("Enter, empty", "repeat the last search"),
            ("Esc", "cancel, back to where you were"),
        ],
    ),
    (
        "In the T go-to-task list",
        &[
            ("type", "filter (letters, j and k included)"),
            ("Backspace", "delete the last filter character"),
            ("Ctrl-W", "delete the last filter word"),
            ("↑ / ↓", "move the highlight"),
            ("Ctrl-P / Ctrl-N", "move the highlight up / down"),
            ("Ctrl-U / Ctrl-D", "move the highlight half a page"),
            ("Enter", "jump to the highlighted task"),
            ("Esc", "close"),
        ],
    ),
    (
        "In this help",
        &[
            ("j k  ↑ ↓", "scroll one line"),
            ("Ctrl-E / Ctrl-Y", "scroll one line"),
            ("Ctrl-D / Ctrl-U", "scroll half a page"),
            ("PageDn / PageUp", "scroll one page"),
            ("any other key", "close"),
        ],
    ),
    (
        "Reset dialogs (R, or q when all done)",
        &[
            ("y", "reset all tasks (and quit, after q)"),
            ("l", "after R: reset only the current list"),
            ("s", "after R: reset only the current sub-list"),
            ("n", "after q: quit without resetting"),
            ("Esc", "cancel (so does any other key)"),
        ],
    ),
];

/// The `?` overlay's key column, indent included: wide enough for the
/// longest key label ("Shift-H / Shift-L") so descriptions always clear it.
const HELP_KEY_COLUMN: u16 = 21;
/// The narrowest a `?` description is wrapped to, so a tiny terminal
/// clips the overlay instead of stacking one word per row.
const HELP_MIN_DESC_WIDTH: u16 = 10;

/// The `?` keybinding cheatsheet overlay, built from [`HELP_SECTIONS`].
/// A description too long for its column wraps onto rows indented under it.
/// Scrolls when it doesn't fit a short terminal; any non-scroll key
/// dismisses it.
fn render_help(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let palette = state.palette;
    let inner_w = area.width.saturating_sub(2);
    let desc_w = inner_w
        .saturating_sub(HELP_KEY_COLUMN)
        .max(HELP_MIN_DESC_WIDTH);
    let key_style = Style::default()
        .fg(palette.current)
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::from("")];
    for (title, rows) in HELP_SECTIONS {
        if !title.is_empty() {
            lines.push(Line::from(""));
            lines.extend(
                wrap_text(title, inner_w.saturating_sub(1))
                    .into_iter()
                    .map(|row| {
                        Line::from(Span::styled(
                            format!(" {row}"),
                            Style::default().add_modifier(Modifier::BOLD),
                        ))
                    }),
            );
        }
        for (k, desc) in rows.iter() {
            for (i, part) in wrap_text(desc, desc_w).into_iter().enumerate() {
                let key_cell = if i == 0 { *k } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {key_cell:<width$}", width = HELP_KEY_COLUMN as usize - 2),
                        key_style,
                    ),
                    Span::raw(part),
                ]));
            }
        }
    }
    lines.push(Line::from(""));
    lines.extend(
        wrap_text(
            "j k ↑ ↓ · Ctrl-E/Y/D/U · PageUp/Dn scroll   ·   any other key closes",
            inner_w,
        )
        .into_iter()
        .map(|row| {
            Line::from(Span::styled(
                row,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .centered()
        }),
    );

    // Size the overlay to its content (plus borders), centered; on a short
    // terminal it fills the height and the body scrolls.
    let total = lines.len();
    let height = (total as u16 + 2).min(area.height);
    let row = card_row(area, height);
    let inner_h = row.height.saturating_sub(2) as usize; // visible content rows
    state.layout.help_max_scroll = total.saturating_sub(inner_h) as u16;
    state.layout.help_viewport_height = inner_h as u16;
    state.help.scroll = state.help.scroll.min(state.layout.help_max_scroll);

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

/// Whether there's room below `card` for the spacer and one link row.
/// Otherwise the card shows an in-border hint instead.
fn link_panel_fits(area: Rect, card: Rect) -> bool {
    card.bottom().saturating_add(1) < area.bottom()
}

/// The current item's link URLs, one per row, in the panel below `card`.
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

/// Clamps without panicking when `area_height < MIN_CARD_HEIGHT`, which
/// `u16::clamp` would.
fn safe_card_height(desired: u16, area_height: u16) -> u16 {
    desired.max(MIN_CARD_HEIGHT).min(area_height)
}

/// The width the current card will actually render at, so height can be
/// measured against the same wrapping: the centered `CENTER_CARD_PCT` share
/// in the stack layout, or the full width in the narrow single-card layout.
fn current_card_width(area: Rect) -> u16 {
    if area.width < NARROW_THRESHOLD {
        area.width
    } else {
        area.width * CENTER_CARD_PCT / 100
    }
}

/// Card height sized to the current item's content: wrapped body
/// lines + top/bottom borders, clamped between `MIN_CARD_HEIGHT` and ~60%
/// of the main area. Short tasks get a compact card (which also removes the
/// empty-body void); long tasks get more room before scrolling.
fn desired_card_height(item: &Item, area: Rect, code: Style) -> u16 {
    let inner_w = current_card_width(area).saturating_sub(2 + 2 * BODY_PAD);
    let body = clamp_to_u16(body_lines(item, inner_w.max(1), code).len());
    // The inside-card title adds its wrapped rows + a blank separator. The
    // note icon is measured as one character, like every icon set's glyph.
    let title_rows = card_title_rows(item, "i", inner_w.max(1)).len();
    let title = if title_rows > 0 {
        clamp_to_u16(title_rows + 1)
    } else {
        0
    };
    // A nested item adds a one-line parent breadcrumb above the title.
    let breadcrumb = u16::from(item.depth > 0);
    let content = body
        .saturating_add(title)
        .saturating_add(breadcrumb)
        .saturating_add(2);
    let max_h = ((area.height as u32 * 3 / 5) as u16).max(MIN_CARD_HEIGHT);
    safe_card_height(content.min(max_h), area.height)
}

fn render_checklist(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let width = current_card_width(area);
    let mut heading = Heading {
        // The `## H2` title above the cards, except for the synthesized
        // default list.
        title: !(state.current_list_index == 0 && state.document.has_default_list),
        // A leading bold-only bullet, wrapped to the card width.
        banner: state
            .current_list()
            .banner
            .as_ref()
            .map(|banner| banner_rows(banner, state.icons, width))
            .unwrap_or_default(),
        // Prose written under the heading above this task, before it (#80).
        preamble: state
            .current_item()
            .and_then(|item| state.document.preambles.get(&item.line_number))
            .map(|blocks| preamble_rows(blocks, state.icons, state.palette, width))
            .unwrap_or_default(),
        // The `### H3`+ sub-section path, so the single-card view still says
        // which sub-section you're in on narrow terminals.
        breadcrumb: state
            .current_item()
            .and_then(|item| section_breadcrumb(&item.section)),
        gap: true,
    };

    let content_h = if let Some(item) = state.current_item() {
        desired_card_height(item, area, state.palette.code)
    } else {
        MIN_CARD_HEIGHT.min(area.height)
    };
    let card_h = fit_heading(&mut heading, area.height, content_h, width);
    let reserved = heading.rows();

    // The heading rows and the card are centered together as one block, so
    // the heading stays tied to the card.
    let [block] = Layout::vertical([Constraint::Length(reserved + card_h)])
        .flex(Flex::Center)
        .areas(area);
    let [top, row] =
        Layout::vertical([Constraint::Length(reserved), Constraint::Length(card_h)]).areas(block);
    let rows = Layout::vertical(vec![Constraint::Length(1); reserved as usize]).split(top);
    let mut i = 0;
    if heading.title {
        render_list_heading(
            frame,
            rows[i],
            &state.current_list().title,
            state.icons,
            state.palette,
        );
        i += 1;
    }
    for banner in &heading.banner {
        render_list_banner(frame, rows[i], banner, state.palette);
        i += 1;
    }
    for preamble in &heading.preamble {
        let line = Line::from(Span::styled(preamble.text.clone(), preamble.style)).centered();
        frame.render_widget(Paragraph::new(line), rows[i]);
        i += 1;
    }
    if let Some(crumb) = &heading.breadcrumb {
        render_section_breadcrumb(frame, rows[i], crumb, width);
    }

    // `row` is a vertical slice of `area`, so it is always as wide as `area`.
    if area.width < NARROW_THRESHOLD {
        render_current_card(frame, row, area, state);
        render_link_panel_below(frame, area, row, state);
        return;
    }

    // Stack geometry: the center card is full height and painted
    // last; the side cards are tucked down and shortened, their inner
    // columns extending underneath the center card.
    let center_width = row.width * CENTER_CARD_PCT / 100;
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
/// color as the overview's current-list row (`palette.current` bold). Kept to
/// one row, so a title wider than the area ends in `…` rather than being
/// clipped mid-word.
fn render_list_heading(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    icons: IconSet,
    palette: Palette,
) {
    let heading = Line::from(Span::styled(
        super::truncate(&format!("{} {title}", icons.list), area.width as usize),
        Style::default()
            .fg(palette.current)
            .add_modifier(Modifier::BOLD),
    ))
    .centered();
    frame.render_widget(Paragraph::new(heading), area);
}

/// A list's banner (a leading bold-only bullet) prefixed with the info icon
/// and word-wrapped to `width`, one string per heading row. The banner carries
/// a warning, so it is never cut short.
fn banner_rows(banner: &str, icons: IconSet, width: u16) -> Vec<String> {
    wrap_text(&format!("{} {banner}", icons.note), width)
}

/// How many rows a preamble may take above the card; more is cut with `…`,
/// so prose can't push the card off a short terminal.
const MAX_PREAMBLE_ROWS: usize = 6;

/// A task's preamble wrapped to `width`, each row with its style: an alert
/// in the warning color behind the note icon (like the banner), a quote
/// behind a bar, code in the code color, plain prose as it is.
fn preamble_rows(
    blocks: &[PreambleBlock],
    icons: IconSet,
    palette: Palette,
    width: u16,
) -> Vec<PreambleRow> {
    let mut rows: Vec<PreambleRow> = Vec::new();
    for block in blocks {
        let (prefix, style) = preamble_style(block.kind, icons, palette);
        for line in block.text.lines() {
            for text in wrap_text(&format!("{prefix}{line}"), width) {
                rows.push(PreambleRow {
                    text,
                    style,
                    alert: block.kind == PreambleKind::Alert,
                });
            }
        }
    }
    if rows.len() > MAX_PREAMBLE_ROWS {
        rows.truncate(MAX_PREAMBLE_ROWS);
        if let Some(last) = rows.last_mut() {
            mark_cut(&mut last.text, width);
        }
    }
    rows
}

/// Ends `row` in `…`, cut to `width`, to show that rows after it were
/// left out.
fn mark_cut(row: &mut String, width: u16) {
    *row = super::truncate(&format!("{row}…"), (width as usize).max(1));
}

/// One wrapped row of a task's preamble.
struct PreambleRow {
    text: String,
    style: Style,
    alert: bool,
}

/// The rows drawn above the card, top to bottom: the list title, the
/// banner, the preamble, the sub-section breadcrumb, then a blank gap.
struct Heading {
    title: bool,
    banner: Vec<String>,
    preamble: Vec<PreambleRow>,
    breadcrumb: Option<String>,
    gap: bool,
}

impl Heading {
    fn content_rows(&self) -> usize {
        usize::from(self.title)
            + self.banner.len()
            + self.preamble.len()
            + usize::from(self.breadcrumb.is_some())
    }

    fn rows(&self) -> u16 {
        let content = self.content_rows();
        clamp_to_u16(content + usize::from(self.gap && content > 0))
    }
}

/// The card's height once the heading has its room, trimming the heading
/// when even the shortest card leaves too little: the card shrinks (its body
/// scrolls) down to `MIN_CARD_HEIGHT` first, then the gap, the breadcrumb and
/// the list title go, then preamble rows from the end, prose before alerts,
/// and the banner last, the last row kept ending in `…`.
fn fit_heading(heading: &mut Heading, area_h: u16, content_h: u16, width: u16) -> u16 {
    let floor = content_h.min(MIN_CARD_HEIGHT);
    let card_h = content_h.min(area_h.saturating_sub(heading.rows()));
    if card_h >= floor {
        return card_h;
    }
    let budget = area_h.saturating_sub(floor);
    let mut preamble_cut = false;
    let mut banner_cut = false;
    while heading.rows() > budget {
        if heading.gap {
            heading.gap = false;
        } else if heading.breadcrumb.is_some() {
            heading.breadcrumb = None;
        } else if heading.title {
            heading.title = false;
        } else if let Some(i) = heading.preamble.iter().rposition(|row| !row.alert) {
            heading.preamble.remove(i);
            preamble_cut = true;
        } else if heading.preamble.pop().is_some() {
            preamble_cut = true;
        } else {
            heading.banner.pop();
            banner_cut = true;
        }
    }
    if preamble_cut && let Some(last) = heading.preamble.last_mut() {
        mark_cut(&mut last.text, width);
    }
    if banner_cut && let Some(last) = heading.banner.last_mut() {
        mark_cut(last, width);
    }
    floor
}

/// A preamble block's row prefix and style, shared with the overview.
pub(super) fn preamble_style(
    kind: PreambleKind,
    icons: IconSet,
    palette: Palette,
) -> (String, Style) {
    match kind {
        PreambleKind::Alert => (
            format!("{} ", icons.note),
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        PreambleKind::Quote => (
            "│ ".to_string(),
            Style::default().add_modifier(Modifier::ITALIC),
        ),
        PreambleKind::Code => (String::new(), palette.code),
        PreambleKind::Paragraph => (String::new(), Style::default()),
    }
}

/// One wrapped row of a list's banner, shown below the list title as a
/// non-navigable warning line in `palette.warning`.
fn render_list_banner(frame: &mut Frame, area: Rect, row: &str, palette: Palette) {
    let line = Line::from(Span::styled(
        row.to_string(),
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

/// The top-border line: just the task-state icon (`☑`/`◐`/`☐`/`⊘`/note) in its
/// state color. The card title (`Item.header`) is drawn inside the card body
/// instead (`card_title_lines`).
fn header_line(
    item: &Item,
    info_state: Option<TaskState>,
    icons: IconSet,
    palette: Palette,
) -> Line<'static> {
    let (icon, color) = state_marker(item.kind, info_state, icons, palette);
    Line::from(Span::styled(format!(" {icon} "), fg(color)))
}

/// A dim breadcrumb of a nested item's parent chain, truncated from the
/// front. `None` for a top-level item.
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
    let labels: Vec<String> = chain.iter().map(|&i| list.items[i].label()).collect();
    let text = format!("{} ›", labels.join(" › "));

    let width = width.max(1) as usize;
    let text = if display_width(&text) > width {
        let budget = width.saturating_sub(1);
        let mut used = 0;
        let start = text
            .char_indices()
            .rev()
            .find_map(|(i, ch)| {
                used += char_width(ch);
                (used > budget).then_some(i + ch.len_utf8())
            })
            .unwrap_or(0);
        format!("…{}", &text[start..])
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

/// The card title (`Item.header`) prefixed with the note icon, word-wrapped
/// to the card's inner `width` so a long title is shown in full instead of
/// being clipped at the border. Shared by the renderer and
/// `desired_card_height`, so the card grows by exactly the rows drawn.
fn card_title_rows(item: &Item, icon: &str, width: u16) -> Vec<String> {
    match &item.header {
        Some(_) => wrap_text(&format!("{icon} {}", item.label()), width),
        None => Vec::new(),
    }
}

/// `card_title_rows` as styled lines: bold title text, note-colored icon.
fn card_title_lines(
    item: &Item,
    width: u16,
    icons: IconSet,
    palette: Palette,
) -> Vec<Line<'static>> {
    let rows = card_title_rows(item, icons.note, width);
    if rows.is_empty() {
        return Vec::new();
    }
    // The title text uses the terminal's **default** foreground (bold) so it
    // always contrasts with the background and stays legible (a colored title
    // could wash out on some backgrounds); the leading note icon on the first
    // row keeps `palette.note` (blue) as a small accent.
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let spans = match row.strip_prefix(icons.note) {
                Some(rest) if i == 0 => vec![
                    Span::styled(icons.note.to_string(), Style::default().fg(palette.note)),
                    Span::styled(rest.to_string(), bold),
                ],
                _ => vec![Span::styled(row, bold)],
            };
            Line::from(spans).centered()
        })
        .collect();
    lines.push(Line::from(""));
    lines
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
/// pending `·` dim. Without color, a started item is a half dot `◐` so it
/// still reads apart from a done one.
fn position_dots(list: &List, current: usize, palette: Palette) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, item) in list.items.iter().enumerate() {
        let (glyph, style) = if i == current {
            ("◉", Style::default().fg(palette.current))
        } else {
            let color = marker_color(item.kind, None, palette);
            // Info items keep the small `·` (not a task, no progress of their
            // own) but in note blue, so the strip stays a faithful item map
            // while reading as information, not a pending task.
            let glyph = match item.kind {
                ItemKind::Checkbox(TaskState::Done) => "●",
                ItemKind::Checkbox(TaskState::Started) if palette.is_mono() => "◐",
                ItemKind::Checkbox(TaskState::Started) => "●",
                ItemKind::Checkbox(TaskState::Skipped) => "-",
                ItemKind::Checkbox(TaskState::NotStarted) | ItemKind::DisplayOnly => "·",
            };
            match color {
                Some(color) => (glyph, Style::default().fg(color)),
                None => (glyph, Style::default().add_modifier(Modifier::DIM)),
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

    let lines = side_card_lines(item, inner.width);
    // Vertically center like the current card, prepending blank lines
    // rather than top-anchoring under the header border.
    let mut padded = vec![Line::from(""); top_pad(lines.len(), inner.height as usize)];
    padded.extend(lines);
    let paragraph = Paragraph::new(padded)
        .style(Style::default().add_modifier(Modifier::DIM))
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, inner);
}

/// A side card's content: the bold title (`Item.header`) when present, a
/// blank separator, then the plain `display_text` body, each wrapped to
/// `width`. A title-only task shows just its title.
fn side_card_lines(item: &Item, width: u16) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = item
        .header
        .as_ref()
        .map(|_| wrap_text(&item.label(), width))
        .unwrap_or_default()
        .into_iter()
        .map(|row| Line::from(Span::styled(row, bold)))
        .collect();
    let has_body = !item.display_text.trim().is_empty();
    if has_body || lines.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        let body = match (&item.header, &item.ordinal) {
            (None, Some(ordinal)) => format!("{ordinal} {}", item.display_text),
            _ => item.display_text.clone(),
        };
        lines.extend(wrap_text(&body, width).into_iter().map(Line::from));
    }
    lines
}

fn render_current_card(frame: &mut Frame, area: Rect, screen_area: Rect, state: &mut AppState) {
    // Record the card's on-screen area for click-to-copy hit-testing.
    state.layout.card_rect = Some(area);
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
    let skipped = item.kind == ItemKind::Checkbox(TaskState::Skipped);
    // A critical step still to do is framed in the warning color and
    // labelled, so it isn't ticked by reflex (#95).
    let critical = !done
        && state
            .document
            .is_critical(state.current_list_index, state.current_item_index);
    let border_style = match accent_color(item, info_state, palette) {
        _ if critical => Style::default()
            .fg(palette.warning)
            .add_modifier(Modifier::BOLD),
        Some(color) => Style::default().fg(color).add_modifier(Modifier::BOLD),
        None => Style::default().add_modifier(Modifier::BOLD),
    };

    // Display-only notes get a rounded border so they read as information,
    // not an actionable step; tasks get a thick border so an actionable step
    // reads as a bold, solid frame.
    let border_type = if matches!(item.kind, ItemKind::DisplayOnly) {
        BorderType::Rounded
    } else {
        BorderType::Thick
    };
    let mut block = item_block(item, info_state, icons, palette, Some(position))
        .border_type(border_type)
        .border_style(border_style)
        .padding(Padding::horizontal(BODY_PAD));
    if critical {
        block = block.title_top(
            Line::from(Span::styled(
                " critical ",
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ))
            .centered(),
        );
    }

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
    lines.extend(card_title_lines(item, inner.width, icons, palette));
    let n_before = lines.len();
    let (body, code_rows) = body_layout(item, inner.width, palette.code);
    lines.extend(body);
    let visible = inner.height as usize;
    let total_lines = lines.len();
    let max_scroll = clamp_to_u16(total_lines.saturating_sub(visible));
    state.layout.card_max_scroll = max_scroll;
    state.layout.card_viewport_height = inner.height;
    state.card_scroll = state.card_scroll.min(max_scroll);

    // Click-to-copy: map each code target to its on-screen row and columns,
    // accounting for the vertical-centering pad (body fits) or the scroll
    // offset (body overflows), and clip to the visible body area.
    state.layout.code_regions.clear();
    let scroll = state.card_scroll as usize;
    let top = if max_scroll == 0 {
        top_pad(lines.len(), visible)
    } else {
        0
    };
    for (k, targets) in code_rows.iter().enumerate() {
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
        for (cols, text) in targets {
            state.layout.code_regions.push((
                Rect::new(
                    inner.x + cols.start,
                    inner.y + vis_row as u16,
                    cols.end - cols.start,
                    1,
                ),
                Rc::clone(text),
            ));
        }
    }

    // The position dots and the scroll indicator are budgeted together; when
    // both don't fit, the indicator wins and the dots are dropped whole.
    let list = state.current_list();
    let scroll_title = (max_scroll > 0).then(|| {
        let first = state.card_scroll as usize + 1;
        let last = (state.card_scroll as usize + visible).min(lines.len());
        format!(" {first}–{last}/{} ", lines.len())
    });
    let scroll_width = scroll_title.as_ref().map_or(0, |s| display_width(s) as u16);
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

    // A skipped task is dimmed and struck through: set aside, not done.
    let body_style = if skipped {
        Style::default().add_modifier(Modifier::DIM | Modifier::CROSSED_OUT)
    } else if done {
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

/// How a body word-token is styled. `code` is the chip click-to-copy keys
/// off; `url` marks a dim `[N]` marker keying a link or command to its digit
/// prompt.
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

/// Splits `text` into word tokens, glued to the previous token unless
/// there was whitespace at the boundary.
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

/// The `[N]` marker style keying a link or command to its digit prompt.
fn marker_token(n: usize, glue: bool) -> Token {
    (
        format!("[{n}]"),
        SpanStyle {
            url: true,
            ..Default::default()
        },
        glue,
    )
}

/// The number `y` then a digit uses for the body's `k`th (0-based) inline
/// command, when inline commands are what `y` chooses between and there are
/// several.
fn inline_command_number(item: &Item, k: usize) -> Option<usize> {
    (item.code_blocks.is_empty() && item.code_spans.len() >= 2).then_some(k + 1)
}

/// The number `y` then a digit uses for the `k`th (0-based) fenced block,
/// when there are several.
fn block_number(item: &Item, k: usize) -> Option<usize> {
    (item.code_blocks.len() >= 2).then_some(k + 1)
}

/// The body's flowed tokens, plus the split-off trailing command and its
/// `[N]` marker number, if any.
type BodyTokens = (Vec<Token>, Option<(String, Option<usize>)>);

/// `item_body_tokens`, led by the bold ordered-list step number when the item
/// has no title to carry it.
fn body_tokens(item: &Item) -> BodyTokens {
    let (mut tokens, trailing) = item_body_tokens(item);
    if let (None, Some(ordinal)) = (&item.header, &item.ordinal) {
        let strong = SpanStyle {
            strong: true,
            ..Default::default()
        };
        tokens.insert(0, (ordinal.clone(), strong, false));
    }
    (tokens, trailing)
}

fn item_body_tokens(item: &Item) -> BodyTokens {
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

    // The URL is not shown inline on the card (it wrapped ugly on narrow
    // cards); it's listed in the link panel below the card instead. When a
    // card has several links, each link text is tagged with a dim `[N]`
    // marker so it maps to the panel's numbered list.
    let link_count = item.link_urls().len();
    let mut link_idx = 0usize;
    let mut code_idx = 0usize;

    let mut tokens = Vec::new();
    let mut trailing = None;
    let mut prev_ws = true; // start of body: nothing to separate from
    for (i, span) in item.body.iter().enumerate() {
        if Some(i) == trailing_idx {
            if let BodySpan::Code(c) = span {
                trailing = Some((c.clone(), inline_command_number(item, code_idx)));
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
                // With several commands, a dim `[N]` right after the chip keys
                // it to `y`'s digit prompt.
                if let Some(n) = inline_command_number(item, code_idx) {
                    tokens.push(marker_token(n, true));
                }
                code_idx += 1;
                prev_ws = true;
            }
            // A link renders as just its underlined text; the URL lives in
            // the link panel below the card, not inline. With several links on
            // one card, a dim `[N]` marker after the text keys it to that list.
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
                    tokens.push(marker_token(link_idx, false));
                    prev_ws = false;
                }
            }
        }
    }
    (tokens, trailing)
}

/// The current card's body as styled, word-wrapped lines, with its code
/// blocks appended below. Measured in display cells so scroll clamping stays
/// exact.
fn body_lines(item: &Item, width: u16, code: Style) -> Vec<Line<'static>> {
    body_layout(item, width, code).0
}

/// One click-to-copy target on a body row: the columns it covers, measured
/// from the card body's left edge, and the full text a click there copies.
/// The text is shared, so every row of one command points at a single copy.
type CopyTarget = (Range<u16>, Rc<str>);

/// A target covering a whole body row, for rows that are entirely one command.
fn full_row(width: usize, text: &Rc<str>) -> Vec<CopyTarget> {
    vec![(0..clamp_to_u16(width), Rc::clone(text))]
}

/// The inline-code chips on one wrapped prose row as copy targets. Each chip
/// maps to its token's full, unsplit text (`chips`, indexed like the tokens),
/// so every fragment of a hard-split chip copies the whole command. The
/// column offset mirrors how `Paragraph` places a centered line.
fn chip_targets(
    line: &Line,
    origins: &[Option<usize>],
    chips: &[Option<Rc<str>>],
    width: usize,
    centered: bool,
) -> Vec<CopyTarget> {
    let mut x = if centered {
        (width / 2).saturating_sub(line.width() / 2)
    } else {
        0
    };
    let mut targets = Vec::new();
    for (span, origin) in line.spans.iter().zip(origins) {
        let w = span.width();
        if let Some(text) = origin.and_then(|i| chips[i].as_ref()) {
            let start = clamp_to_u16(x.min(width));
            let end = clamp_to_u16((x + w).min(width));
            if start < end {
                targets.push((start..end, Rc::clone(text)));
            }
        }
        x += w;
    }
    targets
}

/// `body_lines` plus, index-aligned, the click-to-copy targets on each line.
fn body_layout(item: &Item, width: u16, code: Style) -> (Vec<Line<'static>>, Vec<Vec<CopyTarget>>) {
    let width = width.max(1) as usize;

    // Flow prose + inline code as styled word tokens; a trailing inline code
    // span is split off to render on its own line. Inline code is kept
    // as a single padded chip so a short command stays contiguous.
    let (tokens, trailing_code) = body_tokens(item);

    // Center short prose, left-align it once it wraps past a couple of
    // lines (centered multi-line prose is tiring to read). The trailing
    // command line, if any, shares the prose alignment.
    let prose = wrap_tokens(&tokens, width, code);
    let center_prose = prose.len() <= PROSE_CENTER_MAX_LINES;
    let align = |line: Line<'static>| {
        if center_prose {
            line.centered()
        } else {
            line.left_aligned()
        }
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut code_rows: Vec<Vec<CopyTarget>> = Vec::new();

    let chips: Vec<Option<Rc<str>>> = tokens
        .iter()
        .map(|(text, style, _)| {
            let text = text.trim();
            (style.code && !text.is_empty()).then(|| Rc::from(text))
        })
        .collect();
    for (line, origins) in prose {
        code_rows.push(chip_targets(&line, &origins, &chips, width, center_prose));
        lines.push(align(line));
    }

    // A trailing command drops onto its own line, with a blank line
    // separating it from any prose above (nothing to separate when the body
    // is only the command).
    if let Some((cmd, number)) = trailing_code {
        if !lines.is_empty() {
            lines.push(Line::from(""));
            code_rows.push(Vec::new());
        }
        let chip = SpanStyle {
            code: true,
            ..Default::default()
        };
        let mut trailing = vec![(format!(" {cmd} "), chip, false)];
        trailing.extend(number.map(|n| marker_token(n, true)));
        let cmd: Rc<str> = Rc::from(cmd);
        for (line, _) in wrap_tokens(&trailing, width, code) {
            lines.push(align(line));
            code_rows.push(full_row(width, &cmd));
        }
    }

    // Fenced blocks below, left-aligned in a light box, with a blank
    // separator when there's flowed prose above them. Every box row maps to
    // the block's clean text so a click anywhere on the box copies it.
    for (k, block) in item.code_blocks.iter().enumerate() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
            code_rows.push(Vec::new());
        }
        let text: Rc<str> = Rc::from(block.as_str());
        for line in code_block_lines(block, width, code, block_number(item, k)) {
            lines.push(line);
            code_rows.push(full_row(width, &text));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
        code_rows.push(Vec::new());
    }
    (lines, code_rows)
}

/// Renders one fenced block as left-aligned, code-colored lines inside a
/// light box. Preserves the block's own line breaks (hard-wrapped to
/// fit); falls back to plain left-aligned lines when the card is too narrow
/// for a box. Deterministic line count, so scroll clamping stays exact.
/// A `marker` number, set when the card has several blocks, is drawn into the
/// top border (`┌─[N]──┐`) to key the block to `y`'s digit prompt.
fn code_block_lines(
    block: &str,
    width: usize,
    style: Style,
    marker: Option<usize>,
) -> Vec<Line<'static>> {
    let marker = marker.map(|n| format!("[{n}]"));
    let continued = continuation_style(style);
    // Not enough room for "│ x │": drop the box, keep left-aligned code,
    // still marking wrapped rows when there's a cell to spare for it.
    if width < 6 {
        let mark = width >= 2;
        let rows = soft_wrapped(block, if mark { width - 1 } else { width }).map(|(c, wraps)| {
            let mut spans = vec![Span::styled(c, style)];
            if wraps && mark {
                spans.push(Span::styled(CONTINUATION, continued));
            }
            Line::from(spans)
        });
        return marker
            .map(|m| Line::styled(m, style))
            .into_iter()
            .chain(rows)
            .map(Line::left_aligned)
            .collect();
    }

    let content_w = width - 4; // "│ " + content + " │"
    let chunks: Vec<(String, bool)> = soft_wrapped(block, content_w).collect();
    // The box always spans the full card-body width — the gray
    // background reaches both edges even for a short one-line block — so code
    // blocks align regardless of content length, matching a rendered Markdown
    // block.
    let box_w = content_w;

    // A blank gray row just inside the top and bottom borders gives the code
    // vertical breathing room, like a rendered Markdown block.
    let blank = || Line::styled(format!("│ {} │", " ".repeat(box_w)), style).left_aligned();

    let mut out = Vec::with_capacity(chunks.len() + 4);
    let top = match marker.filter(|m| m.len() < box_w + 2) {
        Some(m) => format!("┌─{m}{}┐", "─".repeat(box_w + 1 - m.len())),
        None => format!("┌{}┐", "─".repeat(box_w + 2)),
    };
    out.push(Line::styled(top, style).left_aligned());
    out.push(blank());
    // A row that a too-long source line wrapped onto the next carries a dim
    // `↩` in its right padding column, so a wrapped command can't be read
    // as two separate lines of the block.
    for (c, wraps) in &chunks {
        let pad = box_w.saturating_sub(display_width(c));
        let row = format!("│ {c}{}", " ".repeat(pad));
        let line = if *wraps {
            Line::from(vec![
                Span::styled(row, style),
                Span::styled(CONTINUATION, continued),
                Span::styled("│", style),
            ])
        } else {
            Line::styled(format!("{row} │"), style)
        };
        out.push(line.left_aligned());
    }
    out.push(blank());
    out.push(Line::styled(format!("└{}┘", "─".repeat(box_w + 2)), style).left_aligned());
    out
}

/// Drawn at the end of a code row that continues on the next one, so a
/// soft-wrapped command line is distinguishable from a real line break.
const CONTINUATION: &str = "↩";

/// The continuation marker's style: the surrounding code style, dimmed.
fn continuation_style(code: Style) -> Style {
    code.add_modifier(Modifier::DIM)
}

/// Hard-wraps each of `block`'s lines to `width` cells, flagging every piece
/// that continues onto the next row (all but the last piece of a line).
fn soft_wrapped(block: &str, width: usize) -> impl Iterator<Item = (String, bool)> + '_ {
    block.lines().flat_map(move |raw| {
        let pieces = hard_wrap(&expand_tabs(raw), width);
        let last = pieces.len() - 1;
        pieces
            .into_iter()
            .enumerate()
            .map(move |(i, p)| (p, i < last))
    })
}

/// One word for `greedy_wrap`: its text, whether it abuts the previous word
/// on the row with no separating space, and a caller tag carried onto every
/// piece cut from it.
struct WrapWord<'a, T> {
    text: &'a str,
    glue: bool,
    tag: T,
}

/// A piece of a wrapped row: the single space between two words, or (part
/// of) a word with its tag.
enum WrapPiece<'a, T> {
    Space,
    Word(&'a str, T),
}

/// A wrapped row. `marked` is set on a row holding a piece hard-split off a
/// word whose tag asked for a continuation marker; one cell is left free for
/// it.
struct WrapRow<'a, T> {
    pieces: Vec<WrapPiece<'a, T>>,
    marked: bool,
}

/// The one greedy word-wrapper behind `wrap_text`, `wrap_tokens` and
/// `wrap_modal_lines`: words are measured in display cells and joined by a
/// single space (none before a glued word), a row is flushed when the next
/// word doesn't fit, and a word wider than the whole row is hard-split onto
/// rows of its own. `marks(tag)` says whether such a split row keeps its last
/// cell for a continuation marker. Rendering and `desired_card_height` both
/// go through here, so their line counts agree.
fn greedy_wrap<'a, T: Copy>(
    words: impl IntoIterator<Item = WrapWord<'a, T>>,
    width: usize,
    marks: impl Fn(T) -> bool,
) -> Vec<WrapRow<'a, T>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut used = 0usize;
    for WrapWord { text, glue, tag } in words {
        let mut rest = text;
        loop {
            let sep = usize::from(!current.is_empty() && !glue);
            let fit = width
                .checked_sub(used + sep)
                .and_then(|avail| width_within(rest, avail));
            if let Some(w) = fit {
                if sep == 1 {
                    current.push(WrapPiece::Space);
                }
                current.push(WrapPiece::Word(rest, tag));
                used += sep + w;
                break;
            }
            if current.is_empty() {
                let marked = marks(tag) && width >= 2;
                let cut = split_at_width(rest, if marked { width - 1 } else { width });
                rows.push(WrapRow {
                    pieces: vec![WrapPiece::Word(&rest[..cut], tag)],
                    marked,
                });
                rest = &rest[cut..];
                if rest.is_empty() {
                    break;
                }
            } else {
                rows.push(WrapRow {
                    pieces: std::mem::take(&mut current),
                    marked: false,
                });
                used = 0;
            }
        }
    }
    if !current.is_empty() {
        rows.push(WrapRow {
            pieces: current,
            marked: false,
        });
    }
    rows
}

/// Greedy word wrap over styled tokens `(text, SpanStyle, glue)` through
/// `greedy_wrap`, emitting styled `Line`s with code fragments in the `code`
/// style; a hard-split code chip ends each continued row with `↩`, so the
/// command doesn't read as several.
/// Each line comes with, per span, the index of the code token it came from
/// (`None` for prose and spacing), so click-to-copy can map a chip — or any
/// fragment of a hard-split one — back to its full text.
fn wrap_tokens(
    tokens: &[Token],
    width: usize,
    code: Style,
) -> Vec<(Line<'static>, Vec<Option<usize>>)> {
    let span_for = |text: &str, style: SpanStyle| {
        if style.code {
            // Inline code keeps its chip style.
            return Span::styled(text.to_string(), code);
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
        Span::styled(text.to_string(), Style::default().add_modifier(modifier))
    };

    let shown: Vec<Cow<'_, str>> = tokens
        .iter()
        .map(|(word, style, _)| {
            if style.code {
                expand_tabs(word)
            } else {
                Cow::Borrowed(word.as_str())
            }
        })
        .collect();
    let words = tokens
        .iter()
        .zip(&shown)
        .enumerate()
        .map(|(index, ((_, style, glue), text))| WrapWord {
            text,
            glue: *glue,
            tag: (index, *style),
        });
    greedy_wrap(words, width, |(_, style)| style.code)
        .into_iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(row.pieces.len() + 1);
            let mut origins = Vec::with_capacity(row.pieces.len() + 1);
            let mut last_origin = None;
            for piece in row.pieces {
                match piece {
                    WrapPiece::Space => {
                        spans.push(Span::raw(" "));
                        origins.push(None);
                    }
                    WrapPiece::Word(text, (index, style)) => {
                        let origin = style.code.then_some(index);
                        spans.push(span_for(text, style));
                        origins.push(origin);
                        last_origin = origin;
                    }
                }
            }
            if row.marked {
                spans.push(Span::styled(CONTINUATION, continuation_style(code)));
                origins.push(last_origin);
            }
            (Line::from(spans), origins)
        })
        .collect()
}

/// The display width of `text` when it fits in `avail` cells, else `None`.
/// Scans no further than the first `avail` cells, so a word far longer than
/// the line costs one row's worth of work per row it is split onto.
fn width_within(text: &str, avail: usize) -> Option<usize> {
    if split_at_width(text, avail) < text.len() {
        return None;
    }
    let width = display_width(text);
    (width <= avail).then_some(width)
}

/// Hard-wraps a single line to `width` **display cells** (no word breaking),
/// used for fenced code where original layout matters.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let ch_width = char_width(ch);
        // A glyph wider than the whole line still has to land somewhere, so
        // only break when something is already on the line — otherwise an
        // over-wide character on an empty line would never make progress.
        if current_width + ch_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Greedy word wrap of plain text on display cells through `greedy_wrap`,
/// with whitespace runs collapsed to one space; always at least one row, an
/// empty one for empty text.
pub(super) fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let words = text.split_whitespace().map(|text| WrapWord {
        text,
        glue: false,
        tag: (),
    });
    let mut lines: Vec<String> = greedy_wrap(words, usize::from(width), |()| false)
        .into_iter()
        .map(|row| {
            row.pieces
                .into_iter()
                .map(|piece| match piece {
                    WrapPiece::Space => " ",
                    WrapPiece::Word(text, ()) => text,
                })
                .collect()
        })
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Word-wraps each line wider than `width` cells through `greedy_wrap`,
/// keeping every word's span style and the line's own style; lines that fit
/// are kept exactly as built.
fn wrap_modal_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let words = line.spans.iter().flat_map(|span| {
            span.content.split_whitespace().map(|text| WrapWord {
                text,
                glue: false,
                tag: span.style,
            })
        });
        out.extend(greedy_wrap(words, width, |_| false).into_iter().map(|row| {
            let spans: Vec<Span<'static>> = row
                .pieces
                .into_iter()
                .map(|piece| match piece {
                    WrapPiece::Space => Span::raw(" "),
                    WrapPiece::Word(text, style) => Span::styled(text.to_string(), style),
                })
                .collect();
            Line::from(spans).style(line.style)
        }));
    }
    out
}

/// The rows a modal card shows when only `room` rows fit inside its
/// borders: the `keys` (the lines saying what to press) always stay, and the
/// descriptive `body` is cut from the bottom, ending on a `…` so the cut is
/// visible.
fn fit_modal_rows(
    mut body: Vec<Line<'static>>,
    keys: Vec<Line<'static>>,
    room: usize,
) -> Vec<Line<'static>> {
    if body.len() + keys.len() > room {
        let keep = room.saturating_sub(keys.len());
        body.truncate(keep);
        if let Some(last) = body.last_mut() {
            *last = Line::from(Span::styled(
                "…",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }
    body.extend(keys);
    body
}

/// A centered completion/confirmation card sized to its content: `body` then
/// `keys`, wrapped to the card's inner width, plus borders, capped at the
/// area. When it can't all fit, the body is trimmed so the key prompts are
/// never the part that disappears.
fn modal_card(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    border_color: Color,
    body: Vec<Line<'static>>,
    keys: Vec<Line<'static>>,
) {
    let inner_width = area.width.saturating_sub(2);
    let body = wrap_modal_lines(body, inner_width);
    let keys = wrap_modal_lines(keys, inner_width);
    let room = usize::from(area.height.saturating_sub(2));
    let lines = fit_modal_rows(body, keys, room);
    let height = safe_card_height(clamp_to_u16(lines.len() + 2), area.height);
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
    ];
    let keys = vec![
        Line::from(""),
        Line::from("Press  l / Enter  to go to next list"),
        Line::from("Press  h  to review tasks"),
    ];
    modal_card(
        frame,
        area,
        "List Complete",
        state.palette.done,
        lines,
        keys,
    );
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
        let (done, total) = list.checkbox_stats();
        // This screen asserts completion, so an incomplete row must say so.
        let (marker, marker_style) = if done == total {
            (icons.done, Style::default().fg(state.palette.done))
        } else {
            (icons.pending, Style::default().fg(state.palette.error))
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), marker_style),
            Span::raw(format!(" {}  {done} / {total}", list.title)),
        ]));
    }

    let keys = vec![
        Line::from(""),
        Line::from(format!("Total: {total_done} / {total_all} tasks")),
        Line::from(""),
        Line::from("Press  R  to reset · h to review · q to quit"),
    ];
    modal_card(
        frame,
        area,
        "All Tasks Complete",
        state.palette.done,
        lines,
        keys,
    );
}

/// A dim rule line used as a divider on the completion screens.
fn completion_rule() -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(20),
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// What a reset will change, e.g. `3 done, 1 started and 2 skipped tasks`.
fn reset_summary(done: usize, started: usize, skipped: usize) -> String {
    let parts: Vec<String> = [(done, "done"), (started, "started"), (skipped, "skipped")]
        .into_iter()
        .filter(|&(count, _)| count > 0)
        .map(|(count, name)| format!("{count} {name}"))
        .collect();
    let joined = match parts.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "no".to_string(),
    };
    let noun = if done + started + skipped == 1 {
        "task"
    } else {
        "tasks"
    };
    format!("{joined} {noun}")
}

fn render_confirm_reset(frame: &mut Frame, area: Rect, state: &AppState) {
    let (done, started, total) = state.document.checkbox_progress();
    let skipped = state.document.skipped_count();
    let total = total + skipped;
    let file_name = state
        .document
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "the file".to_string());

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Reset checklist?",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "This marks {} (of {total}) as not started",
            reset_summary(done, started, skipped)
        )),
        Line::from(format!("and rewrites {file_name}.")),
    ];
    let mut keys = vec![Line::from(""), Line::from("Press  y  to reset everything")];
    // Narrower scopes (#118), each with how many tasks it would change.
    if state.document.lists.len() > 1 {
        keys.push(Line::from(format!(
            "Press  l  to reset only \"{}\" ({})",
            super::truncate(&state.current_list().title, 28),
            state.reset_count(ResetScope::List)
        )));
    }
    if let Some(root) = state.sub_list_root() {
        keys.push(Line::from(format!(
            "Press  s  to reset only \"{}\" and below ({})",
            super::truncate(&state.current_list().items[root].label(), 24),
            state.reset_count(ResetScope::SubList)
        )));
    }
    keys.push(Line::from("Press any other key to cancel"));
    modal_card(
        frame,
        area,
        "Confirm Reset",
        state.palette.started,
        lines,
        keys,
    );
}

fn render_confirm_quit_reset(frame: &mut Frame, area: Rect, state: &AppState) {
    let (_, total) = state.document.checkbox_stats();
    let skipped = state.document.skipped_count();
    let total = total + skipped;
    let heading = if skipped > 0 {
        "All tasks are done or skipped."
    } else {
        "All tasks are done."
    };

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            heading,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Reset all {total} tasks to not done before quitting,"
        )),
        Line::from("so the checklist is ready to run again?"),
    ];
    let keys = vec![
        Line::from(""),
        Line::from("Press  y  to reset and quit"),
        Line::from("Press  n  to quit without resetting"),
        Line::from("Press  Esc  to keep working"),
    ];
    modal_card(
        frame,
        area,
        "All Tasks Complete",
        state.palette.done,
        lines,
        keys,
    );
}

#[cfg(test)]
#[path = "cards_tests.rs"]
mod tests;
