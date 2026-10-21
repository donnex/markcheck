//! [`AppState`]'s behaviour, split by concern: each submodule adds the
//! methods for one area. Shared helpers and the status-message plumbing
//! live here.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::model::{
    AppState, CopyRoute, Document, GitSyncState, HelpState, IconSet, Item, ItemKind, List, Overlay,
    Palette, PendingChord, PickerState, SearchState, TaskState, View, ViewLayout, hash_bytes,
};
use crate::writer;

mod copy;
mod history;
mod keys;
mod links;
mod navigation;
mod reload;
mod reset;
mod search;
mod write;

#[cfg(test)]
mod test_support;

/// How long an ephemeral status message stays before it auto-clears.
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

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
    let meta = fs::symlink_metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
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

fn list_all_done(list: &List) -> bool {
    list.items
        .iter()
        .filter(|i| matches!(i.kind, ItemKind::Checkbox(_)))
        .all(|i| matches!(i.kind, ItemKind::Checkbox(state) if state.is_resolved()))
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
            effects: Vec::new(),
            file_mtime,
            file_size,
            file_content_hash,
            last_update_at: None,
            git_sync: GitSyncState::default(),
            card_scroll: 0,
            layout: ViewLayout::default(),
            pending_chord: PendingChord::None,
            icons: IconSet::unicode(),
            clipboard_primary: false,
            jump_back: Vec::new(),
            critical_armed: None,
            copy_route: CopyRoute::default(),
            auto_copy: false,
            // Named ANSI by default (theme-respecting); main.rs upgrades this
            // to the terminal's detected capability at startup.
            palette: Palette::basic(),
            file_gone: None,
            write_notice: None,
            in_place_noted: false,
            search: SearchState::default(),
            picker: PickerState::default(),
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

    /// `None` when the cursor is past the current list's items. The parser
    /// keeps only lists with a task, so a loaded document always has one.
    pub fn current_item(&self) -> Option<&Item> {
        self.current_list().items.get(self.current_item_index)
    }

    /// Sets an **ephemeral** status message that auto-clears after
    /// `STATUS_TIMEOUT` of idle — copies, reload/reset info, hints.
    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = Some(SystemTime::now() + STATUS_TIMEOUT);
        self.status_is_error = false;
    }

    /// Sets a **sticky**, non-error status message for a state that lasts
    /// until something else replaces it, such as waiting on git sync to quit.
    pub fn set_notice(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_expiry = None;
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

    /// Clears the status message before an input is handled, except while `/`
    /// search is open: a message that arrives during a search (a reload's
    /// summary, a deletion notice, a sync failure) is drawn above the prompt
    /// and stays until the search closes, so typing the query never discards
    /// it unread.
    fn clear_status_outside_search(&mut self) {
        if self.overlay != Some(Overlay::Search) {
            self.clear_status();
        }
    }

    /// Runs `action`, then puts a status message that was already showing in
    /// front of whatever `action` reported (`Reloaded: … · Match 2/5`), rather
    /// than letting it be replaced unread. The merged message is an error if
    /// either part is, and sticky if either part is.
    fn keeping_status(&mut self, action: impl FnOnce(&mut Self)) {
        let earlier = self.status_message.take();
        let earlier_is_error = self.status_is_error;
        let earlier_expiry = self.status_expiry;
        action(self);
        let Some(earlier) = earlier else {
            return;
        };
        self.status_message = Some(match self.status_message.take() {
            Some(reported) => {
                self.status_is_error |= earlier_is_error;
                self.status_expiry = earlier_expiry.and(self.status_expiry);
                format!("{earlier} · {reported}")
            }
            None => {
                self.status_is_error = earlier_is_error;
                self.status_expiry = earlier_expiry;
                earlier
            }
        });
    }

    /// Cancels any armed input chord (`gg`, `o`/`y`'s digit prompt) and the status
    /// message. Mouse input bypasses `handle_key_with_mods`, so a click between
    /// the two halves of a chord would otherwise leave it armed.
    fn clear_pending_chords(&mut self) {
        self.clear_status_outside_search();
        self.pending_chord = PendingChord::None;
    }

    /// Shared guard for every mutating action: once the watched file has
    /// been confirmed deleted or replaced by a non-regular file
    /// (`reload_if_changed`), none of them may write, so we never silently
    /// recreate it or overwrite a symlink behind the user's back. Sets the
    /// sticky error and returns `true` when blocked, so call sites read as
    /// `if self.blocked_by_deletion() { return ...; }`.
    fn blocked_by_deletion(&mut self) -> bool {
        match self.file_gone {
            Some(gone) => {
                self.set_error(gone.message().to_string());
                true
            }
            None => false,
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

    /// Shows a pending [`AppState::write_notice`], after the action's own
    /// status.
    fn show_write_notice(&mut self) {
        if let Some(notice) = self.write_notice.take() {
            self.set_error(notice);
        }
    }
}
