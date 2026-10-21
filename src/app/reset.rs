//! Resetting tasks to not-started (`R`, then the scope: the whole document,
//! the current list or the current sub-list).

use crate::model::{AppState, ItemKind, Overlay, ResetScope, TaskState, View};

impl AppState {
    pub(super) fn request_reset(&mut self) {
        let progress = self.document.progress();
        if progress.open == progress.tasks() {
            self.set_error("Nothing to reset: no tasks are done".to_string());
            return;
        }
        self.overlay = Some(Overlay::ConfirmReset);
    }

    /// Resets every task to not done and saves. Returns whether the reset
    /// reached disk; on failure the sticky error explains why.
    pub(super) fn reset_all(&mut self) -> bool {
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
    pub(super) fn scope_positions(&self, scope: ResetScope) -> Vec<(usize, usize)> {
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
    pub(super) fn reset_scope(&mut self, scope: ResetScope) -> bool {
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
}

#[cfg(test)]
#[path = "reset_tests.rs"]
mod tests;
