use std::path::PathBuf;
use std::time::SystemTime;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

pub type LineNumber = usize;

/// A checkbox task's progress. `Started` is persisted with the `[/]`
/// marker and counts as **not done** for completion and progress stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    NotStarted,
    Started,
    Done,
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
    /// A Markdown link, rendered as `text (url)` and openable with `o`.
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
    /// The active `### H3`+ sub-section path at this item, outermost first.
    /// Empty when the item sits directly under its `## H2`. Every item
    /// in one Markdown list shares the same snapshot, since a heading can't
    /// appear inside a list; the overview diffs adjacent items' paths to place
    /// dividers, and the card shows the path as a breadcrumb.
    pub section: Vec<SubHeading>,
    /// Body text only; a leading `**bold**` span is stripped into `header`.
    pub display_text: String,
    /// Ordered body runs (prose vs inline code) for styled card rendering.
    pub body: Vec<BodySpan>,
    /// Leading `**bold**` at the absolute start of the item — the card title.
    pub header: Option<String>,
    /// Inline `` `code` `` spans, in order.
    pub code_spans: Vec<String>,
    /// Fenced code blocks belonging to this item (inside it, or immediately
    /// following its list), in order.
    pub code_blocks: Vec<String>,
    pub kind: ItemKind,
}

impl Item {
    /// All matchable text for search: the card title, the body
    /// (prose plus inline code, both already folded into `display_text`), and
    /// any fenced command blocks (which are *not* part of `display_text`).
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

#[derive(Debug, Clone)]
pub struct List {
    pub title: String,
    /// A leading bold-only bullet becomes the list's banner — a non-navigable
    /// warning line shown below the list title, not a card.
    pub banner: Option<String>,
    pub items: Vec<Item>,
}

impl List {
    /// Returns (done, started, total) counting only Checkbox items; DisplayOnly
    /// items are excluded since they can't be completed. `started` is the
    /// in-progress state, surfaced distinctly (e.g. the progress bar).
    pub fn checkbox_progress(&self) -> (usize, usize, usize) {
        let mut done = 0;
        let mut started = 0;
        let mut total = 0;
        for item in &self.items {
            if let ItemKind::Checkbox(state) = item.kind {
                total += 1;
                match state {
                    TaskState::Done => done += 1,
                    TaskState::Started => started += 1,
                    TaskState::NotStarted => {}
                }
            }
        }
        (done, started, total)
    }

    /// Returns (done, total) counting only Checkbox items.
    pub fn checkbox_stats(&self) -> (usize, usize) {
        let (done, _started, total) = self.checkbox_progress();
        (done, total)
    }

    /// Indices of the items that **begin** a `### H3`+ sub-section: an
    /// item whose `section` introduces at least one heading beyond its common
    /// prefix with the previous item's — exactly the positions where the
    /// overview draws a `── Text` divider. These are the jump targets for the
    /// `}`/`{` next/previous-sub-section motions.
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
    fn has_children(&self, i: usize) -> bool {
        self.items
            .get(i + 1)
            .is_some_and(|next| next.depth > self.items[i].depth)
    }

    /// The **list-local** ordinal of the sub-list headed by the parent at
    /// `parent_index`: its position among this list's parent items (those
    /// with children). `Document::sublist_slot` offsets it into a document-wide
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

    /// Aggregate state of a display-only item's sub-list, for the "info parent
    /// reflects its children" treatment: `Done` when it has at
    /// least one descendant checkbox and every one is `Done`, `Started` when at
    /// least one descendant checkbox is `Started` or `Done` but not all are
    /// `Done`, and `None` otherwise — including an item with no checkbox
    /// descendants. Checkbox items return `None`; they carry their own state.
    /// This never feeds `checkbox_progress`: an info parent is not a task.
    pub fn info_parent_state(&self, i: usize) -> Option<TaskState> {
        if !matches!(self.items.get(i)?.kind, ItemKind::DisplayOnly) {
            return None;
        }
        let mut total = 0;
        let mut done = 0;
        let mut any_active = false;
        for j in self.descendants(i) {
            if let ItemKind::Checkbox(state) = self.items[j].kind {
                total += 1;
                match state {
                    TaskState::Done => {
                        done += 1;
                        any_active = true;
                    }
                    TaskState::Started => any_active = true,
                    TaskState::NotStarted => {}
                }
            }
        }
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
}

#[derive(Debug, Clone)]
pub struct Document {
    pub file_path: PathBuf,
    /// The document's `# H1` title, if present. `None` when the file
    /// has no (non-empty) level-1 heading.
    pub title: Option<String>,
    /// True when `lists[0]` is the synthesized `(Default)` list for
    /// items before the first `## H2` — i.e. that list has no real H2.
    /// Used to suppress the above-cards list heading for it.
    pub has_default_list: bool,
    pub lists: Vec<List>,
    pub raw_lines: Vec<String>,
    /// True when the source file used `\r\n` line endings (detected once at
    /// parse time from the presence of any `\r\n` in the raw source).
    /// `raw_lines`/`str::lines()` strip the `\r`, so `write_back` needs this
    /// to rejoin with the file's original terminator instead of always `\n`.
    pub uses_crlf: bool,
    /// True when the source file's last byte was a newline (detected once at
    /// parse time). `raw_lines`/`str::lines()` drop the trailing terminator
    /// entirely, so `write_back` needs this to know whether to add one back
    /// — otherwise a file with no final newline would silently gain one on
    /// its first toggle.
    pub trailing_newline: bool,
}

impl Document {
    /// Returns (done, started, total) checkbox counts summed across all lists.
    pub fn checkbox_progress(&self) -> (usize, usize, usize) {
        self.lists.iter().map(List::checkbox_progress).fold(
            (0, 0, 0),
            |(done_acc, started_acc, total_acc), (done, started, total)| {
                (done_acc + done, started_acc + started, total_acc + total)
            },
        )
    }

    /// Returns (done, total) checkbox counts summed across all lists.
    pub fn checkbox_stats(&self) -> (usize, usize) {
        let (done, _started, total) = self.checkbox_progress();
        (done, total)
    }

    /// The number of sub-lists in all lists **before** `list_index` — the
    /// base that turns a list-local `sublist_slot` into a document-wide one.
    pub fn sublist_base(&self, list_index: usize) -> usize {
        self.lists[..list_index]
            .iter()
            .map(List::parent_count)
            .sum()
    }

    /// The **document-wide** color slot for the sub-list headed by
    /// `parent_index` in list `list_index`: the count of sub-lists in all
    /// earlier lists plus the sub-list's list-local ordinal. Every sub-list in
    /// the document gets a distinct slot, so the guide-color cycle
    /// (`Palette::depth_color`) never puts two neighbouring sub-lists — within a
    /// list or across lists — on the same color until it wraps.
    pub fn sublist_slot(&self, list_index: usize, parent_index: usize) -> usize {
        self.sublist_base(list_index) + self.lists[list_index].sublist_slot(parent_index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Checklist,
    ListComplete,
    AllComplete,
    /// Modal confirmation before resetting every task to not-done.
    ConfirmReset,
    /// Offered on quit when all tasks are done: reset before quitting?
    ConfirmQuitReset,
    /// Full keybinding cheatsheet overlay, opened with `?`.
    Help,
    /// Incremental text search input, entered with `/` from `Checklist`; the
    /// cursor jumps live to the first match as the query is typed.
    Search,
    /// Full-list "go to task" overlay, opened with `T`: a filterable list of
    /// every task; typing filters, arrows / Ctrl-N/P move the selection, and
    /// `Enter` jumps to it.
    ListPicker,
}

/// The glyphs used across the UI. Nerd Font icons by default; the
/// `--no-nerd-font` flag swaps in plain-Unicode symbols that render in
/// any terminal font (e.g. bare SSH targets).
#[derive(Debug, Clone, Copy)]
pub struct IconSet {
    pub done: &'static str,
    pub pending: &'static str,
    pub started: &'static str,
    pub current: &'static str,
    pub note: &'static str,
    pub file: &'static str,
    pub list: &'static str,
    pub update: &'static str,
    /// The title-bar git-sync section — paired with a literal "git"
    /// text label wherever it's used (`render_title_bar`), not relied on
    /// alone to convey "this is git" the way the other icons here can rely
    /// on their shape. Deliberately the *same* plain Unicode arrow symbol in
    /// both `nerd()` and `unicode()` below — unlike every other field, this
    /// one intentionally skips the Nerd Font Private Use Area glyphs (a
    /// prior `fa-git`/cloud-upload attempt didn't render legibly on the
    /// reporting user's setup); a plain Arrows-block character is far more
    /// likely to actually be present in whatever font is active.
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
    /// 24-bit `Rgb` (`COLORTERM=truecolor`/`24bit`).
    TrueColor,
    /// 256-color palette (`TERM` contains `256color`).
    Palette256,
    /// Named ANSI-16 only — the safe fallback.
    Basic,
}

/// Decides color depth from the `COLORTERM` and `TERM` values (pure, so it's
/// unit-testable; `detect_color_depth` supplies the real env).
pub fn depth_from_env(colorterm: Option<&str>, term: Option<&str>) -> ColorDepth {
    if let Some(ct) = colorterm {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return ColorDepth::TrueColor;
        }
    }
    if term.is_some_and(|t| t.contains("256color")) {
        return ColorDepth::Palette256;
    }
    ColorDepth::Basic
}

/// Reads `COLORTERM`/`TERM` from the environment to pick a color depth.
pub fn detect_color_depth() -> ColorDepth {
    depth_from_env(
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    )
}

/// Semantic UI colors. Each is resolved once at startup to the best
/// representation the terminal supports — a curated 24-bit hue on truecolor
/// terminals, the nearest 256-palette index on 256-color terminals, or a
/// named ANSI-16 color as the always-safe fallback (which also respects the
/// user's own theme). Curated hues are the widely-liked "One Dark" set.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub done: Color,       // completed tasks, progress fill
    pub started: Color,    // started/in-progress tasks, incomplete counter
    pub code_bg: Color,    // subtle background behind inline + fenced code (markdown-style)
    pub current: Color,    // cursor/current, filename, accents
    pub note: Color,       // display-only note cards
    pub error: Color,      // error text, e.g. the missing-title placeholder
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
            code_bg: Color::DarkGray,
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
            code_bg: Color::Indexed(237), // dark gray (code block)
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
            done: Color::Rgb(0x98, 0xc3, 0x79),
            started: Color::Rgb(0xe5, 0xc0, 0x7b),
            code_bg: Color::Rgb(0x3e, 0x44, 0x51), // One Dark selection gray
            current: Color::Rgb(0x56, 0xb6, 0xc2),
            note: Color::Rgb(0x61, 0xaf, 0xef),
            error: Color::Rgb(0xe0, 0x6c, 0x75),   // One Dark red
            warning: Color::Rgb(0xd1, 0x9a, 0x66), // One Dark orange/amber
            depth: ColorDepth::TrueColor,
        }
    }

    pub const fn for_depth(depth: ColorDepth) -> Self {
        match depth {
            ColorDepth::TrueColor => Self::truecolor(),
            ColorDepth::Palette256 => Self::palette256(),
            ColorDepth::Basic => Self::basic(),
        }
    }

    /// The palette for the current terminal's detected capability.
    pub fn detect() -> Self {
        Self::for_depth(detect_color_depth())
    }

    /// Hard-coded guide color for a nested sub-list: a small cycle of
    /// distinct, legible hues, so hierarchy reads by *which* color rather than
    /// one color fading — which was hard to tell apart. `slot` is the sub-list's
    /// **document-wide** ordinal (`Document::sublist_slot`), so every sub-list
    /// gets its own color and no two neighbouring ones — within a list or across
    /// lists — collide until the cycle wraps. Resolved per color depth.
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
        }
    }

    /// Style for a `done/total` progress counter that fades with completion:
    /// a two-stop ramp started-yellow → done-green driven by the done
    /// ratio, so progress is glanceable from the counter's color. It starts at
    /// yellow rather than gray so the counter is legible even at 0% done.
    /// `total == 0` counts as complete (green), preserving the old
    /// `done == total → green` semantics for a doc with no checkboxes.
    ///
    /// Resolution respects the color depth: a smooth RGB lerp on TrueColor, the
    /// same lerp snapped to the nearest 256-cube index on Palette256, and a
    /// coarse stepped ramp of named colors on Basic (ANSI-16 has no gradient).
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
        }
    }
}

/// The two RGB stops of the progress ramp: started-yellow (0% done) and
/// done-green (100%). Kept in sync in spirit with `Palette::started` /
/// `Palette::done`. The ramp starts at yellow (not gray) so the counter is
/// readable even when nothing is done.
const PROGRESS_YELLOW: (u8, u8, u8) = (0xe5, 0xc0, 0x7b);
const PROGRESS_GREEN: (u8, u8, u8) = (0x98, 0xc3, 0x79);

/// Linear interpolation from started-yellow (0% done) to done-green (100%),
/// returning raw `(r, g, b)`. `ratio` is clamped to `[0, 1]`.
fn progress_rgb_parts(ratio: f32) -> (u8, u8, u8) {
    let ratio = ratio.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8, t: f32| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    (
        lerp(PROGRESS_YELLOW.0, PROGRESS_GREEN.0, ratio),
        lerp(PROGRESS_YELLOW.1, PROGRESS_GREEN.1, ratio),
        lerp(PROGRESS_YELLOW.2, PROGRESS_GREEN.2, ratio),
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

pub struct AppState {
    pub document: Document,
    pub current_list_index: usize,
    pub current_item_index: usize,
    pub screen: Screen,
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
    pub should_quit: bool,
    /// Last known modification time of `document.file_path`, used to detect
    /// external edits. Updated on load and after every write we make
    /// ourselves, so our own writes are never mistaken for external changes.
    pub file_mtime: Option<SystemTime>,
    /// Last known size of `document.file_path`, cross-checked alongside
    /// `file_mtime` (updated in lockstep with it) so a same-instant external
    /// edit that also changes the file's length isn't missed on a
    /// coarse-mtime filesystem (whole-second resolution is common) — a
    /// narrower coincidence than mtime alone, though not a complete fix.
    pub file_size: Option<u64>,
    /// Hash of the file content we last confirmed was on disk — set at load
    /// and after every write we make or reload we pick up. `commit_write`
    /// re-hashes the file immediately before writing and compares against
    /// this: a mismatch means something else (another markcheck instance, an
    /// external editor) changed the file since we last saw it, so the write
    /// is refused and the file reloaded instead of blindly overwritten —
    /// closing the lost-update window that `file_mtime`/`file_size` alone
    /// can miss on a coarse-mtime filesystem.
    pub file_content_hash: Option<u64>,
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
    /// Maximum useful scroll for the current card; written back by the
    /// cards renderer each frame (0 = body fits, no overflow).
    pub card_max_scroll: u16,
    /// Visible height (in rows) of the current card's body viewport; written
    /// back by the cards renderer each frame. Drives half-page (`Ctrl-D`/`U`)
    /// and page (`PageUp`/`Down`) scroll distances. 0 until first render.
    pub card_viewport_height: u16,
    /// The current card's on-screen area, written back by the cards renderer
    /// each frame; used to hit-test left-clicks for click-to-copy.
    /// `None` until the first checklist render.
    pub card_rect: Option<Rect>,
    /// Per-row copyable code regions on the current card, written back by the
    /// cards renderer each frame: `(on-screen row rect, clean code text)`.
    /// A left-click on a row copies that exact fragment.
    pub code_regions: Vec<(Rect, String)>,
    /// Per-row click targets in the overview panel, written back by the
    /// overview renderer each frame: `(on-screen row rect, target)`. A
    /// left-click on a row jumps the cursor there. Cleared when the
    /// overview isn't shown (narrow terminals) so stale rects can't be hit.
    pub overview_rows: Vec<(Rect, OverviewTarget)>,
    /// True after a lone `g`, waiting for the second `g` of the `gg` motion
    /// (go to the first task). Reset by any other key.
    pub pending_g: bool,
    /// True after `o` on a card with several links, waiting for the link
    /// number to open (`o` then a digit). Reset by any other key.
    pub pending_open_link: bool,
    pub icons: IconSet,
    /// Set by `e`; consumed by the main loop, which suspends the TUI and
    /// launches the editor.
    pub editor_requested: bool,
    /// Screen to return to if the reset confirmation is cancelled.
    pub screen_before_confirm: Screen,
    /// Also copy to the X11 PRIMARY selection (`--primary`).
    pub clipboard_primary: bool,
    /// Auto-copy an item's code when navigating to its card (`--auto-copy`);
    /// announces a successful copy, silent otherwise.
    pub auto_copy: bool,
    /// Semantic UI colors, resolved to the terminal's capability.
    pub palette: Palette,
    /// Set when the file is confirmed deleted (not just transiently unreadable).
    /// Blocks all write operations until the file reappears.
    pub file_deleted: bool,
    /// Incremental `/` search state — see [`SearchState`].
    pub search: SearchState,
    /// `T` go-to-task overlay state — see [`PickerState`].
    pub picker: PickerState,
    /// Set by `o` to the current card's sole link URL; consumed by the main
    /// loop, which spawns the browser/opener. Mirrors `editor_requested`.
    pub link_open_request: Option<String>,
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

/// `T` go-to-task overlay state: a filterable list of every task.
#[derive(Debug, Default)]
pub struct PickerState {
    /// The live filter query typed while the `ListPicker` overlay is open.
    pub query: String,
    /// Index of the highlighted row within the picker's current (filtered)
    /// entries; reset to 0 whenever the filter changes.
    pub selection: usize,
    /// Visible row count of the picker's list viewport, written back by
    /// `render_list_picker` each frame; drives the half-page `Ctrl-D`/`Ctrl-U`
    /// selection jumps. 0 until the overlay is first rendered.
    pub viewport_height: u16,
}

/// `?` help overlay scroll state.
#[derive(Debug, Default)]
pub struct HelpState {
    /// Scroll offset (in lines) of the overlay when it's taller than the
    /// terminal. Reset to 0 each time help opens.
    pub scroll: u16,
    /// Maximum useful scroll and the viewport height, written back by
    /// `render_help` each frame; drive the help scroll-key clamping.
    pub max_scroll: u16,
    pub viewport_height: u16,
}

/// Background git-sync state (`--git-sync`).
#[derive(Debug, Default)]
pub struct GitSyncState {
    /// Set once at startup (`--git-sync`/`git_sync_paths` requested it *and*
    /// `GitSync::detect` confirmed the file is in a git work tree). Never
    /// changes after that — unlike `last_at`, this isn't about a particular
    /// sync's timing, just whether the feature is active this session at
    /// all. Drives the persistent git icon in the status bar
    /// (`ui/statusbar.rs`), which is otherwise fully hidden.
    pub active: bool,
    /// When a background git-sync last committed *and pushed*
    /// successfully. Drives the title-bar "Synced … ago" tag, the same way
    /// `AppState.last_update_at` drives the "Updated … ago" tag. Not set for
    /// a skipped sync (nothing to commit) or a failed one (that goes to
    /// `set_error` instead).
    pub last_at: Option<SystemTime>,
    /// Set after a successful write-back (or a markcheck-driven external
    /// edit), describing what changed and capturing the exact file content
    /// expected as a result; consumed by the main loop via
    /// `take_git_sync_request`, which forwards it to `GitSync::request` when
    /// git-sync is active for this file. Mirrors `editor_requested`/
    /// `link_open_request` — `app.rs` stays free of process/thread concerns.
    pub pending: Option<PendingSync>,
}

/// A queued git-sync request: the exact file content expected once the
/// underlying change lands, paired with a human description for the
/// eventual commit message (e.g. `Check "Restart service"`). Carrying the
/// expected content — not just the description — lets the sync worker build
/// its commit directly from this snapshot via git plumbing instead of
/// re-reading the live working-tree file, so a concurrent, unrelated write
/// landing on disk mid-sync can't get silently swept into a commit whose
/// message doesn't describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSync {
    pub content: String,
    pub description: String,
}

/// A full snapshot of every checkbox item's state, keyed by 1-based
/// `line_number` (the item identity used for write-back). Undo/redo restore a
/// snapshot by matching line numbers; a line no longer present (e.g. removed by
/// an external edit before the history was cleared) is skipped.
pub type StateSnapshot = Vec<(LineNumber, TaskState)>;

/// Maximum number of undo entries kept in memory. Snapshots are a few
/// bytes per task, so this is generous; it only bounds pathological sessions.
pub const UNDO_HISTORY_CAP: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    fn checkbox(line_number: LineNumber, completed: bool) -> Item {
        Item {
            line_number,
            depth: 0,
            section: vec![],
            display_text: "task".to_string(),
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

    fn display_only(line_number: LineNumber) -> Item {
        Item {
            line_number,
            depth: 0,
            section: vec![],
            display_text: "heading text".to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind: ItemKind::DisplayOnly,
        }
    }

    #[test]
    fn search_text_combines_header_body_and_code_blocks() {
        // search_text folds in the title, the body (display_text),
        // and fenced command blocks — the last aren't part of display_text.
        let item = Item {
            line_number: 1,
            depth: 0,
            section: vec![],
            display_text: "restart the service".to_string(),
            body: vec![],
            header: Some("Reboot".to_string()),
            code_spans: vec![],
            code_blocks: vec!["systemctl restart svc".to_string()],
            kind: ItemKind::Checkbox(TaskState::NotStarted),
        };
        let text = item.search_text();
        assert!(text.contains("Reboot"), "includes the header");
        assert!(text.contains("restart the service"), "includes the body");
        assert!(
            text.contains("systemctl restart svc"),
            "includes code block"
        );
    }

    #[test]
    fn list_holds_mixed_item_kinds() {
        let list = List {
            title: "List A".to_string(),
            banner: None,
            items: vec![display_only(1), checkbox(2, false), checkbox(3, true)],
        };

        assert_eq!(list.items.len(), 3);
        assert_eq!(list.items[0].kind, ItemKind::DisplayOnly);
        assert_eq!(
            list.items[1].kind,
            ItemKind::Checkbox(TaskState::NotStarted)
        );
        assert_eq!(list.items[2].kind, ItemKind::Checkbox(TaskState::Done));
    }

    fn item_at(line_number: LineNumber, depth: usize, kind: ItemKind) -> Item {
        Item {
            line_number,
            depth,
            section: vec![],
            display_text: "item".to_string(),
            body: vec![],
            header: None,
            code_spans: vec![],
            code_blocks: vec![],
            kind,
        }
    }

    // An info parent (depth 0) with a two-item checkbox sub-list (depth 1),
    // plus a following top-level item that must *not* be treated as a child.
    fn info_parent_list(child_a: TaskState, child_b: TaskState) -> List {
        List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                item_at(1, 0, ItemKind::DisplayOnly),
                item_at(2, 1, ItemKind::Checkbox(child_a)),
                item_at(3, 1, ItemKind::Checkbox(child_b)),
                item_at(4, 0, ItemKind::Checkbox(TaskState::NotStarted)),
            ],
        }
    }

    #[test]
    fn descendants_are_the_deeper_run_after_an_item() {
        let list = info_parent_list(TaskState::NotStarted, TaskState::NotStarted);
        // Items 1 and 2 are the parent's sub-list; item 3 (depth 0) is not.
        assert_eq!(list.descendants(0), vec![1, 2]);
        // A leaf checkbox has no descendants; the trailing top-level item too.
        assert_eq!(list.descendants(1), Vec::<usize>::new());
        assert_eq!(list.descendants(3), Vec::<usize>::new());
    }

    fn item_with_section(line: LineNumber, section: Vec<SubHeading>) -> Item {
        let mut item = checkbox(line, false);
        item.section = section;
        item
    }

    fn h(level: u8, text: &str) -> SubHeading {
        SubHeading {
            level,
            text: text.to_string(),
        }
    }

    #[test]
    fn sub_section_starts_are_the_divider_positions() {
        // A start is any item introducing a heading beyond its common
        // prefix with the previous item — the same positions the overview draws
        // a divider. A shallower path that's an ancestor of the previous one is
        // not a new start.
        let list = List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                item_with_section(1, vec![]),          // 0: under H2, no divider
                item_with_section(2, vec![h(3, "A")]), // 1: starts A
                item_with_section(3, vec![h(3, "A")]), // 2: same group
                item_with_section(4, vec![h(3, "A"), h(4, "B")]), // 3: starts B under A
                item_with_section(5, vec![h(3, "A")]), // 4: back to A, no new divider
                item_with_section(6, vec![h(3, "C")]), // 5: starts C
            ],
        };
        assert_eq!(list.sub_section_starts(), vec![1, 3, 5]);

        // A list with no sub-sections has no starts.
        let flat = List {
            title: "L".to_string(),
            banner: None,
            items: vec![checkbox(1, false), checkbox(2, false)],
        };
        assert!(flat.sub_section_starts().is_empty());
    }

    #[test]
    fn sublist_slot_numbers_each_sublist_distinctly() {
        // Every parent (item with children) gets its own ordinal, so
        // distinct sub-lists — even at the same depth — get distinct colors.
        let list = List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                item_at(1, 0, ItemKind::Checkbox(TaskState::NotStarted)), // 0: parent A
                item_at(2, 1, ItemKind::Checkbox(TaskState::NotStarted)), // 1: parent (has child)
                item_at(3, 2, ItemKind::Checkbox(TaskState::NotStarted)), // 2: leaf
                item_at(4, 0, ItemKind::Checkbox(TaskState::NotStarted)), // 3: parent B
                item_at(5, 1, ItemKind::Checkbox(TaskState::NotStarted)), // 4: leaf
            ],
        };
        assert_eq!(list.sublist_slot(0), 0, "first parent → slot 0");
        assert_eq!(list.sublist_slot(1), 1, "nested parent → slot 1");
        assert_eq!(list.sublist_slot(3), 2, "second top-level parent → slot 2");
    }

    #[test]
    fn info_parent_state_is_done_only_when_all_children_done() {
        let all_done = info_parent_list(TaskState::Done, TaskState::Done);
        assert_eq!(all_done.info_parent_state(0), Some(TaskState::Done));

        let one_left = info_parent_list(TaskState::Done, TaskState::NotStarted);
        assert_eq!(one_left.info_parent_state(0), Some(TaskState::Started));
    }

    #[test]
    fn info_parent_state_is_started_when_any_child_active() {
        // A single started child is enough — no child-count rule.
        let started = info_parent_list(TaskState::Started, TaskState::NotStarted);
        assert_eq!(started.info_parent_state(0), Some(TaskState::Started));

        // A done child among not-started ones is also "under way".
        let one_done = info_parent_list(TaskState::Done, TaskState::NotStarted);
        assert_eq!(one_done.info_parent_state(0), Some(TaskState::Started));
    }

    #[test]
    fn info_parent_state_is_none_when_nothing_begun_or_no_children() {
        let untouched = info_parent_list(TaskState::NotStarted, TaskState::NotStarted);
        assert_eq!(untouched.info_parent_state(0), None);

        // An info item whose only descendant is another info item — no
        // checkbox children — has no aggregate state.
        let no_tasks = List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                item_at(1, 0, ItemKind::DisplayOnly),
                item_at(2, 1, ItemKind::DisplayOnly),
            ],
        };
        assert_eq!(no_tasks.info_parent_state(0), None);

        // A checkbox item never reports an aggregate state; it carries its own.
        let with_child = info_parent_list(TaskState::Done, TaskState::Done);
        assert_eq!(with_child.info_parent_state(1), None);
    }

    #[test]
    fn info_parent_state_counts_deeper_descendants_too() {
        // A grandchild (depth 2) still counts toward the parent (depth 0).
        let list = List {
            title: "L".to_string(),
            banner: None,
            items: vec![
                item_at(1, 0, ItemKind::DisplayOnly),
                item_at(2, 1, ItemKind::Checkbox(TaskState::Done)),
                item_at(3, 2, ItemKind::Checkbox(TaskState::NotStarted)),
            ],
        };
        assert_eq!(list.info_parent_state(0), Some(TaskState::Started));
    }

    #[test]
    fn duplicate_content_items_are_distinct_by_line_number() {
        let items = [checkbox(2, false), checkbox(3, false), checkbox(4, false)];
        let line_numbers: Vec<_> = items.iter().map(|i| i.line_number).collect();
        assert_eq!(line_numbers, vec![2, 3, 4]);
    }

    #[test]
    fn list_checkbox_stats_excludes_display_only_items() {
        let list = List {
            title: "List".to_string(),
            banner: None,
            items: vec![
                display_only(1),
                checkbox(2, true),
                checkbox(3, false),
                checkbox(4, true),
            ],
        };
        assert_eq!(list.checkbox_stats(), (2, 3));
    }

    #[test]
    fn started_tasks_count_as_not_done_in_stats() {
        let mut list = List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
        };
        list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
        // 3 checkboxes total, only the Done one counts as done.
        assert_eq!(list.checkbox_stats(), (1, 3));
    }

    #[test]
    fn checkbox_progress_counts_done_started_total() {
        // The progress bar needs the started count too.
        let mut list = List {
            title: "S".to_string(),
            banner: None,
            items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
        };
        list.items[1].kind = ItemKind::Checkbox(TaskState::Started);
        assert_eq!(list.checkbox_progress(), (1, 1, 3));
    }

    #[test]
    fn depth_from_env_prefers_truecolor_then_256_then_basic() {
        assert_eq!(
            depth_from_env(Some("truecolor"), Some("xterm-256color")),
            ColorDepth::TrueColor
        );
        assert_eq!(depth_from_env(Some("24bit"), None), ColorDepth::TrueColor);
        assert_eq!(
            depth_from_env(None, Some("xterm-256color")),
            ColorDepth::Palette256
        );
        assert_eq!(depth_from_env(None, Some("xterm")), ColorDepth::Basic);
        assert_eq!(depth_from_env(None, None), ColorDepth::Basic);
    }

    #[test]
    fn palette_for_depth_uses_the_matching_color_kind() {
        assert!(matches!(
            Palette::for_depth(ColorDepth::TrueColor).code_bg,
            Color::Rgb(..)
        ));
        assert!(matches!(
            Palette::for_depth(ColorDepth::Palette256).code_bg,
            Color::Indexed(_)
        ));
        assert_eq!(
            Palette::for_depth(ColorDepth::Basic).code_bg,
            Color::DarkGray,
            "basic fallback: a subtle gray code background"
        );
        // The error role (missing-title placeholder) resolves per depth.
        assert!(matches!(
            Palette::for_depth(ColorDepth::TrueColor).error,
            Color::Rgb(..)
        ));
        assert!(matches!(
            Palette::for_depth(ColorDepth::Palette256).error,
            Color::Indexed(_)
        ));
        assert_eq!(Palette::for_depth(ColorDepth::Basic).error, Color::Red);
        // The warning role (list banner) resolves per depth.
        assert!(matches!(
            Palette::for_depth(ColorDepth::TrueColor).warning,
            Color::Rgb(..)
        ));
        assert!(matches!(
            Palette::for_depth(ColorDepth::Palette256).warning,
            Color::Indexed(_)
        ));
        assert_eq!(Palette::for_depth(ColorDepth::Basic).warning, Color::Yellow);
    }

    #[test]
    fn for_depth_records_the_depth() {
        assert_eq!(
            Palette::for_depth(ColorDepth::TrueColor).depth,
            ColorDepth::TrueColor
        );
        assert_eq!(
            Palette::for_depth(ColorDepth::Palette256).depth,
            ColorDepth::Palette256
        );
        assert_eq!(
            Palette::for_depth(ColorDepth::Basic).depth,
            ColorDepth::Basic
        );
    }

    #[test]
    fn progress_rgb_hits_the_two_stops() {
        // Yellow at 0% done, green at 100%.
        assert_eq!(progress_rgb_parts(0.0), PROGRESS_YELLOW);
        assert_eq!(progress_rgb_parts(1.0), PROGRESS_GREEN);
    }

    #[test]
    fn progress_rgb_is_monotonic_between_stops() {
        // Half-way is strictly between yellow and green on the channels that move
        // in one direction (r falls and b falls yellow→green).
        let (r, _g, b) = progress_rgb_parts(0.5);
        assert!(r < PROGRESS_YELLOW.0 && r > PROGRESS_GREEN.0);
        assert!(b < PROGRESS_YELLOW.2 && b > PROGRESS_GREEN.2);
    }

    #[test]
    fn progress_rgb_clamps_out_of_range_ratios() {
        assert_eq!(progress_rgb_parts(-1.0), PROGRESS_YELLOW);
        assert_eq!(progress_rgb_parts(2.0), PROGRESS_GREEN);
    }

    #[test]
    fn truecolor_progress_color_fades_with_completion() {
        let p = Palette::for_depth(ColorDepth::TrueColor);
        let (yr, yg, yb) = PROGRESS_YELLOW;
        let (er, eg, eb) = PROGRESS_GREEN;
        // Starts at yellow (not gray), ends at green.
        assert_eq!(p.progress_color(0, 4).fg, Some(Color::Rgb(yr, yg, yb)));
        assert_eq!(p.progress_color(4, 4).fg, Some(Color::Rgb(er, eg, eb)));
        // Half done is a blend strictly between the yellow and green stops.
        let (mr, mg, mb) = progress_rgb_parts(0.5);
        assert_eq!(p.progress_color(2, 4).fg, Some(Color::Rgb(mr, mg, mb)));
        assert_ne!((mr, mg, mb), PROGRESS_YELLOW);
        assert_ne!((mr, mg, mb), PROGRESS_GREEN);
    }

    #[test]
    fn empty_document_counter_reads_complete() {
        // total == 0 is treated as fully done (green), matching the old
        // done == total semantics rather than the 0%-done yellow.
        let p = Palette::for_depth(ColorDepth::TrueColor);
        let (er, eg, eb) = PROGRESS_GREEN;
        assert_eq!(p.progress_color(0, 0).fg, Some(Color::Rgb(er, eg, eb)));
    }

    #[test]
    fn basic_progress_color_steps_through_named_colors() {
        let p = Palette::for_depth(ColorDepth::Basic);
        assert_eq!(p.progress_color(0, 4).fg, Some(Color::Yellow)); // was DarkGray
        assert_eq!(p.progress_color(1, 4).fg, Some(Color::Yellow));
        assert_eq!(p.progress_color(2, 4).fg, Some(Color::Green));
        let full = p.progress_color(4, 4);
        assert_eq!(full.fg, Some(Color::Green));
        assert!(full.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn palette256_progress_color_uses_indexed_colors() {
        let p = Palette::for_depth(ColorDepth::Palette256);
        for (done, total) in [(0, 4), (2, 4), (4, 4)] {
            assert!(matches!(
                p.progress_color(done, total).fg,
                Some(Color::Indexed(_))
            ));
        }
    }

    #[test]
    fn rgb_to_256_maps_primaries_and_extremes() {
        assert_eq!(rgb_to_256(0, 0, 0), 16); // cube origin (black)
        assert_eq!(rgb_to_256(255, 255, 255), 231); // cube far corner (white)
        assert_eq!(rgb_to_256(255, 0, 0), 196); // pure red cube cell
        // A near-gray triple snaps into the 232–255 grayscale ramp.
        assert!(rgb_to_256(0x80, 0x80, 0x80) >= 232);
    }

    #[test]
    fn document_checkbox_stats_sums_across_lists() {
        let document = Document {
            file_path: PathBuf::from("test.md"),
            title: None,
            has_default_list: false,
            uses_crlf: false,
            trailing_newline: true,
            lists: vec![
                List {
                    title: "A".to_string(),
                    banner: None,
                    items: vec![checkbox(1, true), checkbox(2, false)],
                },
                List {
                    title: "B".to_string(),
                    banner: None,
                    items: vec![display_only(3), checkbox(4, true)],
                },
            ],
            raw_lines: vec![],
        };
        assert_eq!(document.checkbox_stats(), (2, 3));
    }
}
