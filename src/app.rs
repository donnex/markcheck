use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::clipboard;
use crate::model::{
    AppState, Document, GitSyncState, HelpState, IconSet, Item, ItemKind, List, OverviewTarget,
    Palette, PendingSync, PickerState, Screen, SearchState, StateSnapshot, TaskState,
    UNDO_HISTORY_CAP,
};
use crate::parser;
use crate::writer;

/// How long an ephemeral status message stays before it auto-clears.
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

/// The modification time and byte length of `path` from a single `stat()`
/// call, or `None` if it can't be read. Cross-checking both narrows (but
/// doesn't eliminate) the window where a same-instant external edit could be
/// mistaken for a no-op on a coarse-mtime filesystem.
fn current_stat(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// A deterministic (within this process) content hash, used to detect
/// whether the file on disk still matches what we last saw before we
/// overwrite it. `DefaultHasher::new()` always starts from the same fixed
/// keys, so two calls in the same run are comparable — this is a
/// compare-and-swap guard, not a cryptographic or cross-process value.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Hash of `path`'s current on-disk bytes, or `None` if it can't be read.
fn current_content_hash(path: &Path) -> Option<u64> {
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
/// main `/` search stays unchanged.
fn picker_match_text(item: &Item) -> String {
    let mut text = item.search_text();
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
    /// Wraps a parsed [`Document`] for interactive use.
    ///
    /// **Invariant:** `document.lists` must be non-empty. `AppState` navigation
    /// (e.g. [`AppState::current_list`]) indexes `lists` by `current_list_index`
    /// without a bounds fallback, so a list-less document would panic on the
    /// first access. Both entry points uphold this: startup rejects a list-less
    /// file in `main.rs`, and reload refuses a new document with no lists
    /// (`reload_if_changed`). The `debug_assert!` below turns a future violation
    /// into a clear, immediate failure in debug/test builds instead of an
    /// opaque out-of-bounds panic somewhere downstream.
    pub fn new(document: Document) -> Self {
        debug_assert!(
            !document.lists.is_empty(),
            "AppState requires a document with at least one list; the empty case \
             is rejected at load (main.rs) and reload (reload_if_changed)",
        );
        let (file_mtime, file_size) = current_stat(&document.file_path).unzip();
        let file_content_hash = current_content_hash(&document.file_path);
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
            screen: Screen::Checklist,
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
            icons: IconSet::nerd(),
            editor_requested: false,
            screen_before_confirm: Screen::Checklist,
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
        let last_index = self.current_list().items.len().saturating_sub(1);
        if self.current_item_index < last_index {
            self.current_item_index += 1;
        }
        self.reset_card_scroll();
        self.maybe_auto_copy();
    }

    pub fn navigate_prev(&mut self) {
        self.current_item_index = self.current_item_index.saturating_sub(1);
        self.reset_card_scroll();
        self.maybe_auto_copy();
    }

    pub fn jump_to_list(&mut self, index: usize) {
        if index < self.document.lists.len() {
            self.current_list_index = index;
            // Land on the first not-done item in the target list,
            // falling back to its first item when all are done.
            self.current_item_index = first_undone_index(&self.document.lists[index]).unwrap_or(0);
            self.screen = Screen::Checklist;
            self.reset_card_scroll();
            self.maybe_auto_copy();
        }
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
    /// screen, resetting card scroll. Unlike [`AppState::jump_to_list`]
    /// this lands on the given item, not the list's first not-done one, and
    /// does not auto-copy — callers decide when a copy should fire. Out-of-range
    /// indices are clamped/ignored so a stale match can't panic.
    pub fn focus_item(&mut self, list_index: usize, item_index: usize) {
        let Some(list) = self.document.lists.get(list_index) else {
            return;
        };
        if list.items.is_empty() {
            return;
        }
        self.current_list_index = list_index;
        self.current_item_index = item_index.min(list.items.len() - 1);
        self.screen = Screen::Checklist;
        self.reset_card_scroll();
    }

    /// Enter incremental search (`/`): remember the cursor for a possible
    /// `Esc` restore, clear the query, and switch to the `Search` screen.
    fn start_search(&mut self) {
        self.search.origin = (self.current_list_index, self.current_item_index);
        self.search.query.clear();
        self.screen = Screen::Search;
    }

    /// Re-run the live query and jump the cursor to the first match, staying on
    /// the `Search` screen (the query keeps showing in the status bar). No match
    /// or an empty query leaves the cursor put; no auto-copy mid-typing.
    fn update_search(&mut self) {
        if let Some(&(list_index, item_index)) = self.find_matches(&self.search.query).first() {
            // Indices come from `find_matches`, so they are always in range.
            self.current_list_index = list_index;
            self.current_item_index = item_index;
            self.reset_card_scroll();
        }
    }

    /// Commit the search (`Enter`): store the query for `n`/`N`, return to the
    /// `Checklist` screen on the current match, and allow auto-copy now.
    /// Reports the match position, or a sticky "no matches" when the committed
    /// query matched nothing, so a committed search is never silent.
    fn commit_search(&mut self) {
        let matches = self.find_matches(&self.search.query);
        self.search.last = (!self.search.query.is_empty()).then(|| self.search.query.clone());
        self.screen = Screen::Checklist;
        if !self.search.query.is_empty() {
            if matches.is_empty() {
                self.set_error(format!("No matches for \"{}\"", self.search.query));
            } else if matches.len() == 1 {
                self.set_status("Match 1/1".to_string());
            } else {
                // Nudge `n`/`N` here: they cycle matches once the search
                // prompt is dismissed, but the always-on legend only advertises
                // `/`, so a first-time user wouldn't know to reach for them.
                self.set_status(format!("Match 1/{} · n/N to cycle", matches.len()));
            }
        }
        // A successful auto-copy (if enabled) overrides with its own message.
        self.maybe_auto_copy();
    }

    /// Cancel the search (`Esc`): restore the pre-search cursor and return to
    /// the `Checklist` screen.
    fn cancel_search(&mut self) {
        let (list_index, item_index) = self.search.origin;
        self.current_list_index = list_index;
        self.current_item_index = item_index;
        self.reset_card_scroll();
        self.screen = Screen::Checklist;
    }

    /// Cycle to the next (`forward`) or previous match of the last committed
    /// query relative to the cursor, wrapping around; a no-op with no active
    /// search or no matches (`n`/`N`). Matches from `find_matches` are in
    /// document order, so `(list, item)` tuples compare in that order.
    fn search_cycle(&mut self, forward: bool) {
        let Some(query) = self.search.last.clone() else {
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
        self.focus_item(target.0, target.1);
        let position = matches
            .iter()
            .position(|&m| m == target)
            .map_or(0, |i| i + 1);
        self.set_status(format!("Match {position}/{}", matches.len()));
        // A successful auto-copy (if enabled) overrides with its own message.
        self.maybe_auto_copy();
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
                self.focus_item(list_index, i);
                let name = self.document.lists[list_index].items[i]
                    .section
                    .last()
                    .map_or_else(String::new, |h| h.text.clone());
                self.set_status(format!("Sub-section: {name}"));
                // A successful auto-copy (if enabled) overrides with its message.
                self.maybe_auto_copy();
            }
            None => {
                let edge = if forward { "last" } else { "first" };
                self.set_error(format!("Already at the {edge} sub-section"));
            }
        }
    }

    /// Open the "go to task" overlay (`T`): remember the current screen to
    /// return to on `Esc`, clear the filter, and reset the selection.
    fn open_list_picker(&mut self) {
        self.screen_before_confirm = self.screen;
        self.picker.query.clear();
        self.picker.selection = 0;
        self.screen = Screen::ListPicker;
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

    /// Jump to the highlighted task (`Enter`), or just close the overlay when
    /// the filter matched nothing.
    fn picker_commit(&mut self) {
        match self.picker_matches().get(self.picker.selection) {
            Some(&(list_index, item_index)) => {
                self.focus_item(list_index, item_index);
                self.maybe_auto_copy();
            }
            None => self.screen = self.screen_before_confirm,
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
                self.current_list_index = s;
                self.current_item_index = i;
                self.screen = Screen::Checklist;
                self.reset_card_scroll();
                return;
            }
        }
        self.set_error("No unfinished tasks".to_string());
    }

    /// `gg`: jump to the very first task in the document — a literal position,
    /// not first-undone (a raw vim motion).
    pub fn go_to_first_item(&mut self) {
        if self.document.lists.is_empty() {
            return;
        }
        self.current_list_index = 0;
        self.current_item_index = 0;
        self.screen = Screen::Checklist;
        self.reset_card_scroll();
    }

    /// `G`: jump to the very last task in the document (literal position).
    pub fn go_to_last_item(&mut self) {
        let Some(last) = self.document.lists.len().checked_sub(1) else {
            return;
        };
        self.current_list_index = last;
        self.current_item_index = self.document.lists[last].items.len().saturating_sub(1);
        self.screen = Screen::Checklist;
        self.reset_card_scroll();
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

    /// Sets a **sticky, error-colored** status message: failures
    /// (copy/reload/open/editor) and "your keypress did nothing" feedback (a `y`
    /// with no code or an ambiguous set, an `o` with no/unsupported link, an `R`
    /// with nothing to reset, a search with no matches). Persists until the
    /// next input, so it can't silently vanish before it's read, and the
    /// status bar renders it in `palette.error` so problems stand out from
    /// passive confirmations.
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

    /// Cancels any armed input chord (`gg` via `pending_g`, `o`'s digit prompt
    /// via `pending_open_link`) along with the status message. Mouse
    /// input doesn't go through `handle_key_with_mods` — the only place that
    /// otherwise consumes these flags — so without this, a click between the
    /// two halves of a chord left it armed to misfire against whatever the
    /// click just did (e.g. jump to a different card, then have a stray digit
    /// press treated as that *new* card's link-open prompt).
    fn clear_pending_chords(&mut self) {
        self.clear_status();
        self.pending_g = false;
        self.pending_open_link = false;
    }

    /// Shared guard for every mutating action: once the watched file
    /// has been confirmed deleted (`reload_if_changed`), none of them may
    /// write, so we never silently recreate it behind the user's back.
    /// Sets the sticky error and returns `true` when blocked, so call sites
    /// read as `if self.blocked_by_deletion() { return ...; }`.
    fn blocked_by_deletion(&mut self) -> bool {
        if self.file_deleted {
            self.set_error("File was deleted — cannot save changes".to_string());
            true
        } else {
            false
        }
    }

    /// Clears an ephemeral status message once its deadline has passed;
    /// called each event-loop tick. Sticky messages (no expiry) are untouched.
    pub fn expire_status(&mut self, now: SystemTime) {
        if self.status_expiry.is_some_and(|exp| now >= exp) {
            self.clear_status();
        }
    }

    /// With `--auto-copy`, copy the current card's code when it has exactly
    /// one candidate. On a successful copy, surface the same status
    /// message as manual `y`; stay silent for no-code / ambiguous /
    /// failure so passive navigation isn't spammed.
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

    /// Mouse wheel: an overflowing card body takes absolute wheel
    /// priority; otherwise the wheel navigates between items.
    pub fn handle_scroll_up(&mut self) {
        self.clear_pending_chords();
        if self.screen == Screen::Checklist {
            if self.card_max_scroll > 0 {
                self.scroll_card_up();
            } else {
                self.navigate_prev();
            }
        }
    }

    pub fn handle_scroll_down(&mut self) {
        self.clear_pending_chords();
        if self.screen == Screen::Checklist {
            if self.card_max_scroll > 0 {
                self.scroll_card_down();
            } else {
                self.navigate_next();
            }
        }
    }

    /// Left-click at `(col, row)`: a click inside the current card
    /// copies its command, exactly like `y` — copying its sole code candidate
    /// or showing the same hint when there are none or several. Clicks outside
    /// the card, or off the checklist, are ignored.
    pub fn handle_left_click(&mut self, col: u16, row: u16) {
        self.clear_pending_chords();

        // A click on an overview row jumps the cursor there. Handled on the
        // content screens (checklist + completion), where the overview is a plain
        // navigation aid — not while a modal (Help/Search/picker/confirm) is up,
        // even though the overview is still drawn beside those overlays.
        let overview_target = matches!(
            self.screen,
            Screen::Checklist | Screen::ListComplete | Screen::AllComplete
        )
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
                    // Mirror the go-to-task picker: focus the exact item,
                    // then honour --auto-copy (a no-op otherwise).
                    self.focus_item(list_index, item_index);
                    self.maybe_auto_copy();
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
        if self.screen != Screen::Checklist {
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
            // copy_specific only ever copies or fails outright.
            _ => self.set_error("Copy failed: no clipboard available".to_string()),
        }
    }

    pub fn toggle_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let kind = current.kind;
        let state = match kind {
            // Space/Enter can't toggle an info card, so page past it like
            // `l`/next instead of doing nothing — crossing into the
            // next list at a list edge, per `navigate_forward`.
            ItemKind::DisplayOnly => {
                self.navigate_forward();
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
            self.screen = Screen::Checklist;
        }
    }

    /// Toggle an *arbitrary* task done/not-done in place, without moving the
    /// cursor or the card view (the overview marker click). Uses the
    /// same done↔not-started rule as `toggle_current`, writes back, and reports
    /// a confirmation since the toggled task may not be the focused card. A
    /// non-task (display-only) index is ignored. Unlike the keyboard toggle it
    /// never *promotes* to a completion screen (that would steal focus), but it
    /// does drop a now-stale completion screen back to the checklist.
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
        let item_text = item
            .header
            .as_deref()
            .unwrap_or(&item.display_text)
            .to_string();

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
        let still_complete = match self.screen {
            Screen::AllComplete => self.document.lists.iter().all(list_all_done),
            Screen::ListComplete => list_all_done(self.current_list()),
            _ => true,
        };
        if !still_complete {
            self.screen = Screen::Checklist;
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

    /// `s` toggles the *started* state: Started reverts to NotStarted,
    /// anything else (including Done) becomes Started. Never completes and
    /// never auto-advances — the cursor stays put on `Checklist`.
    pub fn start_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let state = match current.kind {
            ItemKind::DisplayOnly => return,
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
        self.screen = Screen::Checklist;
    }

    /// Sets the current checkbox's state and writes it back, refreshing the
    /// self-write mtime guard and the "Updated" tag on success. Returns
    /// whether the write succeeded — callers should skip any further
    /// success-implying screen transition when it didn't, since a
    /// write failure here also sets a sticky error status.
    fn set_current_state(&mut self, state: TaskState) -> bool {
        if self.blocked_by_deletion() {
            return false;
        }
        let pre_state = self.state_snapshot();
        let item_text = self
            .current_item()
            .map(|item| {
                item.header
                    .as_deref()
                    .unwrap_or(&item.display_text)
                    .to_string()
            })
            .unwrap_or_default();
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

    /// Writes the document back to disk, refreshing the self-write mtime
    /// guard and the "Updated" tag on success. Every mutating action
    /// (toggle/start/reset/undo/redo) ends with exactly this sequence, so
    /// it's shared here rather than repeated at each call site. On failure,
    /// restores the document to `pre_state` (the checkbox snapshot captured
    /// before the mutation) before reporting the error — a failed write
    /// must never leave memory diverged from what's actually on disk, since
    /// that phantom state can otherwise persist for the rest of the session
    /// (the file watcher only reloads on a real mtime/size change, and a
    /// failed write never touches the file) and even cascade into a false
    /// "all complete" screen. Returns whether the write succeeded; on
    /// failure sets a sticky error with `fail_msg` so callers can bail out
    /// with `if !self.commit_write(...) { return; }`. On success,
    /// `change_desc` (e.g. `Check "Restart service"`) is stashed as the
    /// pending git-sync request — consumed by the main loop via
    /// `take_git_sync_request`, regardless of whether git-sync is actually
    /// active for this file.
    fn commit_write(
        &mut self,
        pre_state: &StateSnapshot,
        fail_msg: &str,
        change_desc: &str,
    ) -> bool {
        if self.disk_content_diverged() {
            self.apply_snapshot(pre_state);
            self.force_reload();
            self.set_error(
                "File changed on disk — reloaded; change not saved, please retry".to_string(),
            );
            return false;
        }
        let Ok(content) = writer::write_back(&self.document) else {
            self.apply_snapshot(pre_state);
            self.set_error(fail_msg.to_string());
            return false;
        };
        let (file_mtime, file_size) = current_stat(&self.document.file_path).unzip();
        self.file_mtime = file_mtime;
        self.file_size = file_size;
        self.file_content_hash = Some(hash_bytes(content.as_bytes()));
        self.last_update_at = Some(SystemTime::now());
        self.git_sync.pending = Some(PendingSync {
            content,
            description: change_desc.to_string(),
        });
        true
    }

    /// True when the file's current on-disk content no longer matches
    /// `file_content_hash` — the content we last confirmed was there, at
    /// load or after our own last write/reload. A mismatch means something
    /// else (another markcheck instance, an external editor) changed the
    /// file since, so writing now would silently discard that change — the
    /// classic lost-update problem for two writers sharing one file, which
    /// `file_mtime`/`file_size` alone can miss within a single coarse-mtime
    /// timestamp tick. Fails open (`false`, i.e. "proceed") when the file
    /// can't be read at all: that's a distinct failure `write_back` itself
    /// is about to surface with its own, more specific error.
    fn disk_content_diverged(&self) -> bool {
        match current_content_hash(&self.document.file_path) {
            Some(hash) => Some(hash) != self.file_content_hash,
            None => false,
        }
    }

    fn maybe_transition_screen(&mut self) {
        if list_all_done(self.current_list()) {
            let all_done = self.document.lists.iter().all(list_all_done);
            self.screen = if all_done {
                Screen::AllComplete
            } else {
                Screen::ListComplete
            };
        } else {
            self.navigate_next();
        }
    }

    /// Re-reads the file if its mtime or size differs from what we last saw.
    /// Cheap enough to call every event-loop tick since it's just a stat()
    /// call when nothing has changed. The size cross-check (alongside mtime)
    /// narrows the window where a same-instant external edit could be missed
    /// on a coarse-mtime filesystem (whole-second resolution is common) — an
    /// edit landing in the same resolution window as our own write, or a
    /// prior external edit, that also happens to leave the size unchanged is
    /// still missed, but that's a much narrower coincidence than mtime alone.
    /// Returns whether new content was actually loaded (as opposed to
    /// nothing changing, or a reload being skipped/failed) — callers that
    /// themselves triggered the on-disk change (e.g. `e`, the editor) use
    /// this to know whether there's now something worth a git-sync request;
    /// see [`request_external_edit_sync`](Self::request_external_edit_sync).
    pub fn reload_if_changed(&mut self) -> bool {
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            // Distinguish a confirmed deletion from a transient unreadable state.
            let is_deleted = fs::metadata(&self.document.file_path)
                .err()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound);
            if is_deleted && !self.file_deleted {
                self.file_deleted = true;
                self.set_error("File deleted — changes cannot be saved".to_string());
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

    /// Unconditionally re-reads and applies the file, bypassing the
    /// mtime/size "unchanged" short-circuit in [`reload_if_changed`]. Used
    /// by [`commit_write`](Self::commit_write) when `disk_content_diverged`
    /// has already proven the file changed via its content hash — mtime/size
    /// can still read as "unchanged" on a coarse-mtime filesystem even
    /// though the hash caught a real change, and in that case
    /// `reload_if_changed` would otherwise wrongly skip the reload, leaving
    /// the conflict undetected on every retry.
    fn force_reload(&mut self) {
        // The file vanishing in the instant between disk_content_diverged's
        // read and this stat is a real but effectively untestable race
        // (nothing yields between the two calls); reload_if_changed's own
        // next tick will pick up and report the deletion normally.
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            return;
        };
        let was_deleted = std::mem::take(&mut self.file_deleted);
        self.reload_from_disk(modified, size, was_deleted);
    }

    /// Shared reload body for [`reload_if_changed`] and [`force_reload`]:
    /// parses the file fresh, swaps it in on success, and refreshes every
    /// piece of "what we last saw on disk" state (mtime/size/content hash)
    /// so a subsequent write or conflict check compares against the file as
    /// it now stands.
    fn reload_from_disk(&mut self, modified: SystemTime, size: u64, was_deleted: bool) -> bool {
        let reloaded = match parser::parse_document(self.document.file_path.clone()) {
            Ok(new_document) if !new_document.lists.is_empty() => {
                self.remap_position(&new_document);
                self.document = new_document;
                self.screen = Screen::Checklist;
                // An external edit is a hard boundary for undo history: it can
                // change task state and line numbers underneath us, so the
                // snapshots no longer apply cleanly.
                self.undo_stack.clear();
                self.redo_stack.clear();
                let msg = if was_deleted {
                    "File restored — reloaded".to_string()
                } else {
                    "Reloaded: file changed on disk".to_string()
                };
                self.set_status(msg);
                self.last_update_at = Some(SystemTime::now());
                true
            }
            Ok(_) => {
                self.set_status("Reload skipped: file has no checklist items".to_string());
                false
            }
            Err(err) => {
                self.set_error(format!("Reload failed: {err}"));
                false
            }
        };

        self.file_mtime = Some(modified);
        self.file_size = Some(size);
        self.file_content_hash = current_content_hash(&self.document.file_path);
        reloaded
    }

    /// Queues a git-sync request for an edit that happened *outside*
    /// `write_back` — currently just `e`, the external editor — so it isn't
    /// silently left uncommitted until (or missed entirely, if there isn't)
    /// some later markcheck-driven change. Mirrors `commit_write`'s
    /// unconditional stash: `AppState` has no notion of whether git-sync is
    /// even active, `main.rs` decides that when it drains the request.
    /// `editor` is the resolved program name (e.g. `vim`, `code`), not the
    /// literal `$EDITOR`/`$VISUAL` env var.
    pub fn request_external_edit_sync(&mut self, editor: &str) {
        self.git_sync.pending = Some(PendingSync {
            content: writer::document_contents(&self.document),
            description: format!("Edited in {editor}"),
        });
    }

    /// Keeps the cursor on the same list (by title) and item (by line
    /// number) after a reload, falling back to the start of the document
    /// when either can no longer be found in the new content.
    fn remap_position(&mut self, new_document: &Document) {
        let old_title = self
            .document
            .lists
            .get(self.current_list_index)
            .map(|s| s.title.clone());
        let old_line_number = self.current_item().map(|item| item.line_number);

        let new_list_index = old_title
            .as_deref()
            .and_then(|title| new_document.lists.iter().position(|s| s.title == title))
            .unwrap_or(0)
            .min(new_document.lists.len().saturating_sub(1));

        let new_item_index = old_line_number
            .and_then(|line| {
                new_document.lists[new_list_index]
                    .items
                    .iter()
                    .position(|item| item.line_number == line)
            })
            .unwrap_or(0);

        self.current_list_index = new_list_index;
        self.current_item_index = new_item_index;
        self.reset_card_scroll();
        self.maybe_auto_copy();
    }

    /// From a finished list, jump to the next list that still has
    /// incomplete work (landing on its first not-done item), skipping any
    /// already-complete lists in between. If none remain, show the
    /// all-complete summary.
    fn advance_to_next_incomplete_list(&mut self) {
        match self.next_incomplete_list_after(self.current_list_index) {
            Some(index) => self.jump_to_list(index),
            None => self.screen = Screen::AllComplete,
        }
    }

    fn return_to_last_item_of_list(&mut self) {
        self.screen = Screen::Checklist;
    }

    fn return_to_last_list(&mut self) {
        let last = self.document.lists.len().saturating_sub(1);
        self.current_list_index = last;
        self.current_item_index = self.document.lists[last].items.len().saturating_sub(1);
        self.screen = Screen::Checklist;
        self.reset_card_scroll();
    }

    /// Consumed by the main loop each tick.
    pub fn take_editor_request(&mut self) -> bool {
        std::mem::take(&mut self.editor_requested)
    }

    /// `o`: open the current card's link. Opens when the card has exactly
    /// one link with a safe scheme. With several links, `o` arms a follow-up
    /// digit that opens link `[N]` (matching the card's `[N]` markers);
    /// otherwise it gives a sticky hint (nothing to open, or unsupported scheme)
    /// so the keypress is never silent. The actual spawn happens in the main
    /// loop via `take_link_open_request` (keeping `app.rs` process-free).
    fn open_current_link(&mut self) {
        let urls: Vec<String> = self
            .current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();
        match urls.as_slice() {
            [url] if is_safe_link(url) => {
                self.set_status(format!("Opening {url}"));
                self.link_open_request = Some(url.clone());
            }
            [url] => self.set_error(format!("Not opened: unsupported link type ({url})")),
            [] => self.set_error("Nothing to open: card has no link".to_string()),
            // Several links: keep the numbered list visible in the status bar
            // and arm a digit to pick one. No sticky message — that would
            // hide the very list the user is choosing from; the status bar adds
            // a short `press 1–N …` prompt while `pending_open_link` is set.
            _ => self.pending_open_link = true,
        }
    }

    /// Opens link `[n]` (1-based) after `o` on a multi-link card. Mirrors
    /// the single-link `o` result: a safe scheme opens; an unsupported scheme or
    /// an out-of-range number gives a sticky hint. `n` is always 1–9 here (the
    /// arming digit), so `n - 1` is the URL index.
    fn open_link_number(&mut self, n: usize) {
        let urls: Vec<String> = self
            .current_item()
            .map(|item| item.link_urls().iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();
        match urls.get(n - 1) {
            Some(url) if is_safe_link(url) => {
                self.set_status(format!("Opening {url}"));
                self.link_open_request = Some(url.clone());
            }
            Some(url) => self.set_error(format!("Not opened: unsupported link type ({url})")),
            None => self.set_error(format!("No link [{n}] on this card")),
        }
    }

    /// Consumed by the main loop each tick: the URL `o` asked to open.
    pub fn take_link_open_request(&mut self) -> Option<String> {
        self.link_open_request.take()
    }

    /// Consumed by the main loop each tick: the expected content and change
    /// description of the most recent write-back, forwarded to
    /// `GitSync::request` when git-sync is active for this file. Returned
    /// unconditionally — `app.rs` has no notion of whether git-sync is
    /// enabled, matching `take_editor_request`/`take_link_open_request`.
    pub fn take_git_sync_request(&mut self) -> Option<PendingSync> {
        self.git_sync.pending.take()
    }

    /// Records a successful background commit+push, driving the
    /// title-bar "Synced … ago" tag the same way a write-back drives
    /// `last_update_at`.
    pub fn record_git_sync(&mut self) {
        self.git_sync.last_at = Some(SystemTime::now());
    }

    fn request_reset(&mut self) {
        let (done, started, _total) = self.document.checkbox_progress();
        if done == 0 && started == 0 {
            self.set_error("Nothing to reset: no tasks are done".to_string());
            return;
        }
        self.screen_before_confirm = self.screen;
        self.screen = Screen::ConfirmReset;
    }

    fn reset_all(&mut self) {
        if self.blocked_by_deletion() {
            return;
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
            return;
        }
        self.push_undo_point(pre_state);
        self.screen = Screen::Checklist;
        self.set_status("All tasks reset to not done".to_string());
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

    /// Commits `pre_state` (the checkbox snapshot captured before a mutation
    /// that has just been confirmed written to disk) onto the undo stack,
    /// and clears the redo stack (a fresh change invalidates redo). The
    /// history is capped at [`UNDO_HISTORY_CAP`] — the oldest entry is
    /// dropped once it's exceeded. Called only *after* `commit_write`
    /// reports success — a failed write already rolled the document back
    /// to `pre_state` itself, so it must never reach the undo/redo stacks:
    /// pushing it anyway would leave a no-op undo entry and wipe out a
    /// valid redo stack for a change that never actually happened.
    fn push_undo_point(&mut self, pre_state: StateSnapshot) {
        self.undo_stack.push(pre_state);
        if self.undo_stack.len() > UNDO_HISTORY_CAP {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Applies a checkbox-state snapshot to the document, matching items by line
    /// number (a line no longer present is skipped). Returns the line numbers
    /// whose state actually changed, so the caller can report and focus the
    /// change. Snapshot lookups go through a line-number map built once up
    /// front rather than a linear scan per item, so this stays O(items +
    /// snapshot) instead of O(items × snapshot) on a large checklist.
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

    /// `u`: undo the last state-changing action. Restores the previous
    /// checkbox snapshot, writes it back, and pushes the pre-undo state onto the
    /// redo stack so `Ctrl-R` can replay it. Refuses when the file is deleted,
    /// and reports a sticky "Nothing to undo" when the history is empty. If
    /// the write fails, the popped entry goes back onto the undo stack
    /// unchanged — the undo never took effect, so it must still be there to
    /// retry.
    pub fn undo(&mut self) {
        if self.blocked_by_deletion() {
            return;
        }
        let Some(snapshot) = self.undo_stack.pop() else {
            self.set_error("Nothing to undo".to_string());
            return;
        };
        let redo_point = self.state_snapshot();
        let changed = self.apply_snapshot(&snapshot);
        if !self.finish_history_apply(&redo_point, &changed, "Undo") {
            self.undo_stack.push(snapshot);
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
        let undo_point = self.state_snapshot();
        let changed = self.apply_snapshot(&snapshot);
        if !self.finish_history_apply(&undo_point, &changed, "Redo") {
            self.redo_stack.push(snapshot);
            return;
        }
        self.undo_stack.push(undo_point);
    }

    /// Shared tail for [`undo`]/[`redo`]: writes the restored state back,
    /// refreshes the self-write mtime guard and the "Updated" tag, drops any
    /// completion screen to the checklist, focuses the changed task when exactly
    /// one changed, and reports what happened. `verb` is "Undo" or "Redo".
    /// `pre_state` is the snapshot from just before this history application
    /// mutated the document — passed through to `commit_write` so a failed
    /// write rolls the document back to it rather than leaving the
    /// half-applied undo/redo in memory. Returns whether the write
    /// succeeded, so the caller knows whether to commit the swapped
    /// undo/redo stacks or put the popped entry back.
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
        self.screen = Screen::Checklist;

        match changed {
            [] => self.set_status(format!("{verb}: no change")),
            [line] => {
                if let Some((li, ii)) = self.locate_line(*line) {
                    self.focus_item(li, ii);
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
            CopyOutcome::Ambiguous(n) => {
                self.set_error(format!("Not copied: item has {n} code candidates"))
            }
            CopyOutcome::Failed => {
                self.set_error("Copy failed: no clipboard available".to_string())
            }
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

    /// Top-level key dispatch: modal overlays first (so their keys
    /// aren't shadowed by global bindings), then the Checklist-only card
    /// scroll, then the truly global single-key actions, then whatever's
    /// left falls to the current screen's own bindings. The dispatch *order*
    /// is what each of the early guards' comments below is protecting —
    /// keep it front-to-back exactly as laid out here.
    pub fn handle_key_with_mods(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.clear_status();

        // Any key other than a second `g` cancels a pending `gg` motion.
        let pending_g = std::mem::take(&mut self.pending_g);

        if self.handle_pending_open_link_key(code) {
            return;
        }

        // Confirmation modals are handled before the global q/Esc quit
        // check, so Esc cancels rather than quits.
        if matches!(self.screen, Screen::ConfirmReset | Screen::ConfirmQuitReset) {
            self.handle_confirm_key(code);
            return;
        }
        if self.screen == Screen::Help {
            self.handle_help_key(code, modifiers);
            return;
        }
        // Incremental search input: typed keys edit the query (so `q`,
        // `?`, `j`, … are literal text, not commands), with a live cursor jump.
        // Handled before the global command keys, which is why it returns early.
        if self.screen == Screen::Search {
            self.handle_search_key(code, modifiers);
            return;
        }
        // "Go to task" overlay: typing filters the list; because letters
        // are filter input, the selection moves on arrows / Ctrl-N/Ctrl-P (not
        // j/k, which must remain typeable). Enter jumps, Esc closes.
        if self.screen == Screen::ListPicker {
            self.handle_picker_key(code, modifiers);
            return;
        }

        // Card-body viewport scroll. Only the Checklist screen has a
        // scrollable body; intercepted before the plain `e`/`y`/… bindings so
        // Ctrl-E/Ctrl-Y don't trigger "edit"/"yank".
        if self.screen == Screen::Checklist && self.handle_checklist_scroll_key(code, modifiers) {
            return;
        }

        if self.handle_global_key(code, modifiers) {
            return;
        }

        match self.screen {
            Screen::Checklist => self.handle_checklist_key(code, pending_g),
            Screen::ListComplete => self.handle_list_complete_key(code),
            Screen::AllComplete => self.handle_all_complete_key(code),
            // Handled at the top of this function before the global keys.
            Screen::ConfirmReset
            | Screen::ConfirmQuitReset
            | Screen::Help
            | Screen::Search
            | Screen::ListPicker => {}
        }
    }

    /// A pending `o` (a card with several links) waits for the link number to
    /// open. This runs before the global digit→list jump (so the digit
    /// selects a link, not a list) and before the global q/Esc quit (so Esc
    /// cancels the prompt rather than quitting the app). A digit 1–9 opens
    /// that link; Esc cancels — both consumed here (`true`). Any other key is
    /// a soft cancel that still takes its normal action (like the `gg`
    /// chord), so it returns `false` even though the prompt itself is already
    /// cleared (the `mem::take` runs regardless of the key).
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

    fn handle_confirm_key(&mut self, code: KeyCode) {
        if self.screen == Screen::ConfirmReset {
            // Only `y` confirms — `Enter` is too easy to hit by reflex for a
            // whole-file rewrite. Any other key cancels.
            match code {
                KeyCode::Char('y') => self.reset_all(),
                _ => self.screen = self.screen_before_confirm,
            }
        } else {
            // ConfirmQuitReset, offered on quit when all done: y resets
            // then quits, n quits without resetting, Esc/anything else cancels.
            match code {
                KeyCode::Char('y') => {
                    self.reset_all();
                    self.should_quit = true;
                }
                KeyCode::Char('n') => self.should_quit = true,
                _ => self.screen = self.screen_before_confirm,
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
            _ => self.screen = self.screen_before_confirm,
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
            KeyCode::Esc => self.screen = self.screen_before_confirm,
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
            self.screen_before_confirm = self.screen;
            self.help.scroll = 0;
            self.screen = Screen::Help;
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
                self.screen_before_confirm = self.screen;
                self.screen = Screen::ConfirmQuitReset;
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
                self.return_to_last_item_of_list()
            }
            _ => {}
        }
    }

    fn handle_all_complete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.return_to_last_list()
            }
            _ => {}
        }
    }
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
