//! Moving the cursor between tasks and lists, the screen transitions
//! that follow, and card-body scrolling.

use crate::model::{AppState, ItemKind, TaskState, View};

use super::*;

impl AppState {
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

    /// Navigate to `target` and report `msg` (if any) as an ephemeral status.
    /// The auto-copy outcome lands after it, so stays visible too (see
    /// `apply_copy_outcome`).
    pub(super) fn focus_and_report(
        &mut self,
        (list_index, item_index): (usize, usize),
        msg: Option<String>,
    ) {
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
    pub(super) fn jump_sub_section(&mut self, forward: bool) {
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

    /// The first list after `from` that still has incomplete work, if any.
    pub(super) fn next_incomplete_list_after(&self, from: usize) -> Option<usize> {
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
    pub(super) fn prev_incomplete_list_before(&self, from: usize) -> Option<usize> {
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
    pub(super) fn no_further_incomplete_list_message(&self, forward: bool) -> String {
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

    pub(super) fn reset_card_scroll(&mut self) {
        self.card_scroll = 0;
        self.layout.card_max_scroll = 0; // recomputed by the renderer next frame
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
    pub(super) fn half_page(&self) -> u16 {
        (self.layout.card_viewport_height / 2).max(1)
    }

    /// A near-full card viewport (leaving one row of context), for
    /// `PageDown`/`PageUp`. Min 1.
    pub(super) fn page(&self) -> u16 {
        self.layout.card_viewport_height.saturating_sub(1).max(1)
    }

    pub(super) fn maybe_transition_screen(&mut self) {
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
    pub(super) fn advance_to_next_open(&mut self) {
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

    /// From a finished list, jump to the next list with incomplete work,
    /// wrapping to an earlier one; show the all-complete summary only when
    /// nothing is left anywhere.
    pub(super) fn advance_to_next_incomplete_list(&mut self) {
        let target = self
            .next_incomplete_list_after(self.current_list_index)
            .or_else(|| self.prev_incomplete_list_before(self.current_list_index));
        match target {
            Some(index) => self.jump_to_list(index),
            None => self.view = View::AllComplete,
        }
    }

    pub(super) fn review_completed_list(&mut self) {
        self.view = View::Checklist;
    }
}

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;
