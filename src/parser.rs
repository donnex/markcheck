use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use pulldown_cmark::utils::TextMergeWithOffset;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::model::{
    BodySpan, Document, Item, ItemKind, LineNumber, List, SubHeading, TaskState, TextStyle,
};

/// Splits `raw` into lines, keeping each line's own terminator, using
/// CommonMark's line endings (`\r\n`, `\n` or a lone `\r`). The single line
/// model shared by `raw_lines`, `LineCounter` and the `[/]` pre-pass, so it
/// must agree with pulldown-cmark on CR-only files.
pub(crate) fn split_lines_keeping_endings(raw: &str) -> Vec<&str> {
    let bytes = raw.as_bytes();
    let mut lines = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        // `\r`/`\n` are ASCII, and no UTF-8 continuation byte can equal
        // them, so every index here is already a char boundary.
        match bytes[i] {
            b'\n' => {
                lines.push(&raw[start..=i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                let end = if bytes.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                };
                lines.push(&raw[start..end]);
                i = end;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&raw[start..]);
    }
    lines
}

/// Number of CommonMark line endings in `s` — the counting half of
/// [`split_lines_keeping_endings`], kept consistent with it so a byte offset
/// and a `raw_lines` index always agree about which line they name.
fn count_line_endings(s: &str) -> usize {
    let bytes = s.as_bytes();
    let (mut count, mut i) = (0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                count += 1;
                i += 1;
            }
            b'\r' => {
                count += 1;
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            _ => i += 1,
        }
    }
    count
}

/// Converts byte offsets into 1-indexed line numbers, amortized O(n) over
/// non-decreasing offsets; rescans if a query goes backwards.
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
        // Guard the one way incremental counting could disagree with a full
        // rescan: if the previous offset landed *between* a `\r` and its
        // `\n`, that pair would be counted once here and once there. Skip
        // the orphaned `\n` so a CRLF is always counted exactly once.
        let mut from = self.offset;
        if from > 0
            && from < target
            && source.as_bytes()[from - 1] == b'\r'
            && source.as_bytes()[from] == b'\n'
        {
            from += 1;
        }
        self.line += count_line_endings(&source[from..target]);
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

/// Parser options shared by the `[/]` pre-pass and the real parse, so both
/// see exactly the same block structure.
const PARSE_OPTIONS: Options = Options::ENABLE_TASKLISTS.union(Options::ENABLE_STRIKETHROUGH);

/// Front matter is blanked rather than parsed: unparsed, CommonMark reads a
/// YAML block's closing `---` as a setext `## H2` underline. The metadata
/// options are confined to this probe because pulldown-cmark accepts a
/// metadata block at *any* block start, so enabling them for the real parse
/// would swallow a `---` ... `---` span in the middle of a checklist.
const FRONT_MATTER_OPTIONS: Options = Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
    .union(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS);

/// Replaces a leading YAML (`---`) or TOML (`+++`) front-matter block with
/// spaces, keeping line endings and byte length so offsets and line numbers
/// still index `raw_lines`.
fn blank_front_matter(source: &str) -> Cow<'_, str> {
    let front_matter_end = match Parser::new_ext(source, FRONT_MATTER_OPTIONS)
        .into_offset_iter()
        .next()
    {
        Some((Event::Start(Tag::MetadataBlock(_)), range)) if range.start == 0 => range.end,
        _ => return Cow::Borrowed(source),
    };
    let (front_matter, rest) = source.split_at(front_matter_end);
    let mut blanked = String::with_capacity(source.len());
    for ch in front_matter.chars() {
        if ch == '\n' || ch == '\r' {
            blanked.push(ch);
        } else {
            blanked.extend(std::iter::repeat_n(' ', ch.len_utf8()));
        }
    }
    blanked.push_str(rest);
    Cow::Owned(blanked)
}

/// pulldown-cmark only recognizes `[ ]` and `[x]`. This pre-pass parses
/// `raw` once and records the offset of every `[/]` exactly where a task
/// marker would be, rewrites those to `[ ]` (same length, so offsets hold),
/// and returns the offsets so the second parse can promote them to
/// `Started`.
fn extract_started_markers(raw: &str) -> (String, HashSet<usize>) {
    let mut offsets = HashSet::new();
    let mut awaiting_first_inline = false;
    for (event, range) in Parser::new_ext(raw, PARSE_OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Item) => awaiting_first_inline = true,
            Event::Start(Tag::Paragraph) => {}
            Event::Text(_) if awaiting_first_inline => {
                awaiting_first_inline = false;
                if is_started_marker_at(raw, range.start) {
                    offsets.insert(range.start);
                }
            }
            _ => awaiting_first_inline = false,
        }
    }
    let mut processed = raw.to_string();
    for &offset in &offsets {
        processed.replace_range(offset..offset + 3, "[ ]");
    }
    (processed, offsets)
}

/// Whether `raw` holds a `[/]` marker at `offset` followed by whitespace —
/// the same shape pulldown-cmark requires of `[ ]` before it reports a
/// `TaskListMarker` (a `[ ]` at the very end of the input is not one).
fn is_started_marker_at(raw: &str, offset: usize) -> bool {
    raw[offset..].strip_prefix("[/]").is_some_and(|after| {
        after
            .chars()
            .next()
            .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
    })
}

/// Mutable state for one list item while it's being parsed. Kept on a stack
/// so a nested item's Start/End (interleaved inside its parent's) doesn't
/// clobber the parent's fields.
#[derive(Default)]
struct ItemBuilder {
    line: LineNumber,
    depth: usize,
    /// `current_list_items.len()` when this item was pushed. Any growth by its
    /// `End(Item)` is its own children, which keeps a bullet with children from
    /// becoming the banner.
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
    /// Set at a block boundary or dropped HTML tag inside the item, so the
    /// next run is separated from the text before it instead of fused onto it.
    pending_separator: bool,
}

impl ItemBuilder {
    fn mark_boundary(&mut self) {
        self.pending_separator = true;
    }

    /// Emits the space owed by [`Self::mark_boundary`], outside any link so
    /// it never becomes part of the link text.
    fn flush_separator(&mut self) {
        if std::mem::take(&mut self.pending_separator)
            && !self.text.is_empty()
            && !self.text.ends_with(char::is_whitespace)
        {
            self.text.push(' ');
            self.body.push(BodySpan::Text(" ".to_string()));
        }
    }

    /// A leading bold-only run is the card title; a bold run anywhere else
    /// is inline strong styling in the body.
    fn start_strong(&mut self) {
        if !self.header_done && self.text.trim().is_empty() {
            self.capturing_header = true;
        } else {
            self.style.strong = true;
        }
    }

    fn end_strong(&mut self) {
        if self.capturing_header {
            self.capturing_header = false;
            self.header_done = true;
        } else {
            self.style.strong = false;
        }
    }

    fn start_link(&mut self, url: &str) {
        self.flush_separator();
        if !self.capturing_header {
            self.link = Some((url.to_string(), String::new()));
        }
    }

    fn end_link(&mut self) {
        if let Some((url, text)) = self.link.take() {
            self.body.push(BodySpan::Link { text, url });
        }
    }

    /// Link text still flows into `text` (and thus `display_text`) so search
    /// matches it.
    fn push_text(&mut self, text: &str) {
        self.flush_separator();
        if self.capturing_header {
            self.header.push_str(text);
        } else if let Some((_, link_text)) = &mut self.link {
            link_text.push_str(text);
            self.text.push_str(text);
        } else {
            self.text.push_str(text);
            let mut rest_start = 0;
            for bare in find_bare_urls(text) {
                self.push_prose(&text[rest_start..bare.range.start]);
                self.body.push(BodySpan::Link {
                    text: text[bare.range.clone()].to_string(),
                    url: bare.url,
                });
                rest_start = bare.range.end;
            }
            self.push_prose(&text[rest_start..]);
        }
    }

    fn push_prose(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.style;
        self.body.push(if style.is_plain() {
            BodySpan::Text(text.to_string())
        } else {
            BodySpan::Styled {
                text: text.to_string(),
                style,
            }
        });
    }

    /// A soft/hard line break within a paragraph is whitespace: without this
    /// the words on either side would fuse ("to"+"show"→"toshow") in
    /// display_text/search and in the card.
    fn push_break(&mut self) {
        if self.capturing_header {
            self.header.push(' ');
        } else if let Some((_, link_text)) = &mut self.link {
            link_text.push(' ');
            self.text.push(' ');
        } else {
            self.text.push(' ');
            self.body.push(BodySpan::Text(" ".to_string()));
        }
    }

    fn push_code(&mut self, text: &str) {
        self.flush_separator();
        if let Some((_, link_text)) = &mut self.link {
            link_text.push_str(text);
            self.text.push_str(text);
        } else {
            self.code_spans.push(text.to_string());
            if self.capturing_header {
                self.header.push_str(text);
            } else {
                self.text.push_str(text);
                self.body.push(BodySpan::Code(text.to_string()));
            }
        }
    }

    fn into_item(self) -> Item {
        // A checkbox quoted inside a `>` blockquote is an illustrative
        // example, not a real task.
        let kind = match self.kind {
            Some(kind) if !self.in_blockquote => kind,
            _ => ItemKind::DisplayOnly,
        };
        let header = {
            let trimmed = self.header.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let display_text = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        Item {
            line_number: self.line,
            depth: self.depth,
            section: self.section,
            display_text,
            body: self.body,
            header,
            code_spans: self.code_spans,
            code_blocks: self.code_blocks,
            kind,
        }
    }
}

/// A bare URL found in plain text: where it sits, and the URL to open.
#[derive(Debug, PartialEq, Eq)]
struct BareUrl {
    range: std::ops::Range<usize>,
    url: String,
}

const BARE_URL_PREFIXES: [&str; 3] = ["https://", "http://", "www."];

/// Bare `http://`, `https://` and `www.` URLs in a plain text run, following
/// GFM's extended-autolink rules (which pulldown-cmark does not implement):
/// the URL starts at the run's start or after whitespace or `*_~(`, runs to
/// whitespace or `<`, needs a dotted domain, and sheds trailing punctuation
/// and any unbalanced closing `)`. A `www.` URL opens as `http://`.
fn find_bare_urls(text: &str) -> Vec<BareUrl> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    let mut skip_until = 0;
    for (start, ch) in text.char_indices() {
        let at_boundary =
            previous.is_none_or(|p| p.is_whitespace() || matches!(p, '*' | '_' | '~' | '('));
        previous = Some(ch);
        if start < skip_until || !at_boundary {
            continue;
        }
        let rest = &text[start..];
        let Some(prefix) = BARE_URL_PREFIXES.iter().find(|prefix| {
            rest.get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        }) else {
            continue;
        };
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(rest.len());
        let candidate = trim_url_end(&rest[..end]);
        let domain_start = if prefix.starts_with('w') {
            0
        } else {
            prefix.len()
        };
        if !is_valid_domain(&candidate[domain_start..]) {
            continue;
        }
        let url = if domain_start == 0 {
            format!("http://{candidate}")
        } else {
            candidate.to_string()
        };
        skip_until = start + candidate.len();
        found.push(BareUrl {
            range: start..skip_until,
            url,
        });
    }
    found
}

fn trim_url_end(mut url: &str) -> &str {
    loop {
        let trimmed = url.trim_end_matches(['?', '!', '.', ',', ':', '*', '_', '~']);
        let trimmed = match trimmed.strip_suffix(')') {
            Some(shorter) if trimmed.matches(')').count() > trimmed.matches('(').count() => shorter,
            _ => trimmed,
        };
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
}

/// GFM's "valid domain": dot-separated segments of alphanumerics, `_` and
/// `-`, at least one dot, and no `_` in the last two segments.
fn is_valid_domain(after_prefix: &str) -> bool {
    let domain_end = after_prefix
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(after_prefix.len());
    let domain = after_prefix[..domain_end].trim_end_matches('.');
    let segments: Vec<&str> = domain.split('.').collect();
    segments.len() >= 2
        && segments.iter().all(|segment| !segment.is_empty())
        && segments[segments.len() - 2..]
            .iter()
            .all(|segment| !segment.contains('_'))
}

/// Whether a finished top-level-eligible item could be its list's banner,
/// apart from the "first top-level item" rule. A bullet with children is
/// never the banner, which emits no `Item` and would orphan them; a command
/// in the bold or under the bullet keeps it a card, since the one-line text
/// banner would discard it; a blockquoted bullet is an example, not the list's.
fn is_banner_candidate(item: &Item, has_children: bool, in_blockquote: bool) -> bool {
    item.kind == ItemKind::DisplayOnly
        && item.header.is_some()
        && item.display_text.is_empty()
        && item.code_spans.is_empty()
        && item.code_blocks.is_empty()
        && item.depth == 0
        && !has_children
        && !in_blockquote
}

/// Any block-level start other than a fenced code block breaks the
/// fence-attaches-to-previous-list rule.
fn keeps_fence_attachment(event: &Event) -> bool {
    matches!(
        event,
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))
            | Event::Text(_)
            | Event::Code(_)
            | Event::End(_)
            | Event::TaskListMarker(_)
    )
}

fn strip_trailing_newline(content: String) -> String {
    content
        .strip_suffix('\n')
        .map(str::to_string)
        .unwrap_or(content)
}

/// Everything the event walk in [`parse_source`] carries between events.
struct ParseState<'a> {
    source: &'a str,
    started_offsets: HashSet<usize>,
    line_counter: LineCounter,

    lists: Vec<List>,
    default_items: Vec<Item>,
    default_banner: Option<String>,

    /// The open heading's level and the text read so far.
    heading: Option<(HeadingLevel, String)>,
    /// The first non-empty `# H1` becomes the document title.
    document_title: Option<String>,

    /// The active `### H3`+ sub-section path: a heading stack, reduced on
    /// each heading (pop entries at level >= the new one, then push it) and
    /// cleared at every `# H1` / `## H2` boundary, which start a fresh list
    /// context. Snapshotted onto each item as it's opened.
    section_stack: Vec<SubHeading>,

    /// Items are buffered per top-level list: a list containing no checkbox
    /// at all contributes nothing to the document.
    list_depth: usize,
    current_list_items: Vec<Item>,
    /// Tracked apart from item kind so a blockquoted checkbox still counts for
    /// the drop-if-empty rule.
    list_had_checkbox_syntax: bool,

    /// Depth of `>` blockquote nesting, so a checkbox bullet quoted inside
    /// one can be told apart from a real, live task.
    blockquote_depth: usize,

    /// A leading bold-only top-level bullet becomes the list's banner instead
    /// of a card. Reset at each H2.
    seen_first_top_item: bool,

    /// A fenced code block at column 0 immediately after a kept list is
    /// attached to that list's last item. Any other block between the list
    /// and the fence breaks the attachment.
    attach_fence_to_last_item: bool,
    /// The open code block: whether it is fenced, and its content so far.
    code_block: Option<(bool, String)>,

    /// One frame per open item; a stack so nested items don't clobber their
    /// parent's in-progress fields. The innermost open item is the top.
    item_stack: Vec<ItemBuilder>,
}

impl<'a> ParseState<'a> {
    fn new(source: &'a str, started_offsets: HashSet<usize>) -> Self {
        Self {
            source,
            started_offsets,
            line_counter: LineCounter::new(),
            lists: Vec::new(),
            default_items: Vec::new(),
            default_banner: None,
            heading: None,
            document_title: None,
            section_stack: Vec::new(),
            list_depth: 0,
            current_list_items: Vec::new(),
            list_had_checkbox_syntax: false,
            blockquote_depth: 0,
            seen_first_top_item: false,
            attach_fence_to_last_item: false,
            code_block: None,
            item_stack: Vec::new(),
        }
    }

    fn handle(&mut self, event: Event, offset: usize) {
        if !keeps_fence_attachment(&event) {
            self.attach_fence_to_last_item = false;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                self.heading = Some((level, String::new()));
            }
            Event::End(TagEnd::Heading(level)) => self.on_heading_end(level),
            Event::Start(Tag::List(_)) => self.on_list_start(),
            Event::End(TagEnd::List(_)) => self.on_list_end(),
            Event::Start(Tag::BlockQuote(_)) => self.blockquote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => self.on_item_start(offset),
            Event::End(TagEnd::Item) => self.on_item_end(),
            Event::TaskListMarker(checked) => self.on_task_marker(checked, offset),
            Event::Start(Tag::Strong) => self.with_item(ItemBuilder::start_strong),
            Event::End(TagEnd::Strong) => self.with_item(ItemBuilder::end_strong),
            Event::Start(Tag::Emphasis) => self.with_item(|item| item.style.emphasis = true),
            Event::End(TagEnd::Emphasis) => self.with_item(|item| item.style.emphasis = false),
            Event::Start(Tag::Strikethrough) => {
                self.with_item(|item| item.style.strikethrough = true);
            }
            Event::End(TagEnd::Strikethrough) => {
                self.with_item(|item| item.style.strikethrough = false);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.with_item(|item| item.start_link(&dest_url));
            }
            Event::End(TagEnd::Link) => self.with_item(ItemBuilder::end_link),
            Event::Start(Tag::CodeBlock(kind)) => {
                let fenced = matches!(kind, CodeBlockKind::Fenced(_));
                self.code_block = Some((fenced, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => self.on_code_block_end(),
            Event::Text(text) => self.on_text(&text),
            Event::SoftBreak | Event::HardBreak => self.on_break(),
            Event::Code(text) => self.on_code(&text),
            Event::Start(Tag::Paragraph)
            | Event::End(TagEnd::Paragraph)
            | Event::Html(_)
            | Event::InlineHtml(_) => self.with_item(ItemBuilder::mark_boundary),
            _ => {}
        }
    }

    fn with_item(&mut self, f: impl FnOnce(&mut ItemBuilder)) {
        if let Some(item) = self.item_stack.last_mut() {
            f(item);
        }
    }

    /// The items of the list currently being filled: the last `## H2` list,
    /// or the pre-heading `(Default)` one.
    fn target_items(&mut self) -> &mut Vec<Item> {
        match self.lists.last_mut() {
            Some(list) => &mut list.items,
            None => &mut self.default_items,
        }
    }

    fn on_heading_end(&mut self, level: HeadingLevel) {
        let Some((open_level, text)) = self.heading.take() else {
            return;
        };
        if open_level != level {
            return;
        }
        let text = text.trim();
        match level {
            HeadingLevel::H2 => {
                self.lists.push(List {
                    title: text.to_string(),
                    banner: None,
                    items: Vec::new(),
                });
                self.seen_first_top_item = false;
                self.section_stack.clear();
            }
            HeadingLevel::H1 => {
                if self.document_title.is_none() && !text.is_empty() {
                    self.document_title = Some(text.to_string());
                }
                self.section_stack.clear();
            }
            _ if !text.is_empty() => {
                let level = heading_level_number(level);
                self.section_stack.retain(|h| h.level < level);
                self.section_stack.push(SubHeading {
                    level,
                    text: text.to_string(),
                });
            }
            _ => {}
        }
    }

    fn on_list_start(&mut self) {
        if self.list_depth == 0 {
            self.list_had_checkbox_syntax = false;
        }
        self.list_depth += 1;
    }

    fn on_list_end(&mut self) {
        self.list_depth = self.list_depth.saturating_sub(1);
        if self.list_depth > 0 {
            self.with_item(ItemBuilder::mark_boundary);
            return;
        }
        let mut items = std::mem::take(&mut self.current_list_items);
        self.attach_fence_to_last_item = self.list_had_checkbox_syntax;
        if self.list_had_checkbox_syntax {
            // Items arrive post-order; sort by line to restore document order.
            items.sort_by_key(|item| item.line_number);
            self.target_items().append(&mut items);
        }
    }

    fn on_item_start(&mut self, offset: usize) {
        // `list_depth` was already incremented by the enclosing Start(List),
        // so depth 0 = top-level.
        let line = self.line_counter.line_at(offset, self.source);
        self.item_stack.push(ItemBuilder {
            line,
            depth: self.list_depth.saturating_sub(1),
            section: self.section_stack.clone(),
            items_before: self.current_list_items.len(),
            in_blockquote: self.blockquote_depth > 0,
            ..Default::default()
        });
    }

    fn on_task_marker(&mut self, checked: bool, offset: usize) {
        self.list_had_checkbox_syntax = true;
        let state = if checked {
            TaskState::Done
        } else if self.started_offsets.contains(&offset) {
            TaskState::Started
        } else {
            TaskState::NotStarted
        };
        self.with_item(|item| item.kind = Some(ItemKind::Checkbox(state)));
    }

    fn on_item_end(&mut self) {
        let Some(builder) = self.item_stack.pop() else {
            return;
        };
        let has_children = self.current_list_items.len() > builder.items_before;
        let in_blockquote = builder.in_blockquote;
        let mut item = builder.into_item();
        let is_banner =
            !self.seen_first_top_item && is_banner_candidate(&item, has_children, in_blockquote);
        if item.depth == 0 {
            self.seen_first_top_item = true;
        }
        if !is_banner {
            self.current_list_items.push(item);
            return;
        }
        let banner = item.header.take();
        match self.lists.last_mut() {
            Some(list) => list.banner = banner,
            None => self.default_banner = banner,
        }
    }

    /// Inside an item, fenced and indented blocks are both the item's code.
    /// Outside one, only a fence may attach to the previous list; a stray
    /// indented block is dropped.
    fn on_code_block_end(&mut self) {
        let Some((fenced, content)) = self.code_block.take() else {
            return;
        };
        let content = strip_trailing_newline(content);
        if let Some(item) = self.item_stack.last_mut() {
            item.code_blocks.push(content);
            item.mark_boundary();
        } else if fenced && self.attach_fence_to_last_item {
            self.attach_trailing_fence(content);
        }
    }

    /// The attachment flag stays set: several consecutive fences all attach
    /// (and then correctly refuse to copy).
    fn attach_trailing_fence(&mut self, content: String) {
        if let Some(item) = self.target_items().last_mut() {
            item.code_blocks.push(content);
        }
    }

    fn on_text(&mut self, text: &str) {
        if let Some((_, buffer)) = &mut self.code_block {
            buffer.push_str(text);
        } else if let Some((_, heading_text)) = &mut self.heading {
            heading_text.push_str(text);
        } else {
            self.with_item(|item| item.push_text(text));
        }
    }

    /// Code block content arrives via Text events, so a break inside one is
    /// ignored.
    fn on_break(&mut self) {
        if self.code_block.is_some() {
            return;
        }
        if let Some((_, heading_text)) = &mut self.heading {
            heading_text.push(' ');
        } else {
            self.with_item(ItemBuilder::push_break);
        }
    }

    fn on_code(&mut self, text: &str) {
        if let Some((_, heading_text)) = &mut self.heading {
            heading_text.push_str(text);
        } else {
            self.with_item(|item| item.push_code(text));
        }
    }

    fn finish(self, path: PathBuf, raw_lines: Vec<String>) -> Document {
        let has_checkbox = |items: &[Item]| {
            items
                .iter()
                .any(|item| matches!(item.kind, ItemKind::Checkbox(_)))
        };
        let mut lists = self.lists;
        let has_default_list = has_checkbox(&self.default_items);
        if has_default_list {
            lists.insert(
                0,
                List {
                    title: "(Default)".to_string(),
                    banner: self.default_banner,
                    items: self.default_items,
                },
            );
        }
        // A list without a live checkbox is dropped entirely: one that never
        // had a list, whose lists had no checkbox syntax, or whose only
        // checkboxes are blockquoted examples (forced to display-only).
        lists.retain(|list| has_checkbox(&list.items));
        Document {
            file_path: path,
            title: self.document_title,
            has_default_list,
            lists,
            raw_lines,
        }
    }
}

pub fn parse_document(path: PathBuf) -> anyhow::Result<Document> {
    let raw = fs::read_to_string(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::InvalidData {
            anyhow::Error::new(err).context(
                "the file is not valid UTF-8 (markcheck reads UTF-8 files; convert it with e.g. iconv)",
            )
        } else {
            anyhow::Error::new(err)
        }
    })?;
    let document = parse_source(&raw, path);
    Ok(document)
}

fn parse_source(raw: &str, path: PathBuf) -> Document {
    // Lines keep their own terminators — see `split_lines_keeping_endings`.
    // `write_back` indexes this by `Item::line_number` and rejoins by plain
    // concatenation, so every line round-trips exactly as authored.
    let raw_lines: Vec<String> = split_lines_keeping_endings(raw)
        .into_iter()
        .map(str::to_string)
        .collect();

    // A BOM defeats ATX heading recognition. Stripped from the parsing copy
    // only, so write-back keeps it.
    let raw_for_parsing = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);

    // Offsets are preserved by both rewrites, so line numbers still index
    // `raw_lines`.
    let without_front_matter = blank_front_matter(raw_for_parsing);
    let (source, started_offsets) = extract_started_markers(&without_front_matter);

    let mut state = ParseState::new(&source, started_offsets);
    // Merged so a bare URL that pulldown-cmark splits across several text
    // events (at a `_` or `&`, say) reaches `find_bare_urls` whole.
    let events =
        TextMergeWithOffset::new(Parser::new_ext(&source, PARSE_OPTIONS).into_offset_iter());
    for (event, range) in events {
        state.handle(event, range.start);
    }
    state.finish(path, raw_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Every `examples/` file must parse and round-trip byte for byte, since
    /// `write_back` reassembles the file from `raw_lines`.
    #[test]
    fn every_example_file_parses_and_round_trips_byte_for_byte() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut checked = 0;
        for entry in fs::read_dir(&dir).expect("examples/ must exist") {
            let path = entry.expect("readable entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            // README.md indexes the examples; it isn't a checklist itself.
            if path.file_name().and_then(|n| n.to_str()) == Some("README.md") {
                continue;
            }
            let raw = fs::read_to_string(&path).expect("readable example");
            let document = parse_document(path.clone())
                .unwrap_or_else(|err| panic!("{} must parse: {err}", path.display()));
            assert_eq!(
                crate::writer::document_contents(&document),
                raw,
                "{} must round-trip byte for byte",
                path.display()
            );
            assert!(
                !document.lists.is_empty(),
                "{} must yield at least one list, or it demonstrates nothing",
                path.display()
            );
            checked += 1;
        }
        // Without this the whole test passes vacuously if `examples/` is
        // renamed or emptied — the defect class this suite has already been
        // bitten by (DESIGN.md, "Recurring defect classes", 11).
        assert!(
            checked >= 5,
            "expected the full set of example checklists, walked {checked}"
        );
    }

    fn example_item(file: &str, needle: &str) -> Item {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join(file);
        let document = parse_document(path).expect("example parses");
        document
            .lists
            .into_iter()
            .flat_map(|list| list.items)
            .find(|item| item.display_text.contains(needle))
            .unwrap_or_else(|| panic!("{file} must contain an item matching {needle:?}"))
    }

    #[test]
    fn examples_cover_fence_after_list_and_uppercase_done() {
        let smoke = example_item("notes-and-code.md", "Run the smoke test");
        assert_eq!(smoke.code_blocks, vec!["smoke-test --all".to_string()]);
        let upper = example_item("basics.md", "Record the start time");
        assert_eq!(upper.kind, ItemKind::Checkbox(TaskState::Done));
    }

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
        let path = crate::test_support::unique_temp_path("parser", "nonutf8", Some("md"));
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let result = parse_document(path.clone());
        let _ = std::fs::remove_file(&path);
        let message = format!("{:#}", result.expect_err("non-UTF-8 input fails to load"));
        assert!(message.contains("not valid UTF-8"), "{message}");
        assert!(message.contains("iconv"), "names the fix: {message}");
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

    fn bare_urls(text: &str) -> Vec<(&str, String)> {
        find_bare_urls(text)
            .into_iter()
            .map(|bare| (&text[bare.range], bare.url))
            .collect()
    }

    #[test]
    fn bare_url_becomes_a_link_span() {
        let document = parse("## S\n\n- [ ] Check the dashboard at https://example.com/status.\n");
        let item = &document.lists[0].items[0];
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text("Check the dashboard at ".to_string()),
                BodySpan::Link {
                    text: "https://example.com/status".to_string(),
                    url: "https://example.com/status".to_string(),
                },
                BodySpan::Text(".".to_string()),
            ]
        );
        assert_eq!(
            item.display_text,
            "Check the dashboard at https://example.com/status."
        );
        assert_eq!(item.link_urls(), vec!["https://example.com/status"]);
    }

    #[test]
    fn bare_url_sheds_trailing_punctuation_and_unbalanced_parens() {
        assert_eq!(
            bare_urls("see https://example.com/a, then https://example.com/b!?"),
            vec![
                ("https://example.com/a", "https://example.com/a".to_string()),
                ("https://example.com/b", "https://example.com/b".to_string()),
            ]
        );
        assert_eq!(
            bare_urls("(see https://example.com/x)."),
            vec![("https://example.com/x", "https://example.com/x".to_string())]
        );
        assert_eq!(
            bare_urls("https://example.com/wiki/Foo_(bar)"),
            vec![(
                "https://example.com/wiki/Foo_(bar)",
                "https://example.com/wiki/Foo_(bar)".to_string()
            )]
        );
        assert_eq!(
            bare_urls("HTTPS://example.com<br>"),
            vec![("HTTPS://example.com", "HTTPS://example.com".to_string())]
        );
    }

    #[test]
    fn bare_www_url_opens_over_http() {
        assert_eq!(
            bare_urls("go to www.example.com/docs: now"),
            vec![(
                "www.example.com/docs",
                "http://www.example.com/docs".to_string()
            )]
        );
    }

    #[test]
    fn text_that_only_looks_like_a_url_is_not_linked() {
        assert!(bare_urls("https://localhost/x").is_empty());
        assert!(bare_urls("https://").is_empty());
        assert!(bare_urls("www.").is_empty());
        assert!(bare_urls("https://a..b").is_empty());
        assert!(bare_urls("https://example.under_score").is_empty());
        assert!(bare_urls("xhttps://example.com").is_empty());
        assert!(bare_urls("plain text, no links").is_empty());
    }

    #[test]
    fn bare_url_split_by_pulldown_cmark_stays_whole() {
        let document = parse("## S\n\n- [ ] open https://example.com/a_b?x=1&amp;y=2 now\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.link_urls(), vec!["https://example.com/a_b?x=1&y=2"]);
    }

    #[test]
    fn bare_url_in_a_link_or_title_or_code_is_not_linked_again() {
        let linked = parse("## S\n\n- [ ] [https://example.com/a](https://example.com/b)\n");
        assert_eq!(
            linked.lists[0].items[0].link_urls(),
            vec!["https://example.com/b"]
        );
        let titled = parse("## S\n\n- [ ] **See https://example.com/a** body\n");
        assert!(titled.lists[0].items[0].link_urls().is_empty());
        let coded = parse("## S\n\n- [ ] run `curl https://example.com/a`\n");
        assert!(coded.lists[0].items[0].link_urls().is_empty());
    }

    #[test]
    fn bare_url_inside_emphasis_keeps_the_surrounding_style() {
        let document = parse("## S\n\n- [ ] *see www.example.com today*\n");
        let item = &document.lists[0].items[0];
        let style = TextStyle {
            emphasis: true,
            ..TextStyle::default()
        };
        assert_eq!(
            item.body,
            vec![
                BodySpan::Styled {
                    text: "see ".to_string(),
                    style,
                },
                BodySpan::Link {
                    text: "www.example.com".to_string(),
                    url: "http://www.example.com".to_string(),
                },
                BodySpan::Styled {
                    text: " today".to_string(),
                    style,
                },
            ]
        );
    }

    #[test]
    fn link_inside_the_title_loses_its_url_documented_limitation() {
        // A link inside the leading-bold title loses its URL; a documented
        // limitation pinned here.
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
    fn second_paragraph_is_separated_from_the_first() {
        let document = parse("## S\n\n- [ ] Restart the service\n\n  Then check `status`\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.display_text, "Restart the service Then check status");
        assert_eq!(
            item.body,
            vec![
                BodySpan::Text("Restart the service".to_string()),
                BodySpan::Text(" ".to_string()),
                BodySpan::Text("Then check ".to_string()),
                BodySpan::Code("status".to_string()),
            ]
        );
    }

    #[test]
    fn paragraph_after_a_bold_header_gets_no_leading_space() {
        let document = parse("## S\n\n- [ ] **Title**\n\n  body text\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.header.as_deref(), Some("Title"));
        assert_eq!(item.body, vec![BodySpan::Text("body text".to_string())]);
    }

    #[test]
    fn inline_html_tag_is_treated_as_a_space() {
        let document = parse("## S\n\n- [ ] line one<br>line two\n");
        assert_eq!(document.lists[0].items[0].display_text, "line one line two");
    }

    #[test]
    fn separator_before_a_link_stays_out_of_the_link_text() {
        let document = parse("## S\n\n- [ ] see<br>[docs](https://example.com)\n");
        let item = &document.lists[0].items[0];
        assert_eq!(item.display_text, "see docs");
        assert_eq!(
            item.body.last(),
            Some(&BodySpan::Link {
                text: "docs".to_string(),
                url: "https://example.com".to_string(),
            })
        );
    }

    #[test]
    fn text_after_a_tight_nested_fence_or_list_is_separated() {
        let fence = parse("## S\n\n- [ ] before\n  ```\n  cmd\n  ```\n  after\n");
        assert_eq!(fence.lists[0].items[0].display_text, "before after");
        let nested = parse("## S\n\n- [ ] parent\n\n  - [ ] child\n\n  tail\n");
        let parent = &nested.lists[0].items[0];
        assert_eq!(parent.display_text, "parent tail");
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

    const YAML_FRONT_MATTER: &str = "\
---
title: Weekly ops
tags: [ops]
---

# Weekly

- [ ] first task
- [ ] second task
";

    #[test]
    fn yaml_front_matter_is_skipped_not_read_as_a_list_heading() {
        let document = parse(YAML_FRONT_MATTER);
        assert_eq!(document.title.as_deref(), Some("Weekly"));
        assert_eq!(document.lists.len(), 1);
        let list = &document.lists[0];
        assert_eq!(list.title, "(Default)");
        let texts: Vec<&str> = list.items.iter().map(|i| i.display_text.as_str()).collect();
        assert_eq!(texts, ["first task", "second task"]);
        assert_eq!(list.items[0].line_number, 8);
    }

    #[test]
    fn toml_front_matter_is_skipped_and_the_next_h2_names_the_list() {
        let source = "+++\ntitle = \"Ops\"\n+++\n\n## Morning\n\n- [ ] task\n";
        let document = parse(source);
        assert_eq!(document.title, None);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "Morning");
        assert_eq!(document.lists[0].items[0].line_number, 7);
    }

    #[test]
    fn front_matter_after_a_bom_is_still_skipped() {
        let source = format!("\u{FEFF}{YAML_FRONT_MATTER}");
        let document = parse(&source);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "(Default)");
    }

    #[test]
    fn a_block_not_on_the_first_line_is_not_front_matter() {
        let source = format!("\n{YAML_FRONT_MATTER}");
        let document = parse(&source);
        assert_eq!(document.lists[0].title, "title: Weekly ops tags: [ops]");
    }

    #[test]
    fn front_matter_with_crlf_and_non_ascii_keeps_line_numbers() {
        let source = "---\r\ntitle: Café\r\n---\r\n\r\n- [ ] task\r\n";
        let document = parse(source);
        assert_eq!(document.lists[0].title, "(Default)");
        assert_eq!(document.lists[0].items[0].line_number, 5);
        assert_eq!(crate::writer::document_contents(&document), source);
    }

    #[test]
    fn a_dash_line_further_down_is_not_front_matter() {
        let source = "## S\n\n- [ ] task\n\n---\nkey: value\n---\n\n- [ ] later\n";
        let document = parse(source);
        let titles: Vec<&str> = document.lists.iter().map(|l| l.title.as_str()).collect();
        assert_eq!(titles, ["S", "key: value"]);
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
        assert_eq!(document.raw_lines[0], "\u{FEFF}# Title\n");
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
    fn fence_with_trailing_text_on_the_would_be_closing_line_does_not_close_it() {
        // "```done" is not a closing fence, so the `[/]` after it is still inside
        // the code block.
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

- [ ] live task
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
        // A blockquoted `[/]` is kept as a display-only card, like `[ ]`/`[x]`.
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
        // A quoted example must not take the banner slot. The blockquoted checkbox
        // keeps the segment from being dropped.
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

    #[test]
    fn list_whose_only_checkboxes_are_blockquoted_is_dropped() {
        let source = "## Examples\n\n> - [ ] quoted\n\n## Real\n\n- [ ] task\n";
        let document = parse(source);
        assert_eq!(document.lists.len(), 1, "{:?}", document.lists);
        assert_eq!(document.lists[0].title, "Real");
        assert_eq!(document.lists[0].checkbox_stats(), (0, 1));
    }

    #[test]
    fn default_list_with_only_blockquoted_checkboxes_is_dropped() {
        let document = parse("> - [ ] quoted\n\n## Real\n\n- [ ] task\n");
        assert!(!document.has_default_list);
        assert_eq!(document.lists.len(), 1);
        assert_eq!(document.lists[0].title, "Real");
    }

    #[test]
    fn document_with_only_blockquoted_checkboxes_has_no_lists() {
        assert!(parse("## Examples\n\n> - [ ] quoted\n").lists.is_empty());
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
    fn bold_only_first_bullet_with_fenced_command_is_a_card() {
        let document = parse(
            "## Deploy\n\n- **Export the environment first:**\n\n  ```sh\n  export APP_ENV=staging\n  ```\n\n- [ ] step one\n",
        );
        let list = &document.lists[0];
        assert!(list.banner.is_none());
        assert_eq!(list.items.len(), 2);
        let note = &list.items[0];
        assert_eq!(note.kind, ItemKind::DisplayOnly);
        assert_eq!(
            note.header.as_deref(),
            Some("Export the environment first:")
        );
        assert_eq!(note.code_blocks, vec!["export APP_ENV=staging".to_string()]);
        assert_eq!(list.items[1].display_text, "step one");
    }

    #[test]
    fn bold_only_first_bullet_with_inline_command_is_a_card() {
        let document = parse("## S\n\n- **Run `fix-perms` first**\n- [ ] step one\n");
        let list = &document.lists[0];
        assert!(list.banner.is_none());
        assert_eq!(list.items.len(), 2);
        let note = &list.items[0];
        assert_eq!(note.header.as_deref(), Some("Run fix-perms first"));
        assert_eq!(note.code_spans, vec!["fix-perms".to_string()]);
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
    fn raw_lines_keep_each_line_s_own_terminator() {
        // Replaces the old whole-file `uses_crlf`/`trailing_newline` flags:
        // the terminator travels with the line, so a file mixing endings
        // keeps each one rather than having them all rewritten to whichever
        // appeared first.
        assert_eq!(
            parse("## S\n\n- [ ] `alpha`\n").raw_lines,
            vec!["## S\n", "\n", "- [ ] `alpha`\n"]
        );
        assert_eq!(
            parse("## S\r\n\r\n- [ ] `alpha`\r\n").raw_lines,
            vec!["## S\r\n", "\r\n", "- [ ] `alpha`\r\n"]
        );
        // A final line with no terminator keeps not having one.
        assert_eq!(
            parse("## S\n\n- [ ] `alpha`").raw_lines,
            vec!["## S\n", "\n", "- [ ] `alpha`"]
        );
        // Mixed endings survive individually.
        assert_eq!(
            parse("## S\r\n\r\n- [ ] a\n- [ ] b\r\n").raw_lines,
            vec!["## S\r\n", "\r\n", "- [ ] a\n", "- [ ] b\r\n"]
        );
        // An empty file has no lines at all.
        assert!(parse("").raw_lines.is_empty());
    }

    #[test]
    fn split_lines_keeping_endings_handles_every_commonmark_ending() {
        assert_eq!(split_lines_keeping_endings(""), Vec::<&str>::new());
        assert_eq!(split_lines_keeping_endings("a"), vec!["a"]);
        assert_eq!(split_lines_keeping_endings("a\n"), vec!["a\n"]);
        assert_eq!(split_lines_keeping_endings("a\r\n"), vec!["a\r\n"]);
        // A lone `\r` is a line ending per CommonMark — `str::lines()`
        // disagrees, which is the whole reason this function exists.
        assert_eq!(split_lines_keeping_endings("a\rb"), vec!["a\r", "b"]);
        assert_eq!(
            split_lines_keeping_endings("a\rb\nc\r\n"),
            vec!["a\r", "b\n", "c\r\n"]
        );
        // Concatenating the pieces always reproduces the input exactly.
        for src in ["", "a", "a\n\n\r\n\rb", "x\r\ny\rz\n", "日本\r\n語\n"] {
            assert_eq!(split_lines_keeping_endings(src).concat(), src, "{src:?}");
        }
    }

    #[test]
    fn cr_only_line_endings_give_each_item_its_own_line_number() {
        // A lone `\r` is a line ending, so each item gets its own line number.
        let document = parse("## S\r\r- [ ] alpha\r- [ ] beta\r- [ ] gamma\r");
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 3);
        let lines: Vec<_> = items.iter().map(|i| i.line_number).collect();
        assert_eq!(lines, vec![3, 4, 5], "distinct, in document order");
        for item in items {
            assert!(
                document.raw_lines[item.line_number - 1].contains("[ ]"),
                "line_number must index the line the item is actually on"
            );
        }
    }

    #[test]
    fn indented_code_block_inside_a_task_is_its_code() {
        let source = "\
## Restart

- [ ] Restart the service:

      systemctl restart example-service

- [ ] Check the health endpoint
";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(
            items[0].code_blocks,
            vec!["systemctl restart example-service".to_string()]
        );
        assert!(!items[0].display_text.contains("systemctl"));
        assert!(items[1].code_blocks.is_empty());
    }

    #[test]
    fn bold_only_first_bullet_with_indented_code_is_not_a_banner() {
        let source = "## S\n\n- **Export first:**\n\n      export APP_ENV=staging\n\n- [ ] Run\n";
        let document = parse(source);
        let list = &document.lists[0];
        assert_eq!(list.banner, None);
        assert_eq!(
            list.items[0].code_blocks,
            vec!["export APP_ENV=staging".to_string()]
        );
    }

    #[test]
    fn indented_code_block_is_ignored() {
        // A top-level 4-space indented code block (outside any list) is
        // never a copy candidate and never attaches to an item, unlike a
        // fence right after a list.
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

    // `[/]` lookalikes in and around indented code: the pre-pass finds
    // markers with pulldown-cmark itself, so indented code is never mistaken
    // for a task, while an indented bullet that continues a list item is a
    // real nested task. These cases lock both halves in.

    #[test]
    fn indented_lookalike_following_a_list_item_becomes_a_real_nested_task() {
        // The realistic shape for this: a `[/]`-lookalike indented right
        // after a list item reads as list *continuation*, not top-level
        // indented code — pulldown-cmark treats it as a genuine nested
        // list item, so it's rewritten and promoted to Started.
        let source = "## S\n\n- [ ] `alpha`\n\n    - [/] nested example\n";
        let document = parse(source);
        let items = &document.lists[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].depth, 1);
        assert_eq!(items[1].kind, ItemKind::Checkbox(TaskState::Started));
    }

    #[test]
    fn indented_lookalike_before_any_list_produces_no_lists() {
        // No list context at all: an inert indented code block.
        let document = parse("# Example\n\n    - [/]\n");
        assert!(document.lists.is_empty());
    }

    #[test]
    fn blockquoted_indented_lookalike_is_discarded_not_leaked() {
        let source = "## S\n\n- [ ] `alpha`\n\n>     - [/] example\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert!(item.code_blocks.is_empty());
        assert!(!item.display_text.contains("example"));
        assert_eq!(document.lists[0].items.len(), 1);
    }

    #[test]
    fn nested_blockquoted_indented_lookalike_is_discarded_not_leaked() {
        let source = "## S\n\n- [ ] `alpha`\n\n> >     - [/] example\n";
        let document = parse(source);
        let item = &document.lists[0].items[0];
        assert!(item.code_blocks.is_empty());
        assert!(!item.display_text.contains("example"));
        assert_eq!(document.lists[0].items.len(), 1);
    }

    /// Byte offsets of every `TaskListMarker` pulldown-cmark reports for
    /// `source` — the ground truth the `[/]` pre-pass must agree with.
    fn task_marker_offsets(source: &str) -> HashSet<usize> {
        Parser::new_ext(source, PARSE_OPTIONS)
            .into_offset_iter()
            .filter(|(event, _)| matches!(event, Event::TaskListMarker(_)))
            .map(|(_, range)| range.start)
            .collect()
    }

    /// Runs the pre-pass on `source` and checks it against pulldown-cmark:
    /// the rewrite preserves byte length, touches nothing but the recorded
    /// `[/]` ranges, and records exactly the `[/]`s that pulldown-cmark
    /// itself reads as task markers once `[/]` is spelled `[ ]`.
    fn assert_pre_pass_agrees_with_pulldown(source: &str) -> HashSet<usize> {
        let (processed, started) = extract_started_markers(source);
        assert_eq!(processed.len(), source.len(), "length kept: {source:?}");
        for (offset, (before, after)) in source.bytes().zip(processed.bytes()).enumerate() {
            if before != after {
                assert!(
                    started.contains(&(offset - 1)),
                    "only recorded markers are rewritten: {source:?}"
                );
            }
        }
        let expected: HashSet<usize> = task_marker_offsets(&source.replace("[/]", "[ ]"))
            .into_iter()
            .filter(|&offset| source[offset..].starts_with("[/]"))
            .collect();
        assert_eq!(
            started, expected,
            "pre-pass disagrees with pulldown: {source:?}"
        );
        started
    }

    #[test]
    fn extract_started_markers_boundary_sweep_agrees_with_pulldown() {
        // The recorded markers must be exactly pulldown's task markers across
        // indentation, blockquotes, list types and every block context that can
        // hide a `[/]` lookalike.
        let indents = ["", " ", "  ", "   ", "    ", "\t"];
        let quote_depths = ["", "> ", "> > ", ">"];
        let markers = ["-", "*", "+", "1.", "2)"];
        let states = ["[/]", "[ ]", "[x]"];
        for indent in indents {
            for quote in quote_depths {
                for marker in markers {
                    for state in states {
                        let source = format!("## S\n\n{indent}{quote}{marker} {state} task\n");
                        assert_pre_pass_agrees_with_pulldown(&source);
                    }
                }
            }
        }

        let mut wrappers: Vec<(String, String)> = Vec::new();
        for fence_char in ['`', '~'] {
            let info_strings: &[&str] = if fence_char == '`' {
                &["", "rust", "done"]
            } else {
                &["", "rust", "`backtick`", "done"]
            };
            for len in [3usize, 4, 6, 10, 20] {
                let fence: String = std::iter::repeat_n(fence_char, len).collect();
                for &info in info_strings {
                    wrappers.push((format!("{fence}{info}\n"), format!("{fence}\n")));
                    wrappers.push((format!("{fence}{info}\n"), String::new()));
                    wrappers.push((format!("- {fence}{info}\n"), format!("  {fence}\n")));
                }
                wrappers.push((format!("<!--\n{fence}\n"), "-->\n".to_string()));
                wrappers.push((format!("    {fence}\n\n"), String::new()));
            }
        }
        for (open, close) in wrappers {
            for inner in ["- [/] inside", "  - [/] inside"] {
                let source = format!("## S\n\n- [ ] a\n\n{open}{inner}\n{close}\n- [/] after\n");
                assert_pre_pass_agrees_with_pulldown(&source);
            }
        }
    }

    #[test]
    fn started_task_after_a_fence_inside_an_html_comment_is_kept() {
        // A fence-looking line inside an HTML block opens no fence, so the
        // `[/]` task after it must still be a started task rather than its
        // whole list vanishing as checkbox-free.
        let source = "## S\n\n- [ ] a\n\n<!--\n```\n-->\n\n- [/] after\n";
        let items = &parse(source).lists[0].items;
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[1].display_text, "after");
        assert_eq!(items[1].kind, ItemKind::Checkbox(TaskState::Started));
    }

    #[test]
    fn started_task_after_an_indented_fence_lookalike_is_kept() {
        // Four-space-indented backticks are indented code, not a fence.
        let source = "## S\n\n    ```\n\n- [/] x\n- [ ] c\n";
        let items = &parse(source).lists[0].items;
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].display_text, "x");
        assert_eq!(items[0].kind, ItemKind::Checkbox(TaskState::Started));
        assert_eq!(items[1].kind, ItemKind::Checkbox(TaskState::NotStarted));
    }

    #[test]
    fn started_lookalike_in_a_fence_opened_on_a_marker_line_is_not_rewritten() {
        // A fence opened on a list-marker line is still a fence, so a
        // `[/]` lookalike inside it is code and must reach the card (and
        // `y`) exactly as written.
        let source = "## S\n\n- [ ] a\n\n- ```\n  - [/] literal\n  ```\n";
        let document = parse(source);
        let items = &document.lists[0].items;
        let block_owner = items
            .iter()
            .find(|item| !item.code_blocks.is_empty())
            .expect("the fence is kept as a code block");
        assert_eq!(block_owner.code_blocks, vec!["- [/] literal".to_string()]);
        assert!(
            items
                .iter()
                .all(|item| item.kind != ItemKind::Checkbox(TaskState::Started))
        );
    }

    #[test]
    fn started_marker_without_trailing_whitespace_is_left_alone() {
        // `[/]x` isn't task syntax, so it must not be rewritten to `[ ]x`.
        let source = "## S\n\n- [ ] a\n- [/]x\n";
        let items = &parse(source).lists[0].items;
        assert_eq!(items[1].kind, ItemKind::DisplayOnly);
        assert_eq!(items[1].display_text, "[/]x");
    }

    #[test]
    fn bare_started_marker_agrees_with_pulldown() {
        for source in [
            "## S\n\n- [/]\n",
            "## S\n\n- [/]",
            "## S\n\n- [/]\r\n- [ ] b\n",
        ] {
            assert_pre_pass_agrees_with_pulldown(source);
        }
    }

    #[test]
    fn started_marker_after_a_bom_is_still_found() {
        let source = "\u{FEFF}## S\n\n- [/] a\n";
        let items = &parse(source).lists[0].items;
        assert_eq!(items[0].kind, ItemKind::Checkbox(TaskState::Started));
    }
}
