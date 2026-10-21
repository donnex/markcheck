use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Instant, SystemTime};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use sha2::{Digest, Sha256};

use crate::effect::Effect;

pub type LineNumber = usize;

/// SHA-256 of `bytes`, used to detect whether a file still holds what was
/// last seen. Not a security boundary.
pub(crate) fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// A checkbox task's progress. `Started` is persisted with the `[/]`
/// marker and counts as **not done** for completion and progress stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    NotStarted,
    Started,
    Done,
    /// `[-]`: decided not to do it this run (not applicable). Counts as
    /// resolved, like done, but is reported apart from it (#54).
    Skipped,
}

impl TaskState {
    /// Nothing left to decide: done or skipped.
    pub fn is_resolved(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Skipped)
    }

    /// The state a toggle key aimed at `target` leaves: `target`, or back to
    /// not started when the task is already there.
    pub fn toggle(self, target: TaskState) -> TaskState {
        if self == target {
            TaskState::NotStarted
        } else {
            target
        }
    }

    /// The state as a status message names it: `Marked done`,
    /// `Undo: marked not done`.
    pub fn marked(self) -> &'static str {
        match self {
            TaskState::NotStarted => "not done",
            TaskState::Started => "started",
            TaskState::Done => "done",
            TaskState::Skipped => "skipped",
        }
    }

    /// The confirmation for a toggle from `self` to `new`: `Marked skipped`
    /// going in, `Marked not skipped` coming back out.
    pub fn toggle_message(self, new: TaskState) -> String {
        if new == TaskState::NotStarted && self != TaskState::NotStarted {
            format!("Marked not {}", self.marked())
        } else {
            format!("Marked {}", new.marked())
        }
    }

    /// The verb naming a change *into* this state in a git-sync commit
    /// message: `Check "Restart service"`, `Start "..."`, `Uncheck "..."`,
    /// `Skip "..."`.
    pub fn commit_verb(self) -> &'static str {
        match self {
            TaskState::NotStarted => "Uncheck",
            TaskState::Started => "Start",
            TaskState::Done => "Check",
            TaskState::Skipped => "Skip",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Checkbox(TaskState),
    DisplayOnly,
}

/// Inline text styling for a body run: emphasis (`*italic*`), strong
/// (mid-text `**bold**`), and strikethrough (`~~text~~`). These combine, so a
/// run can be several at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub emphasis: bool,
    pub strong: bool,
    pub strikethrough: bool,
}

impl TextStyle {
    /// True when no styling applies — the common case, rendered as plain prose.
    pub fn is_plain(&self) -> bool {
        !self.emphasis && !self.strong && !self.strikethrough
    }
}

/// One run of an item's body, tagged so the card can style it.
/// `display_text` is the plain concatenation of these runs' visible text, kept
/// for the overview and search matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodySpan {
    /// Plain prose.
    Text(String),
    /// Inline `` `code` ``.
    Code(String),
    /// Prose carrying inline styling — emphasis / strong / strikethrough.
    Styled { text: String, style: TextStyle },
    /// A Markdown link, rendered as its underlined text (the URL is listed in
    /// the panel below the card) and openable with `o`.
    Link { text: String, url: String },
}

/// A `### H3`-and-deeper heading that labels a sub-section *within* an `## H2`
/// list. `level` is the raw Markdown heading level (3–6). Sub-headings
/// are display context — a divider in the overview and a breadcrumb above the
/// card — not navigable items, so they carry no state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubHeading {
    pub level: u8,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub line_number: LineNumber,
    /// Nesting level within the list: 0 = top-level, 1 = first indented
    /// sub-list, etc. Items stay a flat Vec in document (pre-order)
    /// order; `depth` carries the hierarchy as metadata.
    pub depth: usize,
    /// The author's step number (`3.` or `3)`) when the item is in an
    /// ordered list, taken verbatim from its marker so non-sequential
    /// numbering is kept. `None` for a bullet.
    pub ordinal: Option<String>,
    /// The active `### H3`+ sub-section path at this item, outermost first.
    /// Empty directly under the `## H2`.
    pub section: Vec<SubHeading>,
    /// Body text only; a leading `**bold**` span is stripped into `header`.
    pub display_text: String,
    /// Ordered body runs (plain prose, styled prose, inline code, links) for
    /// styled card rendering.
    pub body: Vec<BodySpan>,
    /// Leading `**bold**` at the absolute start of the item — the card title.
    pub header: Option<String>,
    /// Inline `` `code` `` spans, in order.
    pub code_spans: Vec<String>,
    /// Code blocks belonging to this item, in order: fenced or indented ones
    /// inside it, and fences immediately following its list.
    pub code_blocks: Vec<String>,
    pub kind: ItemKind,
}

impl Item {
    /// The one-line name used wherever an item is listed (overview row,
    /// `T` picker, parent breadcrumb): the card title, or the body when there
    /// is none, prefixed with the ordered-list step number if any.
    pub fn label(&self) -> String {
        let text = self.header.as_deref().unwrap_or(&self.display_text);
        match &self.ordinal {
            Some(ordinal) => format!("{ordinal} {text}"),
            None => text.to_string(),
        }
    }

    /// All matchable text for search: the card title, the body
    /// (prose plus inline code, both already folded into `display_text`), and
    /// its `code_blocks`, fenced or indented (which are *not* part of
    /// `display_text`).
    pub fn search_text(&self) -> String {
        let mut text = String::new();
        if let Some(header) = &self.header {
            text.push_str(header);
            text.push(' ');
        }
        text.push_str(&self.display_text);
        for block in &self.code_blocks {
            text.push(' ');
            text.push_str(block);
        }
        text
    }

    /// The URLs of the links in this item's body, in order. Drives the
    /// `o` open-link action: exactly one → open it; zero or several → a hint.
    pub fn link_urls(&self) -> Vec<&str> {
        self.body
            .iter()
            .filter_map(|span| match span {
                BodySpan::Link { url, .. } => Some(url.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Task counts by state, for a list or the whole document. Skipped tasks
/// don't apply to this run, so they stay outside [`Progress::total`], the
/// `done/total` every counter shows (#54).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub started: usize,
    pub skipped: usize,
    /// Not started.
    pub open: usize,
}

impl Progress {
    /// The tasks a run has to finish: everything but skipped.
    pub fn total(self) -> usize {
        self.done + self.started + self.open
    }

    /// Done or skipped: nothing left to decide.
    pub fn resolved(self) -> usize {
        self.done + self.skipped
    }

    /// Every task, skipped ones included.
    pub fn tasks(self) -> usize {
        self.total() + self.skipped
    }

    /// Every task is resolved, and there is at least one.
    pub fn is_finished(self) -> bool {
        self.resolved() > 0 && self.resolved() == self.tasks()
    }

    fn with(self, state: TaskState) -> Progress {
        let mut next = self;
        *match state {
            TaskState::NotStarted => &mut next.open,
            TaskState::Started => &mut next.started,
            TaskState::Done => &mut next.done,
            TaskState::Skipped => &mut next.skipped,
        } += 1;
        next
    }

    fn plus(self, other: Progress) -> Progress {
        Progress {
            done: self.done + other.done,
            started: self.started + other.started,
            skipped: self.skipped + other.skipped,
            open: self.open + other.open,
        }
    }
}

#[derive(Debug, Clone)]
pub struct List {
    pub title: String,
    /// A leading bold-only bullet becomes the list's banner — a non-navigable
    /// warning line shown below the list title, not a card.
    pub banner: Option<String>,
    pub items: Vec<Item>,
}

impl List {
    /// How the list's tasks stand; notes are not counted.
    pub fn progress(&self) -> Progress {
        self.items
            .iter()
            .filter_map(|item| match item.kind {
                ItemKind::Checkbox(state) => Some(state),
                ItemKind::DisplayOnly => None,
            })
            .fold(Progress::default(), Progress::with)
    }

    /// Indices of the items that begin a `### H3`+ sub-section, where the
    /// overview draws a divider; the `}`/`{` targets.
    pub fn sub_section_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut prev: &[SubHeading] = &[];
        for (i, item) in self.items.iter().enumerate() {
            let common = prev
                .iter()
                .zip(&item.section)
                .take_while(|(a, b)| a == b)
                .count();
            if item.section.len() > common {
                starts.push(i);
            }
            prev = &item.section;
        }
        starts
    }

    /// Indices of item `i`'s ancestors, outermost first — each the nearest
    /// preceding item one level shallower. Empty for a top-level item.
    /// Best-effort: skips a missing level in a malformed tree.
    pub fn parent_chain(&self, i: usize) -> Vec<usize> {
        let mut want = match self.items.get(i) {
            Some(item) => item.depth,
            None => return Vec::new(),
        };
        let mut chain = Vec::new();
        let mut j = i;
        while want > 0 && j > 0 {
            j -= 1;
            if self.items[j].depth == want - 1 {
                chain.push(j);
                want -= 1;
            }
        }
        chain.reverse();
        chain
    }

    /// Whether item `i` heads a sub-list — i.e. the next item is nested one or
    /// more levels deeper.
    pub fn has_children(&self, i: usize) -> bool {
        self.items
            .get(i + 1)
            .is_some_and(|next| next.depth > self.items[i].depth)
    }

    /// The **list-local** ordinal of the sub-list headed by the parent at
    /// `parent_index`: its position among this list's parent items (those
    /// with children). `Document::sublist_base` offsets it into a document-wide
    /// slot so every sub-list gets its own guide color.
    pub fn sublist_slot(&self, parent_index: usize) -> usize {
        (0..parent_index).filter(|&j| self.has_children(j)).count()
    }

    /// The number of sub-lists in this list — i.e. items that have children.
    /// Used to offset each list's sub-list slots into a global sequence.
    pub fn parent_count(&self) -> usize {
        (0..self.items.len())
            .filter(|&i| self.has_children(i))
            .count()
    }

    /// Indices of item `i`'s descendants — the following items deeper than `i`,
    /// up to (not including) the next item at `i`'s own depth or shallower.
    /// The inverse of `parent_chain`. Empty for a leaf.
    pub fn descendants(&self, i: usize) -> Vec<usize> {
        let base = match self.items.get(i) {
            Some(item) => item.depth,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for (j, item) in self.items.iter().enumerate().skip(i + 1) {
            if item.depth <= base {
                break;
            }
            out.push(j);
        }
        out
    }

    /// Aggregate state of a display-only item's checkbox descendants: `Done`
    /// when all are done, `Started` when some are started or done, `None`
    /// otherwise and for checkbox items. Never counted as progress.
    pub fn info_parent_state(&self, i: usize) -> Option<TaskState> {
        if !matches!(self.items.get(i)?.kind, ItemKind::DisplayOnly) {
            return None;
        }
        let mut total = 0;
        let mut done = 0;
        let mut any_active = false;
        for j in self.descendants(i) {
            if let ItemKind::Checkbox(state) = self.items[j].kind {
                match state {
                    TaskState::Done => {
                        done += 1;
                        any_active = true;
                    }
                    TaskState::Started => any_active = true,
                    TaskState::NotStarted => {}
                    TaskState::Skipped => continue,
                }
                total += 1;
            }
        }
        aggregate_state(total, done, any_active)
    }

    /// `parent_chain`'s last entry, `sublist_slot` and `info_parent_state` for
    /// every item, in one linear pass instead of a scan per item, so the
    /// overview stays linear on long or deeply nested lists.
    pub fn item_links(&self) -> Vec<ItemLinks> {
        let n = self.items.len();
        let mut last_at_depth: Vec<Option<usize>> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        let mut subtree_end = vec![n; n];
        let mut links = Vec::with_capacity(n);
        let mut parents_so_far = 0;
        // Prefix counts over checkbox items: (total, done, active).
        let mut prefix = Vec::with_capacity(n + 1);
        prefix.push((0usize, 0usize, 0usize));
        for (i, item) in self.items.iter().enumerate() {
            while let Some(&j) = open.last()
                && self.items[j].depth >= item.depth
            {
                subtree_end[j] = i;
                open.pop();
            }
            open.push(i);

            let parent = item
                .depth
                .checked_sub(1)
                .and_then(|d| last_at_depth.get(d).copied().flatten());
            if last_at_depth.len() <= item.depth {
                last_at_depth.resize(item.depth + 1, None);
            }
            last_at_depth[item.depth] = Some(i);

            links.push(ItemLinks {
                parent,
                sublist_slot: parents_so_far,
                info_state: None,
            });
            if self.has_children(i) {
                parents_so_far += 1;
            }

            let (total, done, active) = *prefix.last().unwrap_or(&(0, 0, 0));
            prefix.push(match item.kind {
                ItemKind::Checkbox(TaskState::Done) => (total + 1, done + 1, active + 1),
                ItemKind::Checkbox(TaskState::Started) => (total + 1, done, active + 1),
                ItemKind::Checkbox(TaskState::NotStarted) => (total + 1, done, active),
                ItemKind::Checkbox(TaskState::Skipped) | ItemKind::DisplayOnly => {
                    (total, done, active)
                }
            });
        }
        for (i, item) in self.items.iter().enumerate() {
            if matches!(item.kind, ItemKind::DisplayOnly) {
                let (end_total, end_done, end_active) = prefix[subtree_end[i]];
                let (start_total, start_done, start_active) = prefix[i + 1];
                links[i].info_state = aggregate_state(
                    end_total - start_total,
                    end_done - start_done,
                    end_active > start_active,
                );
            }
        }
        links
    }
}

/// `info_parent_state`'s rule over a sub-list's checkbox counts.
fn aggregate_state(total: usize, done: usize, any_active: bool) -> Option<TaskState> {
    if total == 0 {
        None
    } else if done == total {
        Some(TaskState::Done)
    } else if any_active {
        Some(TaskState::Started)
    } else {
        None
    }
}

/// Per-item tree data from `List::item_links`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemLinks {
    /// The last entry of `parent_chain`: the nearest preceding item one level
    /// shallower.
    pub parent: Option<usize>,
    /// The list-local `sublist_slot` this item would have as a parent.
    pub sublist_slot: usize,
    /// `info_parent_state`.
    pub info_state: Option<TaskState>,
}

/// What `R` resets (#118).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetScope {
    All,
    /// The current `## H2` list.
    List,
    /// The sub-list the cursor is in: a parent task and everything below it.
    SubList,
}

/// Where to open the checklist: `markcheck +N file.md` or `file.md#anchor`
/// (#114).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAt {
    /// The task on, or the first one after, this source line.
    Line(LineNumber),
    /// The first task under the heading with this GitHub-style anchor.
    Anchor(String),
}

/// One block of prose under a heading, before its first task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreambleBlock {
    pub kind: PreambleKind,
    /// Paragraphs of a quote are separated by `\n`.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreambleKind {
    Paragraph,
    Quote,
    /// A `> [!WARNING]`, `[!CAUTION]` or `[!IMPORTANT]` quote (in any case),
    /// shown in the warning color; `text` starts with the alert's label.
    Alert,
    Code,
}

/// Blocks outside any task list that markcheck doesn't show (prose between
/// tasks, under a heading with no tasks, after the last task, and the items of
/// a list with no checkbox), counted for the startup report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unshown {
    pub paragraphs: usize,
    pub quotes: usize,
    pub bullets: usize,
    pub code_blocks: usize,
}

impl Unshown {
    pub fn add(&mut self, kind: PreambleKind) {
        match kind {
            PreambleKind::Paragraph => self.paragraphs += 1,
            PreambleKind::Quote | PreambleKind::Alert => self.quotes += 1,
            PreambleKind::Code => self.code_blocks += 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Unshown::default()
    }
}

/// `%C3%A9` as `é`, the form a browser gives a copied link to a heading;
/// text that doesn't decode to UTF-8 is kept as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|pair| bytes[i] == b'%' && pair.iter().all(u8::is_ascii_hexdigit))
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_string())
}

#[derive(Debug, Clone)]
pub struct Document {
    pub file_path: PathBuf,
    /// The document's `# H1` title, if present. `None` when the file
    /// has no (non-empty) level-1 heading.
    pub title: Option<String>,
    /// True when `lists[0]` is the synthesized default list for items
    /// before the first `## H2`, named after the document title.
    /// Used to suppress the above-cards list heading for it.
    pub has_default_list: bool,
    pub lists: Vec<List>,
    /// Prose written under a heading before its first task, keyed by the
    /// line of the task it belongs to, which shows it above its card (#80).
    pub preambles: std::collections::BTreeMap<LineNumber, Vec<PreambleBlock>>,
    /// What the file has outside task lists that markcheck can't show.
    pub unshown: Unshown,
    /// Each heading's GitHub-style anchor (`rollback` for `## Rollback`) and
    /// the line its first task is searched from: the line after the heading,
    /// or a card's own line for a heading inside it, in document order
    /// (#114, #139, #186).
    pub anchors: Vec<(String, LineNumber)>,
    /// The lines of critical tasks, whose bold title started with `!`:
    /// checking one takes a second, deliberate press (#95).
    pub critical: std::collections::BTreeSet<LineNumber>,
    /// The file's lines as markcheck last read or wrote them (a save puts its
    /// own output back here), each including its own terminator; the 1-based
    /// `Item::line_number` is at index `line_number - 1`. `writer::write_back` edits one line and rejoins by
    /// concatenation, so mixed line endings round-trip exactly.
    pub raw_lines: Vec<String>,
}

impl Document {
    /// How the tasks of every list stand.
    pub fn progress(&self) -> Progress {
        self.lists
            .iter()
            .map(List::progress)
            .fold(Progress::default(), Progress::plus)
    }

    /// The first task after the heading `anchor` names (without its `#`,
    /// percent-encoded or not), as `(list, item)`. `None` when no heading has
    /// that anchor or nothing follows it.
    pub fn anchor_target(&self, anchor: &str) -> Option<(usize, usize)> {
        let wanted = percent_decode(anchor).to_lowercase();
        let (_, line) = self.anchors.iter().find(|(taken, _)| *taken == wanted)?;
        self.first_task_from(*line)
    }

    /// The first item on or after source line `line`, as `(list, item)`.
    pub fn first_task_from(&self, line: LineNumber) -> Option<(usize, usize)> {
        self.lists.iter().enumerate().find_map(|(li, list)| {
            list.items
                .iter()
                .position(|item| item.line_number >= line)
                .map(|ii| (li, ii))
        })
    }

    /// Whether the task at `(list, item)` is critical.
    pub fn is_critical(&self, list_index: usize, item_index: usize) -> bool {
        self.lists
            .get(list_index)
            .and_then(|list| list.items.get(item_index))
            .is_some_and(|item| self.critical.contains(&item.line_number))
    }

    /// The number of sub-lists in all lists **before** `list_index` — the
    /// base that turns a list-local `sublist_slot` into a document-wide one.
    pub fn sublist_base(&self, list_index: usize) -> usize {
        self.lists[..list_index]
            .iter()
            .map(List::parent_count)
            .sum()
    }
}

/// Why the checklist path can't currently be read or saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGone {
    Deleted,
    /// A symlink, FIFO, device or directory now sits at the path.
    NotRegular,
}

impl FileGone {
    /// The sticky error shown while the path stays this way.
    pub fn message(self) -> &'static str {
        match self {
            FileGone::Deleted => "File was deleted — changes cannot be saved until it exists again",
            FileGone::NotRegular => {
                "File was replaced by a symlink or special file — not loaded; changes cannot \
                 be saved until it is a regular file again"
            }
        }
    }
}

/// The base view under any overlay. Changed by navigation, toggles and
/// reloads; an open [`Overlay`] is left alone by all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Checklist,
    ListComplete,
    AllComplete,
}

/// An input chord waiting for its next key. At most one is armed at a time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PendingChord {
    #[default]
    None,
    /// A lone `g`, waiting for the second `g` of `gg` (go to the first task).
    G,
    /// `o` on a card with several links, waiting for the link number.
    /// `typed` is the digits typed so far as a number (0 for none), so a card
    /// with ten or more links can pick `[10]`+.
    OpenLink { typed: usize },
    /// `y` on a card with several commands, waiting for the command number,
    /// with `typed` as for [`PendingChord::OpenLink`].
    CopyCode { typed: usize },
}

/// A modal layer over the [`View`]. Closing one is `overlay = None`, which
/// returns to the view it was opened from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// Modal confirmation before resetting tasks to not-done: every task, the
    /// current list or the current sub-list.
    ConfirmReset,
    /// Offered on quit when all tasks are done: reset before quitting?
    ConfirmQuitReset,
    /// Full keybinding cheatsheet overlay, opened with `?`.
    Help,
    /// Incremental text search input, entered with `/` from `Checklist`; as
    /// the query is typed, the cursor jumps live to the first match at or
    /// after where the search started, wrapping around.
    Search,
    /// Full-list "go to task" overlay, opened with `T`: a filterable list of
    /// every task; typing filters, arrows / Ctrl-N/P move the selection, and
    /// `Enter` jumps to it.
    ListPicker,
}

/// The glyphs used across the UI. Plain-Unicode symbols that render in any
/// terminal font by default; the `--nerd-font` flag swaps in Nerd Font
/// icons.
#[derive(Debug, Clone, Copy)]
pub struct IconSet {
    pub done: &'static str,
    pub pending: &'static str,
    pub started: &'static str,
    pub skipped: &'static str,
    pub current: &'static str,
    pub note: &'static str,
    pub file: &'static str,
    pub list: &'static str,
    pub update: &'static str,
    /// The title-bar git-sync icon, always paired with a "git" label. A plain
    /// arrow in both icon sets, since the Nerd Font git glyphs rendered poorly.
    pub sync: &'static str,
}

impl IconSet {
    /// Classic Font Awesome range (U+F000–F2E0), retained by Nerd Fonts v3
    /// and single-cell wide.
    pub const fn nerd() -> Self {
        IconSet {
            done: "\u{f14a}",    // check-square (filled box + check)
            pending: "\u{f096}", // square-o (empty box)
            started: "\u{f252}", // hourglass-half
            skipped: "\u{f05e}", // ban (circle with a slash)
            current: "\u{f0da}", // caret-right
            note: "\u{f05a}",    // info-circle
            file: "\u{f15c}",    // file-text
            list: "\u{f0ca}",    // list-ul
            update: "\u{f021}",  // refresh (circular arrows)
            sync: "⇅",           // plain Unicode by design — see field doc
        }
    }

    pub const fn unicode() -> Self {
        IconSet {
            done: "☑",
            pending: "☐",
            started: "◐",
            skipped: "⊘",
            current: "❯",
            note: "▪",
            file: "≡",
            list: "•",
            update: "↻",
            sync: "⇅",
        }
    }
}

/// Terminal color capability, detected once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit `Rgb` (`COLORTERM=truecolor`/`24bit`, or a known truecolor
    /// `TERM` such as `xterm-kitty`, or one ending in `-direct`).
    TrueColor,
    /// 256-color palette (`TERM` contains `256color`).
    Palette256,
    /// Named ANSI-16 only — the safe fallback.
    Basic,
    /// No color at all (`NO_COLOR` set): state is carried by glyphs and
    /// modifiers alone.
    Mono,
}

/// The `--clipboard` flag / `clipboard` config key (#133).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
pub enum ClipboardChoice {
    /// The system clipboard, falling back to OSC 52; over SSH, both.
    #[default]
    #[value(name = "auto")]
    #[serde(rename = "auto")]
    Auto,
    /// The system clipboard only.
    #[value(name = "system")]
    #[serde(rename = "system")]
    System,
    /// OSC 52 only, through the terminal in front of the user.
    #[value(name = "osc52")]
    #[serde(rename = "osc52")]
    Osc52,
}

/// Where a copy goes, resolved once at startup from [`ClipboardChoice`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CopyRoute {
    /// The system clipboard, then OSC 52 if that fails.
    #[default]
    SystemThenOsc52,
    /// Both: over SSH the system clipboard may be a stale `DISPLAY`'s,
    /// on another machine than the user's.
    Both,
    SystemOnly,
    Osc52Only,
}

impl ClipboardChoice {
    /// `remote` is whether this is an SSH session.
    pub fn route(self, remote: bool) -> CopyRoute {
        match self {
            ClipboardChoice::Auto if remote => CopyRoute::Both,
            ClipboardChoice::Auto => CopyRoute::SystemThenOsc52,
            ClipboardChoice::System => CopyRoute::SystemOnly,
            ClipboardChoice::Osc52 => CopyRoute::Osc52Only,
        }
    }
}

/// The `--color` flag / `color` config key: `Auto` detects the terminal's
/// capability, anything else forces that palette.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
pub enum ColorChoice {
    #[default]
    #[value(name = "auto")]
    #[serde(rename = "auto")]
    Auto,
    #[value(name = "truecolor")]
    #[serde(rename = "truecolor")]
    TrueColor,
    #[value(name = "256")]
    #[serde(rename = "256")]
    Palette256,
    #[value(name = "16")]
    #[serde(rename = "16")]
    Basic,
}

impl ColorChoice {
    /// The depth this choice forces, or `None` for `Auto`.
    pub fn forced_depth(self) -> Option<ColorDepth> {
        match self {
            ColorChoice::Auto => None,
            ColorChoice::TrueColor => Some(ColorDepth::TrueColor),
            ColorChoice::Palette256 => Some(ColorDepth::Palette256),
            ColorChoice::Basic => Some(ColorDepth::Basic),
        }
    }
}

/// `TERM` values of terminals that support 24-bit color but don't put
/// `256color` in their name, and whose `COLORTERM` is lost over SSH, `sudo`
/// or a multiplexer started without it.
const TRUECOLOR_TERMS: &[&str] = &[
    "xterm-kitty",
    "alacritty",
    "foot",
    "foot-extra",
    "wezterm",
    "contour",
    "xterm-ghostty",
];

/// Decides color depth from the `NO_COLOR`, `COLORTERM` and `TERM` values
/// (pure, so it's unit-testable; `detect_color_depth` supplies the real env).
/// A non-empty `NO_COLOR` wins over everything, per no-color.org.
pub fn depth_from_env(
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> ColorDepth {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return ColorDepth::Mono;
    }
    if let Some(ct) = colorterm {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return ColorDepth::TrueColor;
        }
    }
    if term.is_some_and(|t| TRUECOLOR_TERMS.contains(&t) || t.ends_with("-direct")) {
        return ColorDepth::TrueColor;
    }
    if term.is_some_and(|t| t.contains("256color")) {
        return ColorDepth::Palette256;
    }
    ColorDepth::Basic
}

/// Reads `NO_COLOR`/`COLORTERM`/`TERM` from the environment to pick a color
/// depth.
pub fn detect_color_depth() -> ColorDepth {
    depth_from_env(
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Semantic UI colors. Each is resolved once at startup to the best
/// representation the terminal supports — a curated 24-bit hue on truecolor
/// terminals, the nearest 256-palette index on 256-color terminals, or a
/// named ANSI-16 color as the always-safe fallback (which also respects the
/// user's own theme). Curated hues are the widely-liked "One Dark" set.
/// With `NO_COLOR` every role is `Color::Reset`.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub done: Color,       // completed tasks, progress fill
    pub started: Color,    // started/in-progress tasks, incomplete counter
    pub code: Style,       // inline + fenced code: an explicit fg/bg pair, so it reads on any theme
    pub current: Color,    // cursor/current, filename, accents
    pub note: Color,       // display-only note cards
    pub error: Color,      // error text, e.g. a sticky status-bar error
    pub warning: Color,    // list banner — a leading bold-only bullet
    pub depth: ColorDepth, // the tier this palette was resolved for (gradient, depth guides)
}

impl Palette {
    /// Named ANSI-16 — the fallback, and the default used in tests. Respects
    /// the terminal's own theme.
    pub const fn basic() -> Self {
        Palette {
            done: Color::Green,
            started: Color::Yellow,
            code: Style::new().fg(Color::White).bg(Color::DarkGray),
            current: Color::Cyan,
            note: Color::Blue,
            error: Color::Red,
            warning: Color::Yellow, // ANSI-16 has no separate amber
            depth: ColorDepth::Basic,
        }
    }

    /// Nearest 256-palette indices to the curated hues.
    pub const fn palette256() -> Self {
        Palette {
            done: Color::Indexed(114),    // green
            started: Color::Indexed(180), // yellow
            code: Style::new().fg(Color::Indexed(250)).bg(Color::Indexed(237)),
            current: Color::Indexed(73),  // cyan
            note: Color::Indexed(75),     // blue
            error: Color::Indexed(203),   // red
            warning: Color::Indexed(173), // amber/orange
            depth: ColorDepth::Palette256,
        }
    }

    /// Curated 24-bit hues (One Dark).
    pub const fn truecolor() -> Self {
        Palette {
            done: rgb(DONE_RGB),
            started: rgb(STARTED_RGB),
            // One Dark foreground on its selection gray.
            code: Style::new()
                .fg(Color::Rgb(0xab, 0xb2, 0xbf))
                .bg(Color::Rgb(0x3e, 0x44, 0x51)),
            current: Color::Rgb(0x56, 0xb6, 0xc2),
            note: Color::Rgb(0x61, 0xaf, 0xef),
            error: Color::Rgb(0xe0, 0x6c, 0x75),   // One Dark red
            warning: Color::Rgb(0xd1, 0x9a, 0x66), // One Dark orange/amber
            depth: ColorDepth::TrueColor,
        }
    }

    /// No color (`NO_COLOR`): every role is the terminal default, and code
    /// is set off in reverse video instead of a background.
    pub const fn mono() -> Self {
        Palette {
            done: Color::Reset,
            started: Color::Reset,
            code: Style::new().add_modifier(Modifier::REVERSED),
            current: Color::Reset,
            note: Color::Reset,
            error: Color::Reset,
            warning: Color::Reset,
            depth: ColorDepth::Mono,
        }
    }

    pub const fn for_depth(depth: ColorDepth) -> Self {
        match depth {
            ColorDepth::TrueColor => Self::truecolor(),
            ColorDepth::Palette256 => Self::palette256(),
            ColorDepth::Basic => Self::basic(),
            ColorDepth::Mono => Self::mono(),
        }
    }

    pub fn is_mono(&self) -> bool {
        self.depth == ColorDepth::Mono
    }

    /// Style for error and empty-state text: the error color, or bold
    /// reverse video under `NO_COLOR`, where the color alone would leave a
    /// failure looking like a confirmation.
    pub fn error_style(&self) -> Style {
        if self.is_mono() {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(self.error)
        }
    }

    /// The palette `choice` forces, or with `Auto` the one for the current
    /// terminal's detected capability.
    pub fn detect(choice: ColorChoice) -> Self {
        Self::for_depth(choice.forced_depth().unwrap_or_else(detect_color_depth))
    }

    /// Guide color for a nested sub-list, from a small cycle of distinct hues.
    /// `slot` is the document-wide ordinal: `Document::sublist_base` plus the
    /// list-local `sublist_slot`.
    pub fn depth_color(&self, slot: usize) -> Color {
        match self.depth {
            ColorDepth::TrueColor => {
                const RAMP: [(u8, u8, u8); 4] = [
                    (0x61, 0xaf, 0xef), // blue
                    (0xc6, 0x78, 0xdd), // purple
                    (0x56, 0xb6, 0xc2), // cyan
                    (0xd1, 0x9a, 0x66), // orange
                ];
                let (r, g, b) = RAMP[slot % RAMP.len()];
                Color::Rgb(r, g, b)
            }
            ColorDepth::Palette256 => {
                const RAMP: [u8; 4] = [75, 176, 73, 173]; // blue, purple, cyan, orange
                Color::Indexed(RAMP[slot % RAMP.len()])
            }
            ColorDepth::Basic => {
                const RAMP: [Color; 4] = [Color::Blue, Color::Magenta, Color::Cyan, Color::Yellow];
                RAMP[slot % RAMP.len()]
            }
            ColorDepth::Mono => Color::Reset,
        }
    }

    /// Style for a `done/total` counter, ramping from yellow to green with the
    /// done ratio. `total == 0` counts as complete. Stepped named colors on
    /// ANSI-16.
    pub fn progress_color(&self, done: usize, total: usize) -> Style {
        let ratio = if total == 0 {
            1.0
        } else {
            done as f32 / total as f32
        };
        match self.depth {
            ColorDepth::TrueColor => Style::default().fg(progress_rgb(ratio)),
            ColorDepth::Palette256 => {
                let (r, g, b) = progress_rgb_parts(ratio);
                Style::default().fg(Color::Indexed(rgb_to_256(r, g, b)))
            }
            // ANSI-16 can't interpolate, so step through named colors instead.
            // Starts at yellow (never gray) so it's readable at 0% done.
            ColorDepth::Basic => {
                if ratio >= 1.0 {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else if ratio >= 0.5 {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Yellow)
                }
            }
            ColorDepth::Mono if ratio >= 1.0 => Style::default().add_modifier(Modifier::BOLD),
            ColorDepth::Mono => Style::default(),
        }
    }
}

/// The truecolor done and started colors, which are also the progress
/// ramp's stops: yellow, not gray, so 0% stays readable.
const DONE_RGB: (u8, u8, u8) = (0x98, 0xc3, 0x79);
const STARTED_RGB: (u8, u8, u8) = (0xe5, 0xc0, 0x7b);

const fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Linear interpolation from started-yellow (0% done) to done-green (100%),
/// returning raw `(r, g, b)`. `ratio` is clamped to `[0, 1]`.
fn progress_rgb_parts(ratio: f32) -> (u8, u8, u8) {
    let ratio = ratio.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8, t: f32| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    (
        lerp(STARTED_RGB.0, DONE_RGB.0, ratio),
        lerp(STARTED_RGB.1, DONE_RGB.1, ratio),
        lerp(STARTED_RGB.2, DONE_RGB.2, ratio),
    )
}

/// The progress ramp color as a 24-bit `Color::Rgb`.
fn progress_rgb(ratio: f32) -> Color {
    let (r, g, b) = progress_rgb_parts(ratio);
    Color::Rgb(r, g, b)
}

/// Nearest xterm-256 index for an RGB triple: the closer of the best 6×6×6
/// color-cube cell (indices 16–231) and the best gray-ramp step (232–255),
/// compared by squared distance. Used to render the progress gradient on 256-color
/// terminals.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // 6-level cube axis values are 0, 95, 135, 175, 215, 255.
    let cube_axis = |v: u8| -> (u8, u8) {
        let levels = [0u8, 95, 135, 175, 215, 255];
        let mut best = 0usize;
        let mut best_d = u32::MAX;
        for (i, &lv) in levels.iter().enumerate() {
            let d = (lv as i32 - v as i32).unsigned_abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        (best as u8, levels[best])
    };
    let (ri, rv) = cube_axis(r);
    let (gi, gv) = cube_axis(g);
    let (bi, bv) = cube_axis(b);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube_dist = color_dist((rv, gv, bv), (r, g, b));

    // Grayscale ramp: 24 steps from 8 to 238 in increments of 10 (232–255).
    let gray_level = ((r as u32 + g as u32 + b as u32) / 3) as i32;
    let gray_step = (((gray_level - 8).clamp(0, 230)) as f32 / 10.0).round() as u8;
    let gray_val = 8 + gray_step as i32 * 10;
    let gray_index = 232 + gray_step;
    let gray_dist = color_dist((gray_val as u8, gray_val as u8, gray_val as u8), (r, g, b));

    if gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

/// Squared Euclidean distance between two RGB triples.
fn color_dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| {
        let d = x as i32 - y as i32;
        (d * d) as u32
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

/// What a click on an overview row does: jump to a whole list (a list-title
/// row), focus an exact task (an item row's label), or toggle a
/// task done/not-done in place (an item row's marker prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewTarget {
    List(usize),
    Item(usize, usize),
    Toggle(usize, usize),
}

/// The input that confirms a critical step: the one that armed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CriticalAction {
    /// `Space`/`Enter` checking the current task.
    Check,
    /// `-` skipping the current task.
    Skip,
    /// A click on the task's overview marker.
    Click,
}

/// A critical step waiting for its confirming press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CriticalArm {
    /// The task, as `(list, item)`.
    pub position: (usize, usize),
    pub action: CriticalAction,
    /// When the arming input, or the latest too-quick repeat of it, came.
    pub at: Instant,
}

pub struct AppState {
    pub document: Document,
    pub current_list_index: usize,
    pub current_item_index: usize,
    pub view: View,
    /// The modal layer drawn over `view`, if any; it gets every key first.
    pub overlay: Option<Overlay>,
    pub status_message: Option<String>,
    /// When the current `status_message` should auto-clear. `None` means
    /// it persists until the next input — used for error messages so a failure
    /// can't silently vanish; ephemeral messages (copies, reload/reset info)
    /// set it to now + the status timeout.
    pub status_expiry: Option<SystemTime>,
    /// Whether the current `status_message` is a failure / "nothing happened"
    /// message, which the status bar renders in `palette.error` (red) so
    /// problems stand out from passive confirmations. Set by `set_error`;
    /// cleared by `set_status`/`clear_status`.
    pub status_is_error: bool,
    /// Outward actions requested by handlers, in order, for the main loop to
    /// carry out through [`crate::effect::drain`].
    pub effects: Vec<Effect>,
    /// Last known modification time of `document.file_path`, used to detect
    /// external edits. Updated on load and after every write we make
    /// ourselves, so our own writes are never mistaken for external changes.
    pub file_mtime: Option<SystemTime>,
    /// Last known size, cross-checked with `file_mtime` to catch more edits on
    /// a coarse-mtime filesystem.
    pub file_size: Option<u64>,
    /// SHA-256 of the content we last confirmed on disk. `commit_write`
    /// re-hashes before writing and refuses on a mismatch, catching changes
    /// mtime and size can miss.
    pub file_content_hash: Option<[u8; 32]>,
    /// When the document's content last changed — set both when we write it
    /// ourselves (toggle/reset) and when an external change is reloaded.
    /// Drives the title-bar "Updated … ago" tag. Not set for skipped
    /// reloads or failed writes.
    pub last_update_at: Option<SystemTime>,
    /// Background git-sync state (`--git-sync`) — see [`GitSyncState`].
    pub git_sync: GitSyncState,
    /// Scroll offset (in wrapped lines) of the current card's body.
    /// Reset to 0 on any navigation.
    pub card_scroll: u16,
    /// Geometry the renderer measured on the last frame — see [`ViewLayout`].
    pub layout: ViewLayout,
    /// The input chord armed by the last key, waiting for its next key — see
    /// [`PendingChord`]. Reset by any other key.
    pub pending_chord: PendingChord,
    pub icons: IconSet,
    /// Also copy to the X11 PRIMARY selection (`--primary`).
    pub clipboard_primary: bool,
    /// A critical task armed by a first `Space`, `-` or click: the same
    /// input again, after a pause, confirms it; anything else disarms it.
    pub critical_armed: Option<CriticalArm>,
    /// Where `o` on a `#anchor` link jumped from, latest last, for `Ctrl-O`.
    pub jump_back: Vec<(usize, usize)>,
    /// Where `y` copies to (`--clipboard`).
    pub copy_route: CopyRoute,
    /// Auto-copy an item's code when navigating to its card (`--auto-copy`);
    /// announces a successful copy, silent otherwise.
    pub auto_copy: bool,
    /// Semantic UI colors, resolved to the terminal's capability.
    pub palette: Palette,
    /// Set when the file is confirmed deleted or replaced by something that
    /// isn't a regular file (not just transiently unreadable). Blocks all
    /// write operations until a regular file is back.
    pub file_gone: Option<FileGone>,
    /// A warning about the last write, shown once the key or click that made
    /// it has been handled, so the action's own status can't replace it.
    pub write_notice: Option<String>,
    /// Whether this session already said a save went in place (see
    /// `writer::Saved::in_place`); said once, not on every toggle.
    pub in_place_noted: bool,
    /// Incremental `/` search state — see [`SearchState`].
    pub search: SearchState,
    /// `T` go-to-task overlay state — see [`PickerState`].
    pub picker: PickerState,
    /// `?` help overlay scroll state — see [`HelpState`].
    pub help: HelpState,
    /// Undo history for state-changing actions (toggle/start/reset): a
    /// stack of full checkbox-state snapshots, each captured *before* a
    /// mutating write. `u` pops the top and applies it. Cleared on any external
    /// reload (an external edit is a hard boundary); capped at `UNDO_HISTORY_CAP`.
    pub undo_stack: Vec<StateSnapshot>,
    /// Redo history: snapshots pushed here by `u` so `Ctrl-R` can replay
    /// them. Cleared whenever a fresh mutating action happens (the usual
    /// undo/redo rule) and on any external reload.
    pub redo_stack: Vec<StateSnapshot>,
    /// Bumped whenever an external reload clears the undo/redo history, so a
    /// failed undo/redo can tell whether the stack it popped from still exists.
    pub history_epoch: u64,
}

/// Incremental `/` search state.
#[derive(Debug, Default)]
pub struct SearchState {
    /// The live query typed while the `Search` screen is active.
    pub query: String,
    /// The last committed search query, reused by `n`/`N` to cycle matches
    /// after the search prompt has been dismissed.
    pub last: Option<String>,
    /// Cursor `(list, item)` when the current search began; restored if the
    /// search is cancelled with `Esc`.
    pub origin: (usize, usize),
}

/// Geometry the renderer measures while drawing a frame and the key and
/// mouse handlers read back: scroll limits, viewport heights and click
/// regions. `ui::render` fills it every frame; the handlers only empty a
/// part whose subject changed (a new card's scroll limit, a freshly opened
/// picker's rows) until the next frame measures it. Before the first frame
/// it is empty (zero heights, no regions), so a scroll moves nothing and a
/// click hits nothing (#142).
#[derive(Debug, Default)]
pub struct ViewLayout {
    /// Maximum useful scroll of the current card's body (0 = it fits).
    pub card_max_scroll: u16,
    /// Visible height in rows of the card body; the half-page and page
    /// scroll distances.
    pub card_viewport_height: u16,
    /// The current card's area, for click-to-copy. `None` off the checklist.
    pub card_rect: Option<Rect>,
    /// One `(row, code text)` per copy target on the current card, so a
    /// click copies exactly that command. Rows of one command share its text.
    pub code_regions: Vec<(Rect, Rc<str>)>,
    /// One `(row, target)` per clickable overview row. Empty while the
    /// overview is hidden, so stale rows can't be hit.
    pub overview_rows: Vec<(Rect, OverviewTarget)>,
    /// Visible row count of the `T` picker's list; its half-page jumps.
    pub picker_viewport_height: u16,
    /// The picker's entry rows and the index of the entry on its top row,
    /// for click hit-testing. `None` when no entries are drawn.
    pub picker_rows_area: Option<Rect>,
    pub picker_scroll_offset: usize,
    /// Maximum useful scroll and the viewport height of the help overlay.
    pub help_max_scroll: u16,
    pub help_viewport_height: u16,
}

/// `T` go-to-task overlay state: a filterable list of every task.
#[derive(Debug, Default)]
pub struct PickerState {
    /// The live filter query typed while the `ListPicker` overlay is open.
    pub query: String,
    /// Index of the highlighted row within the picker's current (filtered)
    /// entries; reset to 0 whenever the filter changes.
    pub selection: usize,
}

/// `?` help overlay scroll state.
#[derive(Debug, Default)]
pub struct HelpState {
    /// Scroll offset (in lines) of the overlay when it's taller than the
    /// terminal. Reset to 0 each time help opens.
    pub scroll: u16,
}

/// Background git-sync state (`--git-sync`).
#[derive(Debug, Default)]
pub struct GitSyncState {
    /// Whether git-sync is active this session; fixed at startup. Drives the
    /// title bar's `⇅ git` section.
    pub active: bool,
    /// When a sync last committed and pushed successfully. Cleared when a
    /// later attempt fails, so an old success can't vouch for newer changes.
    pub last_at: Option<SystemTime>,
    /// A background sync or push retry is running.
    pub busy: bool,
    /// A change is waiting out git sync's quiet period before it is synced.
    pub waiting: bool,
    /// Why the latest attempt left the file unsynced, as the title bar's short
    /// label; kept until a later sync lands, whatever keys are pressed.
    pub problem: Option<&'static str>,
    /// The reason last reported in the status bar, so a retry failing the
    /// same way doesn't replace whatever message is showing every time.
    pub reported_reason: Option<String>,
    /// The content markcheck's own unsynced changes started from: the file
    /// before the first change since it was loaded or reloaded, or an `e`
    /// edit the user saved. A toggle's sync must find it committed or staged
    /// (or saved in `e`). Kept across failed syncs and dropped when the file
    /// is reloaded with new content, so a change markcheck didn't make is
    /// never published under a toggle's message.
    pub unsynced_base: Option<String>,
    /// Counts the bases taken, so git sync can tell a new base from one it
    /// has already synced past, even when their content is the same.
    pub base_epoch: u64,
}

/// A queued git-sync request: the exact content expected once the change
/// lands, and a description for the commit message. The worker commits
/// this snapshot rather than re-reading the file, so an unrelated write
/// can't be swept in. `content_hash` lets it tell a superseded request
/// from an outside edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSync {
    pub content: String,
    pub content_hash: [u8; 32],
    /// The file's content just before `commit_write` wrote this change; `None`
    /// when it couldn't be read then. For an edit made in `e`, the edited
    /// content itself, since markcheck didn't write it.
    /// Used only by git-sync's staged-target guard, which compares it with the
    /// staged blob through `hash-object --path` so clean filters apply. A full
    /// copy rather than a blob SHA, which would need a git subprocess while the
    /// write lock is held.
    pub previous_content: Option<String>,
    pub description: String,
    /// Sync at once rather than after git sync's quiet period, for an edit
    /// made outside markcheck (the `e` editor) that shouldn't wait on keys.
    pub immediate: bool,
    /// What the sync may publish beyond the change it describes.
    pub base: SyncBase,
}

/// What a git-sync request may publish. `epoch` names the base the request
/// was made on: a new one is taken whenever the file is reloaded with content
/// markcheck didn't write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncBase {
    /// The whole content: an edit the user made in `e` and saved. Later
    /// toggles on the same base may publish it too.
    Edited { epoch: u64 },
    /// Only markcheck's own changes on top of `content`, which must already
    /// be committed or staged. `content` is `None` when it couldn't be read,
    /// and the sync then refuses.
    Own { epoch: u64, content: Option<String> },
}

impl SyncBase {
    pub fn epoch(&self) -> u64 {
        match self {
            SyncBase::Edited { epoch } | SyncBase::Own { epoch, .. } => *epoch,
        }
    }
}

/// Every checkbox item's state, keyed by 1-based line number. Undo skips
/// lines no longer present.
pub type StateSnapshot = Vec<(LineNumber, TaskState)>;

/// Maximum number of undo entries kept in memory. Snapshots are a few
/// bytes per task, so this is generous; it only bounds pathological sessions.
pub const UNDO_HISTORY_CAP: usize = 100;

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
