//! Changing a task's state and saving it to the file: toggling, starting,
//! skipping, critical-step confirmation, the guarded write, and git-sync
//! reporting.

use std::time::{Duration, Instant, SystemTime};

use crate::effect::Effect;
use crate::git_sync::{self, SyncOutcome};
use crate::model::{
    AppState, CriticalAction, CriticalArm, FileGone, ItemKind, PendingSync, StateSnapshot,
    SyncBase, TaskState, View, hash_bytes,
};
use crate::writer;

use super::*;

pub(super) const READ_ONLY_MESSAGE: &str =
    "File is read-only — change not saved (chmod u+w to allow saving)";

/// The quiet spell a critical step's confirming press must follow, so key
/// repeat from a held key, or a reflex double press, can't confirm it (#219).
pub(super) const CRITICAL_PAUSE: Duration = Duration::from_millis(500);

pub(super) const IN_PLACE_MESSAGE: &str = "Saved in place: this folder doesn't let markcheck replace the file, \
     so the checklist is overwritten directly";

impl AppState {
    pub fn toggle_current(&mut self) {
        let Some(current) = self.current_item() else {
            return;
        };
        let kind = current.kind;
        let state = match kind {
            // An info card can't be toggled, so page past it like `l`.
            ItemKind::DisplayOnly => {
                if !self.navigate_forward() {
                    self.set_error(
                        "Nothing to toggle: this is a note, and it's the last card".to_string(),
                    );
                }
                return;
            }
            ItemKind::Checkbox(state) => state,
        };

        let new_state = state.toggle(TaskState::Done);
        let position = (self.current_list_index, self.current_item_index);
        if new_state == TaskState::Done && !self.confirm_critical(position, CriticalAction::Check) {
            return;
        }
        if !self.set_current_state(new_state) {
            return;
        }
        self.set_status(state.toggle_message(new_state));
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
        let new_state = state.toggle(TaskState::Done);
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
        self.set_status(state.toggle_message(new_state));
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
        let new_state = state.toggle(TaskState::Started);
        if !self.set_current_state(new_state) {
            return;
        }
        self.set_status(state.toggle_message(new_state));
        self.view = View::Checklist;
    }

    /// Checking or skipping a critical task takes a second, deliberate press
    /// (#95): the first arms it and says so, and the same input again on the
    /// same task confirms, but only after [`CRITICAL_PAUSE`] without it (#219).
    /// A quicker repeat keeps it armed and restarts the pause, so a held key
    /// never gets through. Returns whether the change may go ahead now.
    pub(super) fn confirm_critical(
        &mut self,
        position: (usize, usize),
        action: CriticalAction,
    ) -> bool {
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
        let new_state = state.toggle(TaskState::Skipped);
        let position = (self.current_list_index, self.current_item_index);
        if new_state == TaskState::Skipped && !self.confirm_critical(position, CriticalAction::Skip)
        {
            return;
        }
        if !self.set_current_state(new_state) {
            return;
        }
        self.set_status(state.toggle_message(new_state));
        if new_state == TaskState::Skipped {
            self.maybe_transition_screen();
        } else {
            self.view = View::Checklist;
        }
    }

    /// Sets the current checkbox's state and writes it back. Returns whether
    /// the write succeeded.
    pub(super) fn set_current_state(&mut self, state: TaskState) -> bool {
        self.set_state(self.current_list_index, self.current_item_index, state)
    }

    /// Sets one checkbox's state and writes it back, the one path every
    /// toggle takes: the keyboard's and the overview click's. Returns whether
    /// the write succeeded.
    pub(super) fn set_state(
        &mut self,
        list_index: usize,
        item_index: usize,
        state: TaskState,
    ) -> bool {
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
            &format!("{} \"{item_text}\"", state.commit_verb()),
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
    pub(super) fn commit_write(
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

        let on_disk = writer::read_checklist(&self.document.file_path).ok();
        if self.content_diverged(on_disk.as_deref()) {
            self.apply_snapshot(pre_state);
            if self.force_reload() {
                self.set_error(
                    "File changed on disk — reloaded; change not saved, please retry".to_string(),
                );
            }
            return false;
        }
        // The pre-write content for git-sync's staged-target guard, which compares
        // in git's terms so clean filters apply. The bytes just checked, so it is
        // the revision the check matched.
        let previous_content = on_disk.and_then(|bytes| String::from_utf8(bytes).ok());
        let content = match writer::write_back(&self.document) {
            Ok(saved) => {
                if saved.in_place && !std::mem::replace(&mut self.in_place_noted, true) {
                    self.write_notice = Some(IN_PLACE_MESSAGE.to_string());
                }
                self.document.raw_lines =
                    crate::parser::split_lines_keeping_endings(&saved.contents)
                        .into_iter()
                        .map(str::to_string)
                        .collect();
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
        if self.git_sync.unsynced_base.is_none() {
            self.git_sync.unsynced_base = previous_content.clone();
            self.git_sync.base_epoch += 1;
        }
        let base = SyncBase::Own {
            epoch: self.git_sync.base_epoch,
            content: self.git_sync.unsynced_base.clone(),
        };
        self.queue_git_sync(PendingSync {
            content,
            content_hash,
            previous_content,
            description: change_desc.to_string(),
            immediate: false,
            base,
        });
        true
    }

    /// True when `on_disk`, the file's bytes as just read, no longer match
    /// `file_content_hash`, i.e. someone else changed it since we last saw it.
    /// Fails open when the file couldn't be read (`None`); `write_back` reports
    /// that.
    pub(super) fn content_diverged(&self, on_disk: Option<&[u8]>) -> bool {
        on_disk.is_some_and(|bytes| Some(hash_bytes(bytes)) != self.file_content_hash)
    }

    /// Queues `sync`, replacing one not yet handed to the worker: only the
    /// latest content needs committing.
    pub(super) fn queue_git_sync(&mut self, sync: PendingSync) {
        self.effects
            .retain(|effect| !matches!(effect, Effect::GitSync(_)));
        self.effects.push(Effect::GitSync(sync));
    }

    /// Drops every sync request for the current content: one still in the
    /// effect queue here, and one waiting in the worker through
    /// `Effect::CancelGitSync`.
    pub(super) fn cancel_git_sync(&mut self) {
        self.effects
            .retain(|effect| !matches!(effect, Effect::GitSync(_)));
        self.effects.push(Effect::CancelGitSync);
    }

    /// Says that `Effect::CancelGitSync` dropped a request the worker was
    /// holding, after the reload's own message.
    pub fn report_dropped_git_sync(&mut self) {
        match self.status_message.take() {
            Some(message) => {
                self.status_message = Some(format!("{message} — pending git sync dropped"));
            }
            None => {
                self.set_status("Pending git sync dropped: the file changed on disk".to_string())
            }
        }
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
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
