use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::model::AppState;

/// How often the file is stat'ed regardless of watcher events.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Watches the parent directory of a file (not the file itself) so that
/// atomic-rename saves — used both by our own writer and by most editors —
/// don't silently drop the watch. Events are filtered down to the target
/// filename before being reported.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<Event>>,
    target_file_name: OsString,
}

impl FileWatcher {
    pub fn new(path: &Path) -> notify::Result<Self> {
        let target_file_name = path
            .file_name()
            .ok_or_else(|| notify::Error::generic("file path has no file name to watch"))?
            .to_owned();
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());

        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)?;
        watcher.watch(
            parent.unwrap_or_else(|| Path::new(".")),
            RecursiveMode::NonRecursive,
        )?;

        Ok(FileWatcher {
            _watcher: watcher,
            receiver,
            target_file_name,
        })
    }

    /// Drains pending events; true if any referenced the watched file, or may
    /// have hidden a change to it (a watcher error, or a rescan after the
    /// kernel's event queue overflowed). Lossy on purpose: it only says when to
    /// check, and `reload_if_changed` decides.
    pub fn poll_changed(&self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.receiver.try_recv() {
            changed |= match result {
                Ok(event) => {
                    event.need_rescan()
                        || event
                            .paths
                            .iter()
                            .any(|p| p.file_name() == Some(self.target_file_name.as_os_str()))
                }
                Err(_) => true,
            };
        }
        changed
    }
}

/// Decides when to look for an external change: on a watcher event (the fast
/// path), and every `RECHECK_INTERVAL` regardless, because a watch can be
/// missing (it failed to start), blind (network and FUSE filesystems report no
/// remote edits) or dropped (the watched directory was removed and recreated).
pub struct ReloadTrigger {
    path: PathBuf,
    watcher: Option<FileWatcher>,
    next_recheck: Instant,
}

impl ReloadTrigger {
    /// Starts watching `path`, returning why the watch couldn't be set up if it
    /// failed; the timed recheck runs either way.
    pub fn new(path: &Path, now: Instant) -> (Self, Option<notify::Error>) {
        let (watcher, error) = match FileWatcher::new(path) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(error)),
        };
        let trigger = ReloadTrigger {
            path: path.to_path_buf(),
            watcher,
            next_recheck: now + RECHECK_INTERVAL,
        };
        (trigger, error)
    }

    /// Reloads `state` if a check is due and the file changed. Returns whether
    /// anything on screen may have changed: a reload, or a deleted, restored,
    /// skipped or failed reload message.
    pub fn check(&mut self, state: &mut AppState, now: Instant) -> bool {
        let event = self.watcher.as_ref().is_some_and(FileWatcher::poll_changed);
        if !event && now < self.next_recheck {
            return false;
        }
        self.next_recheck = now + RECHECK_INTERVAL;
        let before = (state.file_mtime, state.file_size, state.file_gone);
        let reloaded = state.reload_if_changed();
        if reloaded && !event {
            // The timer caught what the watch missed, so the watch is gone or
            // blind; setting it up afresh restores the fast path where it can.
            self.watcher = FileWatcher::new(&self.path).ok();
        }
        reloaded || before != (state.file_mtime, state.file_size, state.file_gone)
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
