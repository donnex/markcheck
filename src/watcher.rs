use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

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

    /// Drains all pending events non-blockingly; returns true if any of
    /// them referenced the watched file.
    pub fn poll_changed(&self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.receiver.try_recv() {
            if let Ok(event) = result {
                let matches = event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(self.target_file_name.as_os_str()));
                changed |= matches;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn unique_temp_path(name_hint: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "markcheck-watcher-test-{name_hint}-{}-{unique}.md",
            std::process::id()
        ))
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

        let link_dir = std::env::temp_dir().join(format!(
            "markcheck-watcher-test-linkdir-{}",
            std::process::id()
        ));
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
