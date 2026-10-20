use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::clipboard::{self, CopyOutcome};
use crate::effect::{CopyKind, Effect};
use crate::git_sync::{self, SyncOutcome};
use crate::model::{
    AppState, CopyRoute, CriticalAction, CriticalArm, Document, FileGone, GitSyncState, HelpState,
    IconSet, Item, ItemKind, List, Overlay, OverviewTarget, Palette, PendingSync, PickerState,
    ResetScope, SearchState, StateSnapshot, TaskState, UNDO_HISTORY_CAP, View, ViewLayout,
    hash_bytes,
};
use crate::parser;
use crate::writer;

/// How long an ephemeral status message stays before it auto-clears.
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

/// An `e` editor that exits successfully sooner than this, leaving the file
/// unchanged, is taken to have handed off to a window instead of waiting.
const EDITOR_RETURNED_AT_ONCE: Duration = Duration::from_secs(1);

const READ_ONLY_MESSAGE: &str = "File is read-only — change not saved (chmod u+w to allow saving)";
/// The quiet spell a critical step's confirming press must follow, so key
/// repeat from a held key, or a reflex double press, can't confirm it (#219).
const CRITICAL_PAUSE: Duration = Duration::from_millis(500);
const IN_PLACE_MESSAGE: &str = "Saved in place: this folder doesn't let markcheck create a temp file, \
     so a crash mid-save could truncate the checklist";
/// What Esc and Ctrl-C say on a screen with nothing to cancel.
const QUIT_HINT: &str = "Type q to quit";
const HIDDEN_CHARACTERS_MESSAGE: &str = "Not copied: command contains hidden control characters";

/// Whether a cursor move lands on a card the user navigated to, so
/// `--auto-copy` fires, or is a side effect (undo, cancelling a search)
/// that must not touch the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMove {
    Navigate,
    Quiet,
}

/// What one key did at an armed `o`/`y` number prompt.
enum NumberKey {
    Typing,
    Picked(usize),
    Cancelled,
    Other,
}

/// The modification time and byte length of `path` from a single `stat()`
/// call, or `None` if it can't be read. Cross-checking both narrows (but
/// doesn't eliminate) the window where a same-instant external edit could be
/// mistaken for a no-op on a coarse-mtime filesystem.
fn current_stat(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::symlink_metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Hash of `path`'s current on-disk bytes, or `None` if it can't be read.
/// See `model::hash_bytes`'s doc comment for what this check is (and isn't)
/// used for — a pre-write content check, not an atomic compare-and-swap.
fn current_content_hash(path: &Path) -> Option<[u8; 32]> {
    writer::read_checklist(path)
        .ok()
        .map(|bytes| hash_bytes(&bytes))
}

/// A program's file name without its directory, falling back to the string
/// as given.
fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

/// Smart-case containment (vim `smartcase`): case-insensitive unless the
/// query contains an uppercase letter, in which case the match is exact. An
/// empty query never matches.
fn smart_case_contains(haystack: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    if query.chars().any(char::is_uppercase) {
        haystack.contains(query)
    } else {
        haystack.to_lowercase().contains(query)
    }
}

/// Match source for the `T` go-to-task picker filter: an item's regular
/// [`Item::search_text`] plus its `### H3`+ sub-section path, so typing a
/// sub-section name filters to the tasks under it. The section text is folded
/// in here — not in `Item::search_text` — so only the picker sees it and the
/// main `/` search stays unchanged. An ordered item's step number is folded in
/// the same way, so `3.` finds step 3.
fn picker_match_text(item: &Item) -> String {
    let mut text = item.search_text();
    if let Some(ordinal) = &item.ordinal {
        text.push(' ');
        text.push_str(ordinal);
    }
    for heading in &item.section {
        text.push(' ');
        text.push_str(&heading.text);
    }
    text
}

/// Schemes `o` will hand to the opener. The allowlist doubles as
/// argument-injection cover: a URL starting with one of these can't start with
/// `-`, so the opener can never read it as an option. Matching is
/// case-insensitive — schemes are case-insensitive per RFC 3986.
const SAFE_LINK_SCHEMES: [&str; 3] = ["http://", "https://", "mailto:"];

fn is_safe_link(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    SAFE_LINK_SCHEMES
        .iter()
        .any(|scheme| lowered.starts_with(scheme))
}

/// First not-done checkbox in the list, or `None` if the list has
/// no incomplete tasks. A `Started` task still counts as not-done.
fn first_undone_index(list: &List) -> Option<usize> {
    list.items.iter().position(|i| {
        matches!(
            i.kind,
            ItemKind::Checkbox(TaskState::NotStarted | TaskState::Started)
        )
    })
}

impl AppState {
    /// Wraps a parsed [`Document`] for interactive use. `document.lists` must
    /// be non-empty: navigation indexes it without a fallback.
    pub fn new(document: Document) -> Self {
        debug_assert!(
            !document.lists.is_empty(),
            "AppState requires a document with at least one list; the empty case \
             is rejected at load (main.rs) and reload (reload_if_changed)",
        );
        // The stat follows the caller's read, so it may describe a newer revision
        // than the document. If the sizes disagree, drop both so the next tick
        // reloads instead of treating the change as already seen.
        let contents = writer::document_contents(&document);
        let (file_mtime, file_size) = match current_stat(&document.file_path) {
            Some((modified, size)) if size == contents.len() as u64 => (Some(modified), Some(size)),
            _ => (None, None),
        };
        // Hashed from the document, not a second read, so the fingerprint can't
        // describe a different revision than the one in memory.
        let file_content_hash = Some(hash_bytes(contents.as_bytes()));
        // Start on the first not-done item: prefer the first list, then
        // fall back to the first list with remaining work, then item 0
        // (everything already done).
        let (current_list_index, current_item_index) = document
            .lists
            .first()
            .and_then(first_undone_index)
            .map(|i| (0, i))
            .or_else(|| {
                document
                    .lists
                    .iter()
                    .enumerate()
                    .find_map(|(s, list)| first_undone_index(list).map(|i| (s, i)))
            })
            .unwrap_or((0, 0));
        AppState {
            document,
            current_list_index,
            current_item_index,
            view: View::Checklist,
            overlay: None,
            status_message: None,
            status_expiry: None,
            status_is_error: false,
            effects: Vec::new(),
            file_mtime,
            file_size,
            file_content_hash,
            last_update_at: None,
            git_sync: GitSyncState::default(),
            card_scroll: 0,
            layout: ViewLayout::default(),
            pending_g: false,
            pending_open_link: false,
            pending_copy_code: false,
            pending_number: 0,
            icons: IconSet::unicode(),
            clipboard_primary: false,
            jump_back: Vec::new(),
            critical_armed: None,
            copy_route: CopyRoute::default(),
            auto_copy: false,
            // Named ANSI by default (theme-respecting); main.rs upgrades this
            // to the terminal's detected capability at startup.
            palette: Palette::basic(),
            file_gone: None,
            write_notice: None,
            in_place_noted: false,
            search: SearchState::default(),
            picker: PickerState::default(),
            help: HelpState::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            history_epoch: 0,
        }
    }

    /// The list under the cursor. Relies on the ≥1-list invariant documented
    /// on [`AppState::new`]; the `.expect` gives a clear message if it is ever
    /// violated in a release build (where the constructor's `debug_assert` is
    /// compiled out).
    pub fn current_list(&self) -> &List {
        self.document
            .lists
            .get(self.current_list_index)
            .expect("current_list_index within bounds (AppState ≥1-list invariant)")
    }

    /// `None` if the current list has no items at all (e.g. a heading
    /// with only prose underneath it, no checklist).
    pub fn current_item(&self) -> Option<&Item> {
        self.current_list().items.get(self.current_item_index)
    }

    pub fn navigate_next(&mut self) {
        self.focus_item(
            self.current_list_index,
            self.current_item_index + 1,
            CursorMove::Navigate,
        );
    }

    pub fn navigate_prev(&mut self) {
        self.focus_item(
            self.current_list_index,
            self.current_item_index.saturating_sub(1),
            CursorMove::Navigate,
        );
    }

    /// Jump to list `index` (0-based); out of range is reported, since the
    /// `1`-`9` binding can ask for a list that doesn't exist.
    pub fn jump_to_list(&mut self, index: usize) {
        let count = self.document.lists.len();
        if index >= count {
            let plural = if count == 1 { "" } else { "s" };
            self.set_error(format!(
                "No list {}: this document has {count} list{plural}",
                index + 1
            ));
            return;
        }
        // Land on the first not-done item in the target list,
        // falling back to its first item when all are done.
        let item_index = first_undone_index(&self.document.lists[index]).unwrap_or(0);
        self.focus_item(index, item_index, CursorMove::Navigate);
    }

    /// All `(list_index, item_index)` whose text matches `query`, in document
    /// order. Matching is smart-case containment over each item's
    /// [`Item::search_text`]; an empty query yields no matches.
    pub fn find_matches(&self, query: &str) -> Vec<(usize, usize)> {
        let mut matches = Vec::new();
        if query.is_empty() {
            return matches;
        }
        for (list_index, list) in self.document.lists.iter().enumerate() {
            for (item_index, item) in list.items.iter().enumerate() {
                if smart_case_contains(&item.search_text(), query) {
                    matches.push((list_index, item_index));
                }
            }
        }
        matches
    }

    /// Move the cursor to an *exact* item and show it on the `Checklist`
    /// view, resetting card scroll — the one primitive every cursor motion
    /// goes through, so none can forget a step. `CursorMove::Navigate` also
    /// honours `--auto-copy`. Out-of-range indices are clamped/ignored so a
    /// stale match can't panic.
    pub fn focus_item(&mut self, list_index: usize, item_index: usize, how: CursorMove) {
        let Some(list) = self.document.lists.get(list_index) else {
            return;
        };
        if list.items.is_empty() {
            return;
        }
        self.current_list_index = list_index;
        self.current_item_index = item_index.min(list.items.len() - 1);
        self.view = View::Checklist;
        self.reset_card_scroll();
        if how == CursorMove::Navigate {
            self.maybe_auto_copy();
        }
    }

    /// Enter incremental search (`/`): remember the cursor for a possible
    /// `Esc` restore, clear the query, and open the `Search` overlay.
    fn start_search(&mut self) {
        self.search.origin = (self.current_list_index, self.current_item_index);
        self.search.query.clear();
        self.overlay = Some(Overlay::Search);
    }

    /// The match `/` lands on, with its 1-based position: the first match at or
    /// after the search origin, wrapping to the first match overall — vim's
    /// forward search, so a search from mid-document doesn't throw the cursor
    /// back to the top. `None` when nothing matches.
    fn search_target(&self, matches: &[(usize, usize)]) -> Option<(usize, (usize, usize))> {
        let origin = self.search.origin;
        let index = matches.iter().position(|&m| m >= origin).unwrap_or(0);
        matches.get(index).map(|&m| (index + 1, m))
    }

    /// Re-run the live query and jump the cursor to the next match from the
    /// search origin (see [`Self::search_target`]), keeping the `Search`
    /// overlay open (the query keeps showing in the status bar). Like vim, no
    /// match or an empty query returns the cursor to the search origin rather
    /// than leaving it on an earlier prefix's match; no auto-copy mid-typing.
    fn update_search(&mut self) {
        let matches = self.find_matches(&self.search.query);
        let (list_index, item_index) = match self.search_target(&matches) {
            Some((_, target)) => target,
            None => self.search.origin,
        };
        if (list_index, item_index) != (self.current_list_index, self.current_item_index) {
            self.focus_item(list_index, item_index, CursorMove::Quiet);
        }
    }

    /// Commit the search (`Enter`): store the query for `n`/`N`, close the
    /// overlay on the current match, and allow auto-copy now.
    /// Reports the match position, or a sticky "no matches" when the committed
    /// query matched nothing, so a committed search is never silent. Only a
    /// search that found a match auto-copies: a failed or empty one leaves the
    /// cursor on the origin card and the clipboard untouched.
    fn commit_search(&mut self) {
        self.overlay = None;
        // As in vim and less, an empty `/` repeats the last search.
        if self.search.query.is_empty() {
            self.search_cycle(true);
            return;
        }
        let matches = self.find_matches(&self.search.query);
        self.search.last = Some(self.search.query.clone());
        match self.search_target(&matches) {
            None => {
                self.set_error(format!("No matches for \"{}\"", self.search.query));
                return;
            }
            Some(_) if matches.len() == 1 => self.set_status("Match 1/1".to_string()),
            Some((position, _)) => {
                // Nudge `n`/`N` here: they cycle matches once the search
                // prompt is dismissed, but the always-on legend only
                // advertises `/`, so a first-time user wouldn't know to
                // reach for them.
                self.set_status(format!("Match {position}/{} · n/N to cycle", matches.len()));
            }
        }
        // A successful auto-copy (if enabled) overrides with its own message.
        self.maybe_auto_copy();
    }

    /// Cancel the search (`Esc`): close the prompt and restore the pre-search
    /// cursor.
    fn cancel_search(&mut self) {
        self.overlay = None;
        let (list_index, item_index) = self.search.origin;
        self.focus_item(list_index, item_index, CursorMove::Quiet);
    }

    /// Cycle to the next (`forward`) or previous match of the last committed
    /// query relative to the cursor, wrapping around (and saying so, like vim's
    /// "search hit BOTTOM"); a no-op with no active
    /// search or no matches (`n`/`N`). Matches from `find_matches` are in
    /// document order, so `(list, item)` tuples compare in that order.
    fn search_cycle(&mut self, forward: bool) {
        let Some(query) = self.search.last.clone() else {
            self.set_error("No previous search — press / to search".to_string());
            return;
        };
        let matches = self.find_matches(&query);
        let Some(&first) = matches.first() else {
            // The query no longer matches anything (e.g. the file changed under
            // us) — say so rather than move nowhere silently.
            self.set_error(format!("No matches for \"{query}\""));
            return;
        };
        let cur = (self.current_list_index, self.current_item_index);
        let next = if forward {
            matches.iter().copied().find(|&m| m > cur)
        } else {
            matches.iter().rev().copied().find(|&m| m < cur)
        };
        let wrapped = next.is_none();
        let target = next.unwrap_or_else(|| {
            if forward {
                first
            } else {
                *matches
                    .last()
                    .expect("matches is non-empty — checked above")
            }
        });
        let position = matches
            .iter()
            .position(|&m| m == target)
            .map_or(0, |i| i + 1);
        let mut msg = format!("Match {position}/{}", matches.len());
        if wrapped {
            msg.push_str(if forward {
                " · wrapped to top"
            } else {
                " · wrapped to bottom"
            });
        }
        self.focus_and_report(target, Some(msg));
    }

    /// Navigate to `target` and report `msg` (if any) as an ephemeral status.
    /// The auto-copy outcome lands after it, so stays visible too (see
    /// `apply_copy_outcome`).
    fn focus_and_report(&mut self, (list_index, item_index): (usize, usize), msg: Option<String>) {
        self.clear_status();
        self.focus_item(list_index, item_index, CursorMove::Navigate);
        if let Some(msg) = msg {
            self.set_status(msg);
        }
    }

    /// Jump the cursor to the first item of the next (`forward`) / previous
    /// `### H3`+ sub-section within the current list (`}`/`{`). Gives
    /// feedback and never moves silently: an error when the list has no
    /// sub-sections, and an "already at the first/last" notice at the ends
    /// rather than wrapping.
    fn jump_sub_section(&mut self, forward: bool) {
        let list_index = self.current_list_index;
        let starts = self.document.lists[list_index].sub_section_starts();
        if starts.is_empty() {
            self.set_error("No sub-sections in this list".to_string());
            return;
        }
        let cur = self.current_item_index;
        let target = if forward {
            starts.iter().copied().find(|&i| i > cur)
        } else {
            starts.iter().rev().copied().find(|&i| i < cur)
        };
        match target {
            Some(i) => {
                let name = self.document.lists[list_index].items[i]
                    .section
                    .last()
                    .map_or_else(String::new, |h| h.text.clone());
                self.set_status(format!("Sub-section: {name}"));
                // A successful auto-copy (if enabled) overrides with its message.
                self.focus_item(list_index, i, CursorMove::Navigate);
            }
            None => {
                let edge = if forward { "last" } else { "first" };
                self.set_error(format!("Already at the {edge} sub-section"));
            }
        }
    }

    /// Open the "go to task" overlay (`T`): clear the filter and reset the
    /// selection.
    fn open_list_picker(&mut self) {
        self.picker.query.clear();
        self.picker.selection = 0;
        self.layout.picker_rows_area = None;
        self.overlay = Some(Overlay::ListPicker);
    }

    /// The picker's current entries: every task when the filter is empty (the
    /// full table of contents), otherwise the matches for the filter.
    /// Matching is the same smart-case containment as `/` search, but over
    /// [`picker_match_text`] — the item's search text plus its `### H3`+
    /// sub-section path — so the filter can find "the task under Pre-flight
    /// checks". The section text is deliberately kept out of the shared
    /// `Item::search_text`, so the main `/` search is unaffected.
    pub fn picker_matches(&self) -> Vec<(usize, usize)> {
        if self.picker.query.is_empty() {
            return self
                .document
                .lists
                .iter()
                .enumerate()
                .flat_map(|(li, list)| (0..list.items.len()).map(move |ii| (li, ii)))
                .collect();
        }
        let mut matches = Vec::new();
        for (list_index, list) in self.document.lists.iter().enumerate() {
            for (item_index, item) in list.items.iter().enumerate() {
                if smart_case_contains(&picker_match_text(item), &self.picker.query) {
                    matches.push((list_index, item_index));
                }
            }
        }
        matches
    }

    /// Half the picker's visible list height (min 1), for the `Ctrl-D`/`Ctrl-U`
    /// half-page jumps — mirrors the card-body `half_page`.
    fn picker_half_page(&self) -> isize {
        (self.layout.picker_viewport_height / 2).max(1) as isize
    }

    /// Move the picker selection by `delta`, clamped to the current entries
    /// (no wrap); a no-op when there are none.
    fn picker_move(&mut self, delta: isize) {
        let count = self.picker_matches().len();
        if count == 0 {
            return;
        }
        let current = self.picker.selection.min(count - 1) as isize;
        self.picker.selection = (current + delta).clamp(0, count as isize - 1) as usize;
    }

    /// Left-click in the picker: a click on an entry row highlights it, and a
    /// click on the already-highlighted row jumps there like `Enter`. Clicks
    /// off the entry rows are ignored.
    fn picker_click(&mut self, col: u16, row: u16) {
        let Some(area) = self.layout.picker_rows_area else {
            return;
        };
        if !area.contains((col, row).into()) {
            return;
        }
        let index = self.layout.picker_scroll_offset + usize::from(row - area.y);
        let count = self.picker_matches().len();
        if index >= count {
            return;
        }
        if index == self.picker.selection.min(count - 1) {
            self.picker_commit();
        } else {
            self.picker.selection = index;
        }
    }

    /// Jump to the highlighted task (`Enter`), or just close the overlay when
    /// the filter matched nothing.
    fn picker_commit(&mut self) {
        self.overlay = None;
        if let Some(&(list_index, item_index)) = self.picker_matches().get(self.picker.selection) {
            self.focus_item(list_index, item_index, CursorMove::Navigate);
        }
    }

    /// Scroll the `?` help overlay down/up by `n` lines, clamped.
    fn scroll_help_down_by(&mut self, n: u16) {
        self.help.scroll = self
            .help
            .scroll
            .saturating_add(n)
            .min(self.layout.help_max_scroll);
    }
    fn scroll_help_up_by(&mut self, n: u16) {
        self.help.scroll = self.help.scroll.saturating_sub(n);
    }
    /// Half / near-full help viewport, for `Ctrl-D`/`Ctrl-U` and `PageDown`/`Up`
    /// — mirrors the card-body `half_page`/`page`.
    fn help_half_page(&self) -> u16 {
        (self.layout.help_viewport_height / 2).max(1)
    }
    fn help_page(&self) -> u16 {
        self.layout.help_viewport_height.saturating_sub(1).max(1)
    }

    /// The first list after `from` that still has incomplete work, if any.
    fn next_incomplete_list_after(&self, from: usize) -> Option<usize> {
        (from + 1..self.document.lists.len())
            .find(|&i| first_undone_index(&self.document.lists[i]).is_some())
    }

    /// `Shift-L`: jump to the next incomplete list from anywhere, landing on
    /// its first not-done item; reports a sticky message rather than moving
    /// silently when nothing incomplete follows.
    pub fn jump_to_next_incomplete_list(&mut self) {
        match self.next_incomplete_list_after(self.current_list_index) {
            Some(index) => self.jump_to_list(index),
            None => self.set_error(self.no_further_incomplete_list_message(true)),
        }
    }

    /// The nearest list before `from` that still has incomplete work.
    fn prev_incomplete_list_before(&self, from: usize) -> Option<usize> {
        (0..from)
            .rev()
            .find(|&i| first_undone_index(&self.document.lists[i]).is_some())
    }

    /// `Shift-H`: jump to the previous incomplete list, landing on its first
    /// not-done item; reports a sticky message rather than moving silently
    /// when nothing incomplete precedes.
    pub fn jump_to_prev_incomplete_list(&mut self) {
        match self.prev_incomplete_list_before(self.current_list_index) {
            Some(index) => self.jump_to_list(index),
            None => self.set_error(self.no_further_incomplete_list_message(false)),
        }
    }

    /// Message for `jump_to_{next,prev}_incomplete_list`'s no-op path,
    /// distinguishing "no list in the document has unfinished tasks" from
    /// "already at the last/first incomplete list" — the same
    /// none-at-all vs. at-an-edge distinction `jump_sub_section` makes.
    fn no_further_incomplete_list_message(&self, forward: bool) -> String {
        let any_incomplete = self
            .document
            .lists
            .iter()
            .any(|list| first_undone_index(list).is_some());
        if any_incomplete {
            let edge = if forward { "last" } else { "first" };
            format!("Already at the {edge} incomplete list")
        } else {
            "No lists have unfinished tasks".to_string()
        }
    }

    /// `Tab`: jump to the next unfinished task (`NotStarted` or `Started`)
    /// anywhere in the document, after the cursor, wrapping around — and
    /// saying so, since a wrap jumps backwards across the document; reports a
    /// sticky message rather than moving silently when none exist.
    /// Complements `Shift-L` (next unfinished *list*).
    pub fn jump_to_next_incomplete_task(&mut self) {
        let order: Vec<(usize, usize)> = self
            .document
            .lists
            .iter()
            .enumerate()
            .flat_map(|(s, list)| (0..list.items.len()).map(move |i| (s, i)))
            .collect();
        if order.is_empty() {
            self.set_error("No unfinished tasks".to_string());
            return;
        }
        let cur = order
            .iter()
            .position(|&(s, i)| s == self.current_list_index && i == self.current_item_index)
            .unwrap_or(0);
        let n = order.len();
        for step in 1..=n {
            let (s, i) = order[(cur + step) % n];
            if matches!(
                self.document.lists[s].items[i].kind,
                ItemKind::Checkbox(TaskState::NotStarted | TaskState::Started)
            ) {
                let wrapped = cur + step >= n;
                let msg = wrapped.then(|| {
                    format!(
                        "Wrapped to the first unfinished task ({})",
                        self.document.lists[s].title
                    )
                });
                self.focus_and_report((s, i), msg);
                return;
            }
        }
        self.set_error("No unfinished tasks".to_string());
    }

    /// `gg`: jump to the very first task in the document — a literal position,
    /// not first-undone (a raw vim motion).
    pub fn go_to_first_item(&mut self) {
        self.focus_item(0, 0, CursorMove::Navigate);
    }

    /// `G`: jump to the very last task in the document (literal position).
    pub fn go_to_last_item(&mut self) {
        let last = self
            .document
            .lists
            .len()
            .checked_sub(1)
            .expect("AppState ≥1-list invariant");
        self.focus_item(last, usize::MAX, CursorMove::Navigate);
    }

    /// `l`/→: step to the next item, or — at the last item of a list —
    /// move sequentially into the next list (landing on its first
    /// not-done item), so review isn't a dead-end. Clamps at the last
    /// item of the last list.
    pub fn navigate_forward(&mut self) {
        let last_index = self.current_list().items.len().saturating_sub(1);
        if self.current_item_index < last_index {
            self.navigate_next();
        } else if self.current_list_index + 1 < self.document.lists.len() {
            self.jump_to_list(self.current_list_index + 1);
        }
    }

    /// `h`/←: mirror of `navigate_forward` — step to the previous item, or —
    /// at the first item of a list — move into the previous list,
    /// landing on its first not-done item (or its first item when all done),
    /// via `jump_to_list`. Clamps at the first item of the first
    /// list.
    pub fn navigate_backward(&mut self) {
        if self.current_item_index > 0 {
            self.navigate_prev();
        } else if self.current_list_index > 0 {
            self.jump_to_list(self.current_list_index - 1);
        }
    }

    fn reset_card_scroll(&mut self) {
        self.card_scroll = 0;
        self.layout.card_max_scroll = 0; // recomputed by the renderer next frame
    }

    /// Sets an **ephemeral** status message that auto-clears after
    /// `STATUS_TIMEOUT` of idle — copies, reload/reset info, hints.
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = Some(SystemTime::now() + STATUS_TIMEOUT);
        self.status_is_error = false;
    }

    /// Sets a **sticky**, non-error status message for a state that lasts
    /// until something else replaces it, such as waiting on git sync to quit.
    pub fn set_notice(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = None;
        self.status_is_error = false;
    }

    /// Sets a **sticky, error-colored** status message for failures and "your
    /// keypress did nothing" feedback. Persists until the next input.
    pub fn set_error(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = None;
        self.status_is_error = true;
    }

    /// Clears the status message and any pending expiry.
    fn clear_status(&mut self) {
        self.status_message = None;
        self.status_expiry = None;
        self.status_is_error = false;
    }

    /// Cancels any armed input chord (`gg`, `o`/`y`'s digit prompt) and the status
    /// message. Mouse input bypasses `handle_key_with_mods`, so a click between
    /// the two halves of a chord would otherwise leave it armed.
    fn clear_pending_chords(&mut self) {
        self.clear_status();
        self.pending_g = false;
        self.pending_open_link = false;
        self.pending_copy_code = false;
        self.pending_number = 0;
    }

    /// Shared guard for every mutating action: once the watched file has
    /// been confirmed deleted or replaced by a non-regular file
    /// (`reload_if_changed`), none of them may write, so we never silently
    /// recreate it or overwrite a symlink behind the user's back. Sets the
    /// sticky error and returns `true` when blocked, so call sites read as
    /// `if self.blocked_by_deletion() { return ...; }`.
    fn blocked_by_deletion(&mut self) -> bool {
        match self.file_gone {
            Some(gone) => {
                self.set_error(gone.message().to_string());
                true
            }
            None => false,
        }
    }

    /// Clears an ephemeral status message once its deadline has passed;
    /// called each event-loop tick. Sticky messages (no expiry) are untouched.
    /// Returns whether a message was cleared, i.e. the screen needs a redraw.
    pub fn expire_status(&mut self, now: SystemTime) -> bool {
        let expired = self.status_expiry.is_some_and(|exp| now >= exp);
        if expired {
            self.clear_status();
        }
        expired
    }

    /// With `--auto-copy`, copy the current card's code when it has exactly
    /// one candidate. Silent for no-code, ambiguous or failure, except a
    /// refusal over hidden characters, which is always shown.
    fn maybe_auto_copy(&mut self) {
        if !self.auto_copy {
            return;
        }
        if let Some([text]) = self.current_item().map(clipboard::candidates) {
            let text = text.clone();
            self.request_copy(text, CopyKind::Auto);
        }
    }

    fn request_copy(&mut self, text: String, kind: CopyKind) {
        let primary = self.clipboard_primary;
        self.effects.push(Effect::Copy {
            text,
            primary,
            kind,
        });
    }

    /// Reports a finished copy the way its `kind` calls for. `text` is what
    /// was copied, named in the message only when a link had nowhere else
    /// to go.
    pub fn apply_copy_outcome(&mut self, kind: CopyKind, text: &str, outcome: CopyOutcome) {
        const COPIED: &str = "Copied to clipboard";
        const SENT: &str = "Sent to clipboard (OSC 52)";
        const BOTH: &str = "Copied (system clipboard + OSC 52)";
        match (kind, outcome) {
            (CopyKind::Auto, CopyOutcome::CopiedSystem) => self.report_auto_copy(COPIED),
            (CopyKind::Auto, CopyOutcome::CopiedOsc52) => self.report_auto_copy(SENT),
            (CopyKind::Auto, CopyOutcome::CopiedBoth) => self.report_auto_copy(BOTH),
            (CopyKind::Command, CopyOutcome::CopiedSystem) => self.set_status(COPIED.to_string()),
            (CopyKind::Command, CopyOutcome::CopiedOsc52) => self.set_status(SENT.to_string()),
            (CopyKind::Command, CopyOutcome::CopiedBoth) => self.set_status(BOTH.to_string()),
            (CopyKind::Auto | CopyKind::Command, CopyOutcome::HiddenCharacters) => {
                self.set_error(HIDDEN_CHARACTERS_MESSAGE.to_string())
            }
            (CopyKind::Auto, _) => {}
            (CopyKind::Command, CopyOutcome::TooLarge) => self.set_error(
                "Not copied: this command is too large for the terminal clipboard".to_string(),
            ),
            (CopyKind::Command, CopyOutcome::Failed) => {
                self.set_error("Copy failed: no clipboard available".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedSystem) => {
                self.set_status("No browser here — link copied to clipboard".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedOsc52) => {
                self.set_status("No browser here — link sent to clipboard (OSC 52)".to_string())
            }
            (CopyKind::LinkFallback, CopyOutcome::CopiedBoth) => self.set_status(
                "No browser here — link copied (system clipboard + OSC 52)".to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::TooLarge) => self.set_error(
                "No browser here, and the link is too large for the terminal clipboard".to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::HiddenCharacters) => self.set_error(
                "No browser here, and the link contains hidden control characters — not copied"
                    .to_string(),
            ),
            (CopyKind::LinkFallback, CopyOutcome::Failed) => {
                self.set_error(format!("No browser or clipboard here — link: {text}"))
            }
        }
    }

    pub fn scroll_card_down(&mut self) {
        self.scroll_card_down_by(1);
    }

    pub fn scroll_card_up(&mut self) {
        self.scroll_card_up_by(1);
    }

    /// Scroll the card body down by `n` wrapped lines, clamped to the bottom.
    pub fn scroll_card_down_by(&mut self, n: u16) {
        self.card_scroll = self
            .card_scroll
            .saturating_add(n)
            .min(self.layout.card_max_scroll);
    }

    /// Scroll the card body up by `n` wrapped lines, clamped to the top.
    pub fn scroll_card_up_by(&mut self, n: u16) {
        self.card_scroll = self.card_scroll.saturating_sub(n);
    }

    /// Half the current card viewport height (min 1), for `Ctrl-D`/`Ctrl-U`.
    fn half_page(&self) -> u16 {
        (self.layout.card_viewport_height / 2).max(1)
    }

    /// A near-full card viewport (leaving one row of context), for
    /// `PageDown`/`PageUp`. Min 1.
    fn page(&self) -> u16 {
        self.layout.card_viewport_height.saturating_sub(1).max(1)
    }

    /// Mouse wheel, routed per screen: on the checklist an overflowing card
    /// body takes absolute priority, otherwise the wheel steps between tasks
    /// like `k`/`j` (crossing list edges); it scrolls the `?` help overlay
    /// and moves the `T` picker highlight.
    pub fn handle_scroll_up(&mut self) {
        self.critical_armed = None;
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.layout.card_max_scroll > 0 => self.scroll_card_up(),
            (None, View::Checklist) => self.navigate_backward(),
            (Some(Overlay::Help), _) => self.scroll_help_up_by(1),
            (Some(Overlay::ListPicker), _) => self.picker_move(-1),
            _ => {}
        }
    }

    pub fn handle_scroll_down(&mut self) {
        self.critical_armed = None;
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.layout.card_max_scroll > 0 => self.scroll_card_down(),
            (None, View::Checklist) => self.navigate_forward(),
            (Some(Overlay::Help), _) => self.scroll_help_down_by(1),
            (Some(Overlay::ListPicker), _) => self.picker_move(1),
            _ => {}
        }
    }

    /// A bracketed paste: on `/` search and the `T` picker it is appended to
    /// the query as one line (line breaks and tabs become spaces, other control
    /// characters are dropped); everywhere else it is ignored with a sticky
    /// error, so a stray paste never runs its characters as commands.
    pub fn handle_paste(&mut self, text: &str) {
        self.critical_armed = None;
        self.clear_pending_chords();
        let line: String = text
            .trim_end()
            .chars()
            .filter_map(|c| match c {
                '\n' | '\r' | '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        match self.overlay {
            Some(Overlay::Search) => {
                self.search.query.push_str(&line);
                self.update_search();
            }
            Some(Overlay::ListPicker) => {
                self.picker.query.push_str(&line);
                self.picker.selection = 0;
            }
            _ => self.set_error("Paste ignored — markcheck takes single-key commands".to_string()),
        }
    }

    /// Left-click at `(col, row)`: a click inside the current card
    /// copies its command, exactly like `y` — copying its sole code candidate
    /// or showing the same hint when there are none or several. In the `T`
    /// picker a click selects a row (see `picker_click`). Clicks outside the
    /// card, or off the checklist, are ignored.
    pub fn handle_left_click(&mut self, col: u16, row: u16) {
        let armed = self.critical_armed;
        self.dispatch_left_click(col, row);
        self.disarm_unless_used(armed);
        self.show_write_notice();
    }

    /// Shows a pending [`AppState::write_notice`], after the action's own
    /// status.
    fn show_write_notice(&mut self) {
        if let Some(notice) = self.write_notice.take() {
            self.set_error(notice);
        }
    }

    fn dispatch_left_click(&mut self, col: u16, row: u16) {
        self.clear_pending_chords();

        if self.overlay == Some(Overlay::ListPicker) {
            self.picker_click(col, row);
            return;
        }

        // A click on an overview row jumps the cursor there. Handled with no
        // overlay open (checklist + completion views), where the overview is a plain
        // navigation aid — not while a modal (Help/Search/picker/confirm) is up,
        // even though the overview is still drawn beside those overlays.
        let overview_target = self
            .overlay
            .is_none()
            .then(|| {
                self.layout
                    .overview_rows
                    .iter()
                    .find(|(rect, _)| rect.contains((col, row).into()))
                    .map(|(_, target)| *target)
            })
            .flatten();
        if let Some(target) = overview_target {
            match target {
                OverviewTarget::List(list_index) => self.jump_to_list(list_index),
                OverviewTarget::Item(list_index, item_index) => {
                    self.focus_item(list_index, item_index, CursorMove::Navigate)
                }
                // The marker prefix toggles in place, cursor unchanged.
                OverviewTarget::Toggle(list_index, item_index) => {
                    self.toggle_item(list_index, item_index)
                }
            }
            return;
        }

        // The card click-to-copy actions below only apply on the checklist
        // screen.
        if self.overlay.is_some() || self.view != View::Checklist {
            return;
        }
        // A click on a specific code row copies that exact fragment,
        // overriding the fenced-over-inline priority that governs `y`.
        if let Some(text) = self
            .layout
            .code_regions
            .iter()
            .find(|(rect, _)| rect.contains((col, row).into()))
            .map(|(_, text)| text.clone())
        {
            self.copy_specific(&text);
            return;
        }
        // Otherwise a click anywhere on the card copies its sole candidate,
        // exactly like `y` (the MVP behavior).
        if self
            .layout
            .card_rect
            .is_some_and(|r| r.contains((col, row).into()))
        {
            self.copy_current_code();
        }
    }

    /// Copies an exact code fragment (a specific clicked row), bypassing
    /// the single-candidate rule; reported the same way as `y`.
    fn copy_specific(&mut self, text: &str) {
        self.request_copy(text.to_string(), CopyKind::Command);
    }

    pub fn toggle_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let kind = current.kind;
        let state = match kind {
            // An info card can't be toggled, so page past it like `l`.
            ItemKind::DisplayOnly => {
                let before = (self.current_list_index, self.current_item_index);
                self.navigate_forward();
                if (self.current_list_index, self.current_item_index) == before {
                    self.set_error(
                        "Nothing to toggle: this is a note, and it's the last card".to_string(),
                    );
                }
                return;
            }
            ItemKind::Checkbox(state) => state,
        };

        let new_state = toggled_done(state);
        let position = (self.current_list_index, self.current_item_index);
        if new_state == TaskState::Done && !self.confirm_critical(position, CriticalAction::Check) {
            return;
        }
        if !self.set_current_state(new_state) {
            return;
        }
        self.set_status(toggle_message(new_state).to_string());
        if new_state == TaskState::Done {
            self.maybe_transition_screen();
        } else {
            self.view = View::Checklist;
        }
    }

    /// Toggle an arbitrary task in place without moving the cursor (the
    /// overview marker click). Never promotes to a completion screen, but drops
    /// a now-stale one.
    pub fn toggle_item(&mut self, list_index: usize, item_index: usize) {
        if self.blocked_by_deletion() {
            return;
        }
        // A click on a missing or display-only row does nothing, and pushes
        // no undo point.
        let Some(ItemKind::Checkbox(state)) = self
            .document
            .lists
            .get(list_index)
            .and_then(|list| list.items.get(item_index))
            .map(|item| item.kind)
        else {
            return;
        };
        let new_state = toggled_done(state);
        if new_state == TaskState::Done
            && !self.confirm_critical((list_index, item_index), CriticalAction::Click)
        {
            return;
        }
        if !self.set_state(list_index, item_index, new_state) {
            return;
        }

        // Leave the cursor put. Only demote a completion screen that this
        // toggle just invalidated (a task un-done); never promote to one.
        let still_complete = match self.view {
            View::AllComplete => self.document.lists.iter().all(list_all_done),
            View::ListComplete => list_all_done(self.current_list()),
            View::Checklist => true,
        };
        if !still_complete {
            self.view = View::Checklist;
        }
        self.set_status(toggle_message(new_state).to_string());
    }

    /// `s` toggles the *started* state. Never completes and never advances.
    pub fn start_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let state = match current.kind {
            ItemKind::DisplayOnly => {
                self.set_error("Nothing to start: this card is a note, not a task".to_string());
                return;
            }
            ItemKind::Checkbox(state) => state,
        };
        let new_state = if state == TaskState::Started {
            TaskState::NotStarted
        } else {
            TaskState::Started
        };
        if !self.set_current_state(new_state) {
            return;
        }
        self.view = View::Checklist;
    }

    /// Checking or skipping a critical task takes a second, deliberate press
    /// (#95): the first arms it and says so, and the same input again on the
    /// same task confirms, but only after [`CRITICAL_PAUSE`] without it (#219).
    /// A quicker repeat keeps it armed and restarts the pause, so a held key
    /// never gets through. Returns whether the change may go ahead now.
    fn confirm_critical(&mut self, position: (usize, usize), action: CriticalAction) -> bool {
        if !self.document.is_critical(position.0, position.1) {
            return true;
        }
        let now = Instant::now();
        let (again, others) = match action {
            CriticalAction::Check => ("press Space again to check it", "key"),
            CriticalAction::Skip => ("press - again to skip it", "key"),
            CriticalAction::Click => ("click its marker again to check it", "click or key"),
        };
        match self.critical_armed {
            Some(arm) if arm.position == position && arm.action == action => {
                if now.saturating_duration_since(arm.at) >= CRITICAL_PAUSE {
                    self.critical_armed = None;
                    return true;
                }
                self.set_notice(format!(
                    "Critical step still open: too quick — pause, then {again}"
                ));
            }
            _ => self.set_notice(format!(
                "Critical step — {again}, any other {others} leaves it open"
            )),
        }
        self.critical_armed = Some(CriticalArm {
            position,
            action,
            at: now,
        });
        false
    }

    /// `-` sets a task aside as skipped (`[-]`, not applicable this run), or
    /// back to not started. Like completing it, skipping moves on, since
    /// nothing is left to decide there (#54).
    pub fn skip_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let ItemKind::Checkbox(state) = current.kind else {
            self.set_error("Nothing to skip: this card is a note, not a task".to_string());
            return;
        };
        let new_state = if state == TaskState::Skipped {
            TaskState::NotStarted
        } else {
            TaskState::Skipped
        };
        let position = (self.current_list_index, self.current_item_index);
        if new_state == TaskState::Skipped && !self.confirm_critical(position, CriticalAction::Skip)
        {
            return;
        }
        if !self.set_current_state(new_state) {
            return;
        }
        if new_state == TaskState::Skipped {
            self.set_status("Marked skipped".to_string());
            self.maybe_transition_screen();
        } else {
            self.set_status("Marked not skipped".to_string());
            self.view = View::Checklist;
        }
    }

    /// Sets the current checkbox's state and writes it back. Returns whether
    /// the write succeeded.
    fn set_current_state(&mut self, state: TaskState) -> bool {
        self.set_state(self.current_list_index, self.current_item_index, state)
    }

    /// Sets one checkbox's state and writes it back, the one path every
    /// toggle takes: the keyboard's and the overview click's. Returns whether
    /// the write succeeded.
    fn set_state(&mut self, list_index: usize, item_index: usize, state: TaskState) -> bool {
        if self.blocked_by_deletion() {
            return false;
        }
        let pre_state = self.state_snapshot();
        let Some(item) = self
            .document
            .lists
            .get_mut(list_index)
            .and_then(|list| list.items.get_mut(item_index))
        else {
            return false;
        };
        let item_text = item.label();
        item.kind = ItemKind::Checkbox(state);
        if !self.commit_write(
            &pre_state,
            "Write failed — change not saved to disk",
            &format!("{} \"{item_text}\"", state_verb(state)),
        ) {
            return false;
        }
        self.push_undo_point(pre_state);
        true
    }

    /// Writes the document back to disk; every mutating action ends here. On
    /// failure, restores `pre_state` so memory never diverges from disk, and
    /// sets a sticky error with `fail_msg`. On success, queues a git-sync
    /// request described by `change_desc`.
    fn commit_write(
        &mut self,
        pre_state: &StateSnapshot,
        fail_msg: &str,
        change_desc: &str,
    ) -> bool {
        // Held across the divergence check and the write, making the two one
        // compare-and-swap. `Unavailable` proceeds unlocked so a machine without a
        // writable temp directory can still save.
        let _write_lock = match writer::WriteLock::acquire(&self.document.file_path) {
            writer::LockOutcome::Acquired(lock) => Some(lock),
            writer::LockOutcome::Unavailable => None,
            writer::LockOutcome::Busy(lock_path) => {
                self.apply_snapshot(pre_state);
                // Name the file: a lock held by a recycled PID persists until removed by
                // hand.
                self.set_error(format!(
                    "Another markcheck is saving this file — change not saved, please retry \
                     (if this persists, remove {})",
                    lock_path.display()
                ));
                return false;
            }
        };

        if self.disk_content_diverged() {
            self.apply_snapshot(pre_state);
            self.force_reload();
            self.set_error(
                "File changed on disk — reloaded; change not saved, please retry".to_string(),
            );
            return false;
        }
        // The pre-write content for git-sync's staged-target guard, which compares
        // in git's terms so clean filters apply.
        let previous_content = writer::read_checklist(&self.document.file_path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let content = match writer::write_back(&self.document) {
            Ok(saved) => {
                if saved.in_place && !std::mem::replace(&mut self.in_place_noted, true) {
                    self.write_notice = Some(IN_PLACE_MESSAGE.to_string());
                }
                saved.contents
            }
            Err(err) => {
                self.apply_snapshot(pre_state);
                if writer::is_not_regular(&err) {
                    self.file_gone = Some(FileGone::NotRegular);
                    self.set_error(FileGone::NotRegular.message().to_string());
                } else if writer::is_read_only(&self.document.file_path) {
                    self.set_error(READ_ONLY_MESSAGE.to_string());
                } else {
                    self.set_error(format!(
                        "{fail_msg}: {}",
                        writer::describe_write_error(&err)
                    ));
                }
                return false;
            }
        };
        let (file_mtime, file_size) = current_stat(&self.document.file_path).unzip();
        self.file_mtime = file_mtime;
        self.file_size = file_size;
        let content_hash = hash_bytes(content.as_bytes());
        self.file_content_hash = Some(content_hash);
        self.last_update_at = Some(SystemTime::now());
        self.queue_git_sync(PendingSync {
            content,
            content_hash,
            previous_content,
            description: change_desc.to_string(),
            immediate: false,
        });
        true
    }

    /// True when the file's on-disk content no longer matches
    /// `file_content_hash`, i.e. someone else changed it since we last saw it.
    /// Fails open when the file can't be read; `write_back` reports that.
    fn disk_content_diverged(&self) -> bool {
        match current_content_hash(&self.document.file_path) {
            Some(hash) => Some(hash) != self.file_content_hash,
            None => false,
        }
    }

    fn maybe_transition_screen(&mut self) {
        if list_all_done(self.current_list()) {
            let all_done = self.document.lists.iter().all(list_all_done);
            self.view = if all_done {
                View::AllComplete
            } else {
                View::ListComplete
            };
        } else {
            self.advance_to_next_open();
        }
    }

    /// After completing a task, move to the next item that isn't a done task
    /// (notes still stop the cursor), wrapping to the list's first open task.
    /// Landing on a done task would let the next `Space` silently uncheck it.
    fn advance_to_next_open(&mut self) {
        let list = self.current_list();
        let target = list
            .items
            .iter()
            .enumerate()
            .skip(self.current_item_index + 1)
            .find(
                |(_, item)| !matches!(item.kind, ItemKind::Checkbox(state) if state.is_resolved()),
            )
            .map(|(index, _)| index)
            .or_else(|| first_undone_index(list));
        if let Some(target) = target {
            self.focus_item(self.current_list_index, target, CursorMove::Navigate);
        }
    }

    /// Re-reads the file if its mtime or size changed. Cheap enough for every
    /// tick. Returns whether new content was loaded.
    pub fn reload_if_changed(&mut self) -> bool {
        // A confirmed deletion or swap, as opposed to a transient unreadable state.
        let gone = match fs::symlink_metadata(&self.document.file_path) {
            Ok(meta) if meta.is_file() => None,
            Ok(_) => Some(FileGone::NotRegular),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Some(FileGone::Deleted),
            Err(_) => return false,
        };
        if let Some(gone) = gone {
            if self.file_gone != Some(gone) {
                self.file_gone = Some(gone);
                self.set_error(gone.message().to_string());
            }
            return false;
        }
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            return false;
        };

        // File is a regular file again. Clear the flag and reload regardless of
        // mtime/size so the restored content is always picked up.
        let was_deleted = self.file_gone.take().is_some();
        let unchanged = self.file_mtime == Some(modified) && self.file_size == Some(size);

        if !was_deleted && unchanged {
            return false;
        }

        self.reload_from_disk(modified, size, was_deleted)
    }

    /// Reloads unconditionally, for when the content hash caught a change that
    /// mtime and size missed on a coarse-mtime filesystem.
    fn force_reload(&mut self) {
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            return;
        };
        let was_deleted = self.file_gone.take().is_some();
        self.reload_from_disk(modified, size, was_deleted);
    }

    /// Parses the file fresh and swaps it in on success. `file_mtime` and
    /// `file_size` are refreshed on every branch, but `file_content_hash` only
    /// when the new content is adopted: it must name the revision the in-memory
    /// document came from, or the next toggle would overwrite the newer file.
    fn reload_from_disk(&mut self, modified: SystemTime, size: u64, was_deleted: bool) -> bool {
        // Refreshed on every path below, including the two that decline to
        // load; only the fingerprint distinguishes them.
        self.file_mtime = Some(modified);
        self.file_size = Some(size);

        match parser::parse_document(self.document.file_path.clone()) {
            Ok(new_document) if !new_document.lists.is_empty() => {
                let search_origin = self.remapped(self.search.origin, &new_document);
                let task_lost = self.remap_position(&new_document);
                let changes = reload_changes(&self.document, &new_document);
                self.document = new_document;
                let cancelled = self.keep_overlay_across_reload(search_origin);
                // An external edit is a hard boundary for undo history: it can
                // change task state and line numbers underneath us, so the
                // snapshots no longer apply cleanly.
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.history_epoch += 1;
                // Positions from before the edit may no longer mean the same card.
                self.jump_back.clear();
                self.critical_armed = None;
                // A queued git-sync request describes content this reload replaced. The
                // new content queues nothing: markcheck doesn't commit edits it didn't
                // make.
                self.cancel_git_sync();
                // The cursor may now be on a different card, so a half-typed chord must
                // not act on it.
                self.pending_g = false;
                self.pending_open_link = false;
                self.pending_copy_code = false;
                self.pending_number = 0;
                if let Some(cancelled) = cancelled {
                    self.set_error(format!("File changed on disk — {cancelled} cancelled"));
                } else {
                    let reloaded = if was_deleted {
                        "File restored — reloaded".to_string()
                    } else if let Some(changes) = changes {
                        format!("Reloaded: {changes}")
                    } else {
                        "Reloaded: file changed on disk".to_string()
                    };
                    if task_lost {
                        self.set_status(format!("{reloaded} — task not found, cursor moved"));
                    } else {
                        self.set_status(reloaded);
                    }
                }
                self.last_update_at = Some(SystemTime::now());
                // Taken from the adopted document, never a fresh read.
                self.file_content_hash = Some(hash_bytes(
                    writer::document_contents(&self.document).as_bytes(),
                ));
                true
            }
            // Sticky: the state persists until the file gains tasks again.
            Ok(_) => {
                self.set_error(
                    "Reload skipped: file has no checklist items — changes cannot be saved \
                     until it has tasks again"
                        .to_string(),
                );
                false
            }
            Err(err) => {
                self.set_error(format!("Reload failed: {err}"));
                false
            }
        }
    }

    /// Queues a git-sync request for an edit made outside `write_back` (the
    /// `e` editor). `editor` is the resolved program; only its file name goes
    /// into the commit message, so a path from the user's environment is never
    /// published.
    pub fn request_external_edit_sync(&mut self, editor: &str) {
        let content = writer::document_contents(&self.document);
        let content_hash = hash_bytes(content.as_bytes());
        self.queue_git_sync(PendingSync {
            // The editor wrote this, so the content being replaced is the content
            // carried.
            previous_content: Some(content.clone()),
            content,
            content_hash,
            description: format!("Edited in {}", program_name(editor)),
            immediate: true,
        });
    }

    /// Queues `sync`, replacing one not yet handed to the worker: only the
    /// latest content needs committing.
    fn queue_git_sync(&mut self, sync: PendingSync) {
        self.cancel_git_sync();
        self.effects.push(Effect::GitSync(sync));
    }

    fn cancel_git_sync(&mut self) {
        self.effects
            .retain(|effect| !matches!(effect, Effect::GitSync(_)));
    }

    /// An auto-copy confirmation goes after whatever the motion reported
    /// (`Match 2/5 · Copied to clipboard`), and never hides an error.
    fn report_auto_copy(&mut self, copied: &str) {
        if self.status_is_error {
            return;
        }
        let message = match self.status_message.take() {
            Some(reported) => format!("{reported} · {copied}"),
            None => copied.to_string(),
        };
        self.set_status(message);
    }

    /// Settles the view and overlay after a reload. The view always becomes
    /// the checklist, since a completion summary may no longer hold. Text-input
    /// and read-only overlays stay open, so keys typed into them never fall
    /// through to the checklist as commands; a confirmation is cancelled, since
    /// it was asked about the old content, and its name is returned for the
    /// status message. Must run after the new document is adopted.
    fn keep_overlay_across_reload(
        &mut self,
        search_origin: (usize, usize),
    ) -> Option<&'static str> {
        self.view = View::Checklist;
        match self.overlay {
            Some(Overlay::Search) => {
                self.search.origin = search_origin;
                self.update_search();
                None
            }
            Some(Overlay::ListPicker) => {
                let count = self.picker_matches().len();
                self.picker.selection = self.picker.selection.min(count.saturating_sub(1));
                None
            }
            Some(Overlay::ConfirmReset) => {
                self.overlay = None;
                Some("reset")
            }
            Some(Overlay::ConfirmQuitReset) => {
                self.overlay = None;
                Some("quit")
            }
            Some(Overlay::Help) | None => None,
        }
    }

    /// Keeps the cursor on the same list and task after a reload (see
    /// `remapped`). Returns whether the task was lost and the cursor had to
    /// move. Never auto-copies: a reload is not the user navigating.
    fn remap_position(&mut self, new_document: &Document) -> bool {
        let (list_index, item_index, lost) = self.remapped_with_loss(
            (self.current_list_index, self.current_item_index),
            new_document,
        );
        self.current_list_index = list_index;
        self.current_item_index = item_index;
        self.reset_card_scroll();
        lost
    }

    /// Where the `(list, item)` position in the current document lands in
    /// `new_document`.
    fn remapped(&self, position: (usize, usize), new_document: &Document) -> (usize, usize) {
        let (list_index, item_index, _) = self.remapped_with_loss(position, new_document);
        (list_index, item_index)
    }

    /// Matches the list by title and occurrence, then the item by content
    /// (header, body text and depth), preferring the candidate nearest the old
    /// line number, since inserting a line above it shifts every line number.
    /// With no content match it falls back to the item on the old line (a task
    /// edited in place), then to the nearest one by line number, reporting the
    /// task as lost; a list that is gone falls back to the first list's start.
    fn remapped_with_loss(
        &self,
        (list_index, item_index): (usize, usize),
        new_document: &Document,
    ) -> (usize, usize, bool) {
        let old_list = self.document.lists.get(list_index);
        let old_title = old_list.map(|s| s.title.clone());
        let old_item = old_list.and_then(|list| list.items.get(item_index));

        // Two lists can share a title, so the old list's rank among same-titled
        // lists picks the new one.
        let old_occurrence = old_title.as_deref().map(|title| {
            self.document.lists[..list_index]
                .iter()
                .filter(|s| s.title == title)
                .count()
        });

        let found_list_index =
            old_title
                .as_deref()
                .zip(old_occurrence)
                .and_then(|(title, occurrence)| {
                    new_document
                        .lists
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.title == title)
                        .nth(occurrence)
                        .map(|(i, _)| i)
                });
        let new_list_index = found_list_index
            .unwrap_or(0)
            .min(new_document.lists.len().saturating_sub(1));

        let Some(old_item) = old_item else {
            return (new_list_index, 0, false);
        };
        if found_list_index.is_none() {
            return (new_list_index, 0, true);
        }
        let items = &new_document.lists[new_list_index].items;
        let distance = |index: &usize| items[*index].line_number.abs_diff(old_item.line_number);
        let same_task = |index: &usize| {
            let item = &items[*index];
            item.header == old_item.header
                && item.display_text == old_item.display_text
                && item.depth == old_item.depth
        };

        if let Some(index) = (0..items.len()).filter(same_task).min_by_key(distance) {
            return (new_list_index, index, false);
        }
        if let Some(index) =
            (0..items.len()).find(|&i| items[i].line_number == old_item.line_number)
        {
            return (new_list_index, index, false);
        }
        let nearest = (0..items.len()).min_by_key(distance).unwrap_or(0);
        (new_list_index, nearest, true)
    }

    /// From a finished list, jump to the next list with incomplete work,
    /// wrapping to an earlier one; show the all-complete summary only when
    /// nothing is left anywhere.
    fn advance_to_next_incomplete_list(&mut self) {
        let target = self
            .next_incomplete_list_after(self.current_list_index)
            .or_else(|| self.prev_incomplete_list_before(self.current_list_index));
        match target {
            Some(index) => self.jump_to_list(index),
            None => self.view = View::AllComplete,
        }
    }

    fn review_completed_list(&mut self) {
        self.view = View::Checklist;
    }

    /// Called by the main loop once `fg` has resumed it, or once the suspend
    /// failed. The file may have been edited from the shell meanwhile.
    pub fn finish_suspend(&mut self, outcome: Result<(), String>) {
        if let Err(err) = outcome {
            self.set_error(format!("Couldn't suspend: {err}"));
            return;
        }
        self.reload_if_changed();
        if self.status_message.is_none() {
            self.set_status("Resumed".to_string());
        }
    }

    /// After the `e` editor returns: reload, queueing a git-sync request when
    /// a successful editor changed the file (that edit never went through
    /// `write_back`), then report `warning` (from resolving the editor) and a
    /// launch failure or non-zero exit. Those go after the reload so its own
    /// message can't replace them. A non-zero exit is the editor's abort
    /// signal, so its edit is reloaded but never synced. `program` is the
    /// resolved editor. A successful editor that returned within
    /// [`EDITOR_RETURNED_AT_ONCE`] without changing the file most likely
    /// opened a window and went on editing there, where its save would reach
    /// markcheck as an unsynced external edit, so that case gets a hint about
    /// the editor's wait flag.
    pub fn finish_editor(
        &mut self,
        program: &str,
        warning: Option<String>,
        status: io::Result<std::process::ExitStatus>,
        elapsed: Duration,
    ) {
        let failure = match status {
            Err(err) => Some(format!("Failed to launch {program}: {err}")),
            Ok(status) if !status.success() => Some(format!("{program} exited with {status}")),
            Ok(_) => None,
        };
        let failed = failure.is_some();
        self.clear_status();
        let reloaded = self.reload_if_changed();
        if reloaded && !failed {
            self.request_external_edit_sync(program);
        }
        let mut problems: Vec<String> = warning.into_iter().chain(failure).collect();
        if !failed && !reloaded && elapsed < EDITOR_RETURNED_AT_ONCE {
            problems.push(format!(
                "{} returned immediately — if it opened a window, make it wait: \
                 EDITOR=\"code --wait\" (subl -w, gedit --wait)",
                program_name(program)
            ));
        }
        if problems.is_empty() {
            return;
        }
        if let Some(reload_message) = self.status_message.take() {
            problems.push(if reloaded && failed && self.git_sync.active {
                format!("{reload_message} — not synced")
            } else {
                reload_message
            });
        }
        self.set_error(problems.join("; "));
    }

    /// `o`: open the current card's link. With several links, arm a digit that
    /// picks link `[N]`. The spawn happens in the main loop.
    fn open_current_link(&mut self) {
        match self.current_link_urls().as_slice() {
            [url] => self.open_url(url),
            [] => self.set_error("Nothing to open: card has no link".to_string()),
            // Several links: arm a digit. No sticky message, which would hide the list
            // being chosen from.
            _ => self.pending_open_link = true,
        }
    }

    /// Opens link `[n]` (1-based) after `o` on a multi-link card.
    fn open_link_number(&mut self, n: usize) {
        match self.current_link_urls().get(n - 1) {
            Some(url) => self.open_url(url),
            None => self.set_error(format!("No link [{n}] on this card")),
        }
    }

    /// `o` on `[Rollback](#rollback)`: jump to the first task under that
    /// heading, remembering where from for `Ctrl-O`, as in vim (#139).
    fn follow_anchor(&mut self, anchor: &str) {
        let Some(target) = self.document.anchor_target(anchor) else {
            self.set_error(format!("No section named #{anchor} in this file"));
            return;
        };
        self.jump_back
            .push((self.current_list_index, self.current_item_index));
        self.focus_and_report(
            target,
            Some(format!("Jumped to #{anchor} · Ctrl-O goes back")),
        );
        self.view = View::Checklist;
    }

    /// `Ctrl-O`: back to where the last `#anchor` jump left from.
    fn jump_back(&mut self) {
        match self.jump_back.pop() {
            Some(target) => self.focus_and_report(target, Some("Jumped back".to_string())),
            None => self.set_error(
                "Nothing to go back to: Ctrl-O returns from following a #link with o".to_string(),
            ),
        }
    }

    /// Moves to where `markcheck file.md#anchor` or `+N` asked to start
    /// (#114). `Ok` is the confirmation to show; `Err` says why it couldn't,
    /// with the cursor left on the first unfinished task. The caller shows
    /// either alongside the other startup messages.
    pub fn open_at(&mut self, start: &crate::model::StartAt) -> Result<String, String> {
        let (target, description) = match start {
            crate::model::StartAt::Line(line) => {
                (self.document.first_task_from(*line), format!("line {line}"))
            }
            crate::model::StartAt::Anchor(anchor) => {
                (self.document.anchor_target(anchor), format!("#{anchor}"))
            }
        };
        let (list_index, item_index) = target
            .ok_or_else(|| format!("no list or task at {description}, so opened at the start"))?;
        self.focus_item(list_index, item_index, CursorMove::Navigate);
        Ok(format!("Opened at {description}"))
    }

    fn current_link_urls(&self) -> Vec<String> {
        self.current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default()
    }

    /// Hands `url` to the main loop's opener, unless its scheme isn't one
    /// markcheck opens. A `#anchor` link is followed within the document.
    fn open_url(&mut self, url: &str) {
        if let Some(anchor) = url.strip_prefix('#') {
            self.follow_anchor(anchor);
        } else if is_safe_link(url) {
            self.effects.push(Effect::OpenLink(url.to_string()));
        } else {
            self.set_error(format!("Not opened: unsupported link type ({url})"));
        }
    }

    /// `o`'s opener has started. Worded as an attempt, not a result: a
    /// quick failed exit is reported afterwards (`launch::opener_failure_message`).
    pub fn report_link_sent(&mut self, program: &str, url: &str) {
        let name = crate::launch::program_name(program);
        self.set_status(format!("Sent to {name}: {url}"));
    }

    /// `o` with nowhere to send the link (`launch::no_browser_available`):
    /// copy it instead, which over SSH reaches the local terminal via OSC 52.
    pub fn copy_link_instead(&mut self, url: &str) {
        self.request_copy(url.to_string(), CopyKind::LinkFallback);
    }

    /// Records a successful background commit+push, driving the
    /// title-bar "Synced … ago" tag the same way a write-back drives
    /// `last_update_at`.
    pub fn record_git_sync(&mut self) {
        self.git_sync.last_at = Some(SystemTime::now());
    }

    /// Folds a finished background sync into the title bar's sync indicator
    /// and reports it in the status bar. A failure keeps the indicator in its
    /// unsynced state until a later sync lands. A push failing again for the
    /// same reason only updates the indicator: retries repeat every few
    /// seconds to minutes, and each report would replace whatever message the
    /// user is reading.
    pub fn apply_git_sync_outcome(&mut self, outcome: &SyncOutcome) {
        let (label, push_reason) = match outcome {
            SyncOutcome::Synced => {
                self.record_git_sync();
                self.git_sync.reported_reason = None;
                if self.git_sync.problem.take().is_some() {
                    self.set_status("Git sync caught up: committed and pushed".to_string());
                }
                return;
            }
            SyncOutcome::CaughtUp { .. } => {
                self.record_git_sync();
                self.git_sync.reported_reason = None;
                self.git_sync.problem = None;
                if let Some(message) = git_sync::status_message(outcome) {
                    self.set_status(message);
                }
                return;
            }
            SyncOutcome::Skipped | SyncOutcome::RetryAbandoned => return,
            SyncOutcome::SkippedUntracked => ("not tracked", None),
            SyncOutcome::CommittedNotPushed { message, .. } | SyncOutcome::RetryFailed(message) => {
                ("not pushed", Some(message))
            }
            SyncOutcome::Failed(_) => ("not synced", None),
            SyncOutcome::NotCaughtUp(_) => ("not pushed", None),
        };
        self.git_sync.problem = Some(label);
        self.git_sync.last_at = None;
        if push_reason.is_some() && push_reason == self.git_sync.reported_reason.as_ref() {
            return;
        }
        self.git_sync.reported_reason = push_reason.cloned();
        if let Some(message) = git_sync::status_message(outcome) {
            self.set_error(message);
        }
    }

    fn request_reset(&mut self) {
        let (done, started, _total) = self.document.checkbox_progress();
        if done == 0 && started == 0 && self.document.skipped_count() == 0 {
            self.set_error("Nothing to reset: no tasks are done".to_string());
            return;
        }
        self.overlay = Some(Overlay::ConfirmReset);
    }

    /// Resets every task to not done and saves. Returns whether the reset
    /// reached disk; on failure the sticky error explains why.
    fn reset_all(&mut self) -> bool {
        self.reset_scope(ResetScope::All)
    }

    /// The root of the sub-list the cursor is in: the current task when it has
    /// sub-tasks, otherwise its parent. `None` at the top level with none.
    pub fn sub_list_root(&self) -> Option<usize> {
        let list = self.current_list();
        let index = self.current_item_index;
        if list.has_children(index) {
            return Some(index);
        }
        list.parent_chain(index).last().copied()
    }

    /// The `(list, item)` positions `scope` covers.
    fn scope_positions(&self, scope: ResetScope) -> Vec<(usize, usize)> {
        let li = self.current_list_index;
        match scope {
            ResetScope::All => self
                .document
                .lists
                .iter()
                .enumerate()
                .flat_map(|(li, list)| (0..list.items.len()).map(move |ii| (li, ii)))
                .collect(),
            ResetScope::List => (0..self.current_list().items.len())
                .map(|ii| (li, ii))
                .collect(),
            ResetScope::SubList => self
                .sub_list_root()
                .map(|root| {
                    std::iter::once(root)
                        .chain(self.current_list().descendants(root))
                        .map(|ii| (li, ii))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// How many tasks a reset of `scope` would change: done, started or
    /// skipped ones.
    pub fn reset_count(&self, scope: ResetScope) -> usize {
        self.scope_positions(scope)
            .into_iter()
            .filter(|&(li, ii)| {
                matches!(self.document.lists[li].items[ii].kind,
                    ItemKind::Checkbox(state) if state != TaskState::NotStarted)
            })
            .count()
    }

    /// Resets the tasks `scope` covers to not done and saves, as one undo
    /// step. Returns whether the reset reached disk.
    fn reset_scope(&mut self, scope: ResetScope) -> bool {
        if self.blocked_by_deletion() {
            return false;
        }
        let (description, status) = match scope {
            ResetScope::All => (
                "Reset all tasks to not done".to_string(),
                "All tasks reset to not done".to_string(),
            ),
            ResetScope::List => {
                let title = &self.current_list().title;
                (
                    format!("Reset \"{title}\" to not done"),
                    format!("\"{title}\" reset to not done"),
                )
            }
            ResetScope::SubList => {
                let label = self
                    .sub_list_root()
                    .map(|root| self.current_list().items[root].label())
                    .unwrap_or_default();
                (
                    format!("Reset \"{label}\" and its sub-tasks to not done"),
                    format!("\"{label}\" and its sub-tasks reset to not done"),
                )
            }
        };
        let pre_state = self.state_snapshot();
        for (li, ii) in self.scope_positions(scope) {
            let item = &mut self.document.lists[li].items[ii];
            if let ItemKind::Checkbox(_) = item.kind {
                item.kind = ItemKind::Checkbox(TaskState::NotStarted);
            }
        }
        if !self.commit_write(
            &pre_state,
            "Write failed — reset not saved to disk",
            &description,
        ) {
            return false;
        }
        self.push_undo_point(pre_state);
        self.view = View::Checklist;
        self.set_status(status);
        true
    }

    // ----- Undo / redo -------------------------------------------------

    /// A full snapshot of every checkbox item's current state, keyed by line
    /// number. This is the unit of undo/redo history.
    fn state_snapshot(&self) -> StateSnapshot {
        self.document
            .lists
            .iter()
            .flat_map(|list| &list.items)
            .filter_map(|item| match item.kind {
                ItemKind::Checkbox(state) => Some((item.line_number, state)),
                ItemKind::DisplayOnly => None,
            })
            .collect()
    }

    /// Pushes `pre_state` onto the capped undo stack and clears redo. Only
    /// called after a successful write.
    fn push_undo_point(&mut self, pre_state: StateSnapshot) {
        self.undo_stack.push(pre_state);
        if self.undo_stack.len() > UNDO_HISTORY_CAP {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Applies a snapshot by line number, returning the lines whose state
    /// changed.
    fn apply_snapshot(&mut self, snapshot: &StateSnapshot) -> Vec<usize> {
        let wanted: std::collections::HashMap<usize, TaskState> =
            snapshot.iter().copied().collect();
        let mut changed = Vec::new();
        for list in &mut self.document.lists {
            for item in &mut list.items {
                let ItemKind::Checkbox(state) = item.kind else {
                    continue;
                };
                if let Some(&want) = wanted.get(&item.line_number)
                    && state != want
                {
                    item.kind = ItemKind::Checkbox(want);
                    changed.push(item.line_number);
                }
            }
        }
        changed
    }

    /// `u`: undo the last state-changing action. If the write fails, the entry
    /// goes back onto the stack, unless the failure reloaded the file and so
    /// cleared the history the entry belonged to.
    pub fn undo(&mut self) {
        if self.blocked_by_deletion() {
            return;
        }
        let Some(snapshot) = self.undo_stack.pop() else {
            self.set_error("Nothing to undo".to_string());
            return;
        };
        let epoch = self.history_epoch;
        let redo_point = self.state_snapshot();
        let changed = self.apply_snapshot(&snapshot);
        if !self.finish_history_apply(&redo_point, &changed, "Undo") {
            if self.history_epoch == epoch {
                self.undo_stack.push(snapshot);
            }
            return;
        }
        self.redo_stack.push(redo_point);
    }

    /// `Ctrl-R`: redo the last undone action. Mirror of [`undo`], moving
    /// a snapshot from the redo stack back onto the undo stack.
    pub fn redo(&mut self) {
        if self.blocked_by_deletion() {
            return;
        }
        let Some(snapshot) = self.redo_stack.pop() else {
            self.set_error("Nothing to redo".to_string());
            return;
        };
        let epoch = self.history_epoch;
        let undo_point = self.state_snapshot();
        let changed = self.apply_snapshot(&snapshot);
        if !self.finish_history_apply(&undo_point, &changed, "Redo") {
            if self.history_epoch == epoch {
                self.redo_stack.push(snapshot);
            }
            return;
        }
        self.undo_stack.push(undo_point);
    }

    /// Shared tail for [`undo`]/[`redo`]. Returns whether the write succeeded.
    fn finish_history_apply(
        &mut self,
        pre_state: &StateSnapshot,
        changed: &[usize],
        verb: &str,
    ) -> bool {
        let change_desc = format!(
            "{verb}: {} item{}",
            changed.len(),
            if changed.len() == 1 { "" } else { "s" }
        );
        if !self.commit_write(
            pre_state,
            &format!("{verb} failed — write error, change not saved"),
            &change_desc,
        ) {
            return false;
        }
        self.view = View::Checklist;

        match changed {
            [] => self.set_status(format!("{verb}: no change")),
            [line] => {
                if let Some((li, ii)) = self.locate_line(*line) {
                    self.focus_item(li, ii, CursorMove::Quiet);
                }
                let desc = match self.current_item().map(|item| item.kind) {
                    Some(ItemKind::Checkbox(TaskState::Done)) => "marked done",
                    Some(ItemKind::Checkbox(TaskState::Started)) => "marked started",
                    Some(ItemKind::Checkbox(TaskState::NotStarted)) => "marked not done",
                    _ => "restored",
                };
                self.set_status(format!("{verb}: {desc}"));
            }
            many => self.set_status(format!("{verb}: restored {} tasks", many.len())),
        }
        true
    }

    /// Finds the `(list, item)` indices of the checkbox item with the given line
    /// number, if it still exists (undo focus).
    fn locate_line(&self, line: usize) -> Option<(usize, usize)> {
        self.document
            .lists
            .iter()
            .enumerate()
            .find_map(|(li, list)| {
                list.items
                    .iter()
                    .position(|item| item.line_number == line)
                    .map(|ii| (li, ii))
            })
    }

    fn copy_current_code(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        match clipboard::candidates(current) {
            [] => self.set_error("Nothing to copy: item has no code".to_string()),
            [text] => {
                let text = text.clone();
                self.request_copy(text, CopyKind::Command);
            }
            // Several commands: arm a digit that picks command `[N]`. No sticky
            // message, so the status bar shows the prompt instead.
            _ => self.pending_copy_code = true,
        }
    }

    /// Copies command `[n]` (1-based) after `y` on a card with several.
    fn copy_code_number(&mut self, n: usize) {
        let text = self
            .current_item()
            .and_then(|item| clipboard::candidates(item).get(n - 1).cloned());
        match text {
            Some(text) => self.copy_specific(&text),
            None => self.set_error(format!("No command [{n}] on this card")),
        }
    }

    /// Convenience entry that treats the key as unmodified. Kept so headless
    /// tests can drive input without constructing `KeyModifiers`; the main
    /// loop calls [`handle_key_with_mods`] with the real modifiers so that
    /// Ctrl combos (card scrolling) are distinguishable.
    #[cfg(test)]
    pub fn handle_key(&mut self, code: KeyCode) {
        self.handle_key_with_mods(code, KeyModifiers::NONE);
    }

    /// Top-level key dispatch: modal overlays first, then card scroll, then
    /// global keys, then the view's bindings. The order matters.
    pub fn handle_key_with_mods(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let armed = self.critical_armed;
        if armed.is_some() && code == KeyCode::Esc {
            self.critical_armed = None;
            self.set_status("Critical step not checked".to_string());
            return;
        }
        self.dispatch_key(code, modifiers);
        self.disarm_unless_used(armed);
        self.show_write_notice();
    }

    /// Any key or click other than the confirming one leaves an armed
    /// critical task unchecked.
    fn disarm_unless_used(&mut self, armed: Option<CriticalArm>) {
        if armed.is_some() && self.critical_armed == armed {
            self.critical_armed = None;
        }
    }

    fn dispatch_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.clear_status();

        // Ctrl-Z suspends from every screen, overlays and search input
        // included, as in other full-screen terminal programs.
        if code == KeyCode::Char('z') && modifiers.contains(KeyModifiers::CONTROL) {
            self.clear_pending_chords();
            self.effects.push(Effect::Suspend);
            return;
        }

        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.handle_ctrl_c();
            return;
        }

        // A Ctrl/Alt chord runs only where it is explicitly bound. Every
        // binding below matches on `KeyCode` alone, so without this Ctrl-S
        // would act as `s` and Ctrl-Y would confirm the reset like `y`.
        if !self.is_bound_chord(code, modifiers) {
            self.clear_pending_chords();
            self.set_error(format!(
                "{} is not bound (? lists every key)",
                chord_name(code, modifiers)
            ));
            return;
        }

        // Any key other than a second `g` cancels a pending `gg` motion.
        let pending_g = std::mem::take(&mut self.pending_g);

        if self.handle_pending_open_link_key(code) || self.handle_pending_copy_code_key(code) {
            return;
        }

        // Confirmation modals are handled before the global q/Esc quit
        // check, so Esc cancels rather than quits.
        // An open overlay takes every key, before the global keys, so `Esc`
        // closes or cancels it rather than quitting, and typed text is never
        // run as commands.
        if let Some(overlay) = self.overlay {
            match overlay {
                Overlay::ConfirmReset | Overlay::ConfirmQuitReset => {
                    self.handle_confirm_key(overlay, code)
                }
                Overlay::Help => self.handle_help_key(code, modifiers),
                Overlay::Search => self.handle_search_key(code, modifiers),
                // Letters are filter input, so the selection moves on arrows /
                // Ctrl-N/Ctrl-P, not j/k.
                Overlay::ListPicker => self.handle_picker_key(code, modifiers),
            }
            return;
        }

        // Card-body viewport scroll. Only the Checklist screen has a
        // scrollable body; intercepted before the plain `e`/`y`/… bindings so
        // Ctrl-E/Ctrl-Y don't trigger "edit"/"yank".
        if self.view == View::Checklist && self.handle_checklist_scroll_key(code, modifiers) {
            return;
        }

        if self.handle_global_key(code, modifiers) {
            return;
        }

        match self.view {
            View::Checklist => self.handle_checklist_key(code, pending_g),
            View::ListComplete => self.handle_list_complete_key(code),
            View::AllComplete => self.handle_all_complete_key(code),
        }
    }

    /// Whether `code` with `modifiers` may reach the screen's bindings: plain
    /// and Shift-modified keys always may (capitals can arrive with SHIFT);
    /// a Ctrl chord only where the current screen binds it; Alt (and any
    /// other modifier) never, since nothing binds it.
    fn is_bound_chord(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let chord = modifiers - KeyModifiers::SHIFT;
        if chord.is_empty() {
            return true;
        }
        if chord != KeyModifiers::CONTROL {
            return false;
        }
        let KeyCode::Char(c) = code else {
            return false;
        };
        let bound: &[char] = match (self.overlay, self.view) {
            (None, View::Checklist) => &['e', 'y', 'd', 'u', 'r', 'o'],
            (None, View::ListComplete | View::AllComplete) => &['r'],
            (Some(Overlay::Help), _) => &['e', 'y', 'd', 'u'],
            (Some(Overlay::ListPicker), _) => &['p', 'n', 'd', 'u', 'w'],
            (Some(Overlay::Search), _) => &['u', 'w'],
            (Some(Overlay::ConfirmReset | Overlay::ConfirmQuitReset), _) => &[],
        };
        bound.contains(&c)
    }

    /// Raw mode delivers Ctrl-C as a key, not `SIGINT`. As in vim, it cancels
    /// an overlay or search input like `Esc`, and elsewhere says how to quit
    /// rather than quitting.
    fn handle_ctrl_c(&mut self) {
        self.clear_pending_chords();
        match self.overlay {
            Some(Overlay::Search) => self.cancel_search(),
            Some(_) => self.overlay = None,
            None => self.set_error(QUIT_HINT.to_string()),
        }
    }

    /// A pending `o` waits for a link number. Runs before the digit list jump and
    /// the global Esc. Any other key cancels the prompt and still takes its normal
    /// action, returning `false`.
    fn handle_pending_open_link_key(&mut self, code: KeyCode) -> bool {
        if !std::mem::take(&mut self.pending_open_link) {
            return false;
        }
        let count = self.current_item().map_or(0, |item| item.link_urls().len());
        match self.pick_number_key(code, count) {
            NumberKey::Typing => {
                self.pending_open_link = true;
                true
            }
            NumberKey::Picked(n) => {
                self.open_link_number(n);
                true
            }
            NumberKey::Cancelled => true,
            NumberKey::Other => false,
        }
    }

    /// A pending `y` waits for a command number, exactly like `o`'s link prompt.
    fn handle_pending_copy_code_key(&mut self, code: KeyCode) -> bool {
        if !std::mem::take(&mut self.pending_copy_code) {
            return false;
        }
        let count = self
            .current_item()
            .map_or(0, |item| clipboard::candidates(item).len());
        match self.pick_number_key(code, count) {
            NumberKey::Typing => {
                self.pending_copy_code = true;
                true
            }
            NumberKey::Picked(n) => {
                self.copy_code_number(n);
                true
            }
            NumberKey::Cancelled => true,
            NumberKey::Other => false,
        }
    }

    /// One key at an armed `o`/`y` prompt choosing among `count` entries. A digit
    /// picks at once unless a longer valid number could still follow (`1` with
    /// twelve links might be `[12]`); then it waits for another digit or `Enter`.
    fn pick_number_key(&mut self, code: KeyCode, count: usize) -> NumberKey {
        let typed = std::mem::take(&mut self.pending_number);
        match code {
            KeyCode::Char(c @ '0'..='9') if typed > 0 || c != '0' => {
                let n = typed * 10 + usize::from(c as u8 - b'0');
                if n * 10 <= count {
                    self.pending_number = n;
                    NumberKey::Typing
                } else {
                    NumberKey::Picked(n)
                }
            }
            KeyCode::Enter if typed > 0 => NumberKey::Picked(typed),
            KeyCode::Backspace if typed > 0 => {
                self.pending_number = typed / 10;
                NumberKey::Typing
            }
            KeyCode::Esc => NumberKey::Cancelled,
            _ => NumberKey::Other,
        }
    }

    /// A failed reset leaves either dialog open, so its error is read first.
    fn handle_confirm_key(&mut self, overlay: Overlay, code: KeyCode) {
        if overlay == Overlay::ConfirmReset {
            // Only `y` (everything), `l` (this list) and `s` (this sub-list)
            // confirm — `Enter` is too easy to hit by reflex for a rewrite.
            // Any other key cancels.
            let scope = match code {
                KeyCode::Char('y') => ResetScope::All,
                KeyCode::Char('l') => ResetScope::List,
                KeyCode::Char('s') if self.sub_list_root().is_some() => ResetScope::SubList,
                _ => {
                    self.overlay = None;
                    return;
                }
            };
            if self.reset_count(scope) == 0 {
                self.set_error("Nothing to reset there: none of those tasks are done".to_string());
            } else if self.reset_scope(scope) {
                self.overlay = None;
            }
        } else {
            // ConfirmQuitReset, offered on quit when all done: y resets
            // then quits, n quits without resetting, Esc/anything else cancels.
            match code {
                KeyCode::Char('y') => {
                    if self.reset_all() {
                        self.effects.push(Effect::Quit);
                    }
                }
                KeyCode::Char('n') => self.effects.push(Effect::Quit),
                _ => self.overlay = None,
            }
        }
    }

    /// The help overlay scrolls when it overflows a short terminal:
    /// the card scroll keys plus j/k and arrows scroll; every other
    /// key closes it.
    fn handle_help_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_help_down_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_help_up_by(1),
            KeyCode::Char('e') if ctrl => self.scroll_help_down_by(1),
            KeyCode::Char('y') if ctrl => self.scroll_help_up_by(1),
            KeyCode::Char('d') if ctrl => self.scroll_help_down_by(self.help_half_page()),
            KeyCode::Char('u') if ctrl => self.scroll_help_up_by(self.help_half_page()),
            KeyCode::PageDown => self.scroll_help_down_by(self.help_page()),
            KeyCode::PageUp => self.scroll_help_up_by(self.help_page()),
            _ => self.overlay = None,
        }
    }

    fn handle_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter => self.commit_search(),
            KeyCode::Backspace => {
                self.search.query.pop();
                self.update_search();
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.clear();
                self.update_search();
            }
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                delete_last_word(&mut self.search.query);
                self.update_search();
            }
            // Plain typed text edits the query (Shift for capitals is fine);
            // Ctrl-combos (e.g. Ctrl-H) are ignored rather than inserting
            // their base letter into the query.
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.push(c);
                self.update_search();
            }
            _ => {}
        }
    }

    fn handle_picker_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.picker_commit(),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Char('p') if ctrl => self.picker_move(-1),
            KeyCode::Char('n') if ctrl => self.picker_move(1),
            // Half-page jumps, matching the card-body Ctrl-D/Ctrl-U.
            KeyCode::Char('d') if ctrl => self.picker_move(self.picker_half_page()),
            KeyCode::Char('u') if ctrl => self.picker_move(-self.picker_half_page()),
            KeyCode::Char('w') if ctrl => {
                delete_last_word(&mut self.picker.query);
                self.picker.selection = 0;
            }
            KeyCode::Backspace => {
                self.picker.query.pop();
                self.picker.selection = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.picker.query.push(c);
                self.picker.selection = 0;
            }
            _ => {}
        }
    }

    /// Card-body viewport scroll, vim-style: Ctrl-E/Y one line, Ctrl-D/U
    /// half a page, PageDown/Up a page. Returns whether it handled the key.
    fn handle_checklist_scroll_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('e') => {
                    self.scroll_card_down_by(1);
                    return true;
                }
                KeyCode::Char('y') => {
                    self.scroll_card_up_by(1);
                    return true;
                }
                KeyCode::Char('d') => {
                    self.scroll_card_down_by(self.half_page());
                    return true;
                }
                KeyCode::Char('u') => {
                    self.scroll_card_up_by(self.half_page());
                    return true;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::PageDown => {
                self.scroll_card_down_by(self.page());
                true
            }
            KeyCode::PageUp => {
                self.scroll_card_up_by(self.page());
                true
            }
            _ => false,
        }
    }

    /// The single-key actions available from any non-modal screen: help/task
    /// picker overlays, quit (with the all-done reset offer), editor, reset,
    /// undo/redo, the `1`-`9` list jump, and the Tab/Shift-L/Shift-H
    /// incomplete-task/list jumps. Returns whether it handled the key.
    fn handle_global_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Open the help overlay from any non-modal screen, scrolled to
        // the top.
        if code == KeyCode::Char('?') {
            self.help.scroll = 0;
            self.overlay = Some(Overlay::Help);
            return true;
        }

        // Open the "go to task" overlay from any non-modal screen (`T`).
        if code == KeyCode::Char('T') {
            self.open_list_picker();
            return true;
        }

        if code == KeyCode::Char('q') {
            // When everything is done, offer to reset before quitting
            // instead of quitting outright.
            let (done, total) = self.document.checkbox_stats();
            if total > 0 && done == total {
                self.overlay = Some(Overlay::ConfirmQuitReset);
            } else {
                self.effects.push(Effect::Quit);
            }
            return true;
        }
        // As in vim, Esc cancels (search, the picker, help, dialogs and
        // prompts all take it first) and never quits; here, with nothing left
        // to cancel, it says how to quit rather than doing nothing.
        if code == KeyCode::Esc {
            self.set_error(QUIT_HINT.to_string());
            return true;
        }

        if code == KeyCode::Char('e') {
            let line = self.current_item().map(|item| item.line_number);
            self.effects.push(Effect::OpenEditor { line });
            return true;
        }
        if code == KeyCode::Char('R') {
            self.request_reset();
            return true;
        }

        // Undo / redo of state-changing actions. `u` undoes; `Ctrl-R`
        // redoes (vim-idiomatic). Guarded on the modifier so `u` never fires on
        // `Ctrl-U` (card scroll) and `R`/`Ctrl-R` stay distinct from `R` (reset).
        if code == KeyCode::Char('u') && !modifiers.contains(KeyModifiers::CONTROL) {
            self.undo();
            return true;
        }
        if code == KeyCode::Char('r') && modifiers.contains(KeyModifiers::CONTROL) {
            self.redo();
            return true;
        }
        if code == KeyCode::Char('o')
            && modifiers.contains(KeyModifiers::CONTROL)
            && self.view == View::Checklist
        {
            self.jump_back();
            return true;
        }

        if let KeyCode::Char(c @ '1'..='9') = code {
            let index = (c as u8 - b'1') as usize;
            self.jump_to_list(index);
            return true;
        }

        // Global jumps from any non-modal screen: Tab → next unfinished *task*
        // anywhere; Shift-L / Shift-H → next / prev unfinished *list*.
        if code == KeyCode::Tab {
            self.jump_to_next_incomplete_task();
            return true;
        }
        if code == KeyCode::Char('L') {
            self.jump_to_next_incomplete_list();
            return true;
        }
        if code == KeyCode::Char('H') {
            self.jump_to_prev_incomplete_list();
            return true;
        }

        false
    }

    /// All four home-row/arrow motions navigate between tasks: `h`/`l`/
    /// ←/→ read as the carousel sliding left/right, `j`/`k`/↑/↓ as walking the
    /// list-shaped overview. Card-body scrolling lives on Ctrl-E/Y/D/U and
    /// PageUp/Down (handled by `handle_checklist_scroll_key` before this).
    fn handle_checklist_key(&mut self, code: KeyCode, pending_g: bool) {
        match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.navigate_backward()
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('j') | KeyCode::Down => {
                self.navigate_forward()
            }
            // `gg` (two presses) → first task; `G` → last task.
            KeyCode::Char('g') => {
                if pending_g {
                    self.go_to_first_item();
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('G') => self.go_to_last_item(),
            // `}`/`{` jump to the next/previous `### H3`+ sub-section, vim
            // paragraph-motion style.
            KeyCode::Char('}') => self.jump_sub_section(true),
            KeyCode::Char('{') => self.jump_sub_section(false),
            // `/` opens incremental search; `n`/`N` cycle its matches.
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.search_cycle(true),
            KeyCode::Char('N') => self.search_cycle(false),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_current(),
            KeyCode::Char('s') => self.start_current(),
            KeyCode::Char('-') => self.skip_current(),
            KeyCode::Char('y') => self.copy_current_code(),
            KeyCode::Char('o') => self.open_current_link(),
            _ => {}
        }
    }

    fn handle_list_complete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('l')
            | KeyCode::Right
            | KeyCode::Char('j')
            | KeyCode::Down
            | KeyCode::Enter => self.advance_to_next_incomplete_list(),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.review_completed_list()
            }
            _ => self
                .set_error("List complete — press l for the next list or h to review".to_string()),
        }
    }

    fn handle_all_complete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.go_to_last_item()
            }
            _ => self.set_error(
                "All tasks complete — press h to review, R to reset or q to quit".to_string(),
            ),
        }
    }
}

/// A chord as the status bar names it, e.g. `Ctrl-S`, `Alt-1`, `Ctrl-Left`.
fn chord_name(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut name = String::new();
    for (flag, prefix) in [
        (KeyModifiers::CONTROL, "Ctrl-"),
        (KeyModifiers::ALT, "Alt-"),
        (KeyModifiers::SUPER, "Super-"),
        (KeyModifiers::HYPER, "Hyper-"),
        (KeyModifiers::META, "Meta-"),
    ] {
        if modifiers.contains(flag) {
            name.push_str(prefix);
        }
    }
    match code {
        KeyCode::Char(c) if c.is_ascii_alphabetic() => name.push(c.to_ascii_uppercase()),
        other => name.push_str(&other.to_string()),
    }
    name
}

fn list_all_done(list: &List) -> bool {
    list.items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Checkbox(_)))
        .all(|i| matches!(i.kind, ItemKind::Checkbox(state) if state.is_resolved()))
}

/// The verb naming a transition *into* `state`, used to build the git-sync
/// commit message: `Check "Restart service"`, `Start "..."`,
/// `Ctrl-W` in a prompt: drop trailing spaces, then the word before them,
/// as in the shell and vim's command line.
fn delete_last_word(query: &mut String) {
    let kept = query
        .trim_end()
        .trim_end_matches(|c: char| !c.is_whitespace());
    query.truncate(kept.len());
}

/// What a reload changed in task terms, for its status message (#140):
/// tasks whose state moved, by direction and named (the first two), and
/// tasks added or removed. Tasks are matched by list, label and depth, so a
/// task that only moved lines isn't counted. `None` when no task changed.
fn reload_changes(old: &Document, new: &Document) -> Option<String> {
    type Key = (String, String, usize);
    let key = |list: &List, item: &Item| (list.title.clone(), item.label(), item.depth);
    let mut before: std::collections::HashMap<Key, Vec<TaskState>> = Default::default();
    for list in &old.lists {
        for item in &list.items {
            if let ItemKind::Checkbox(state) = item.kind {
                before.entry(key(list, item)).or_default().push(state);
            }
        }
    }
    let (mut done, mut started, mut reopened, mut skipped, mut added) = (0, 0, 0, 0, 0);
    let mut names = Vec::new();
    for list in &new.lists {
        for item in &list.items {
            let ItemKind::Checkbox(state) = item.kind else {
                continue;
            };
            let Some(old_state) = before
                .get_mut(&key(list, item))
                .and_then(|states| (!states.is_empty()).then(|| states.remove(0)))
            else {
                added += 1;
                continue;
            };
            if old_state == state {
                continue;
            }
            *match state {
                TaskState::Done => &mut done,
                TaskState::Started => &mut started,
                TaskState::NotStarted => &mut reopened,
                TaskState::Skipped => &mut skipped,
            } += 1;
            names.push(item.label());
        }
    }
    let removed: usize = before.values().map(Vec::len).sum();
    let counted = |count: usize, what: &str| (count > 0).then(|| format!("{count} {what}"));
    let moved: Vec<String> = [
        counted(done, "done"),
        counted(started, "started"),
        counted(reopened, "reopened"),
        counted(skipped, "skipped"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut parts = Vec::new();
    if !moved.is_empty() {
        let mut shown: Vec<String> = names
            .iter()
            .take(2)
            .map(|name| format!("\"{name}\""))
            .collect();
        if names.len() > 2 {
            shown.push("…".to_string());
        }
        parts.push(format!("{} ({})", moved.join(", "), shown.join(", ")));
    }
    parts.extend(counted(added, "added"));
    parts.extend(counted(removed, "removed"));
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Space/Enter and the overview click toggle *done*: a done task reverts to
/// not started, anything else (started included) completes.
fn toggled_done(state: TaskState) -> TaskState {
    if state == TaskState::Done {
        TaskState::NotStarted
    } else {
        TaskState::Done
    }
}

fn toggle_message(state: TaskState) -> &'static str {
    if state == TaskState::Done {
        "Marked done"
    } else {
        "Marked not done"
    }
}

/// `Uncheck "..."`.
fn state_verb(state: TaskState) -> &'static str {
    match state {
        TaskState::Done => "Check",
        TaskState::Started => "Start",
        TaskState::NotStarted => "Uncheck",
        TaskState::Skipped => "Skip",
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
