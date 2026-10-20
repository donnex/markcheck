//! Picking up edits made outside markcheck: reloading the file, keeping the
//! cursor on the same task across it, and returning from the editor or a
//! suspend.

use std::fs;
use std::io;
use std::time::{Duration, SystemTime};

use crate::launch::program_name;
use crate::model::{
    AppState, Document, FileGone, Item, ItemKind, List, Overlay, PendingChord, PendingSync,
    TaskState, View, hash_bytes,
};
use crate::parser;
use crate::writer;

use super::*;

/// An `e` editor that exits successfully sooner than this, leaving the file
/// unchanged, is taken to have handed off to a window instead of waiting.
pub(super) const EDITOR_RETURNED_AT_ONCE: Duration = Duration::from_secs(1);

/// What a reload changed in task terms, for its status message (#140):
/// tasks whose state moved, by direction and named (the first two), and
/// tasks added or removed. Tasks are matched by list, label and depth, so a
/// task that only moved lines isn't counted. `None` when no task changed.
pub(super) fn reload_changes(old: &Document, new: &Document) -> Option<String> {
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

impl AppState {
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
    pub(super) fn force_reload(&mut self) {
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
    pub(super) fn reload_from_disk(
        &mut self,
        modified: SystemTime,
        size: u64,
        was_deleted: bool,
    ) -> bool {
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
                self.pending_chord = PendingChord::None;
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

    /// Settles the view and overlay after a reload. The view always becomes
    /// the checklist, since a completion summary may no longer hold. Text-input
    /// and read-only overlays stay open, so keys typed into them never fall
    /// through to the checklist as commands; a confirmation is cancelled, since
    /// it was asked about the old content, and its name is returned for the
    /// status message. Must run after the new document is adopted.
    pub(super) fn keep_overlay_across_reload(
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
    pub(super) fn remap_position(&mut self, new_document: &Document) -> bool {
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
    pub(super) fn remapped(
        &self,
        position: (usize, usize),
        new_document: &Document,
    ) -> (usize, usize) {
        let (list_index, item_index, _) = self.remapped_with_loss(position, new_document);
        (list_index, item_index)
    }

    /// Matches the list by title and occurrence, then the item by content
    /// (header, body text and depth), preferring the candidate nearest the old
    /// line number, since inserting a line above it shifts every line number.
    /// With no content match it falls back to the item on the old line (a task
    /// edited in place), then to the nearest one by line number, reporting the
    /// task as lost; a list that is gone falls back to the first list's start.
    pub(super) fn remapped_with_loss(
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
}

#[cfg(test)]
#[path = "reload_tests.rs"]
mod tests;
