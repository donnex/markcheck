use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::clipboard;
use crate::git_sync::{self, SyncOutcome};
use crate::model::{
    AppState, Document, GitSyncState, HelpState, IconSet, Item, ItemKind, List, Overlay,
    OverviewTarget, Palette, PendingSync, PickerState, SearchState, StateSnapshot, TaskState,
    UNDO_HISTORY_CAP, View, hash_bytes,
};
use crate::parser;
use crate::writer;

/// How long an ephemeral status message stays before it auto-clears.
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

const READ_ONLY_MESSAGE: &str = "File is read-only — change not saved (chmod u+w to allow saving)";
const HIDDEN_CHARACTERS_MESSAGE: &str = "Not copied: command contains hidden control characters";

/// Whether a cursor move lands on a card the user navigated to, so
/// `--auto-copy` fires, or is a side effect (undo, cancelling a search)
/// that must not touch the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMove {
    Navigate,
    Quiet,
}

/// The modification time and byte length of `path` from a single `stat()`
/// call, or `None` if it can't be read. Cross-checking both narrows (but
/// doesn't eliminate) the window where a same-instant external edit could be
/// mistaken for a no-op on a coarse-mtime filesystem.
fn current_stat(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Hash of `path`'s current on-disk bytes, or `None` if it can't be read.
/// See `model::hash_bytes`'s doc comment for what this check is (and isn't)
/// used for — a pre-write content check, not an atomic compare-and-swap.
fn current_content_hash(path: &Path) -> Option<[u8; 32]> {
    fs::read(path).ok().map(|bytes| hash_bytes(&bytes))
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
            should_quit: false,
            file_mtime,
            file_size,
            file_content_hash,
            last_update_at: None,
            git_sync: GitSyncState::default(),
            card_scroll: 0,
            card_max_scroll: 0,
            card_viewport_height: 0,
            card_rect: None,
            code_regions: Vec::new(),
            overview_rows: Vec::new(),
            pending_g: false,
            pending_open_link: false,
            pending_copy_code: false,
            icons: IconSet::nerd(),
            editor_requested: false,
            suspend_requested: false,
            clipboard_primary: false,
            auto_copy: false,
            // Named ANSI by default (theme-respecting); main.rs upgrades this
            // to the terminal's detected capability at startup.
            palette: Palette::basic(),
            file_deleted: false,
            search: SearchState::default(),
            picker: PickerState::default(),
            link_open_request: None,
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

    fn current_item_mut(&mut self) -> Option<&mut Item> {
        self.document.lists[self.current_list_index]
            .items
            .get_mut(self.current_item_index)
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
    /// overlay open (the query keeps showing in the status bar). No match or an
    /// empty query leaves the cursor put; no auto-copy mid-typing.
    fn update_search(&mut self) {
        let matches = self.find_matches(&self.search.query);
        if let Some((_, (list_index, item_index))) = self.search_target(&matches) {
            // Indices come from `find_matches`, so they are always in range.
            self.current_list_index = list_index;
            self.current_item_index = item_index;
            self.reset_card_scroll();
        }
    }

    /// Commit the search (`Enter`): store the query for `n`/`N`, close the
    /// overlay on the current match, and allow auto-copy now.
    /// Reports the match position, or a sticky "no matches" when the committed
    /// query matched nothing, so a committed search is never silent.
    fn commit_search(&mut self) {
        let matches = self.find_matches(&self.search.query);
        self.search.last = (!self.search.query.is_empty()).then(|| self.search.query.clone());
        self.overlay = None;
        if !self.search.query.is_empty() {
            match self.search_target(&matches) {
                None => self.set_error(format!("No matches for \"{}\"", self.search.query)),
                Some(_) if matches.len() == 1 => self.set_status("Match 1/1".to_string()),
                Some((position, _)) => {
                    // Nudge `n`/`N` here: they cycle matches once the search
                    // prompt is dismissed, but the always-on legend only
                    // advertises `/`, so a first-time user wouldn't know to
                    // reach for them.
                    self.set_status(format!("Match {position}/{} · n/N to cycle", matches.len()));
                }
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
    /// query relative to the cursor, wrapping around; a no-op with no active
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
        let target = if forward {
            matches.iter().copied().find(|&m| m > cur).unwrap_or(first)
        } else {
            matches
                .iter()
                .rev()
                .copied()
                .find(|&m| m < cur)
                .unwrap_or_else(|| {
                    *matches
                        .last()
                        .expect("matches is non-empty — checked above")
                })
        };
        let position = matches
            .iter()
            .position(|&m| m == target)
            .map_or(0, |i| i + 1);
        self.set_status(format!("Match {position}/{}", matches.len()));
        // A successful auto-copy (if enabled) overrides with its own message.
        self.focus_item(target.0, target.1, CursorMove::Navigate);
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
        self.picker.rows_area = None;
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
        (self.picker.viewport_height / 2).max(1) as isize
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
        let Some(area) = self.picker.rows_area else {
            return;
        };
        if !area.contains((col, row).into()) {
            return;
        }
        let index = self.picker.scroll_offset + usize::from(row - area.y);
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
        self.help.scroll = self.help.scroll.saturating_add(n).min(self.help.max_scroll);
    }
    fn scroll_help_up_by(&mut self, n: u16) {
        self.help.scroll = self.help.scroll.saturating_sub(n);
    }
    /// Half / near-full help viewport, for `Ctrl-D`/`Ctrl-U` and `PageDown`/`Up`
    /// — mirrors the card-body `half_page`/`page`.
    fn help_half_page(&self) -> u16 {
        (self.help.viewport_height / 2).max(1)
    }
    fn help_page(&self) -> u16 {
        self.help.viewport_height.saturating_sub(1).max(1)
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
    /// anywhere in the document, after the cursor, wrapping around; reports a
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
                self.focus_item(s, i, CursorMove::Navigate);
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
        let Some(last) = self.document.lists.len().checked_sub(1) else {
            return;
        };
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
        self.card_max_scroll = 0; // recomputed by the renderer next frame
    }

    /// Sets an **ephemeral** status message that auto-clears after
    /// `STATUS_TIMEOUT` of idle — copies, reload/reset info, hints.
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = Some(SystemTime::now() + STATUS_TIMEOUT);
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
    }

    /// Shared guard for every mutating action: once the watched file
    /// has been confirmed deleted (`reload_if_changed`), none of them may
    /// write, so we never silently recreate it behind the user's back.
    /// Sets the sticky error and returns `true` when blocked, so call sites
    /// read as `if self.blocked_by_deletion() { return ...; }`.
    fn blocked_by_deletion(&mut self) -> bool {
        if self.file_deleted {
            self.set_error(
                "File was deleted — changes cannot be saved until it exists again".to_string(),
            );
            true
        } else {
            false
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
        use clipboard::CopyOutcome;
        let primary = self.clipboard_primary;
        let outcome = self
            .current_item()
            .map(|item| clipboard::copy_item_code(item, primary));
        match outcome {
            Some(CopyOutcome::CopiedSystem) => self.set_status("Copied to clipboard".to_string()),
            Some(CopyOutcome::CopiedOsc52) => {
                self.set_status("Sent to clipboard (OSC 52)".to_string())
            }
            Some(CopyOutcome::HiddenCharacters) => {
                self.set_error(HIDDEN_CHARACTERS_MESSAGE.to_string())
            }
            _ => {}
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
        self.card_scroll = self.card_scroll.saturating_add(n).min(self.card_max_scroll);
    }

    /// Scroll the card body up by `n` wrapped lines, clamped to the top.
    pub fn scroll_card_up_by(&mut self, n: u16) {
        self.card_scroll = self.card_scroll.saturating_sub(n);
    }

    /// Half the current card viewport height (min 1), for `Ctrl-D`/`Ctrl-U`.
    fn half_page(&self) -> u16 {
        (self.card_viewport_height / 2).max(1)
    }

    /// A near-full card viewport (leaving one row of context), for
    /// `PageDown`/`PageUp`. Min 1.
    fn page(&self) -> u16 {
        self.card_viewport_height.saturating_sub(1).max(1)
    }

    /// Mouse wheel, routed per screen: on the checklist an overflowing card
    /// body takes absolute priority, otherwise the wheel steps between tasks
    /// like `k`/`j` (crossing list edges); it scrolls the `?` help overlay
    /// and moves the `T` picker highlight.
    pub fn handle_scroll_up(&mut self) {
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.card_max_scroll > 0 => self.scroll_card_up(),
            (None, View::Checklist) => self.navigate_backward(),
            (Some(Overlay::Help), _) => self.scroll_help_up_by(1),
            (Some(Overlay::ListPicker), _) => self.picker_move(-1),
            _ => {}
        }
    }

    pub fn handle_scroll_down(&mut self) {
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.card_max_scroll > 0 => self.scroll_card_down(),
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
                self.overview_rows
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
            .card_rect
            .is_some_and(|r| r.contains((col, row).into()))
        {
            self.copy_current_code();
        }
    }

    /// Copies an exact code fragment (a specific clicked row), bypassing
    /// the single-candidate rule; sets the same status strings as `y`.
    fn copy_specific(&mut self, text: &str) {
        use clipboard::CopyOutcome;
        match clipboard::copy_specific(text, self.clipboard_primary) {
            CopyOutcome::CopiedSystem => self.set_status("Copied to clipboard".to_string()),
            CopyOutcome::CopiedOsc52 => self.set_status("Sent to clipboard (OSC 52)".to_string()),
            CopyOutcome::TooLarge => self.set_error(
                "Not copied: this command is too large for the terminal clipboard".to_string(),
            ),
            CopyOutcome::HiddenCharacters => self.set_error(HIDDEN_CHARACTERS_MESSAGE.to_string()),
            // copy_specific only ever copies, is too large, or fails outright.
            _ => self.set_error("Copy failed: no clipboard available".to_string()),
        }
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

        // Space/Enter toggles *done*: a Done task reverts to NotStarted,
        // anything else (including Started) completes.
        let new_state = if state == TaskState::Done {
            TaskState::NotStarted
        } else {
            TaskState::Done
        };
        if !self.set_current_state(new_state) {
            return;
        }

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
        // Validate there's a checkbox to toggle (immutable borrow) before
        // recording an undo point, so a click on a missing/display-only row
        // doesn't push a no-op entry.
        let Some(item) = self
            .document
            .lists
            .get(list_index)
            .and_then(|list| list.items.get(item_index))
        else {
            return;
        };
        let ItemKind::Checkbox(state) = item.kind else {
            return; // nothing to toggle on a display-only item
        };
        let new_state = if state == TaskState::Done {
            TaskState::NotStarted
        } else {
            TaskState::Done
        };
        let item_text = item.label();

        let pre_state = self.state_snapshot();
        // Re-fetch mutably now that the snapshot is taken.
        if let Some(item) = self
            .document
            .lists
            .get_mut(list_index)
            .and_then(|list| list.items.get_mut(item_index))
        {
            item.kind = ItemKind::Checkbox(new_state);
        }

        if !self.commit_write(
            &pre_state,
            "Write failed — change not saved to disk",
            &format!("{} \"{item_text}\"", state_verb(new_state)),
        ) {
            return;
        }
        self.push_undo_point(pre_state);

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

        self.set_status(
            if new_state == TaskState::Done {
                "Marked done"
            } else {
                "Marked not done"
            }
            .to_string(),
        );
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

    /// Sets the current checkbox's state and writes it back. Returns whether
    /// the write succeeded.
    fn set_current_state(&mut self, state: TaskState) -> bool {
        if self.blocked_by_deletion() {
            return false;
        }
        let pre_state = self.state_snapshot();
        let item_text = self.current_item().map(Item::label).unwrap_or_default();
        if let Some(current) = self.current_item_mut() {
            current.kind = ItemKind::Checkbox(state);
        }
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
    /// sets a sticky error with `fail_msg`. On success, stashes `change_desc`
    /// as the pending git-sync request.
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
        let previous_content = fs::read_to_string(&self.document.file_path).ok();
        let Ok(content) = writer::write_back(&self.document) else {
            self.apply_snapshot(pre_state);
            if writer::is_read_only(&self.document.file_path) {
                self.set_error(READ_ONLY_MESSAGE.to_string());
            } else {
                self.set_error(fail_msg.to_string());
            }
            return false;
        };
        let (file_mtime, file_size) = current_stat(&self.document.file_path).unzip();
        self.file_mtime = file_mtime;
        self.file_size = file_size;
        let content_hash = hash_bytes(content.as_bytes());
        self.file_content_hash = Some(content_hash);
        self.last_update_at = Some(SystemTime::now());
        self.git_sync.pending = Some(PendingSync {
            content,
            content_hash,
            previous_content,
            description: change_desc.to_string(),
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
            self.navigate_next();
        }
    }

    /// Re-reads the file if its mtime or size changed. Cheap enough for every
    /// tick. Returns whether new content was loaded.
    pub fn reload_if_changed(&mut self) -> bool {
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            // Distinguish a confirmed deletion from a transient unreadable state.
            let is_deleted = fs::metadata(&self.document.file_path)
                .err()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound);
            if is_deleted && !self.file_deleted {
                self.file_deleted = true;
                self.set_error(
                    "File was deleted — changes cannot be saved until it exists again".to_string(),
                );
            }
            return false;
        };

        // File is accessible again. Clear the deleted flag and reload regardless
        // of mtime/size so the restored content is always picked up.
        let was_deleted = std::mem::take(&mut self.file_deleted);
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
        let was_deleted = std::mem::take(&mut self.file_deleted);
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
                self.document = new_document;
                let cancelled = self.keep_overlay_across_reload(search_origin);
                // An external edit is a hard boundary for undo history: it can
                // change task state and line numbers underneath us, so the
                // snapshots no longer apply cleanly.
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.history_epoch += 1;
                // A queued git-sync request describes content this reload replaced. The
                // new content queues nothing: markcheck doesn't commit edits it didn't
                // make.
                self.git_sync.pending = None;
                // The cursor may now be on a different card, so a half-typed chord must
                // not act on it.
                self.pending_g = false;
                self.pending_open_link = false;
                self.pending_copy_code = false;
                if let Some(cancelled) = cancelled {
                    self.set_error(format!("File changed on disk — {cancelled} cancelled"));
                } else {
                    let reloaded = if was_deleted {
                        "File restored — reloaded"
                    } else {
                        "Reloaded: file changed on disk"
                    };
                    if task_lost {
                        self.set_status(format!("{reloaded} — task not found, cursor moved"));
                    } else {
                        self.set_status(reloaded.to_string());
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
    /// `e` editor). `editor` is the resolved program name.
    pub fn request_external_edit_sync(&mut self, editor: &str) {
        let content = writer::document_contents(&self.document);
        let content_hash = hash_bytes(content.as_bytes());
        self.git_sync.pending = Some(PendingSync {
            // The editor wrote this, so the content being replaced is the content
            // carried.
            previous_content: Some(content.clone()),
            content,
            content_hash,
            description: format!("Edited in {editor}"),
        });
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

    /// Consumed by the main loop each tick.
    pub fn take_editor_request(&mut self) -> bool {
        std::mem::take(&mut self.editor_requested)
    }

    /// Consumed by the main loop each tick, like [`take_editor_request`].
    pub fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_requested)
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

    /// After the `e` editor returns: report a launch failure or a non-zero
    /// exit, then reload, queueing a git-sync request when the editor changed
    /// the file (that edit never went through `write_back`). `program` is the
    /// resolved editor.
    pub fn finish_editor(&mut self, program: &str, status: io::Result<std::process::ExitStatus>) {
        match status {
            Err(err) => self.set_error(format!("Failed to launch {program}: {err}")),
            Ok(status) if !status.success() => {
                self.set_error(format!("{program} exited with {status}"))
            }
            Ok(_) => {}
        }
        if self.reload_if_changed() {
            self.request_external_edit_sync(program);
        }
    }

    /// `o`: open the current card's link. With several links, arm a digit that
    /// picks link `[N]`. The spawn happens in the main loop.
    fn open_current_link(&mut self) {
        let urls: Vec<String> = self
            .current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();
        match urls.as_slice() {
            [url] if is_safe_link(url) => {
                self.link_open_request = Some(url.clone());
            }
            [url] => self.set_error(format!("Not opened: unsupported link type ({url})")),
            [] => self.set_error("Nothing to open: card has no link".to_string()),
            // Several links: arm a digit. No sticky message, which would hide the list
            // being chosen from.
            _ => self.pending_open_link = true,
        }
    }

    /// Opens link `[n]` (1-based) after `o` on a multi-link card.
    fn open_link_number(&mut self, n: usize) {
        let urls: Vec<String> = self
            .current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();
        match urls.get(n - 1) {
            Some(url) if is_safe_link(url) => {
                self.link_open_request = Some(url.clone());
            }
            Some(url) => self.set_error(format!("Not opened: unsupported link type ({url})")),
            None => self.set_error(format!("No link [{n}] on this card")),
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
        use clipboard::CopyOutcome;
        match clipboard::copy_specific(url, self.clipboard_primary) {
            CopyOutcome::CopiedSystem => {
                self.set_status("No browser here — link copied to clipboard".to_string())
            }
            CopyOutcome::CopiedOsc52 => {
                self.set_status("No browser here — link sent to clipboard (OSC 52)".to_string())
            }
            CopyOutcome::TooLarge => self.set_error(
                "No browser here, and the link is too large for the terminal clipboard".to_string(),
            ),
            CopyOutcome::HiddenCharacters => self.set_error(
                "No browser here, and the link contains hidden control characters — not copied"
                    .to_string(),
            ),
            _ => self.set_error(format!("No browser or clipboard here — link: {url}")),
        }
    }

    /// Consumed by the main loop each tick: the URL `o` asked to open.
    pub fn take_link_open_request(&mut self) -> Option<String> {
        self.link_open_request.take()
    }

    /// The most recent write-back's git-sync request, drained by the main loop
    /// whether or not git-sync is active.
    pub fn take_git_sync_request(&mut self) -> Option<PendingSync> {
        self.git_sync.pending.take()
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
            SyncOutcome::Skipped | SyncOutcome::RetryAbandoned => return,
            SyncOutcome::SkippedUntracked => ("not tracked", None),
            SyncOutcome::CommittedNotPushed { message, .. } | SyncOutcome::RetryFailed(message) => {
                ("not pushed", Some(message))
            }
            SyncOutcome::Failed(_) => ("not synced", None),
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
        if done == 0 && started == 0 {
            self.set_error("Nothing to reset: no tasks are done".to_string());
            return;
        }
        self.overlay = Some(Overlay::ConfirmReset);
    }

    /// Resets every task to not done and saves. Returns whether the reset
    /// reached disk; on failure the sticky error explains why.
    fn reset_all(&mut self) -> bool {
        if self.blocked_by_deletion() {
            return false;
        }
        let pre_state = self.state_snapshot();
        for list in &mut self.document.lists {
            for item in &mut list.items {
                if let ItemKind::Checkbox(_) = item.kind {
                    item.kind = ItemKind::Checkbox(TaskState::NotStarted);
                }
            }
        }
        if !self.commit_write(
            &pre_state,
            "Write failed — reset not saved to disk",
            "Reset all tasks to not done",
        ) {
            return false;
        }
        self.push_undo_point(pre_state);
        self.view = View::Checklist;
        self.set_status("All tasks reset to not done".to_string());
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
        use clipboard::CopyOutcome;

        let Some(current) = self.current_item() else {
            return;
        };
        match clipboard::copy_item_code(current, self.clipboard_primary) {
            CopyOutcome::CopiedSystem => self.set_status("Copied to clipboard".to_string()),
            CopyOutcome::CopiedOsc52 => self.set_status("Sent to clipboard (OSC 52)".to_string()),
            CopyOutcome::NoCandidates => {
                self.set_error("Nothing to copy: item has no code".to_string())
            }
            // Several commands: arm a digit that picks command `[N]`. No sticky
            // message, so the status bar shows the prompt instead.
            CopyOutcome::Ambiguous(_) => self.pending_copy_code = true,
            CopyOutcome::TooLarge => self.set_error(
                "Not copied: this command is too large for the terminal clipboard".to_string(),
            ),
            CopyOutcome::HiddenCharacters => self.set_error(HIDDEN_CHARACTERS_MESSAGE.to_string()),
            CopyOutcome::Failed => {
                self.set_error("Copy failed: no clipboard available".to_string())
            }
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
        self.clear_status();

        // Ctrl-Z suspends from every screen, overlays and search input
        // included, as in other full-screen terminal programs.
        if code == KeyCode::Char('z') && modifiers.contains(KeyModifiers::CONTROL) {
            self.clear_pending_chords();
            self.suspend_requested = true;
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
            (None, View::Checklist) => &['e', 'y', 'd', 'u', 'r'],
            (None, View::ListComplete | View::AllComplete) => &['r'],
            (Some(Overlay::Help), _) => &['e', 'y', 'd', 'u'],
            (Some(Overlay::ListPicker), _) => &['p', 'n', 'd', 'u'],
            (Some(Overlay::Search | Overlay::ConfirmReset | Overlay::ConfirmQuitReset), _) => &[],
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
            None => self.set_error("Type q to quit".to_string()),
        }
    }

    /// A pending `o` waits for a link digit. Runs before the digit list jump and
    /// the Esc quit. Any other key cancels the prompt and still takes its normal
    /// action, returning `false`.
    fn handle_pending_open_link_key(&mut self, code: KeyCode) -> bool {
        if !std::mem::take(&mut self.pending_open_link) {
            return false;
        }
        match code {
            KeyCode::Char(c @ '1'..='9') => {
                self.open_link_number((c as u8 - b'0') as usize);
                true
            }
            KeyCode::Esc => true,
            _ => false,
        }
    }

    /// A pending `y` waits for a command digit, exactly like `o`'s link prompt.
    fn handle_pending_copy_code_key(&mut self, code: KeyCode) -> bool {
        if !std::mem::take(&mut self.pending_copy_code) {
            return false;
        }
        match code {
            KeyCode::Char(c @ '1'..='9') => {
                self.copy_code_number((c as u8 - b'0') as usize);
                true
            }
            KeyCode::Esc => true,
            _ => false,
        }
    }

    /// A failed reset leaves either dialog open, so its error is read first.
    fn handle_confirm_key(&mut self, overlay: Overlay, code: KeyCode) {
        if overlay == Overlay::ConfirmReset {
            // Only `y` confirms — `Enter` is too easy to hit by reflex for a
            // whole-file rewrite. Any other key cancels.
            match code {
                KeyCode::Char('y') => {
                    if self.reset_all() {
                        self.overlay = None;
                    }
                }
                _ => self.overlay = None,
            }
        } else {
            // ConfirmQuitReset, offered on quit when all done: y resets
            // then quits, n quits without resetting, Esc/anything else cancels.
            match code {
                KeyCode::Char('y') => self.should_quit = self.reset_all(),
                KeyCode::Char('n') => self.should_quit = true,
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

        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            // When everything is done, offer to reset before quitting
            // instead of quitting outright.
            let (done, total) = self.document.checkbox_stats();
            if total > 0 && done == total {
                self.overlay = Some(Overlay::ConfirmQuitReset);
            } else {
                self.should_quit = true;
            }
            return true;
        }

        if code == KeyCode::Char('e') {
            self.editor_requested = true;
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
        .all(|i| matches!(i.kind, ItemKind::Checkbox(TaskState::Done)))
}

/// The verb naming a transition *into* `state`, used to build the git-sync
/// commit message: `Check "Restart service"`, `Start "..."`,
/// `Uncheck "..."`.
fn state_verb(state: TaskState) -> &'static str {
    match state {
        TaskState::Done => "Check",
        TaskState::Started => "Start",
        TaskState::NotStarted => "Uncheck",
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
