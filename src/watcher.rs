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
        let before = (state.file_mtime, state.file_size, state.file_deleted);
        let reloaded = state.reload_if_changed();
        if reloaded && !event {
            // The timer caught what the watch missed, so the watch is gone or
            // blind; setting it up afresh restores the fast path where it can.
            self.watcher = FileWatcher::new(&self.path).ok();
        }
        reloaded || before != (state.file_mtime, state.file_size, state.file_deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    fn unique_temp_path(name_hint: &str) -> std::path::PathBuf {
        crate::test_support::unique_temp_path("watcher", name_hint, Some("md"))
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn detects_write_to_watched_file() {
        let path = unique_temp_path("target");
        fs::write(&path, "initial").unwrap();
        let watcher = FileWatcher::new(&path).unwrap();

        fs::write(&path, "changed").unwrap();

        assert!(
            wait_until(|| watcher.poll_changed()),
            "expected a change event for the watched file"
        );

        fs::remove_file(&path).ok();
    }

    /// With the canonicalized path (as main.rs resolves at startup), the
    /// watcher watches the symlink target's parent directory, so edits to
    /// the target are detected even when the symlink lives in a different
    /// directory.
    #[cfg(unix)]
    #[test]
    fn detects_target_changes_through_canonicalized_symlink() {
        let target = unique_temp_path("linktarget");
        fs::write(&target, "initial").unwrap();

        let link_dir = crate::test_support::unique_temp_path("watcher", "linkdir", None);
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("linked.md");
        fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let canonical = fs::canonicalize(&link).unwrap();
        let watcher = FileWatcher::new(&canonical).unwrap();

        fs::write(&target, "changed").unwrap();

        assert!(
            wait_until(|| watcher.poll_changed()),
            "expected a change event for the symlink target"
        );

        fs::remove_file(&link).ok();
        fs::remove_dir(&link_dir).ok();
        fs::remove_file(&target).ok();
    }

    #[test]
    fn detects_delete_then_recreate_of_the_watched_file() {
        // Some editors save by deleting the target and writing a fresh file
        // at the same path, rather than a rename. Since the watch is on the
        // *directory* and events are matched by filename (not inode), this
        // should be detected exactly like an atomic-rename save.
        let path = unique_temp_path("target");
        fs::write(&path, "initial").unwrap();
        let watcher = FileWatcher::new(&path).unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(&path, "recreated").unwrap();

        assert!(
            wait_until(|| watcher.poll_changed()),
            "expected a change event after delete-then-recreate"
        );

        fs::remove_file(&path).ok();
    }

    fn state_for(path: &Path) -> AppState {
        AppState::new(crate::parser::parse_document(path.to_path_buf()).unwrap())
    }

    fn write_with_new_mtime(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mtime = fs::metadata(path).unwrap().modified().unwrap() + Duration::from_secs(1);
        fs::File::open(path).unwrap().set_modified(mtime).unwrap();
    }

    #[test]
    fn a_failed_watch_is_reported_and_the_timer_still_picks_up_edits() {
        let dir = crate::test_support::unique_temp_path("watcher", "nodir", None);
        let missing = dir.join("list.md");
        let start = Instant::now();
        let (mut trigger, error) = ReloadTrigger::new(&missing, start);
        assert!(error.is_some(), "watching a missing directory must fail");
        assert!(trigger.watcher.is_none());

        fs::create_dir_all(&dir).unwrap();
        fs::write(&missing, "## List\n\n- [ ] one\n").unwrap();
        let mut state = state_for(&missing);
        write_with_new_mtime(&missing, "## List\n\n- [ ] one\n- [ ] two\n");

        assert!(!trigger.check(&mut state, start), "not due yet");
        assert_eq!(state.document.lists[0].items.len(), 1);

        assert!(trigger.check(&mut state, start + RECHECK_INTERVAL));
        assert_eq!(state.document.lists[0].items.len(), 2);
        assert!(
            trigger.watcher.is_some(),
            "a change the watch missed sets the watch up again"
        );
        assert!(
            !trigger.check(&mut state, start + RECHECK_INTERVAL * 2),
            "nothing changed since"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_removed_and_recreated_directory_clears_the_deleted_state() {
        let dir = crate::test_support::unique_temp_path("watcher", "recreated", None);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.md");
        fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
        let mut state = state_for(&path);
        let start = Instant::now();
        let (mut trigger, error) = ReloadTrigger::new(&path, start);
        assert!(error.is_none());

        fs::remove_dir_all(&dir).unwrap();
        assert!(trigger.check(&mut state, start + RECHECK_INTERVAL));
        assert!(state.file_deleted);

        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
        assert!(trigger.check(&mut state, start + RECHECK_INTERVAL * 2));
        assert!(!state.file_deleted, "the restored file is picked up");
        assert_eq!(
            state.status_message.as_deref(),
            Some("File restored — reloaded")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_watcher_event_triggers_a_check_before_the_timer_is_due() {
        let path = unique_temp_path("event");
        fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
        let mut state = state_for(&path);
        let start = Instant::now();
        let (mut trigger, _) = ReloadTrigger::new(&path, start);

        write_with_new_mtime(&path, "## List\n\n- [ ] one\n- [ ] two\n");
        assert!(wait_until(|| trigger.check(&mut state, start)));
        assert_eq!(state.document.lists[0].items.len(), 2);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn errors_and_rescans_count_as_a_possible_change() {
        let (sender, receiver) = mpsc::channel();
        let watcher = FileWatcher {
            _watcher: notify::recommended_watcher(|_| {}).unwrap(),
            receiver,
            target_file_name: OsString::from("list.md"),
        };
        let unrelated = Event::new(notify::EventKind::Any).add_path(PathBuf::from("other.md"));

        sender.send(Ok(unrelated.clone())).unwrap();
        assert!(!watcher.poll_changed());
        sender
            .send(Ok(unrelated.set_flag(notify::event::Flag::Rescan)))
            .unwrap();
        assert!(watcher.poll_changed(), "a rescan means events were lost");
        sender
            .send(Err(notify::Error::generic("queue overflow")))
            .unwrap();
        assert!(watcher.poll_changed(), "an error may hide a change");
    }

    #[test]
    fn ignores_writes_to_other_files_in_same_directory() {
        let path = unique_temp_path("target");
        let sibling = unique_temp_path("sibling");
        fs::write(&path, "initial").unwrap();
        let watcher = FileWatcher::new(&path).unwrap();

        fs::write(&sibling, "unrelated").unwrap();
        // Give the OS a moment to deliver (or not deliver) an event, then
        // confirm none of what arrived matches our target file.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!watcher.poll_changed());

        fs::remove_file(&sibling).ok();
        fs::remove_file(&path).ok();
    }
}
