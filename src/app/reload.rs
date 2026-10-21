//! Picking up edits made outside markcheck: reloading the file, keeping the
//! cursor on the same task across it, and returning from the editor or a
//! suspend.

use std::fs;
use std::io;
use std::time::{Duration, SystemTime};

use crate::effect::Effect;
use crate::launch::program_name;
use crate::model::{
    AppState, Document, FileGone, Item, ItemKind, Overlay, PendingChord, PendingSync, SyncBase,
    TaskState, View, hash_bytes,
};
use crate::parser;
use crate::writer;

use super::*;

/// Shown while `Ctrl-Z` waits for a running git sync before stopping.
const WAITING_TO_SUSPEND: &str = "Waiting for git sync before suspending…";

/// Shown for `Ctrl-Z` when no job-control shell could take the terminal.
const NO_JOB_CONTROL: &str =
    "Can't suspend: markcheck wasn't started from a shell with job control (q quits)";

/// An `e` editor that exits successfully sooner than this, leaving the file
/// unchanged, is taken to have handed off to a window instead of waiting.
pub(super) const EDITOR_RETURNED_AT_ONCE: Duration = Duration::from_secs(1);

/// What a reload changed in task terms, for its status message (#140):
/// tasks whose state moved, by direction and named (the first two), and
/// tasks added or removed. Tasks are matched by list, label and depth, then
/// any left over by label and depth alone, so a task that only moved lines
/// or sits under a renamed heading isn't counted. `None` when no task
/// changed.
pub(super) fn reload_changes(old: &Document, new: &Document) -> Option<String> {
    use std::collections::{HashMap, VecDeque};
    let tasks = |document: &Document| -> Vec<(String, String, usize, TaskState)> {
        document
            .lists
            .iter()
            .flat_map(|list| {
                list.items.iter().filter_map(|item| match item.kind {
                    ItemKind::Checkbox(state) => {
                        Some((list.title.clone(), item.label(), item.depth, state))
                    }
                    ItemKind::DisplayOnly => None,
                })
            })
            .collect()
    };
    let old_tasks = tasks(old);
    let new_tasks = tasks(new);
    let mut by_list: HashMap<(&str, &str, usize), VecDeque<usize>> = HashMap::new();
    for (index, (title, label, depth, _)) in old_tasks.iter().enumerate() {
        by_list
            .entry((title, label, *depth))
            .or_default()
            .push_back(index);
    }
    let mut matches: Vec<Option<usize>> = new_tasks
        .iter()
        .map(|(title, label, depth, _)| {
            by_list
                .get_mut(&(title.as_str(), label.as_str(), *depth))
                .and_then(VecDeque::pop_front)
        })
        .collect();
    let mut leftover: Vec<usize> = by_list.into_values().flatten().collect();
    leftover.sort_unstable();
    let mut by_label: HashMap<(&str, usize), VecDeque<usize>> = HashMap::new();
    for index in leftover {
        let (_, label, depth, _) = &old_tasks[index];
        by_label
            .entry((label, *depth))
            .or_default()
            .push_back(index);
    }
    for (found, (_, label, depth, _)) in matches.iter_mut().zip(&new_tasks) {
        if found.is_none() {
            *found = by_label
                .get_mut(&(label.as_str(), *depth))
                .and_then(VecDeque::pop_front);
        }
    }
    let (mut done, mut started, mut reopened, mut skipped, mut added) = (0, 0, 0, 0, 0);
    let mut names = Vec::new();
    for (found, (_, label, _, state)) in matches.iter().zip(&new_tasks) {
        let Some(old_index) = found else {
            added += 1;
            continue;
        };
        if old_tasks[*old_index].3 == *state {
            continue;
        }
        *match state {
            TaskState::Done => &mut done,
            TaskState::Started => &mut started,
            TaskState::NotStarted => &mut reopened,
            TaskState::Skipped => &mut skipped,
        } += 1;
        names.push(label.clone());
    }
    let removed = old_tasks.len() - matches.iter().flatten().count();
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
    /// mtime and size missed on a coarse-mtime filesystem. Returns whether the
    /// new content was adopted; when it wasn't, a sticky error says why.
    pub(super) fn force_reload(&mut self) -> bool {
        let Some((modified, size)) = current_stat(&self.document.file_path) else {
            self.set_error("Reload failed: the file can't be read".to_string());
            return false;
        };
        let was_deleted = self.file_gone.take().is_some();
        self.reload_from_disk(modified, size, was_deleted)
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
                let content_hash = Some(hash_bytes(
                    writer::document_contents(&self.document).as_bytes(),
                ));
                // New content is a change markcheck didn't make: its next toggle
                // must not publish it on top of the old base, and a waiting sync
                // request carries content that is gone. The new content queues
                // nothing.
                if content_hash != self.file_content_hash {
                    self.git_sync.unsynced_base = None;
                    self.cancel_git_sync();
                    self.effects.push(Effect::CheckRemote);
                }
                self.file_content_hash = content_hash;
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
        // This edit replaces the request the reload just cancelled, so a
        // waiting toggle folds into its commit instead of being dropped, and
        // its own push asks the remote what the reload's check would have.
        self.effects
            .retain(|effect| !matches!(effect, Effect::CancelGitSync | Effect::CheckRemote));
        let content = writer::document_contents(&self.document);
        let content_hash = hash_bytes(content.as_bytes());
        self.git_sync.unsynced_base = Some(content.clone());
        self.git_sync.base_epoch += 1;
        self.queue_git_sync(PendingSync {
            // The editor wrote this, so the content being replaced is the content
            // carried.
            previous_content: Some(content.clone()),
            content,
            content_hash,
            description: format!("Edited in {}", program_name(editor)),
            immediate: true,
            base: SyncBase::Edited {
                epoch: self.git_sync.base_epoch,
            },
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
            Some(Overlay::ConfirmSkip) => {
                self.overlay = None;
                Some("skip")
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
    /// task as lost. A list whose title is gone (a renamed heading) is found
    /// by content instead: the list now holding the task, else the list at the
    /// same index when the list count is unchanged, else the list holding the
    /// line nearest the task's.
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
        let same_count = new_document.lists.len() == self.document.lists.len();
        let last_list = new_document.lists.len().saturating_sub(1);

        let Some(old_item) = old_item else {
            let fallback = if same_count { list_index } else { 0 };
            return (
                found_list_index.unwrap_or(fallback).min(last_list),
                0,
                false,
            );
        };
        let same_task = |item: &Item| {
            item.header == old_item.header
                && item.display_text == old_item.display_text
                && item.depth == old_item.depth
        };
        let distance = |item: &Item| item.line_number.abs_diff(old_item.line_number);
        let nearest_where = |matches: &dyn Fn(&Item) -> bool| {
            new_document
                .lists
                .iter()
                .enumerate()
                .flat_map(|(l, list)| {
                    list.items
                        .iter()
                        .enumerate()
                        .map(move |(i, item)| (l, i, item))
                })
                .filter(|(_, _, item)| matches(item))
                .min_by_key(|(_, _, item)| distance(item))
                .map(|(l, i, _)| (l, i))
        };

        let new_list_index = match found_list_index {
            Some(index) => index,
            None => {
                if let Some((list, item)) = nearest_where(&same_task) {
                    return (list, item, false);
                }
                if same_count {
                    list_index.min(last_list)
                } else {
                    nearest_where(&|_| true).map_or(0, |(list, _)| list)
                }
            }
        };
        let items = &new_document.lists[new_list_index].items;

        if let Some(index) = (0..items.len())
            .filter(|&i| same_task(&items[i]))
            .min_by_key(|&i| distance(&items[i]))
        {
            return (new_list_index, index, false);
        }
        if let Some(index) =
            (0..items.len()).find(|&i| items[i].line_number == old_item.line_number)
        {
            return (new_list_index, index, false);
        }
        let nearest = (0..items.len())
            .min_by_key(|&i| distance(&items[i]))
            .unwrap_or(0);
        (new_list_index, nearest, true)
    }

    /// Before `Ctrl-Z` stops the process: a sync still running is waited for,
    /// since a stopped process can't commit or push it.
    pub fn wait_for_sync_before_suspend(&mut self) {
        self.set_notice(WAITING_TO_SUSPEND.to_string());
    }

    /// Clears the [`AppState::wait_for_sync_before_suspend`] notice, keeping
    /// a message the finished sync reported in its place.
    pub fn finish_waiting_to_suspend(&mut self) {
        if self.status_message.as_deref() == Some(WAITING_TO_SUSPEND) {
            self.clear_status();
        }
    }

    /// `Ctrl-Z` where a stop would be discarded: no job-control shell is
    /// there to go to or to resume markcheck, so it stays on screen.
    pub fn refuse_suspend(&mut self) {
        self.set_error(NO_JOB_CONTROL.to_string());
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
            Err(err) if err.kind() == io::ErrorKind::NotFound => Some(format!(
                "{} not found — set $EDITOR (or $VISUAL) to your editor",
                program_name(program)
            )),
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
