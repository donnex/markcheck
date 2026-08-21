use std::fs;
use std::path::PathBuf;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::model::{
    BodySpan, Document, Item, ItemKind, LineNumber, List, SubHeading, TaskState, TextStyle,
};

/// Converts byte offsets into 1-indexed line numbers by counting preceding
/// newlines. Amortizes to O(n) total across the many non-decreasing offsets
/// pulldown-cmark's single-pass event stream naturally produces, instead
/// of the O(n) per call / O(n·m) total of rescanning from byte 0
/// every time. Falls back to a full rescan if a query ever goes backwards,
/// so it stays correct even if that assumption doesn't hold in some case.
struct LineCounter {
    offset: usize,
    line: LineNumber,
}

impl LineCounter {
    fn new() -> Self {
        Self { offset: 0, line: 1 }
    }

    fn line_at(&mut self, target: usize, source: &str) -> LineNumber {
        if target < self.offset {
            self.offset = 0;
            self.line = 1;
        }
        self.line += source[self.offset..target]
            .chars()
            .filter(|&c| c == '\n')
            .count();
        self.offset = target;
        self.line
    }
}

/// The raw Markdown level (1–6) of a `pulldown-cmark` `HeadingLevel`, used to
/// order the `### H3`+ sub-section stack.
fn heading_level_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// pulldown-cmark's task-list extension only recognizes `[ ]` and `[x]`,
/// not the `[/]` "started" marker. Since `[/]` and `[ ]` are both
/// three bytes, we rewrite `[/]` task markers to `[ ]` before parsing —
/// byte offsets (and therefore line numbers) are preserved — and return
/// the 1-indexed lines that were `[/]` so their items can be promoted to
/// `Started` afterwards.
fn extract_started_markers(raw: &str) -> (String, std::collections::HashSet<LineNumber>) {
    let mut started_lines = std::collections::HashSet::new();
    // Track fenced-code-block boundaries so a code line that merely
    // *looks* like a `[/]` bullet (e.g. a runbook documenting markcheck's own
    // syntax) is never rewritten — this scan runs on raw text, before
    // pulldown-cmark has identified any fence boundaries itself.
    let mut fence: Option<(char, usize)> = None;
    let processed = raw
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            if let Some((fence_char, fence_len)) = fence {
                if is_closing_fence(line, fence_char, fence_len) {
                    fence = None;
                }
                return line.to_string();
            }
            if let Some(marker) = fence_delimiter(line) {
                fence = Some(marker);
                return line.to_string();
            }
            if is_started_task_line(line) {
                started_lines.insert(index + 1);
                line.replacen("[/]", "[ ]", 1)
            } else {
                line.to_string()
            }
        })
        .collect();
    (processed, started_lines)
}

/// Strips any number of leading `>` blockquote markers (each optionally
/// followed by whitespace) plus ordinary leading whitespace, per CommonMark —
/// shared by `is_started_task_line` and `fence_delimiter` so both agree on
/// where a blockquote's content starts. A fence delimiter quoted inside a
/// `>` blockquote must be recognized as a fence boundary the same way a
/// plain one is, or a `[/]`-lookalike line inside it gets incorrectly
/// rewritten.
fn strip_blockquote_markers(line: &str) -> &str {
    let mut rest = line.trim_start();
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.trim_start();
    }
    rest
}

/// A fenced-code-block delimiter line: an (optionally indented, optionally
/// blockquoted) run of 3+ backticks or 3+ tildes, per CommonMark. Returns the
/// fence character and its length so callers can match an opening fence to
/// its closer.
fn fence_delimiter(line: &str) -> Option<(char, usize)> {
    let trimmed = strip_blockquote_markers(line);
    let fence_char = trimmed.chars().next()?;
    if fence_char != '`' && fence_char != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == fence_char).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string can't itself contain a backtick.
    if fence_char == '`' && trimmed[len..].contains('`') {
        return None;
    }
    Some((fence_char, len))
}

/// Whether `line` closes a fence opened with `open_char`/`open_len`. Unlike
/// an opening fence, CommonMark allows a closing fence line to contain
/// *nothing* after the fence run but trailing whitespace — no info string.
/// `fence_delimiter` alone doesn't check this (it's shared with opening-line
/// detection, where trailing content is an info string, not a violation),
/// so without this a line like `` ```done `` would wrongly be treated as a
/// closer here while pulldown-cmark itself does not see it as one — the
/// exact preprocessor/parser disagreement this scan exists to avoid.
fn is_closing_fence(line: &str, open_char: char, open_len: usize) -> bool {
    let trimmed = strip_blockquote_markers(line);
    let len = trimmed.chars().take_while(|&c| c == open_char).count();
    len >= open_len && trimmed[len..].trim().is_empty()
}

/// True if `line` is a task-list bullet whose marker is `[/]`, i.e. optional
/// indentation and blockquote nesting, a list marker, whitespace, then `[/]`.
fn is_started_task_line(line: &str) -> bool {
    // Any number of leading `>` blockquote levels: a `[/]` bullet nested
    // inside one (`> - [/] x`, possibly several `>` deep) still needs
    // rewriting so pulldown recognizes it as a checkbox at all — it's then
    // forced to a non-interactive `DisplayOnly` card the same way it
    // already does for a blockquoted `[ ]`/`[x]`, rather than the marker
    // surviving unrewritten and the bullet reading as plain, non-task text.
    let rest = strip_blockquote_markers(line);
    let Some(after_marker) = strip_list_marker(rest) else {
        return false;
    };
    // CommonMark (and pulldown-cmark's own checkbox detection) accepts a
    // space *or* a tab between the marker and its content — matching only
    // a literal space here let a tab-separated `[/]` bullet fall through
    // to a plain, non-checkbox item.
    match after_marker.chars().next() {
        Some(c) if c.is_whitespace() => after_marker.trim_start().starts_with("[/]"),
        _ => false,
    }
}

/// Strips a leading list-item marker — an unordered bullet (`-`/`*`/`+`) or
/// an ordered marker (one to nine ASCII digits, the CommonMark limit,
/// followed by `.` or `)`) — returning the remainder, or `None` if `rest`
/// doesn't start with one.
fn strip_list_marker(rest: &str) -> Option<&str> {
    if let Some(after) = rest
        .strip_prefix('-')
        .or_else(|| rest.strip_prefix('*'))
        .or_else(|| rest.strip_prefix('+'))
    {
        return Some(after);
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    // ASCII digits are one byte each, so this index always lands on a char
    // boundary.
    let after_digits = &rest[digits..];
    after_digits
        .strip_prefix('.')
        .or_else(|| after_digits.strip_prefix(')'))
}

/// Mutable state for one list item while it's being parsed. Kept on a stack
/// so a nested item's Start/End (interleaved inside its parent's) doesn't
/// clobber the parent's fields.
#[derive(Default)]
struct ItemBuilder {
    line: LineNumber,
    depth: usize,
    /// `current_list_items.len()` at the moment this item was pushed.
    /// Since a bullet's descendants always finish (post-order `End(Item)`)
    /// before the bullet's own `End(Item)`, any growth by then is exactly
    /// this bullet's own children — used to keep a bullet with children from
    /// being eaten as the list banner, which would orphan them.
    items_before: usize,
    /// True if this item's bullet sits inside a `>` blockquote. A
    /// blockquoted checkbox reads as a quoted/illustrative example, not a
    /// real task, so it's forced to a non-interactive display-only card
    /// regardless of any `TaskListMarker` pulldown-cmark reports for it.
    in_blockquote: bool,
    /// Snapshot of the active `### H3`+ sub-section path when this item was
    /// opened. A heading can't appear inside a list, so this is stable
    /// for the whole item and shared by every item in the same Markdown list.
    section: Vec<SubHeading>,
    kind: Option<ItemKind>,
    text: String,
    header: String,
    capturing_header: bool,
    header_done: bool,
    code_spans: Vec<String>,
    code_blocks: Vec<String>,
    body: Vec<BodySpan>,
    /// Active inline styling for the text runs currently being read,
    /// toggled by Emphasis/Strong/Strikethrough start/end events.
    style: TextStyle,
    /// `Some((url, text))` while inside a link: the destination plus the link
    /// text accumulated so far; flushed to a `BodySpan::Link` on link end.
    link: Option<(String, String)>,
}

pub fn parse_document(path: PathBuf) -> anyhow::Result<Document> {
    let raw = fs::read_to_string(&path)?;
    let document = parse_source(&raw, path);
    Ok(document)
}

fn parse_source(raw: &str, path: PathBuf) -> Document {
    let raw_lines: Vec<String> = raw.lines().map(str::to_string).collect();
    let uses_crlf = raw.contains("\r\n");
    let trailing_newline = raw.ends_with('\n');

    // A leading UTF-8 BOM defeats ATX heading recognition (CommonMark
    // requires `#`/`##` to start the line) and would otherwise silently lose
    // the document title or misfile a section's items into the default
    // list. Stripped only from the parsing copy — `raw_lines` keeps the
    // original bytes (BOM included) so write-back never touches it.
    let raw_for_parsing = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);

    // Rewrite `[/]` started markers to `[ ]` so pulldown recognizes them as
    // task items; `started_lines` records which to promote to `Started`.
    // Offsets are preserved, so line numbers still index `raw_lines`.
    let (source, started_lines) = extract_started_markers(raw_for_parsing);

    let mut lists: Vec<List> = Vec::new();
    let mut default_items: Vec<Item> = Vec::new();

    let mut in_heading = false;
    let mut heading_level: Option<HeadingLevel> = None;
    let mut heading_text = String::new();
    // The first non-empty `# H1` becomes the document title.
    let mut document_title: Option<String> = None;

    // The active `### H3`+ sub-section path: a heading stack, reduced on
    // each heading (pop entries at level >= the new one, then push it) and
    // cleared at every `# H1` / `## H2` boundary, which start a fresh list
    // context. Snapshotted onto each item as it's opened.
    let mut section_stack: Vec<SubHeading> = Vec::new();

    // Items are buffered per top-level list: a list containing no checkbox
    // at all contributes nothing to the document.
    let mut list_depth: usize = 0;
    let mut current_list_items: Vec<Item> = Vec::new();
    // True once a real `TaskListMarker` has been seen in the current
    // top-level list, reset at each new one. This is the has-checkbox
    // signal for the drop-if-empty rule — tracked independently of item
    // *kind* so a blockquoted checkbox (forced to `DisplayOnly`) still
    // counts as "had real task syntax" and doesn't vanish the whole segment.
    let mut list_had_checkbox_syntax = false;

    // Depth of `>` blockquote nesting, so a checkbox bullet quoted inside
    // one (e.g. a runbook illustrating example output) can be told apart
    // from a real, live task.
    let mut blockquote_depth: usize = 0;

    // The first *top-level* item of a list becomes that list's banner (a
    // non-navigable warning line) instead of a card when it's a bold-only
    // bullet; every later bold-only bullet is a normal display-only card.
    // `banner` is set directly on the list (for H2 lists) or on
    // `default_banner` (for the pre-heading list). Gated to depth 0 and reset
    // at each H2. Nested bold-only bullets never become banners.
    let mut seen_first_top_item = false;
    let mut default_banner: Option<String> = None;

    // A fenced code block at column 0 immediately after a kept list is
    // attached to that list's last item. Any other event between the
    // list and the fence breaks the attachment.
    let mut attach_fence_to_last_item = false;
    let mut code_block: Option<(bool, String)> = None; // (fenced, content)

    // One frame per open item; a stack so nested items don't clobber their
    // parent's in-progress fields. The innermost open item is the top.
    let mut item_stack: Vec<ItemBuilder> = Vec::new();
    let mut line_counter = LineCounter::new();

    let options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(&source, options).into_offset_iter();

    for (event, range) in parser {
        // Any block-level start other than a fenced code block breaks the
        // fence-attaches-to-previous-list rule.
        let keeps_attach = matches!(
            event,
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))
                | Event::Text(_)
                | Event::Code(_)
                | Event::End(_)
                | Event::TaskListMarker(_)
        );
        if !keeps_attach {
            attach_fence_to_last_item = false;
        }

        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_level = Some(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(level)) => {
                if heading_level == Some(level) {
                    let text = heading_text.trim();
                    if level == HeadingLevel::H2 {
                        lists.push(List {
                            title: text.to_string(),
                            banner: None,
                            items: Vec::new(),
                        });
                        seen_first_top_item = false;
                        // A new list is a fresh sub-section context.
                        section_stack.clear();
                    } else if level == HeadingLevel::H1 {
                        // First non-empty H1 is the document title. H1 is
                        // top-level, so it also clears any sub-section.
                        if document_title.is_none() && !text.is_empty() {
                            document_title = Some(text.to_string());
                        }
                        section_stack.clear();
                    } else if !text.is_empty() {
                        // `### H3`+ opens a sub-section within the current
                        // list: pop any siblings/deeper levels, then push it.
                        let heading_level = heading_level_number(level);
                        section_stack.retain(|h| h.level < heading_level);
                        section_stack.push(SubHeading {
                            level: heading_level,
                            text: text.to_string(),
                        });
                    }
                }
                in_heading = false;
                heading_level = None;
            }
            Event::Start(Tag::List(_)) => {
                if list_depth == 0 {
                    list_had_checkbox_syntax = false;
                }
                list_depth += 1;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                blockquote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                blockquote_depth = blockquote_depth.saturating_sub(1);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 {
                    if list_had_checkbox_syntax {
                        let target = match lists.last_mut() {
                            Some(list) => &mut list.items,
                            None => &mut default_items,
                        };
                        // Items are pushed at End(Item), so a nested subtree
                        // arrives post-order (children before their parent,
                        // whose End fires last). Sort by source line to restore
                        // document/pre-order — a parent bullet always precedes
                        // its indented children.
                        current_list_items.sort_by_key(|item| item.line_number);
                        target.append(&mut current_list_items);
                        attach_fence_to_last_item = true;
                    } else {
                        current_list_items.clear();
                        attach_fence_to_last_item = false;
                    }
                }
            }
            Event::Start(Tag::Item) => {
                // A stack frame per open item so nested items don't clobber
                // their parent's fields; list_depth was already incremented by
                // the enclosing Start(List), so depth 0 = top-level.
                item_stack.push(ItemBuilder {
                    line: line_counter.line_at(range.start, &source),
                    depth: list_depth.saturating_sub(1),
                    section: section_stack.clone(),
                    items_before: current_list_items.len(),
                    in_blockquote: blockquote_depth > 0,
                    ..Default::default()
                });
            }
            Event::TaskListMarker(checked) => {
                list_had_checkbox_syntax = true;
                if let Some(item) = item_stack.last_mut() {
                    item.kind = Some(ItemKind::Checkbox(if checked {
                        TaskState::Done
                    } else {
                        TaskState::NotStarted
                    }));
                }
            }
            Event::Start(Tag::Strong) => {
                if let Some(item) = item_stack.last_mut() {
                    // A leading bold-only run is the card title; a bold
                    // run anywhere else is inline strong styling in the body.
                    if !item.header_done && item.text.trim().is_empty() {
                        item.capturing_header = true;
                    } else {
                        item.style.strong = true;
                    }
                }
            }
            Event::End(TagEnd::Strong) => {
                if let Some(item) = item_stack.last_mut() {
                    if item.capturing_header {
                        item.capturing_header = false;
                        item.header_done = true;
                    } else {
                        item.style.strong = false;
                    }
                }
            }
            // Inline emphasis / strikethrough styling for body runs.
            Event::Start(Tag::Emphasis) => {
                if let Some(item) = item_stack.last_mut() {
                    item.style.emphasis = true;
                }
            }
            Event::End(TagEnd::Emphasis) => {
                if let Some(item) = item_stack.last_mut() {
                    item.style.emphasis = false;
                }
            }
            Event::Start(Tag::Strikethrough) => {
                if let Some(item) = item_stack.last_mut() {
                    item.style.strikethrough = true;
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                if let Some(item) = item_stack.last_mut() {
                    item.style.strikethrough = false;
                }
            }
            // A link: capture the destination, accumulate its text, and flush a
            // Link span on close. Link text still flows into `text` (and thus
            // display_text) so search matches it.
            Event::Start(Tag::Link { dest_url, .. }) => {
                if let Some(item) = item_stack.last_mut()
                    && !item.capturing_header
                {
                    item.link = Some((dest_url.to_string(), String::new()));
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some(item) = item_stack.last_mut()
                    && let Some((url, text)) = item.link.take()
                {
                    item.body.push(BodySpan::Link { text, url });
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let fenced = matches!(kind, CodeBlockKind::Fenced(_));
                code_block = Some((fenced, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                // Indented code blocks are never copy candidates; their
                // text is also kept out of display_text.
                if let Some((true, content)) = code_block.take() {
                    let content = content
                        .strip_suffix('\n')
                        .map(str::to_string)
                        .unwrap_or(content);
                    if let Some(item) = item_stack.last_mut() {
                        item.code_blocks.push(content);
                    } else if attach_fence_to_last_item {
                        let target = match lists.last_mut() {
                            Some(list) => &mut list.items,
                            None => &mut default_items,
                        };
                        if let Some(item) = target.last_mut() {
                            item.code_blocks.push(content);
                        }
                        // Flag stays set: several consecutive fences all
                        // attach (and then correctly refuse to copy).
                    }
                }
            }
            Event::Text(text) => {
                if let Some((_, buffer)) = &mut code_block {
                    buffer.push_str(&text);
                } else if in_heading {
                    heading_text.push_str(&text);
                } else if let Some(item) = item_stack.last_mut() {
                    if item.capturing_header {
                        item.header.push_str(&text);
                    } else if let Some((_, link_text)) = &mut item.link {
                        // Inside a link: accumulate its text (flushed on End),
                        // but still feed display_text so search matches it.
                        link_text.push_str(&text);
                        item.text.push_str(&text);
                    } else {
                        item.text.push_str(&text);
                        let style = item.style;
                        if style.is_plain() {
                            item.body.push(BodySpan::Text(text.to_string()));
                        } else {
                            item.body.push(BodySpan::Styled {
                                text: text.to_string(),
                                style,
                            });
                        }
                    }
                }
            }
            // A soft/hard line break within a paragraph is whitespace: without
            // this the words on either side would fuse ("to"+"show"→"toshow")
            // in display_text/search and in the card.
            Event::SoftBreak | Event::HardBreak => {
                if code_block.is_some() {
                    // Fenced/indented code content arrives via Text events.
                } else if in_heading {
                    heading_text.push(' ');
                } else if let Some(item) = item_stack.last_mut() {
                    if item.capturing_header {
                        item.header.push(' ');
                    } else if let Some((_, link_text)) = &mut item.link {
                        link_text.push(' ');
                        item.text.push(' ');
                    } else {
                        item.text.push(' ');
                        item.body.push(BodySpan::Text(" ".to_string()));
                    }
                }
            }
            Event::Code(text) => {
                if in_heading {
                    heading_text.push_str(&text);
                } else if let Some(item) = item_stack.last_mut() {
                    if let Some((_, link_text)) = &mut item.link {
                        // Code inside a link becomes part of the link text.
                        link_text.push_str(&text);
                        item.text.push_str(&text);
                    } else {
                        item.code_spans.push(text.to_string());
                        if item.capturing_header {
                            item.header.push_str(&text);
                        } else {
                            item.text.push_str(&text);
                            item.body.push(BodySpan::Code(text.to_string()));
                        }
                    }
                }
            }
            Event::End(TagEnd::Item) => {
                if let Some(builder) = item_stack.pop() {
                    let mut kind = builder.kind.unwrap_or(ItemKind::DisplayOnly);
                    // Promote a rewritten `[/]` marker to the Started state.
                    if kind == ItemKind::Checkbox(TaskState::NotStarted)
                        && started_lines.contains(&builder.line)
                    {
                        kind = ItemKind::Checkbox(TaskState::Started);
                    }
                    // A checkbox quoted inside a `>` blockquote is an
                    // illustrative example, not a real task — force it to a
                    // plain, non-interactive display-only card regardless of
                    // any `TaskListMarker` pulldown-cmark reported for it.
                    if builder.in_blockquote {
                        kind = ItemKind::DisplayOnly;
                    }
                    let header = {
                        let trimmed = builder.header.trim();
                        (!trimmed.is_empty()).then(|| trimmed.to_string())
                    };
                    let display_text = builder
                        .text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");

                    // Only the first *top-level* bold-only bullet becomes the
                    // list banner; nested and later bold-only bullets are
                    // ordinary display-only cards. A bullet with its own
                    // nested children is never banner-eligible either:
                    // the banner emits no `Item`, which would leave those
                    // children with no depth-0 parent in the flat list.
                    let has_children = current_list_items.len() > builder.items_before;
                    let is_bold_only = kind == ItemKind::DisplayOnly
                        && header.is_some()
                        && display_text.is_empty();
                    let is_banner = !seen_first_top_item
                        && is_bold_only
                        && builder.depth == 0
                        && !has_children
                        && !builder.in_blockquote;
                    if builder.depth == 0 {
                        seen_first_top_item = true;
                    }
                    if is_banner {
                        match lists.last_mut() {
                            Some(list) => list.banner = header,
                            None => default_banner = header,
                        }
                    } else {
                        current_list_items.push(Item {
                            line_number: builder.line,
                            depth: builder.depth,
                            section: builder.section,
                            display_text,
                            body: builder.body,
                            header,
                            code_spans: builder.code_spans,
                            code_blocks: builder.code_blocks,
                            kind,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let has_default_list = !default_items.is_empty();
    if has_default_list {
        lists.insert(
            0,
            List {
                title: "(Default)".to_string(),
                banner: default_banner,
                items: default_items,
            },
        );
    }

    // A list left without any items (its lists had no checkboxes, or it
    // never had a list) is dropped entirely.
    lists.retain(|list| !list.items.is_empty());

    Document {
        file_path: path,
        title: document_title,
        has_default_list,
        lists,
        raw_lines,
        uses_crlf,
        trailing_newline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const EXAMPLE: &str = "\
## Prepare workspace

- **Steps for the first workspace**
- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] `build-tool sync --profile default`
- [ ] `refresh-cache`
- [ ] `verify-output`
- [ ] `restart-service`

## Second workspace

- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] notes page `workspace-notes.md`
- [ ] `check-status example-host test`
";

    fn parse(source: &str) -> Document {
        parse_source(source, PathBuf::from("test.md"))
    }

    // --- Nested / sub-list support.

    const NESTED: &str = "\
## Setup

- **Do not skip**
- [ ] top task
  - [ ] child one
  - [ ] child two
    - [ ] grandchild
- [ ] another top
";

    #[test]
    fn nested_items_get_document_order_and_depth() {
        let list = &parse(NESTED).lists[0];
        // Banner consumed the bold-only bullet; five checkboxes remain, in
        // source order (parents before their children, despite post-order push).
        let got: Vec<(&str, usize)> = list
            .items
            .iter()
            .map(|i| (i.display_text.as_str(), i.depth))
            .collect();
        assert_eq!(
            got,
            vec![
                ("top task", 0),
                ("child one", 1),
                ("child two", 1),
                ("grandchild", 2),
                ("another top", 0),
            ]
        );
        assert_eq!(list.banner.as_deref(), Some("Do not skip"));
    }

    #[test]
    fn parent_chain_walks_ancestors_outermost_first() {
        let list = &parse(NESTED).lists[0];
        assert_eq!(list.parent_chain(0), Vec::<usize>::new()); // top task
        assert_eq!(list.parent_chain(1), vec![0]); // child one -> top task
        assert_eq!(list.parent_chain(3), vec![0, 2]); // grandchild -> top, child two
        assert_eq!(list.parent_chain(4), Vec::<usize>::new()); // another top
    }

    #[test]
    fn nested_bold_only_bullet_is_a_card_not_a_banner() {
        // Only the first top-level bold-only bullet is the banner; a bold-only
        // bullet inside a nested list is an ordinary display-only card.
        let list =
            &parse("## S\n\n- **Top banner**\n- [ ] parent\n  - **Nested note**\n  - [ ] child\n")
                .lists[0];
        assert_eq!(list.banner.as_deref(), Some("Top banner"));
        let nested_note = list
            .items
            .iter()
            .find(|i| i.header.as_deref() == Some("Nested note"))
            .expect("nested bold-only bullet is kept as a card");
        assert!(matches!(nested_note.kind, ItemKind::DisplayOnly));
        assert_eq!(nested_note.depth, 1);
    }

    // --- `### H3`+ sub-section support.

    /// The sub-section path (levels + texts) of each item, for asserting.
    fn sections(list: &List) -> Vec<Vec<(u8, &str)>> {
        list.items
            .iter()
            .map(|i| {
                i.section
                    .iter()
                    .map(|h| (h.level, h.text.as_str()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn h3_groups_following_items_into_a_sub_section() {
        let list =
            &parse("## Section\n\n- [ ] before\n\n### Sub\n\n- [ ] under a\n- [ ] under b\n").lists
                [0];
        assert_eq!(
            sections(list),
            vec![vec![], vec![(3, "Sub")], vec![(3, "Sub")]],
            "items before the H3 have no section; items after share it"
        );
    }

    #[test]
    fn h4_nests_under_h3_as_a_deeper_path() {
        let list = &parse("## S\n\n### Outer\n\n- [ ] a\n\n#### Inner\n\n- [ ] b\n").lists[0];
        assert_eq!(
            sections(list),
            vec![vec![(3, "Outer")], vec![(3, "Outer"), (4, "Inner")]],
            "an H4 nests as a second path segment under the H3"
        );
    }

    #[test]
    fn shallower_heading_replaces_deeper_levels() {
        // Outer H3 → Inner H4 → then a new H3 pops the H4 and replaces the H3.
        let list =
            &parse("## S\n\n### Outer\n\n#### Inner\n\n- [ ] a\n\n### Next\n\n- [ ] b\n").lists[0];
        assert_eq!(
            sections(list),
            vec![vec![(3, "Outer"), (4, "Inner")], vec![(3, "Next")]]
        );
    }

    #[test]
    fn item_less_sub_heading_is_dropped() {
        // An H3 immediately followed by another H3 (no items between) is never
        // referenced by any item, so it simply doesn't appear.
        let list = &parse("## S\n\n### Empty\n\n### Real\n\n- [ ] a\n").lists[0];
        assert_eq!(sections(list), vec![vec![(3, "Real")]]);
    }

    #[test]
    fn sub_section_resets_at_the_next_h2() {
        let doc = parse("## A\n\n### Sub\n\n- [ ] a\n\n## B\n\n- [ ] b\n");
        assert_eq!(sections(&doc.lists[0]), vec![vec![(3, "Sub")]]);
        assert_eq!(
            sections(&doc.lists[1]),
            vec![vec![]],
            "the new H2 starts a fresh sub-section context"
        );
    }

    #[test]
    fn h1_clears_any_sub_section() {
        // An H1 (document title) is top-level and clears the sub-section stack.
        let list = &parse("## S\n\n### Sub\n\n- [ ] a\n\n# Title\n\n- [ ] b\n").lists[0];
        assert_eq!(
            sections(list),
            vec![vec![(3, "Sub")], vec![]],
            "the item after the H1 has no sub-section"
        );
    }

    // --- Unusual / invalid documents: lock in the (sensible) behavior.

    #[test]
    fn ordered_list_items_are_checkboxes() {
        let s = &parse("## S\n\n1. [ ] one\n2. [x] two\n").lists[0];
        assert_eq!(s.items.len(), 2);
        assert!(matches!(
            s.items[0].kind,
            ItemKind::Checkbox(TaskState::NotStarted)
        ));
        assert!(matches!(
            s.items[1].kind,
            ItemKind::Checkbox(TaskState::Done)
        ));
    }

    #[test]
    fn loose_list_keeps_all_checkboxes() {
        // A blank line between bullets makes a "loose" list; both are still kept.
        let s = &parse("## S\n\n- [ ] a\n\n- [ ] b\n").lists[0];
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn mixed_checkbox_and_plain_bullets() {
        let s = &parse("## S\n\n- [ ] task\n- plain note\n- [x] done\n").lists[0];
        let kinds: Vec<_> = s.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Checkbox(TaskState::NotStarted),
                ItemKind::DisplayOnly,
                ItemKind::Checkbox(TaskState::Done),
            ]
        );
    }

    #[test]
    fn deeply_nested_lists_get_increasing_depth() {
        let s = &parse("## S\n\n- [ ] a\n  - [ ] b\n    - [ ] c\n      - [ ] d\n").lists[0];
        let depths: Vec<usize> = s.items.iter().map(|i| i.depth).collect();
        assert_eq!(depths, vec![0, 1, 2, 3]);
    }

    #[test]
    fn h3_and_deeper_headings_are_not_structural() {
        // Only `#` and `##` are structural; items under `###` fall into the
        // synthesized default list.
        let d = parse("### Deep\n\n- [ ] a\n");
        assert!(d.has_default_list);
        assert_eq!(d.lists[0].title, "(Default)");
    }

    #[test]
    fn uppercase_x_marker_parses_as_done() {
        let s = &parse("## S\n\n- [X] done\n").lists[0];
        assert!(matches!(
            s.items[0].kind,
            ItemKind::Checkbox(TaskState::Done)
        ));
    }

    #[test]
    fn empty_document_has_no_lists() {
        assert!(parse("").lists.is_empty());
    }

    #[test]
    fn banner_only_list_is_dropped() {
        // A list whose only content is a bold-only banner (no checkbox) leaves
        // the list empty, so it's dropped entirely.
        assert!(parse("## S\n\n- **just a banner**\n").lists.is_empty());
    }

    #[test]
    fn non_utf8_file_is_rejected_at_load() {
        let path =
            std::env::temp_dir().join(format!("markcheck-nonutf8-{}.md", std::process::id()));
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let result = parse_document(path.clone());
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err(), "non-UTF-8 input fails to load, not panics");
    }

    #[test]
    fn list_count_matches_h2_headings() {
        let document = parse(EXAMPLE);
        assert_eq!(document.lists.len(), 2);
        assert_eq!(document.lists[0].title, "Prepare workspace");
        assert_eq!(document.lists[1].title, "Second workspace");
    }

    #[test]
    fn first_list_has_expected_item_counts() {
        let document = parse(EXAMPLE);
        let list = &document.lists[0];
        // The leading bold-only bullet becomes the list banner, leaving
        // only the seven checkboxes as items.
        assert_eq!(list.items.len(), 7);
        assert!(
            list.items
                .iter()
                .all(|i| matches!(i.kind, ItemKind::Checkbox(_)))
        );
    }

    #[test]
    fn first_bold_only_bullet_becomes_list_banner() {
        let document = parse(EXAMPLE);
        assert_eq!(
            document.lists[0].banner.as_deref(),
            Some("Steps for the first workspace")
        );
        // The next list has no leading bold-only bullet.
        assert!(document.lists[1].banner.is_none());
    }

    #[test]
    fn duplicate_items_have_distinct_line_numbers() {
        let document = parse(EXAMPLE);
        let list = &document.lists[0];
        let duplicate_lines: Vec<_> = list
            .items
            .iter()
            .filter(|i| i.display_text == "refresh-cache")
            .map(|i| i.line_number)
            .collect();

        assert_eq!(duplicate_lines.len(), 4);
        let mut sorted = duplicate_lines.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "line numbers must all be distinct");
    }

    #[test]
    fn code_span_is_extracted_from_checkbox_item() {
        let document = parse(EXAMPLE);
        let list = &document.lists[0];
        let restart_item = list
            .items
            .iter()
            .find(|i| i.display_text == "restart-service")
            .expect("restart-service item not found");
        assert_eq!(restart_item.code_spans, vec!["restart-service".to_string()]);
    }

    #[test]
    fn item_with_text_and_code_span_combines_both() {
        let document = parse(EXAMPLE);
        let list = &document.lists[1];
        let notes_item = list
            .items
            .iter()
            .find(|i| i.display_text.starts_with("notes page"))
            .expect("notes page item not found");
        assert_eq!(notes_item.display_text, "notes page workspace-notes.md");
        assert_eq!(
            notes_item.code_spans,
            vec!["workspace-notes.md".to_string()]
        );
    }

    #[test]
    fn body_spans_preserve_text_and_code_order() {
        let source = "## S\n\n- [ ] run `build` then `deploy` now\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text("run ".to_string()),
                BodySpan::Code("build".to_string()),
                BodySpan::Text(" then ".to_string()),
                BodySpan::Code("deploy".to_string()),
                BodySpan::Text(" now".to_string()),
            ]
        );
        // display_text is still the flat concatenation used elsewhere.
        assert_eq!(item.display_text, "run build then deploy now");
    }

    #[test]
    fn emphasis_and_strikethrough_become_styled_spans() {
        let document = parse("## S\n\n- [ ] see *this* and ~~that~~ ok\n");
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text("see ".to_string()),
                BodySpan::Styled {
                    text: "this".to_string(),
                    style: TextStyle {
                        emphasis: true,
                        ..Default::default()
                    },
                },
                BodySpan::Text(" and ".to_string()),
                BodySpan::Styled {
                    text: "that".to_string(),
                    style: TextStyle {
                        strikethrough: true,
                        ..Default::default()
                    },
                },
                BodySpan::Text(" ok".to_string()),
            ]
        );
        // display_text stays plain, so search still matches the words.
        assert_eq!(item.display_text, "see this and that ok");
    }

    #[test]
    fn mid_text_bold_becomes_strong_styled() {
        // Leading bold is the card title; bold elsewhere is inline strong.
        let document = parse("## S\n\n- [ ] run the **important** step\n");
        let item = &document.lists[0].items[0];
        assert!(item.header.is_none());
        assert!(item.body.contains(&BodySpan::Styled {
            text: "important".to_string(),
            style: TextStyle {
                strong: true,
                ..Default::default()
            },
        }));
    }

    #[test]
    fn link_becomes_link_span_and_feeds_display_text() {
        let document = parse("## S\n\n- [ ] see the [runbook](https://example.com/rb) now\n");
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text("see the ".to_string()),
                BodySpan::Link {
                    text: "runbook".to_string(),
                    url: "https://example.com/rb".to_string(),
                },
                BodySpan::Text(" now".to_string()),
            ]
        );
        // Link text flows into display_text so search finds it; the URL is
        // exposed for the `o` open action.
        assert_eq!(item.display_text, "see the runbook now");
        assert_eq!(item.link_urls(), vec!["https://example.com/rb"]);
    }

    #[test]
    fn link_inside_the_title_loses_its_url_documented_limitation() {
        // A link entirely inside the leading-bold title is gated out
        // (`capturing_header`) — its text still folds into the plain title,
        // but the URL is discarded. Documented as a known limitation
        // (DESIGN.md/README) rather than fixed, since extracting it would
        // need a dedicated `Item` field threaded through every construction
        // site for a rare pattern. This test locks the documented behavior
        // in so a future change doesn't silently alter it either way.
        let document = parse("## S\n\n- [ ] **[Link](https://example.com/x) more** body\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.header.as_deref(), Some("Link more"));
        assert!(item.link_urls().is_empty());
    }

    #[test]
    fn soft_line_break_is_treated_as_a_space() {
        // An item wrapped across two source lines must not fuse the words at
        // the break ("beta"+"gamma") in display_text/search or the body.
        let document = parse("## S\n\n- [ ] alpha beta\n  gamma delta\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.display_text, "alpha beta gamma delta");
    }

    #[test]
    fn body_excludes_leading_bold_header() {
        let source = "## S\n\n- [ ] **Title** do `thing`\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(item.header.as_deref(), Some("Title"));
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text(" do ".to_string()),
                BodySpan::Code("thing".to_string()),
            ]
        );
    }

    #[test]
    fn items_before_first_heading_go_into_default_list() {
        let source = "- [ ] `orphan task`\n\n## List 1\n\n- [ ] `task`\n";
        let document = parse(source);
        assert_eq!(document.lists.len(), 2);
        assert_eq!(document.lists[0].title, "(Default)");
        assert_eq!(document.lists[0].items.len(), 1);
        assert_eq!(document.lists[0].items[0].display_text, "orphan task");
    }

    #[test]
    fn line_numbers_match_source_positions() {
        let document = parse(EXAMPLE);
        let list = &document.lists[0];
        // EXAMPLE starts with the H2 heading on line 1; the group-header
        // bullet on line 3 emits no item, so the checkboxes on lines 4-10
        // are the list's items.
        assert_eq!(list.items[0].line_number, 4);
        assert_eq!(list.items[6].line_number, 10);
    }

    #[test]
    fn document_with_no_lists_has_no_lists() {
        let source = "## Just a heading\n\nSome paragraph text, no list items.\n";
        let document = parse(source);
        assert!(document.lists.is_empty());
    }

    // --- Document title from the first H1 ---

    #[test]
    fn first_h1_becomes_document_title() {
        let document = parse("# My Runbook\n\n## S\n\n- [ ] `a`\n");
        assert_eq!(document.title.as_deref(), Some("My Runbook"));
        // H1 creates no list; only the H2 does.
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "S");
    }

    #[test]
    fn first_of_multiple_h1s_wins() {
        let document = parse("# First\n\n# Second\n\n## S\n\n- [ ] `a`\n");
        assert_eq!(document.title.as_deref(), Some("First"));
    }

    #[test]
    fn no_h1_leaves_title_none() {
        let document = parse("## S\n\n- [ ] `a`\n");
        assert_eq!(document.title, None);
    }

    #[test]
    fn leading_bold_only_in_default_list_becomes_banner() {
        // A banner attaches to the pre-heading (Default) list too.
        let document = parse("- **Read me first**\n- [ ] `orphan`\n\n## S\n\n- [ ] `a`\n");
        assert!(document.has_default_list);
        assert_eq!(document.lists[0].title, "(Default)");
        assert_eq!(document.lists[0].banner.as_deref(), Some("Read me first"));
        assert_eq!(document.lists[0].items.len(), 1);
    }

    #[test]
    fn checkbox_first_list_has_no_banner() {
        let document = parse("## S\n\n- [ ] `a`\n- **Bold**\n");
        assert!(document.lists[0].banner.is_none());
        // The bold-only bullet (not first) is a display-only card.
        assert_eq!(document.lists[0].items.len(), 2);
        assert_eq!(document.lists[0].items[1].kind, ItemKind::DisplayOnly);
    }

    #[test]
    fn has_default_list_flags_pre_heading_items() {
        // Items before the first H2 → a synthesized (Default) list.
        let with_default = parse("- [ ] `orphan`\n\n## S\n\n- [ ] `a`\n");
        assert!(with_default.has_default_list);
        // Everything under an H2 → no default list.
        let without = parse("## S\n\n- [ ] `a`\n");
        assert!(!without.has_default_list);
    }

    #[test]
    fn blank_h1_is_not_a_title() {
        let document = parse("#\n\n## S\n\n- [ ] `a`\n");
        assert_eq!(document.title, None);
    }

    #[test]
    fn leading_bom_does_not_defeat_h1_title() {
        // A UTF-8 BOM before the `#` used to stop it being recognized as
        // an ATX heading at all.
        let document = parse("\u{FEFF}# My Runbook\n\n## S\n\n- [ ] `a`\n");
        assert_eq!(document.title.as_deref(), Some("My Runbook"));
    }

    #[test]
    fn leading_bom_does_not_defeat_h2_list_title() {
        let document = parse("\u{FEFF}## Heading\n\n- [ ] `a`\n");
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "Heading");
        assert!(!document.has_default_list);
    }

    #[test]
    fn leading_bom_is_preserved_in_raw_lines_for_write_back() {
        // The BOM is stripped only for parsing; write-back must reproduce
        // the original bytes (including the BOM) untouched.
        let document = parse("\u{FEFF}# Title\n\n## S\n\n- [ ] `a`\n");
        assert_eq!(document.raw_lines[0], "\u{FEFF}# Title");
    }

    // --- LineCounter (offset_to_line perf refactor) ---

    #[test]
    fn line_counter_matches_naive_recount_for_increasing_offsets() {
        let source = "a\nbb\nccc\n\nddd\neeee\n";
        let naive = |offset: usize| source[..offset].chars().filter(|&c| c == '\n').count() + 1;
        let mut counter = LineCounter::new();
        for offset in [0, 2, 5, 9, 10, 14, 19] {
            assert_eq!(counter.line_at(offset, source), naive(offset));
        }
    }

    #[test]
    fn line_counter_falls_back_correctly_on_a_backwards_query() {
        let source = "a\nbb\nccc\n";
        let mut counter = LineCounter::new();
        assert_eq!(counter.line_at(9, source), 4);
        // A query behind the last one still returns the correct line.
        assert_eq!(counter.line_at(2, source), 2);
    }

    // --- The `[/]` started marker ---

    #[test]
    fn started_marker_parses_as_started_state_with_clean_text() {
        let source = "## S\n\n- [/] `deploy` the service\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(item.kind, ItemKind::Checkbox(TaskState::Started));
        assert_eq!(item.display_text, "deploy the service");
        assert!(!item.display_text.contains("[/]"), "marker stripped");
    }

    #[test]
    fn mixed_task_states_parse_independently() {
        let source = "## S\n\n- [ ] `a`\n- [/] `b`\n- [x] `c`\n";
        let document = parse(source);
        let kinds: Vec<_> = document.lists[0].items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Checkbox(TaskState::NotStarted),
                ItemKind::Checkbox(TaskState::Started),
                ItemKind::Checkbox(TaskState::Done),
            ]
        );
    }

    #[test]
    fn started_marker_parses_on_an_ordered_list() {
        // `[/]` used to only be recognized after a `-`/`*`/`+` bullet, so
        // an ordered-list `[/]` fell through unrewritten and the item
        // showed up as a broken DisplayOnly card with the raw marker still
        // in its text. `[ ]`/`[x]` already worked on ordered lists
        // (`ordered_list_items_are_checkboxes`) — only `[/]` was affected.
        let source = "## S\n\n1. [ ] `a`\n2. [/] `b`\n3. [x] `c`\n";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(
            items.iter().map(|i| i.kind).collect::<Vec<_>>(),
            vec![
                ItemKind::Checkbox(TaskState::NotStarted),
                ItemKind::Checkbox(TaskState::Started),
                ItemKind::Checkbox(TaskState::Done),
            ]
        );
        assert_eq!(items[1].display_text, "b");
        assert!(
            !items[1].display_text.contains("[/]"),
            "marker stripped, not left as raw text"
        );
    }

    #[test]
    fn started_marker_line_numbers_stay_aligned() {
        // The `[/]`→`[ ]` rewrite is length-preserving, so line numbers
        // still index the original source correctly.
        let source = "## S\n\n- [ ] `a`\n- [/] `b`\n";
        let document = parse(source);
        assert_eq!(document.lists[0].items[0].line_number, 3);
        assert_eq!(document.lists[0].items[1].line_number, 4);
    }

    #[test]
    fn is_started_task_line_only_matches_started_bullets() {
        assert!(is_started_task_line("- [/] task"));
        assert!(is_started_task_line("  * [/] indented"));
        assert!(!is_started_task_line("- [ ] not started"));
        assert!(!is_started_task_line("- [x] done"));
        assert!(!is_started_task_line("plain text [/] not a bullet"));
    }

    #[test]
    fn is_started_task_line_accepts_a_tab_separator() {
        // CommonMark (and pulldown-cmark's own checkbox detection) accepts
        // a tab between the bullet and its content, not just a space.
        assert!(is_started_task_line("-\t[/] tab-separated"));
        assert!(is_started_task_line("+\t[/] tab-separated"));
        assert!(!is_started_task_line("-\t[ ] not started"));
    }

    #[test]
    fn is_started_task_line_accepts_ordered_markers() {
        // An ordered-list marker (`.` or `)` delimiter) qualifies too, not
        // just `-`/`*`/`+`.
        assert!(is_started_task_line("1. [/] ordered task"));
        assert!(is_started_task_line("2) [/] ordered task"));
        assert!(is_started_task_line("  10. [/] indented ordered task"));
        assert!(!is_started_task_line("1. [ ] not started"));
        assert!(!is_started_task_line("0123456789. [/] too many digits"));
    }

    #[test]
    fn is_started_task_line_accepts_blockquote_nesting() {
        // A `[/]` bullet nested inside one or more `>` blockquote levels
        // still needs rewriting, so it can then be demoted to DisplayOnly
        // rather than the marker surviving unrewritten.
        assert!(is_started_task_line("> - [/] quoted"));
        assert!(is_started_task_line(">- [/] quoted, no space"));
        assert!(is_started_task_line("> > - [/] nested quote"));
        assert!(is_started_task_line("> 1. [/] quoted ordered"));
        assert!(!is_started_task_line("> - [ ] not started"));
    }

    #[test]
    fn fence_delimiter_recognizes_backtick_and_tilde_fences() {
        assert_eq!(fence_delimiter("```"), Some(('`', 3)));
        assert_eq!(fence_delimiter("````shell\n"), Some(('`', 4)));
        assert_eq!(fence_delimiter("  ~~~\n"), Some(('~', 3)));
        assert_eq!(fence_delimiter("not a fence"), None);
        assert_eq!(fence_delimiter("``\n"), None, "only 2 backticks");
        assert_eq!(
            fence_delimiter("```has ` a backtick\n"),
            None,
            "backtick info string can't contain a backtick"
        );
    }

    #[test]
    fn is_closing_fence_requires_only_whitespace_after_the_fence_run() {
        assert!(is_closing_fence("```\n", '`', 3), "bare closer");
        assert!(
            is_closing_fence("```   \n", '`', 3),
            "trailing whitespace only"
        );
        assert!(is_closing_fence("````\n", '`', 3), "longer run also closes");
        assert!(
            !is_closing_fence("```shell\n", '`', 3),
            "an info string closes nothing per CommonMark, unlike an opener"
        );
        assert!(
            !is_closing_fence("```done\n", '`', 3),
            "trailing non-whitespace content after the run is not a closer"
        );
        assert!(!is_closing_fence("~~~\n", '`', 3), "wrong fence character");
        assert!(
            !is_closing_fence("``\n", '`', 3),
            "run shorter than the opener"
        );
    }

    #[test]
    fn fence_with_trailing_text_on_the_would_be_closing_line_does_not_close_it() {
        // CommonMark requires a closing fence line to contain nothing but
        // the fence run and trailing whitespace; a line like "```done" is
        // NOT a closer to pulldown-cmark. Before this fix, the preprocessing
        // scan disagreed and closed the fence early, so the `[/]` marker on
        // the line after would have been incorrectly rewritten to `[ ]`
        // (it's plain text inside what's still — per the real parser — a
        // fenced code block).
        let source = "\
## S

- [ ] task with a fence

  ```
  first line
  ```done
  - [/] still inside the fence per CommonMark
  ```
";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.code_blocks,
            vec!["first line\n```done\n- [/] still inside the fence per CommonMark".to_string()]
        );
    }

    #[test]
    fn started_marker_lookalike_inside_fenced_block_is_not_rewritten() {
        // `extract_started_markers` must not touch a line inside a fenced
        // code block just because it looks like a `[/]` bullet (e.g.
        // a runbook documenting markcheck's own syntax).
        let source = "\
## S

- [ ] task with a fence

  ```
  - [/] literal example line
  ```
";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.code_blocks,
            vec!["- [/] literal example line".to_string()]
        );
    }

    #[test]
    fn started_marker_lookalike_inside_blockquoted_fenced_block_is_not_rewritten() {
        // A fenced code block quoted inside a `>` blockquote must be
        // recognized as a fence boundary the same way a plain one is
        // (started_marker_lookalike_inside_fenced_block_is_not_rewritten
        // above), so a `[/]`-lookalike line inside it isn't rewritten either.
        let source = "\
## S

> - [ ] task with a quoted fence
>
>   ```
>   - [/] literal example line
>   ```
";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.code_blocks,
            vec!["- [/] literal example line".to_string()]
        );
    }

    #[test]
    fn tab_separated_started_marker_parses_as_started_state() {
        // End-to-end: a tab-separated `[/]` bullet must be promoted to
        // `Started`, not fall through to a plain non-checkbox item (which,
        // as the list's only entry, would otherwise drop the whole list).
        let source = "## S\n\n-\t[/] `deploy` the service\n";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1);
        let item = &document.lists[0].items[0];
        assert_eq!(item.kind, ItemKind::Checkbox(TaskState::Started));
    }

    // --- Blockquoted checkboxes are non-interactive ---

    #[test]
    fn blockquoted_checkbox_is_display_only_not_a_task() {
        let source = "\
## S

> - [ ] quoted example task

- [ ] normal task
";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(items[0].display_text, "quoted example task");
        assert_eq!(items[1].kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn blockquoted_checked_box_is_also_display_only() {
        let source = "## S\n\n> - [x] quoted done example\n\n- [ ] `a`\n";
        let document = parse(source);
        assert_eq!(document.lists[0].items[0].kind, ItemKind::DisplayOnly);
    }

    #[test]
    fn blockquoted_started_marker_is_display_only_not_dropped() {
        // A blockquoted `[/]` used to never be rewritten to `[ ]`, so
        // pulldown never saw it as a checkbox at all; since that made it
        // the *only* checkbox-like syntax in its own top-level list, the
        // no-checkbox-list rule dropped the whole quoted item rather than
        // keeping it as a DisplayOnly card the way a blockquoted `[ ]`/`[x]`
        // is already treated.
        let source = "## S\n\n> - [/] quoted started example\n\n- [ ] normal task\n";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 2, "the quoted item survives: {items:?}");
        assert_eq!(items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(items[0].display_text, "quoted started example");
        assert_eq!(items[1].kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn blockquoted_bold_only_bullet_is_not_a_banner() {
        // A quoted example shouldn't hijack the list's banner slot either.
        // (The blockquoted checkbox sibling is needed so this segment isn't
        // itself dropped by the no-checkbox-list rule before we can inspect
        // it — banner assignment happens before that gate and would
        // otherwise slip through undetected.)
        let source = "\
## S

> - **Looks like a banner**
> - [ ] quoted task

- [ ] `a`
";
        let document = parse(source);
        let list = &document.lists[0];
        assert!(list.banner.is_none());
        assert_eq!(list.items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(list.items[0].header.as_deref(), Some("Looks like a banner"));
        assert_eq!(list.items[1].kind, ItemKind::DisplayOnly); // quoted task
        assert_eq!(
            list.items[2].kind,
            ItemKind::Checkbox(TaskState::NotStarted)
        ); // real task
    }

    // --- Lists without checkboxes / empty lists are dropped ---

    #[test]
    fn notes_only_list_contributes_nothing_and_list_is_dropped() {
        let source = "\
## Notes only

- first note
- second note

## Real tasks

- [ ] `alpha`
";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "Real tasks");
    }

    #[test]
    fn orphan_notes_list_creates_no_default_list() {
        let source = "- just a note\n\n## Tasks\n\n- [ ] `alpha`\n";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "Tasks");
    }

    #[test]
    fn blank_separated_bullets_form_one_loose_list_and_are_kept() {
        // A blank line between bullets does NOT split the list in
        // CommonMark — it makes one loose list. Since that list contains a
        // checkbox, the note bullets survive as DisplayOnly items.
        let source = "\
## Mixed

- note one
- note two

- [ ] `alpha`
- plain bullet in checklist
";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].items.len(), 4);
    }

    #[test]
    fn list_keeps_checklist_but_drops_separate_notes_list() {
        // A paragraph between the lists genuinely separates them; the
        // notes-only list is dropped, the checklist survives.
        let source = "\
## Mixed

- note one
- note two

Separator paragraph.

- [ ] `alpha`
- plain bullet in checklist
";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1);
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].display_text, "alpha");
        assert_eq!(items[1].kind, ItemKind::DisplayOnly);
    }

    #[test]
    fn remaining_lists_keep_document_order() {
        let source = "\
## One

- [ ] `a`

## Notes

- nope

## Three

- [ ] `c`
";
        let document = parse(source);
        let titles: Vec<_> = document.lists.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["One", "Three"]);
    }

    // --- Leading bold as header / group header ---

    #[test]
    fn leading_bold_with_content_becomes_item_header() {
        let source = "## S\n\n- [ ] **Reboot** run `restart-service` afterwards\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(item.header.as_deref(), Some("Reboot"));
        assert_eq!(item.display_text, "run restart-service afterwards");
        assert_eq!(item.kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn bold_only_checkbox_is_item_with_header_and_empty_body() {
        let source = "## S\n\n- [ ] **Reboot**\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(item.header.as_deref(), Some("Reboot"));
        assert_eq!(item.display_text, "");
        assert_eq!(item.kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn leading_bold_only_banner_is_per_list() {
        let source = "\
## First

- **Group A**
- [ ] `a`

## Second

- [ ] `b`
";
        let document = parse(source);
        assert_eq!(document.lists[0].banner.as_deref(), Some("Group A"));
        assert!(document.lists[1].banner.is_none());
        // The banner emits no item; only the checkbox remains.
        assert_eq!(document.lists[0].items.len(), 1);
    }

    #[test]
    fn bold_only_bullet_with_children_is_not_a_banner() {
        // A bold-only first bullet with its own nested sub-list must not
        // be eaten as the banner (which emits no `Item`), or its
        // children would be orphaned with no depth-0 parent.
        let source = "\
## S

- **Group A**
  - [ ] nested under banner
- [ ] normal
";
        let document = parse(source);
        let list = &document.lists[0];
        assert!(
            list.banner.is_none(),
            "a bullet with children falls back to a normal card, not a banner"
        );
        assert_eq!(list.items.len(), 3);
        assert_eq!(list.items[0].header.as_deref(), Some("Group A"));
        assert_eq!(list.items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(list.items[0].depth, 0);
        assert_eq!(list.items[1].depth, 1);
        // The nested child now has a real depth-0 ancestor in the flat list.
        assert_eq!(list.parent_chain(1), vec![0]);
    }

    #[test]
    fn non_first_bold_only_bullet_is_a_display_only_card() {
        let source = "\
## S

- **Group A**
- [ ] `a`
- **Group B**
- [ ] `b`
";
        let document = parse(source);
        let list = &document.lists[0];
        // Only the first bold-only bullet is the banner.
        assert_eq!(list.banner.as_deref(), Some("Group A"));
        // "Group B" is not first, so it renders as a display-only card.
        let items = &list.items;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, ItemKind::Checkbox(TaskState::NotStarted));
        assert_eq!(items[1].kind, ItemKind::DisplayOnly);
        assert_eq!(items[1].header.as_deref(), Some("Group B"));
        assert!(items[1].display_text.is_empty());
        assert_eq!(items[2].kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn mid_text_bold_stays_in_body() {
        let source = "## S\n\n- [ ] run the **important** step\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert!(item.header.is_none());
        assert_eq!(item.display_text, "run the important step");
    }

    #[test]
    fn display_only_with_leading_bold_and_text_stays_an_item() {
        let source = "## S\n\n- **Note** read this first\n- [ ] `a`\n";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(items[0].header.as_deref(), Some("Note"));
        assert_eq!(items[0].display_text, "read this first");
    }

    // --- Fenced code block capture ---

    #[test]
    fn fence_indented_under_item_is_captured() {
        let source = "\
## S

- [ ] `alpha` with details

  ```shell
  run-command --flag
  ```
- [ ] `beta`
";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(items[0].code_blocks, vec!["run-command --flag".to_string()]);
        assert!(
            !items[0].display_text.contains("run-command"),
            "code block text must not leak into display_text"
        );
        assert!(items[1].code_blocks.is_empty());
    }

    #[test]
    fn fence_after_list_attaches_to_last_item() {
        let source = "\
## S

- [ ] `alpha`
- [ ] `beta`

```shell
run-command --flag
```
";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert!(items[0].code_blocks.is_empty());
        assert_eq!(items[1].code_blocks, vec!["run-command --flag".to_string()]);
    }

    #[test]
    fn paragraph_between_list_and_fence_prevents_attachment() {
        let source = "\
## S

- [ ] `alpha`

Some paragraph in between.

```shell
run-command
```
";
        let document = parse(source);
        assert!(document.lists[0].items[0].code_blocks.is_empty());
    }

    #[test]
    fn fence_after_discarded_notes_list_attaches_nowhere() {
        let source = "\
## S

- [ ] `alpha`

- just a note

```shell
run-command
```
";
        let document = parse(source);
        // The notes-only list is discarded; the fence must not attach to
        // the earlier checklist's last item.
        assert!(document.lists[0].items[0].code_blocks.is_empty());
    }

    #[test]
    fn consecutive_fences_after_item_are_all_captured() {
        let source = "\
## S

- [ ] `alpha`

```shell
first
```

```shell
second
```
";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.code_blocks,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn uses_crlf_detects_crlf_and_lf_sources() {
        let lf_source = "## S\n\n- [ ] `alpha`\n";
        assert!(!parse(lf_source).uses_crlf);

        let crlf_source = "## S\r\n\r\n- [ ] `alpha`\r\n";
        assert!(parse(crlf_source).uses_crlf);
    }

    #[test]
    fn trailing_newline_detects_presence_and_absence() {
        let with_newline = "## S\n\n- [ ] `alpha`\n";
        assert!(parse(with_newline).trailing_newline);

        let without_newline = "## S\n\n- [ ] `alpha`";
        assert!(!parse(without_newline).trailing_newline);
    }

    #[test]
    fn indented_code_block_is_ignored() {
        // A top-level 4-space indented code block (outside any list) is
        // never a copy candidate and never attaches to an item. (Inside a
        // list item, 4-space content is a continuation paragraph per
        // CommonMark, not a code block.)
        let source = "\
## S

Intro paragraph.

    indented code line

- [ ] `alpha`
";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert!(item.code_blocks.is_empty());
        assert!(!item.display_text.contains("indented"));
    }
}
