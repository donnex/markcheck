//! Incremental search (`/`), the go-to-task picker (`T`) and help-overlay
//! scrolling.

use crate::model::{AppState, Item, Overlay};

use super::*;

/// Smart-case containment (vim `smartcase`): case-insensitive unless the
/// query contains an uppercase letter, in which case the match is exact. An
/// empty query never matches.
pub(super) fn smart_case_contains(haystack: &str, query: &str) -> bool {
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
pub(super) fn picker_match_text(item: &Item) -> String {
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

/// `Ctrl-W` in a prompt: drop trailing spaces, then the word before them,
/// as in the shell and vim's command line.
pub(super) fn delete_last_word(query: &mut String) {
    let kept = query
        .trim_end()
        .trim_end_matches(|c: char| !c.is_whitespace());
    query.truncate(kept.len());
}

impl AppState {
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

    /// Enter incremental search (`/`): remember the cursor for a possible
    /// `Esc` restore, clear the query, and open the `Search` overlay.
    pub(super) fn start_search(&mut self) {
        self.search.origin = (self.current_list_index, self.current_item_index);
        self.search.query.clear();
        self.overlay = Some(Overlay::Search);
    }

    /// The match `/` lands on, with its 1-based position: the first match at or
    /// after the search origin, wrapping to the first match overall — vim's
    /// forward search, so a search from mid-document doesn't throw the cursor
    /// back to the top. `None` when nothing matches.
    pub(super) fn search_target(
        &self,
        matches: &[(usize, usize)],
    ) -> Option<(usize, (usize, usize))> {
        let origin = self.search.origin;
        let index = matches.iter().position(|&m| m >= origin).unwrap_or(0);
        matches.get(index).map(|&m| (index + 1, m))
    }

    /// Re-run the live query and jump the cursor to the next match from the
    /// search origin (see [`Self::search_target`]), keeping the `Search`
    /// overlay open (the query keeps showing in the status bar). Like vim, no
    /// match or an empty query returns the cursor to the search origin rather
    /// than leaving it on an earlier prefix's match; no auto-copy mid-typing.
    pub(super) fn update_search(&mut self) {
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
    pub(super) fn commit_search(&mut self) {
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
    pub(super) fn cancel_search(&mut self) {
        self.overlay = None;
        let (list_index, item_index) = self.search.origin;
        self.focus_item(list_index, item_index, CursorMove::Quiet);
    }

    /// Cycle to the next (`forward`) or previous match of the last committed
    /// query relative to the cursor, wrapping around (and saying so, like vim's
    /// "search hit BOTTOM"); a no-op with no active
    /// search or no matches (`n`/`N`). Matches from `find_matches` are in
    /// document order, so `(list, item)` tuples compare in that order.
    pub(super) fn search_cycle(&mut self, forward: bool) {
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

    /// Open the "go to task" overlay (`T`): clear the filter and highlight
    /// the task the cursor is on, so the list opens where the reader is.
    pub(super) fn open_list_picker(&mut self) {
        self.picker.query.clear();
        self.picker_select_from_cursor();
        self.layout.picker_rows_area = None;
        self.overlay = Some(Overlay::ListPicker);
    }

    /// Highlight the first picker entry at or after the cursor, wrapping to
    /// the first entry — the cursor's own row when the filter is empty, and
    /// otherwise the next match onward, as `/` lands.
    pub(super) fn picker_select_from_cursor(&mut self) {
        let cursor = (self.current_list_index, self.current_item_index);
        self.picker.selection = self
            .picker_matches()
            .iter()
            .position(|&entry| entry >= cursor)
            .unwrap_or(0);
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
    pub(super) fn picker_half_page(&self) -> isize {
        (self.layout.picker_viewport_height / 2).max(1) as isize
    }

    /// Move the picker selection by `delta`, clamped to the current entries
    /// (no wrap); a no-op when there are none.
    pub(super) fn picker_move(&mut self, delta: isize) {
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
    pub(super) fn picker_click(&mut self, col: u16, row: u16) {
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
    /// the filter matched nothing. Committing the task the cursor is already
    /// on leaves the card as it was and says so.
    pub(super) fn picker_commit(&mut self) {
        self.overlay = None;
        let Some(&target) = self.picker_matches().get(self.picker.selection) else {
            return;
        };
        if target == (self.current_list_index, self.current_item_index) {
            self.set_status("Already on this task".to_string());
        } else {
            self.focus_item(target.0, target.1, CursorMove::Navigate);
        }
    }

    /// Scroll the `?` help overlay down/up by `n` lines, clamped.
    pub(super) fn scroll_help_down_by(&mut self, n: u16) {
        self.help.scroll = self
            .help
            .scroll
            .saturating_add(n)
            .min(self.layout.help_max_scroll);
    }

    pub(super) fn scroll_help_up_by(&mut self, n: u16) {
        self.help.scroll = self.help.scroll.saturating_sub(n);
    }

    /// Half / near-full help viewport, for `Ctrl-D`/`Ctrl-U` and `PageDown`/`Up`
    /// — mirrors the card-body `half_page`/`page`.
    pub(super) fn help_half_page(&self) -> u16 {
        (self.layout.help_viewport_height / 2).max(1)
    }

    pub(super) fn help_page(&self) -> u16 {
        self.layout.help_viewport_height.saturating_sub(1).max(1)
    }
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
