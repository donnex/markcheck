//! Undo and redo of task-state changes, as snapshots of every task's state.

use crate::model::{AppState, ItemKind, StateSnapshot, TaskState, UNDO_HISTORY_CAP, View};

use super::write::state_verb;
use super::*;

/// How many task labels an undo or redo of several tasks names in its
/// git-sync commit message before eliding the rest.
const HISTORY_NAMED_TASKS: usize = 3;

impl AppState {
    // ----- Undo / redo -------------------------------------------------

    /// A full snapshot of every checkbox item's current state, keyed by line
    /// number. This is the unit of undo/redo history.
    pub(super) fn state_snapshot(&self) -> StateSnapshot {
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
    pub(super) fn push_undo_point(&mut self, pre_state: StateSnapshot) {
        self.undo_stack.push(pre_state);
        if self.undo_stack.len() > UNDO_HISTORY_CAP {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// Applies a snapshot by line number, returning the lines whose state
    /// changed.
    pub(super) fn apply_snapshot(&mut self, snapshot: &StateSnapshot) -> Vec<usize> {
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
    pub(super) fn finish_history_apply(
        &mut self,
        pre_state: &StateSnapshot,
        changed: &[usize],
        verb: &str,
    ) -> bool {
        let change_desc = format!("{verb}: {}", self.history_change_desc(changed));
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
                    Some(ItemKind::Checkbox(TaskState::Skipped)) => "marked skipped",
                    _ => "restored",
                };
                self.set_status(format!("{verb}: {desc}"));
            }
            many => self.set_status(format!("{verb}: restored {} tasks", many.len())),
        }
        true
    }

    /// Names what an undo or redo changed, for the git-sync commit message:
    /// one task as its transition (`Uncheck "Restart service"`), several as a
    /// count with the first few labels.
    pub(super) fn history_change_desc(&self, changed: &[usize]) -> String {
        let items: Vec<(TaskState, &Item)> = changed
            .iter()
            .filter_map(|&line| self.locate_line(line))
            .map(|(li, ii)| &self.document.lists[li].items[ii])
            .filter_map(|item| match item.kind {
                ItemKind::Checkbox(state) => Some((state, item)),
                ItemKind::DisplayOnly => None,
            })
            .collect();
        match items.as_slice() {
            [] => "no change".to_string(),
            [(state, item)] => format!("{} \"{}\"", state_verb(*state), item.label()),
            many => {
                let mut names: Vec<String> = many
                    .iter()
                    .take(HISTORY_NAMED_TASKS)
                    .map(|(_, item)| format!("\"{}\"", item.label()))
                    .collect();
                if many.len() > HISTORY_NAMED_TASKS {
                    names.push("\u{2026}".to_string());
                }
                format!("{} tasks ({})", many.len(), names.join(", "))
            }
        }
    }

    /// Finds the `(list, item)` indices of the checkbox item with the given line
    /// number, if it still exists (undo focus).
    pub(super) fn locate_line(&self, line: usize) -> Option<(usize, usize)> {
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
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
