use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph};

use crate::model::{AppState, IconSet, ItemKind, OverviewTarget, Palette, SubHeading, TaskState};

/// Leading indent for an item row. When a list-title row is shown, items
/// get a base level so they sit under it; with a single list that row is
/// dropped, so top-level items start at the left margin. Nesting adds
/// one more level per `depth` in both cases.
fn item_indent(depth: usize, has_list_header: bool) -> String {
    "  ".repeat(depth + has_list_header as usize)
}

/// The `### H3`+ sub-section headings that are *new* at an item versus the
/// previous item in the same list — the tail of `section` past its
/// common prefix with `prev`. A divider row is drawn for each; sharing a prefix
/// means the group continues, so nothing is redrawn.
fn new_sub_headings<'a>(prev: &[SubHeading], section: &'a [SubHeading]) -> &'a [SubHeading] {
    let common = prev.iter().zip(section).take_while(|(a, b)| a == b).count();
    &section[common..]
}

/// A `── Text ──────` divider line for a `### H3`+ sub-section: a bold label
/// between dim rules, indented by heading level and filled to the panel's
/// inner width. Shared by the in-list divider row and the sticky header.
fn section_divider_line(
    heading: &SubHeading,
    has_list_header: bool,
    inner_width: usize,
) -> Line<'static> {
    // Sub-sections nest under the list: H3 sits at the base item indent, each
    // deeper level (H4, H5, …) adds one more step.
    let indent = "  ".repeat(has_list_header as usize + heading.level.saturating_sub(3) as usize);
    // "── " (3 cells) before the label, one trailing space after it: truncate
    // the label with an ellipsis (matching the card-side breadcrumb's
    // truncation) so a long sub-heading is never hard-clipped instead.
    let prefix_width = indent.chars().count() + 3;
    let label_budget = inner_width.saturating_sub(prefix_width + 1);
    let label = format!("{} ", super::truncate(&heading.text, label_budget));
    let used = prefix_width + label.chars().count();
    let trailing = inner_width.saturating_sub(used);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(format!("{indent}── "), dim),
        Span::styled(label, bold),
        Span::styled("─".repeat(trailing), dim),
    ])
}

/// The divider as a non-selectable list row, like the list banner.
fn section_divider(
    heading: &SubHeading,
    has_list_header: bool,
    inner_width: usize,
) -> ListItem<'static> {
    ListItem::new(section_divider_line(heading, has_list_header, inner_width))
}

/// The `{list icon} [n] Title` overview header line for a list; the current
/// list is cyan-accented. Shared by the in-panel row and the sticky header
/// pin.
fn list_header_line(
    icons: IconSet,
    palette: Palette,
    list_index: usize,
    title: &str,
    is_current: bool,
) -> Line<'static> {
    let style = if is_current {
        Style::default()
            .fg(palette.current)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Line::from(Span::styled(
        format!("{} [{}] {}", icons.list, list_index + 1, title),
        style,
    ))
}

/// The pinnable ancestor context of an overview row: the list's header
/// row (multi-list only) and the active `### H3`+ sub-heading path, each paired
/// with the row index it was drawn at. When the row is the first visible one and
/// an ancestor's source row has scrolled above the viewport, that ancestor is
/// pinned as a sticky header so orientation isn't lost in a long group.
#[derive(Clone, Default)]
struct StickyContext {
    /// `(row index, list index)` of the `[n] Title` header, when one is shown.
    list_header: Option<(usize, usize)>,
    /// Active sub-heading path — `(heading, its divider row index)`, outermost
    /// first — mirroring the item's `section`.
    stack: Vec<(SubHeading, usize)>,
}

/// Purely informational progress view: rows for lists, group headers, and
/// items with done/current/pending markers.
pub fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let icons = state.icons;
    let palette = state.palette;
    let mut list_items = Vec::new();
    // How each row responds to a click, index-aligned with `list_items`.
    // Computed here, mapped to on-screen `Rect`s after the render.
    let mut row_clicks: Vec<RowClick> = Vec::new();
    // The pinnable ancestor context of each row (list header + active
    // sub-heading path with their row indices), index-aligned with `list_items`.
    // Drives the sticky header when a group's ancestors scroll off.
    let mut row_sticky: Vec<StickyContext> = Vec::new();
    let mut selected_index = 0;

    // With a single list the `[1] Title` row is redundant (the number is
    // meaningless, the title already shows above the cards, and a title-less
    // default list would read "[1] (Default)"), so it's dropped. Items
    // and any banner de-indent to the left margin in that case.
    let has_list_header = state.document.lists.len() > 1;

    // Inner content width of the panel (inside borders + 1 col padding each
    // side), for filling sub-section divider rules.
    let inner_width = area.width.saturating_sub(4) as usize;

    for (list_index, list) in state.document.lists.iter().enumerate() {
        // The active sub-heading path (heading + its divider row index) as we
        // walk this list's rows; resets per list, since sub-sections never
        // cross an H2 boundary. Mirrors each item's `section`.
        let mut active_stack: Vec<(SubHeading, usize)> = Vec::new();
        // This list's `[n] Title` header row, if shown — pinned above the
        // sub-heading path in the sticky header.
        let mut list_header: Option<(usize, usize)> = None;

        if has_list_header {
            let row = list_items.len();
            list_items.push(ListItem::new(list_header_line(
                icons,
                palette,
                list_index,
                &list.title,
                list_index == state.current_list_index,
            )));
            row_clicks.push(RowClick::Whole(OverviewTarget::List(list_index)));
            list_header = Some((row, list_index));
            row_sticky.push(StickyContext {
                list_header,
                stack: Vec::new(),
            });
        }

        // The list banner: a non-selectable warning row below the
        // list title, in the warning color, prefixed with the info icon.
        if let Some(banner) = &list.banner {
            list_items.push(ListItem::new(Line::from(Span::styled(
                format!("{}{} {banner}", item_indent(0, has_list_header), icons.note),
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ))));
            row_clicks.push(RowClick::None);
            row_sticky.push(StickyContext {
                list_header,
                stack: active_stack.clone(),
            });
        }

        // Sub-section path of the previous item, to diff dividers against.
        // Resets per list — sub-sections never cross an H2 boundary.
        let mut prev_section: &[SubHeading] = &[];

        for (item_index, item) in list.items.iter().enumerate() {
            // Draw a divider for each sub-section heading newly in effect at
            // this item, before the item's own row so the group reads
            // "heading, then its cards". Non-selectable, like the banner.
            let new = new_sub_headings(prev_section, &item.section);
            // Drop the entries the new item no longer shares (a shallower path
            // returning to an ancestor), then push a fresh entry per new
            // heading so `active_stack` mirrors this item's section path.
            active_stack.truncate(item.section.len() - new.len());
            for heading in new {
                let divider_row = list_items.len();
                list_items.push(section_divider(heading, has_list_header, inner_width));
                row_clicks.push(RowClick::None);
                active_stack.push((heading.clone(), divider_row));
                row_sticky.push(StickyContext {
                    list_header,
                    stack: active_stack.clone(),
                });
            }
            prev_section = &item.section;

            let is_current =
                list_index == state.current_list_index && item_index == state.current_item_index;
            if is_current {
                selected_index = list_items.len();
            }

            let label = item.header.as_deref().unwrap_or(&item.display_text);
            // Depth guides and the selected-marker tint both key off the item's
            // ancestor chain (outermost-first), so compute it once up front.
            let chain = list.parent_chain(item_index);
            // A nested current row's `❯` marker would otherwise take
            // `palette.current` as its reversed background (the row uses
            // `Modifier::REVERSED`), clashing with the colored depth guides on
            // the same row. Tint it with the item's own sub-list color —
            // the same slot as its deepest guide — so the highlighted
            // background matches instead; top-level rows (no guides) keep the
            // cyan current accent.
            let current_marker_fg = if item.depth > 0 {
                let slot = chain.get(item.depth - 1).map_or(item.depth - 1, |&parent| {
                    state.document.sublist_slot(list_index, parent)
                });
                palette.depth_color(slot)
            } else {
                palette.current
            };
            // Precedence: done > started > info > current > pending, so an
            // item's kind/state stays visible even on the current row (which
            // the list's own highlight already marks).
            let info_state = list.info_parent_state(item_index);
            let (marker, marker_style, text_style) = match (item.kind, is_current) {
                (ItemKind::Checkbox(TaskState::Done), _) => (
                    icons.done,
                    Style::default()
                        .fg(palette.done)
                        .add_modifier(Modifier::BOLD),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                (ItemKind::Checkbox(TaskState::Started), _) => (
                    icons.started,
                    Style::default().fg(palette.started),
                    Style::default(),
                ),
                // Info item: the note glyph (shape says "information") tinted
                // by its sub-list's aggregate state — green
                // all-done with a dimmed label like a done task, yellow
                // in-progress, else the plain note blue.
                (ItemKind::DisplayOnly, _) => {
                    let color = match info_state {
                        Some(TaskState::Done) => palette.done,
                        Some(TaskState::Started) => palette.started,
                        _ => palette.note,
                    };
                    let text_style = if matches!(info_state, Some(TaskState::Done)) {
                        Style::default().add_modifier(Modifier::DIM)
                    } else {
                        Style::default()
                    };
                    (icons.note, Style::default().fg(color), text_style)
                }
                (_, true) => (
                    icons.current,
                    Style::default().fg(current_marker_fg),
                    Style::default(),
                ),
                (_, false) => (
                    icons.pending,
                    Style::default().add_modifier(Modifier::DIM),
                    Style::default(),
                ),
            };

            // Nesting depth guides: the has_list_header base stays blank
            // (it aligns items under the list title, not a nesting level), then
            // one `│` guide per depth level. Each guide is colored by the
            // sub-list it belongs to — the ancestor at that level, via
            // `sublist_slot` — so distinct sub-lists (even at the same depth)
            // get distinct colors and don't blend. Each guide and the marker
            // are single display cells, so the marker prefix keeps the same
            // on-screen width and the click toggle-zone (`marker_cells`) is
            // unchanged.
            let mut spans: Vec<Span> = Vec::new();
            if has_list_header {
                spans.push(Span::raw("  "));
            }
            for level in 0..item.depth {
                let slot = chain.get(level).map_or(level, |&ancestor| {
                    state.document.sublist_slot(list_index, ancestor)
                });
                spans.push(Span::styled(
                    "│ ",
                    Style::default().fg(palette.depth_color(slot)),
                ));
            }
            let marker_cells = (has_list_header as usize + item.depth + 1) as u16 * 2;
            spans.push(Span::styled(format!("{marker} "), marker_style));
            spans.push(Span::styled(label.to_string(), text_style));
            list_items.push(ListItem::new(Line::from(spans)));
            // A checkbox row splits into a marker zone (toggle) and a label
            // zone (navigate); a display-only row navigates whole.
            row_clicks.push(match item.kind {
                ItemKind::Checkbox(_) => RowClick::TaskItem {
                    list_index,
                    item_index,
                    marker_cells,
                },
                ItemKind::DisplayOnly => {
                    RowClick::Whole(OverviewTarget::Item(list_index, item_index))
                }
            });
            row_sticky.push(StickyContext {
                list_header,
                stack: active_stack.clone(),
            });
        }
    }

    let total_rows = list_items.len();

    // Draw the panel frame ourselves: the sticky header reserves rows at
    // the *top* of the content area and the scrollable list renders below it, so
    // a pinned header never covers a list row — reserve-space, not overlay. The
    // border + 1 col of horizontal padding matches the card body's BODY_PAD;
    // the panel carries no `done/total` counter since the title-bar counter
    // above it is always shown in this wide layout.
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" Overview ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner_height = inner.height as usize;

    let list_widget =
        List::new(list_items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    // The ancestor rows to pin when the row at content `offset` is the first
    // visible one: the `[n] Title` list header and each `── divider` of the
    // active `### H3`+ path whose own row has scrolled above the content area.
    // Always a prefix of the chain (an ancestor's row precedes its descendants'),
    // capped so it can't fill the whole panel.
    let sticky_lines = |offset: usize| -> Vec<Line<'static>> {
        let mut lines: Vec<Line> = Vec::new();
        let Some(ctx) = row_sticky.get(offset) else {
            return lines;
        };
        if let Some((row, list_index)) = ctx.list_header
            && row < offset
        {
            lines.push(list_header_line(
                icons,
                palette,
                list_index,
                &state.document.lists[list_index].title,
                list_index == state.current_list_index,
            ));
        }
        for (heading, divider_row) in &ctx.stack {
            if *divider_row < offset {
                lines.push(section_divider_line(heading, has_list_header, inner_width));
            }
        }
        lines.truncate(inner_height.saturating_sub(1));
        lines
    };

    // How many rows to reserve depends on the scroll offset, which depends on
    // the reserved rows (they shrink the list viewport). Iterate to a fixed
    // point — it converges immediately while the top row stays in one group,
    // since that group's ancestor chain is constant. The last render in the loop
    // is the one that stays: its list content sits in the content area below the
    // reserved rows.
    let mut reserved = 0usize;
    let mut offset = 0usize;
    for _ in 0..4 {
        let content = Rect::new(
            inner.x,
            inner.y + reserved as u16,
            inner.width,
            inner.height - reserved as u16,
        );
        let mut list_state = ListState::default().with_selected(Some(selected_index));
        frame.render_stateful_widget(list_widget.clone(), content, &mut list_state);
        offset = list_state.offset();
        let want = sticky_lines(offset).len();
        if want == reserved {
            break;
        }
        reserved = want;
    }

    // Pin the reserved rows at the top of the content area. Clear each first so
    // no stale content from an earlier loop pass shows through.
    let pinned = sticky_lines(offset);
    reserved = pinned.len().min(inner_height.saturating_sub(1));
    for (k, line) in pinned.into_iter().take(reserved).enumerate() {
        let row = Rect::new(inner.x, inner.y + k as u16, inner.width, 1);
        frame.render_widget(Clear, row);
        frame.render_widget(Paragraph::new(line), row);
    }

    // A scrollbar on the right border when the rows overflow, reflecting
    // the list's scroll within the (reserved-shrunk) content viewport.
    let content_viewport = inner_height - reserved;
    super::render_scrollbar(frame, area, total_rows, content_viewport, offset);

    // Record each visible clickable row's on-screen Rect for hit-testing left
    // clicks. List rows render below the reserved header rows, each
    // exactly one line tall (the List truncates rather than wraps): the item at
    // list index `i` sits at `inner.y + reserved + (i - offset)`. The reserved
    // header rows carry no click target (non-interactive, like a divider).
    state.overview_rows.clear();
    let row_end = area.x + area.width; // one past the row's last column
    for (i, click) in row_clicks.iter().enumerate() {
        if i < offset || i >= offset + content_viewport {
            continue; // scrolled out of view
        }
        let y = inner.y + (reserved + (i - offset)) as u16;
        match *click {
            RowClick::None => {}
            RowClick::Whole(target) => {
                state
                    .overview_rows
                    .push((Rect::new(area.x, y, area.width, 1), target));
            }
            RowClick::TaskItem {
                list_index,
                item_index,
                marker_cells,
            } => {
                // The marker prefix begins at the content origin (left border +
                // 1 col of Padding::horizontal). The toggle zone spans the whole
                // left edge through the marker; the label to the right edge
                // navigates. The two tile the row with no gap.
                let marker_end = (area.x + 2 + marker_cells).min(row_end);
                state.overview_rows.push((
                    Rect::new(area.x, y, marker_end - area.x, 1),
                    OverviewTarget::Toggle(list_index, item_index),
                ));
                if marker_end < row_end {
                    state.overview_rows.push((
                        Rect::new(marker_end, y, row_end - marker_end, 1),
                        OverviewTarget::Item(list_index, item_index),
                    ));
                }
            }
        }
    }
}

/// How an overview row responds to a left-click, before it's mapped to an
/// on-screen `Rect`.
enum RowClick {
    /// Not clickable (a banner row).
    None,
    /// The whole row maps to one target (list-title row, or a display-only
    /// item row that navigates).
    Whole(OverviewTarget),
    /// A checkbox item row: its marker prefix toggles, its label navigates.
    TaskItem {
        list_index: usize,
        item_index: usize,
        marker_cells: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_indent_grows_with_depth() {
        // With a list header, items get a base level plus one per depth.
        assert_eq!(item_indent(0, true), "  ");
        assert_eq!(item_indent(1, true), "    ");
        assert_eq!(item_indent(2, true), "      ");
    }

    #[test]
    fn item_indent_drops_base_without_list_header() {
        // Single-list: no base level, so top-level items sit flush.
        assert_eq!(item_indent(0, false), "");
        assert_eq!(item_indent(1, false), "  ");
        assert_eq!(item_indent(2, false), "    ");
    }

    fn sub(level: u8, text: &str) -> SubHeading {
        SubHeading {
            level,
            text: text.to_string(),
        }
    }

    #[test]
    fn section_divider_line_truncates_a_long_heading_with_an_ellipsis() {
        // A long sub-heading used to hard-clip mid-word with no indication,
        // unlike the equivalent card-side breadcrumb.
        let heading = sub(3, "A very long sub-section heading that will not fit");
        let line = section_divider_line(&heading, true, 20);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains('…'), "truncated with an ellipsis: {text:?}");
        assert!(
            !text.contains("will not fit"),
            "the tail of the heading was actually cut: {text:?}"
        );
    }

    #[test]
    fn section_divider_line_leaves_a_short_heading_untouched() {
        let heading = sub(3, "Short");
        let line = section_divider_line(&heading, true, 40);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("Short"));
        assert!(!text.contains('…'));
    }

    #[test]
    fn new_sub_headings_returns_the_divergent_tail() {
        let outer = vec![sub(3, "A")];
        let inner = vec![sub(3, "A"), sub(4, "B")];
        // Entering the group: everything is new.
        assert_eq!(new_sub_headings(&[], &outer), &outer[..]);
        // Descending into a deeper level: only the new deeper heading.
        assert_eq!(new_sub_headings(&outer, &inner), &inner[1..]);
        // Continuing the same group: nothing new (no divider redrawn).
        assert_eq!(new_sub_headings(&inner, &inner), &[] as &[SubHeading]);
        // Switching to a sibling replaces the tail from the divergence point.
        let sibling = vec![sub(3, "C")];
        assert_eq!(new_sub_headings(&inner, &sibling), &sibling[..]);
        // Leaving the sub-section entirely: no new headings to draw.
        assert_eq!(new_sub_headings(&outer, &[]), &[] as &[SubHeading]);
    }
}
