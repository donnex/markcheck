//! Skipping, or reopening, a whole sub-list at once: `-` on a parent whose
//! sub-tasks are open asks to skip them all, and on one whose sub-tasks are
//! all skipped asks to reopen them, as one write and one undo step (#259).

use crate::model::{AppState, BranchSkip, ItemKind, Overlay, TaskState, View};

/// `"3 sub-tasks"`, `"1 sub-task"`.
pub(crate) fn sub_task_count(n: usize) -> String {
    if n == 1 {
        "1 sub-task".to_string()
    } else {
        format!("{n} sub-tasks")
    }
}

impl AppState {
    /// What `-` on the current card would do to the branch under it, or
    /// `None` when it isn't a branch action: no sub-tasks, or nothing for a
    /// branch skip or reopen to change, which leaves a task to the one-task
    /// toggle. Skipping takes every open sub-task (and the parent, if it is an
    /// open task) except critical steps; reopening needs every sub-task
    /// resolved and at least one skipped, and leaves done ones alone.
    pub fn branch_skip(&self) -> Option<BranchSkip> {
        let li = self.current_list_index;
        let root = self.current_item_index;
        let list = self.current_list();
        let state_of = |i: usize| match list.items.get(i).map(|item| item.kind) {
            Some(ItemKind::Checkbox(state)) => Some(state),
            _ => None,
        };
        let tasks: Vec<(usize, TaskState)> = list
            .descendants(root)
            .into_iter()
            .filter_map(|i| state_of(i).map(|state| (i, state)))
            .collect();
        let own = state_of(root);
        let root_open = own.is_some_and(|state| !state.is_resolved());
        let open: Vec<usize> = tasks
            .iter()
            .filter(|(_, state)| !state.is_resolved())
            .map(|&(i, _)| i)
            .collect();
        if !open.is_empty() {
            let critical = |i: &usize| self.document.is_critical(li, *i);
            let root_part = root_open.then_some(root);
            let (held, targets): (Vec<usize>, Vec<usize>) =
                root_part.into_iter().chain(open).partition(critical);
            let sub_tasks = targets.iter().filter(|&&i| i != root).count();
            return Some(BranchSkip {
                reopen: false,
                root,
                targets,
                sub_tasks,
                held,
            });
        }
        let skipped: Vec<usize> = tasks
            .iter()
            .filter(|(_, state)| *state == TaskState::Skipped)
            .map(|&(i, _)| i)
            .collect();
        if skipped.is_empty() || root_open {
            return None;
        }
        let sub_tasks = skipped.len();
        let targets = (own == Some(TaskState::Skipped))
            .then_some(root)
            .into_iter()
            .chain(skipped)
            .collect();
        Some(BranchSkip {
            reopen: true,
            root,
            targets,
            sub_tasks,
            held: Vec::new(),
        })
    }

    /// `-` on a branch: opens the confirmation, unless every open sub-task is
    /// a critical step, which is never skipped in bulk.
    pub(super) fn request_branch_skip(&mut self, plan: &BranchSkip) {
        if !plan.reopen
            && plan.sub_tasks == 0
            && let Some(&critical) = plan.held.iter().find(|&&i| i != plan.root)
        {
            let list = self.current_list();
            self.set_error(format!(
                "Critical step under \"{}\": skip \"{}\" on its own first (- twice, pausing between)",
                list.items[plan.root].label(),
                list.items[critical].label()
            ));
            return;
        }
        self.overlay = Some(Overlay::ConfirmSkip);
    }

    /// The skip dialog's keys: `y` applies the plan, anything else cancels and
    /// says so. A failed write leaves the dialog open, so its error is read
    /// first.
    pub(super) fn handle_confirm_skip_key(&mut self, code: crossterm::event::KeyCode) {
        let reopen = self.branch_skip().is_some_and(|plan| plan.reopen);
        if code != crossterm::event::KeyCode::Char('y') {
            self.overlay = None;
            let what = if reopen { "Reopen" } else { "Skip" };
            self.set_status(format!("{what} cancelled: nothing changed"));
            return;
        }
        self.apply_branch_skip();
    }

    /// Sets every task the plan covers to skipped, or back to not started,
    /// and saves, as one undo step and one git-sync commit. Returns whether
    /// it reached disk.
    pub(super) fn apply_branch_skip(&mut self) -> bool {
        let Some(plan) = self.branch_skip() else {
            self.overlay = None;
            self.set_error("Nothing to skip or reopen here any more".to_string());
            return false;
        };
        if self.blocked_by_deletion() {
            return false;
        }
        let li = self.current_list_index;
        let label = self.current_list().items[plan.root].label();
        let (state, verb, done) = if plan.reopen {
            (TaskState::NotStarted, "Reopen", "Reopened")
        } else {
            (TaskState::Skipped, "Skip", "Skipped")
        };
        let pre_state = self.state_snapshot();
        for &i in &plan.targets {
            self.document.lists[li].items[i].kind = ItemKind::Checkbox(state);
        }
        if !self.commit_write(
            &pre_state,
            &format!("Write failed — {} not saved to disk", verb.to_lowercase()),
            &format!("{verb} \"{label}\" and its sub-tasks"),
        ) {
            return false;
        }
        self.push_undo_point(pre_state);
        self.overlay = None;
        let count = sub_task_count(plan.sub_tasks);
        let message = if plan.targets.contains(&plan.root) {
            format!("{done} \"{label}\" and its {count}")
        } else {
            format!("{done} {count} of \"{label}\"")
        };
        if plan.reopen {
            self.view = View::Checklist;
        } else {
            self.maybe_transition_screen();
        }
        match plan.held.len() {
            0 => self.set_status(message),
            n => {
                let steps = if n == 1 {
                    "1 critical step stays open: skip it on its own".to_string()
                } else {
                    format!("{n} critical steps stay open: skip each on its own")
                };
                self.set_notice(format!("{message} · {steps}"));
            }
        }
        true
    }
}

#[cfg(test)]
#[path = "skip_branch_tests.rs"]
mod tests;
