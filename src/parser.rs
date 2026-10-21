use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use pulldown_cmark::utils::TextMergeWithOffset;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::model::{
    Anchor, BodySpan, Document, Item, ItemKind, LineNumber, List, PreambleBlock, PreambleKind,
    SubHeading, TaskState, TextStyle, Unshown,
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
/// Deeper than any Markdown heading, so the next heading of any level ends
/// the section of a heading inside a card.
const IN_CARD_HEADING_LEVEL: u8 = 7;

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

/// pulldown-cmark only recognizes `[ ]` and `[x]`. This pre-pass records
/// the offset of every `[/]` (started) and `[-]` (skipped) exactly where a
/// task marker would be, rewrites those to `[ ]` (same length, so offsets
/// hold), and returns them with their state so the final parse can promote
/// them.
///
/// It repeats until a parse finds no new one, because a rewrite can change
/// the block structure: as text, `[/]` makes a paragraph that swallows an
/// ordered sub-list starting at 2 on the next line, which as a marker it
/// leaves alone, hiding that sub-list's own `[/]` from the first parse.
/// Each round only uncovers one more level of such a chain, so the rounds
/// stop after `EXTENDED_MARKER_ROUNDS`, leaving deeper lookalikes as text
/// rather than reparsing a crafted file once per level (#216).
///
/// A heading inside an item, or an item whose first line is empty, can put
/// a lookalike where a marker would sit without pulldown-cmark reading one
/// there, so a last parse restores every rewrite it doesn't report as a
/// `TaskListMarker`, keeping the card's text the file's.
fn extract_extended_markers(raw: &str) -> (String, HashMap<usize, TaskState>) {
    let mut processed = raw.to_string();
    let mut markers = HashMap::new();
    for _ in 0..EXTENDED_MARKER_ROUNDS {
        let found: Vec<(usize, TaskState)> = extended_markers_in(&processed)
            .into_iter()
            .filter(|(offset, _)| !markers.contains_key(offset))
            .collect();
        if found.is_empty() {
            break;
        }
        for (offset, state) in found {
            processed.replace_range(offset..offset + 3, "[ ]");
            markers.insert(offset, state);
        }
    }
    restore_non_markers(raw, &mut processed, &mut markers);
    (processed, markers)
}

/// How many parses `extract_extended_markers` spends finding `[/]` and
/// `[-]` markers. A real checklist settles in two; each further round only
/// reaches one level deeper into a chain of bare markers over ordered
/// sub-lists starting at 2.
const EXTENDED_MARKER_ROUNDS: usize = 8;

/// Undoes every rewrite in `markers` that pulldown-cmark doesn't report as
/// a task marker in `processed`, putting back the bytes `raw` has there.
fn restore_non_markers(raw: &str, processed: &mut String, markers: &mut HashMap<usize, TaskState>) {
    let reported: HashSet<usize> = Parser::new_ext(processed, PARSE_OPTIONS)
        .into_offset_iter()
        .filter(|(event, _)| matches!(event, Event::TaskListMarker(_)))
        .map(|(_, range)| range.start)
        .collect();
    markers.retain(|&offset, _| {
        let keep = reported.contains(&offset);
        if !keep {
            processed.replace_range(offset..offset + 3, &raw[offset..offset + 3]);
        }
        keep
    });
}

/// The `[/]` and `[-]` markers one parse of `source` sees, by offset.
fn extended_markers_in(source: &str) -> Vec<(usize, TaskState)> {
    let mut markers = Vec::new();
    let mut awaiting_first_inline = false;
    for (event, range) in Parser::new_ext(source, PARSE_OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::Item) => awaiting_first_inline = true,
            // A `-` line under the task's text makes it a setext heading, and
            // pulldown-cmark still reads `[ ]` there as a task marker.
            Event::Start(Tag::Paragraph | Tag::Heading { .. }) => {}
            Event::Text(_) if awaiting_first_inline => {
                awaiting_first_inline = false;
                if let Some(state) = extended_marker_at(source, range.start) {
                    markers.push((range.start, state));
                }
            }
            _ => awaiting_first_inline = false,
        }
    }
    markers
}

/// The state of a `[/]` or `[-]` marker at `offset` followed by whitespace —
/// the same shape pulldown-cmark requires of `[ ]` before it reports a
/// `TaskListMarker` (a `[ ]` at the very end of the input is not one).
fn extended_marker_at(raw: &str, offset: usize) -> Option<TaskState> {
    let rest = &raw[offset..];
    let (state, after) = match rest.strip_prefix("[/]") {
        Some(after) => (TaskState::Started, after),
        None => (TaskState::Skipped, rest.strip_prefix("[-]")?),
    };
    after
        .chars()
        .next()
        .is_some_and(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
        .then_some(state)
}

/// Mutable state for one list item while it's being parsed. Kept on a stack
/// so a nested item's Start/End (interleaved inside its parent's) doesn't
/// clobber the parent's fields.
#[derive(Default)]
struct ItemBuilder {
    line: LineNumber,
    depth: usize,
    ordinal: Option<String>,
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
    /// opened. A heading inside a list item is item content, never
    /// structural, so this is stable for the whole item and shared by every
    /// item in the same Markdown list.
    section: Vec<SubHeading>,
    kind: Option<ItemKind>,
    text: String,
    header: String,
    capturing_header: bool,
    header_done: bool,
    /// Inline code inside the bold title: drawn as plain title text, so not
    /// a copy candidate, but it still keeps a bold-only first bullet a card.
    header_has_code: bool,
    code_spans: Vec<String>,
    code_blocks: Vec<String>,
    body: Vec<BodySpan>,
    /// Active inline styling for the text runs currently being read,
    /// toggled by Emphasis/Strong/Strikethrough start/end events.
    style: TextStyle,
    /// `Some((url, text))` while inside a link: the destination plus the link
    /// text accumulated so far; flushed to a `BodySpan::Link` on link end.
    link: Option<(String, String)>,
    /// One entry per open image: where its alt text starts in `text` when the
    /// image opened its own link, `None` when it sits inside a link or the
    /// title and just lends its alt text.
    images: Vec<Option<usize>>,
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
    /// is inline strong styling in the body, as is bold inside a link.
    fn start_strong(&mut self) {
        if !self.header_done && self.link.is_none() && self.text.trim().is_empty() {
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

    /// An image reads as a link labelled `image: <alt>` (`image` with no alt
    /// text), so the card shows there was a picture and `o` opens it.
    fn start_image(&mut self, url: &str) {
        if self.link.is_some() || self.capturing_header {
            self.images.push(None);
            return;
        }
        self.start_link(url);
        self.images.push(Some(self.text.len()));
    }

    fn end_image(&mut self) {
        let Some(Some(alt_start)) = self.images.pop() else {
            return;
        };
        if let Some((_, link_text)) = &mut self.link {
            let label = if link_text.trim().is_empty() {
                "image".to_string()
            } else {
                format!("image: {}", link_text.trim())
            };
            self.text.truncate(alt_start);
            self.text.push_str(&label);
            *link_text = label;
        }
        self.end_link();
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
        } else if self.capturing_header {
            self.header.push_str(text);
            self.header_has_code = true;
        } else {
            self.code_spans.push(text.to_string());
            self.text.push_str(text);
            self.body.push(BodySpan::Code(text.to_string()));
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
            ordinal: self.ordinal,
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

/// The ordered-list marker (`3.` or `3)`) at the start of an item's
/// source, or `None` for a bullet. CommonMark allows 1-9 digits.
fn ordered_marker(item_source: &str) -> Option<String> {
    let digits = item_source
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(item_source.len());
    let delimiter = item_source[digits..].chars().next()?;
    ((1..=9).contains(&digits) && matches!(delimiter, '.' | ')'))
        .then(|| item_source[..=digits].to_string())
}

/// Whether a finished top-level-eligible item could be its list's banner,
/// apart from the "first top-level item" rule. A bullet with children is
/// never the banner, which emits no `Item` and would orphan them; code in the
/// bold or a command under the bullet keeps it a card, as written; a blockquoted bullet is an example, not the list's.
fn is_banner_candidate(
    item: &Item,
    header_has_code: bool,
    has_children: bool,
    in_blockquote: bool,
) -> bool {
    !header_has_code
        && item.kind == ItemKind::DisplayOnly
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

/// A heading's GitHub anchor: lowercase, punctuation other than `-` and `_`
/// dropped, spaces turned into `-`.
pub(crate) fn slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// A quote opening with one of GitHub's alert markers, in any case, gets
/// the marker replaced by its label: `[!WARNING]`, `[!CAUTION]` and
/// `[!IMPORTANT]` become an alert, `[!NOTE]` and `[!TIP]` stay a quote.
fn as_alert(block: PreambleBlock) -> PreambleBlock {
    for (marker, label, kind) in [
        ("[!WARNING]", "Warning", PreambleKind::Alert),
        ("[!CAUTION]", "Caution", PreambleKind::Alert),
        ("[!IMPORTANT]", "Important", PreambleKind::Alert),
        ("[!NOTE]", "Note", PreambleKind::Quote),
        ("[!TIP]", "Tip", PreambleKind::Quote),
    ] {
        let Some(head) = block.text.get(..marker.len()) else {
            continue;
        };
        if head.eq_ignore_ascii_case(marker) {
            let rest = block.text[marker.len()..].trim_start();
            return PreambleBlock {
                kind,
                text: if rest.is_empty() {
                    label.to_string()
                } else {
                    format!("{label}: {rest}")
                },
            };
        }
    }
    block
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
    /// The `[/]` and `[-]` markers the pre-pass found, by offset.
    extended_markers: HashMap<usize, TaskState>,
    line_counter: LineCounter,

    lists: Vec<List>,
    default_items: Vec<Item>,
    default_banner: Option<String>,

    /// The open heading's level and the text read so far. Only set for a
    /// heading outside any list item; one inside an item is that item's
    /// content and is read by the item itself.
    heading: Option<(HeadingLevel, String)>,
    /// The open heading's source line, for its anchor.
    heading_line: LineNumber,
    /// Every heading's GitHub-style anchor and the lines its target is
    /// searched in, in order.
    anchors: Vec<Anchor>,
    /// The anchors whose section is still open, by heading level, as indexes
    /// into `anchors`; a heading inside a card counts as the deepest level.
    open_anchor_sections: Vec<(u8, usize)>,
    /// The text of a heading inside a list item, read so far, for its anchor.
    item_heading: Option<String>,
    /// The anchors already in `anchors`, so a repeat is checked in O(1).
    taken_anchors: HashSet<String>,
    /// The next `-N` suffix to try for each slug, so the k-th repeat of a
    /// heading doesn't retry every suffix before it (#148).
    next_anchor_suffix: HashMap<String, usize>,
    /// The lines of critical tasks (`**! Title**`).
    critical: std::collections::BTreeSet<LineNumber>,
    /// Set while the open heading sits inside a `>` blockquote outside any
    /// item: a quoted heading never opens a list, sets the title or starts a
    /// sub-section.
    heading_quoted: bool,
    /// The first non-empty `# H1` becomes the document title.
    document_title: Option<String>,
    /// The name of the pre-heading default list: the document title when
    /// its `# H1` opens the file, so it names no other list.
    default_title: Option<String>,

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
    /// of a card. Reset at each H2; only a kept Markdown list sets it.
    seen_first_top_item: bool,
    /// Whether the open top-level Markdown list has had a top-level item. It
    /// and `pending_banner` are committed at the list's end only if the list
    /// is kept, so a dropped checkbox-less list neither supplies the banner
    /// nor takes the banner slot.
    list_saw_top_item: bool,
    pending_banner: Option<String>,

    /// A fenced code block at column 0 immediately after a kept list is
    /// attached to that list's last item. Any other block between the list
    /// and the fence breaks the attachment.
    attach_fence_to_last_item: bool,
    /// The open code block: whether it is fenced, and its content so far.
    code_block: Option<(bool, String)>,

    /// One frame per open item; a stack so nested items don't clobber their
    /// parent's in-progress fields. The innermost open item is the top.
    item_stack: Vec<ItemBuilder>,

    /// The paragraph or quote being read outside any list (#80).
    prose: Option<PreambleBlock>,
    /// Whether blocks read now are a preamble: after a heading (or the start
    /// of the file) and before its first task. Anything else is unshown.
    preamble_open: bool,
    /// The level of the heading whose scope the reader is in (0 before any).
    scope_level: u8,
    /// Preamble blocks waiting for their task, each with the level of the
    /// heading it was written under.
    pending_preamble: Vec<(u8, PreambleBlock)>,
    /// Preambles claimed by the task starting at a line, in the list open
    /// then (`None` for the pre-heading default list); mapped onto the first
    /// task actually kept there in `finish`.
    preamble_claims: Vec<(Option<usize>, LineNumber, Vec<PreambleBlock>)>,
    unshown: Unshown,
}

impl<'a> ParseState<'a> {
    fn new(source: &'a str, extended_markers: HashMap<usize, TaskState>) -> Self {
        Self {
            source,
            extended_markers,
            line_counter: LineCounter::new(),
            lists: Vec::new(),
            default_items: Vec::new(),
            default_banner: None,
            heading: None,
            heading_line: 0,
            anchors: Vec::new(),
            open_anchor_sections: Vec::new(),
            item_heading: None,
            taken_anchors: HashSet::new(),
            next_anchor_suffix: HashMap::new(),
            critical: Default::default(),
            heading_quoted: false,
            document_title: None,
            default_title: None,
            section_stack: Vec::new(),
            list_depth: 0,
            current_list_items: Vec::new(),
            list_had_checkbox_syntax: false,
            blockquote_depth: 0,
            seen_first_top_item: false,
            list_saw_top_item: false,
            pending_banner: None,
            attach_fence_to_last_item: false,
            code_block: None,
            item_stack: Vec::new(),
            prose: None,
            preamble_open: true,
            scope_level: 0,
            pending_preamble: Vec::new(),
            preamble_claims: Vec::new(),
            unshown: Unshown::default(),
        }
    }

    /// Outside any list, item and heading: where a preamble block is read.
    fn at_top_level(&self) -> bool {
        self.item_stack.is_empty() && self.list_depth == 0 && self.heading.is_none()
    }

    fn start_prose(&mut self, kind: PreambleKind) {
        self.prose = Some(PreambleBlock {
            kind,
            text: String::new(),
        });
    }

    fn finish_prose(&mut self) {
        let Some(mut block) = self.prose.take() else {
            return;
        };
        block.text = block.text.trim().to_string();
        if block.text.is_empty() {
            return;
        }
        if block.kind == PreambleKind::Quote {
            block = as_alert(block);
        }
        self.add_block(block);
    }

    fn add_block(&mut self, block: PreambleBlock) {
        if self.preamble_open {
            self.pending_preamble.push((self.scope_level, block));
        } else {
            self.unshown.add(block.kind);
        }
    }

    /// A heading closes the scope of any same-level or deeper heading before
    /// it: prose left there without a task is unshown. A parent heading's
    /// prose carries on to the first task below.
    fn open_preamble_scope(&mut self, level: u8) {
        let (kept, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_preamble)
            .into_iter()
            .partition(|(origin, _)| *origin < level);
        for (_, block) in dropped {
            self.unshown.add(block.kind);
        }
        self.pending_preamble = kept;
        self.preamble_open = true;
        self.scope_level = level;
    }

    /// Records the heading's anchor the way GitHub derives it, numbering a
    /// repeat `-1`, `-2`, …, so `[Rollback](#rollback)` links work as they do
    /// there (#139). Every rendered heading takes part, quoted and in-item
    /// ones included, so the numbering matches GitHub's (#186); `target` is
    /// the line the jump searches for a task from.
    fn add_anchor(&mut self, text: &str, target: LineNumber, level: u8) {
        let base = slug(text);
        let repeat = self.next_anchor_suffix.entry(base.clone()).or_insert(0);
        let mut anchor = match *repeat {
            0 => base.clone(),
            n => format!("{base}-{n}"),
        };
        while self.taken_anchors.contains(&anchor) {
            *repeat += 1;
            anchor = format!("{base}-{repeat}");
        }
        *repeat += 1;
        self.taken_anchors.insert(anchor.clone());
        self.open_anchor_sections.push((level, self.anchors.len()));
        self.anchors.push(Anchor {
            name: anchor,
            from: target,
            until: None,
        });
    }

    /// A heading outside a quote or card ends the section of every anchor at
    /// its own level or deeper, at its line (#240).
    fn close_anchor_sections(&mut self, level: u8, line: LineNumber) {
        let anchors = &mut self.anchors;
        self.open_anchor_sections.retain(|&(open, index)| {
            let closes = open >= level;
            if closes {
                anchors[index].until = Some(line);
            }
            !closes
        });
    }

    /// The first task after a heading takes the preamble written above it.
    fn claim_preamble(&mut self, line: LineNumber) {
        self.preamble_open = false;
        if self.pending_preamble.is_empty() {
            return;
        }
        let blocks = std::mem::take(&mut self.pending_preamble)
            .into_iter()
            .map(|(_, block)| block)
            .collect();
        let list = self.lists.len().checked_sub(1);
        self.preamble_claims.push((list, line, blocks));
    }

    fn handle(&mut self, event: Event, offset: usize) {
        if !keeps_fence_attachment(&event) {
            self.attach_fence_to_last_item = false;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => self.on_heading_start(level, offset),
            Event::End(TagEnd::Heading(level)) => self.on_heading_end(level),
            Event::Start(Tag::List(_)) => self.on_list_start(),
            Event::End(TagEnd::List(_)) => self.on_list_end(),
            Event::Start(Tag::BlockQuote(_)) => {
                if self.blockquote_depth == 0 && self.at_top_level() {
                    self.start_prose(PreambleKind::Quote);
                }
                self.blockquote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                if self.blockquote_depth == 0 {
                    self.finish_prose();
                }
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
            Event::Start(Tag::Image { dest_url, .. }) => {
                self.with_item(|item| item.start_image(&dest_url));
            }
            Event::End(TagEnd::Image) => self.with_item(ItemBuilder::end_image),
            Event::Start(Tag::CodeBlock(kind)) => {
                let fenced = matches!(kind, CodeBlockKind::Fenced(_));
                self.code_block = Some((fenced, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => self.on_code_block_end(),
            Event::Text(text) => self.on_text(&text),
            Event::SoftBreak | Event::HardBreak => self.on_break(),
            Event::Code(text) => self.on_code(&text),
            Event::Start(Tag::Paragraph) if self.at_top_level() => match &mut self.prose {
                // A quote's next paragraph.
                Some(quote) if !quote.text.is_empty() => quote.text.push('\n'),
                Some(_) => {}
                None => self.start_prose(PreambleKind::Paragraph),
            },
            Event::End(TagEnd::Paragraph) if self.at_top_level() && self.blockquote_depth == 0 => {
                self.finish_prose();
            }
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
    /// or the pre-heading default one.
    fn target_items(&mut self) -> &mut Vec<Item> {
        match self.lists.last_mut() {
            Some(list) => &mut list.items,
            None => &mut self.default_items,
        }
    }

    /// A `## H2`, or a `# H1` other than the document title, starts a list.
    fn start_list(&mut self, title: &str) {
        self.lists.push(List {
            title: title.to_string(),
            banner: None,
            items: Vec::new(),
        });
        self.seen_first_top_item = false;
        self.section_stack.clear();
    }

    fn has_any_item(&self) -> bool {
        !self.default_items.is_empty() || self.lists.iter().any(|list| !list.items.is_empty())
    }

    /// A heading inside a list item (`- ## Rollback`) is read like a bold
    /// run: a leading one becomes the card title, a later one bold body text.
    fn on_heading_start(&mut self, level: HeadingLevel, offset: usize) {
        if let Some(item) = self.item_stack.last_mut() {
            item.mark_boundary();
            item.start_strong();
            self.item_heading = Some(String::new());
        } else {
            self.heading = Some((level, String::new()));
            self.heading_line = self.line_counter.line_at(offset, self.source);
            self.heading_quoted = self.blockquote_depth > 0;
        }
    }

    fn on_heading_end(&mut self, level: HeadingLevel) {
        let Some((open_level, text)) = self.heading.take() else {
            self.with_item(|item| {
                item.end_strong();
                item.mark_boundary();
            });
            if let (Some(text), Some(line)) = (
                self.item_heading.take(),
                self.item_stack.last().map(|item| item.line),
            ) {
                self.add_anchor(text.trim(), line, IN_CARD_HEADING_LEVEL);
            }
            return;
        };
        let quoted = std::mem::take(&mut self.heading_quoted);
        if open_level != level {
            return;
        }
        let text = text.trim();
        let number = heading_level_number(level);
        if !quoted {
            self.close_anchor_sections(number, self.heading_line);
        }
        self.add_anchor(text, self.heading_line + 1, number);
        if quoted {
            return;
        }
        self.open_preamble_scope(number);
        match level {
            HeadingLevel::H2 => self.start_list(text),
            HeadingLevel::H1 => {
                let titles_document = self.document_title.is_none() && !self.has_any_item();
                if self.document_title.is_none() && !text.is_empty() {
                    self.document_title = Some(text.to_string());
                }
                if titles_document {
                    self.default_title = self.document_title.clone();
                    self.section_stack.clear();
                } else {
                    self.start_list(text);
                }
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
            self.list_saw_top_item = false;
            self.pending_banner = None;
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
        let pending_banner = self.pending_banner.take();
        self.attach_fence_to_last_item = self.list_had_checkbox_syntax;
        if self.list_had_checkbox_syntax {
            self.seen_first_top_item |= std::mem::take(&mut self.list_saw_top_item);
            if let Some(banner) = pending_banner {
                let banner = Some(banner);
                match self.lists.last_mut() {
                    Some(list) => list.banner = banner,
                    None => self.default_banner = banner,
                }
            }
            // Items arrive post-order; sort by line to restore document order.
            items.sort_by_key(|item| item.line_number);
            self.target_items().append(&mut items);
        } else if self.blockquote_depth == 0 {
            let top_level = items.iter().filter(|item| item.depth == 0).count();
            self.unshown.bullets += top_level + usize::from(pending_banner.is_some());
        }
    }

    fn on_item_start(&mut self, offset: usize) {
        // pulldown-cmark can start a nested item's range at the end of the line
        // before it (a tab-indented item does), which would put it on its
        // parent's line, so the item starts at its first non-blank byte.
        let rest = &self.source[offset..];
        let offset = offset + rest.len() - rest.trim_start_matches([' ', '\t', '\r', '\n']).len();
        // `list_depth` was already incremented by the enclosing Start(List),
        // so depth 0 = top-level.
        let line = self.line_counter.line_at(offset, self.source);
        if self.item_stack.is_empty() {
            self.claim_preamble(line);
        }
        self.item_stack.push(ItemBuilder {
            line,
            depth: self.list_depth.saturating_sub(1),
            ordinal: ordered_marker(&self.source[offset..]),
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
        } else if let Some(&state) = self.extended_markers.get(&offset) {
            state
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
        let header_has_code = builder.header_has_code;
        let mut item = builder.into_item();
        let is_banner = !self.seen_first_top_item
            && !self.list_saw_top_item
            && is_banner_candidate(&item, header_has_code, has_children, in_blockquote);
        if item.depth == 0 {
            self.list_saw_top_item = true;
        }
        if is_banner {
            self.pending_banner = item.header.take();
        } else {
            // A task whose bold title starts with `!` is critical (#95): the
            // `!` stays readable in an editor or on GitHub, and here becomes a
            // confirmation on checking it.
            if matches!(item.kind, ItemKind::Checkbox(_))
                && let Some(rest) = item.header.as_deref().and_then(|h| h.strip_prefix('!'))
                && !rest.trim().is_empty()
            {
                item.header = Some(rest.trim_start().to_string());
                self.critical.insert(item.line_number);
            }
            self.current_list_items.push(item);
        }
    }

    /// Inside an item, fenced and indented blocks are both the item's code.
    /// Outside one, only a fence may attach to the previous list; any other
    /// top-level block is preamble for the next task, or counted as unshown.
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
        } else if self.at_top_level() && !content.trim().is_empty() {
            self.add_block(PreambleBlock {
                kind: PreambleKind::Code,
                text: content,
            });
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
        } else if let Some(prose) = self.prose.as_mut().filter(|_| self.item_stack.is_empty()) {
            if self.list_depth == 0 {
                prose.text.push_str(text);
            }
        } else {
            self.push_item_heading(text);
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
        } else if let Some(prose) = self.prose.as_mut().filter(|_| self.item_stack.is_empty()) {
            if self.list_depth == 0 {
                prose.text.push(' ');
            }
        } else {
            self.push_item_heading(" ");
            self.with_item(ItemBuilder::push_break);
        }
    }

    fn on_code(&mut self, text: &str) {
        if let Some((_, heading_text)) = &mut self.heading {
            heading_text.push_str(text);
        } else if let Some(prose) = self.prose.as_mut().filter(|_| self.item_stack.is_empty()) {
            if self.list_depth == 0 {
                prose.text.push_str(text);
            }
        } else {
            self.push_item_heading(text);
            self.with_item(|item| item.push_code(text));
        }
    }

    fn push_item_heading(&mut self, text: &str) {
        if let Some(heading) = &mut self.item_heading {
            heading.push_str(text);
        }
    }

    fn finish(mut self, path: PathBuf, raw_lines: Vec<String>) -> Document {
        let has_checkbox = |items: &[Item]| {
            items
                .iter()
                .any(|item| matches!(item.kind, ItemKind::Checkbox(_)))
        };
        // A claim goes to the first task kept at or after its line in its list:
        // the claiming bullet may have become the banner, which is no task.
        let mut preambles: std::collections::BTreeMap<LineNumber, Vec<PreambleBlock>> =
            std::collections::BTreeMap::new();
        for (list, line, blocks) in std::mem::take(&mut self.preamble_claims) {
            let items = match list {
                Some(index) => &self.lists[index].items,
                None => &self.default_items,
            };
            match items.iter().find(|item| item.line_number >= line) {
                Some(item) => preambles
                    .entry(item.line_number)
                    .or_default()
                    .extend(blocks),
                None => blocks.iter().for_each(|block| self.unshown.add(block.kind)),
            }
        }
        for (_, block) in std::mem::take(&mut self.pending_preamble) {
            self.unshown.add(block.kind);
        }
        let mut unshown = self.unshown;
        let mut lists = self.lists;
        let has_default_list = has_checkbox(&self.default_items);
        if has_default_list {
            lists.insert(
                0,
                List {
                    title: self
                        .default_title
                        .unwrap_or_else(|| crate::scaffold::derive_title(&path)),
                    banner: self.default_banner,
                    items: self.default_items,
                },
            );
        }
        // A list without a live checkbox is dropped entirely: one that never
        // had a list, whose lists had no checkbox syntax, or whose only
        // checkboxes are blockquoted examples (forced to display-only).
        lists.retain(|list| has_checkbox(&list.items));
        let kept_lines: HashSet<LineNumber> = lists
            .iter()
            .flat_map(|list| &list.items)
            .map(|item| item.line_number)
            .collect();
        preambles.retain(|line, blocks| {
            let kept = kept_lines.contains(line);
            if !kept {
                blocks.iter().for_each(|block| unshown.add(block.kind));
            }
            kept
        });
        Document {
            file_path: path,
            title: self.document_title,
            has_default_list,
            lists,
            preambles,
            unshown,
            anchors: self.anchors,
            critical: self.critical,
            raw_lines,
        }
    }
}

pub fn parse_document(path: PathBuf) -> anyhow::Result<Document> {
    let bytes = crate::writer::read_checklist(&path)?;
    let raw = String::from_utf8(bytes).map_err(|err| {
        anyhow::Error::new(err).context(
            "the file is not valid UTF-8 (markcheck reads UTF-8 files; convert it with e.g. iconv)",
        )
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
    let (source, extended_markers) = extract_extended_markers(&without_front_matter);

    let mut state = ParseState::new(&source, extended_markers);
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
#[path = "parser_tests.rs"]
mod tests;
