//! Shared `#[cfg(test)]`-only helpers, used across this crate's unit test
//! modules so each doesn't hand-roll its own unique-temp-path counter.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Paths handed out on this thread, removed when it exits. libtest runs
/// every test on its own thread, so this cleans up after each test even
/// when it panics, without every caller holding a guard.
#[derive(Default)]
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            remove_path(path);
        }
    }
}

fn remove_path(path: &std::path::Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

thread_local! {
    static CLEANUP: RefCell<Cleanup> = RefCell::default();
}

/// A unique, not-yet-created path under the temp dir, removed (file or
/// whole directory) when the calling test's thread ends. `module` names
/// the caller so leftovers are traceable; `ext` adds a suffix.
pub fn unique_temp_path(module: &str, hint: &str, ext: Option<&str>) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = if hint.is_empty() {
        format!("markcheck-{module}-test-{pid}-{n}")
    } else {
        format!("markcheck-{module}-test-{hint}-{pid}-{n}")
    };
    let path = match ext {
        Some(ext) => std::env::temp_dir().join(format!("{name}.{ext}")),
        None => std::env::temp_dir().join(name),
    };
    CLEANUP.with(|c| c.borrow_mut().0.push(path.clone()));
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_paths_are_removed_when_the_test_thread_ends() {
        let (file, dir) = std::thread::spawn(|| {
            let file = unique_temp_path("support", "file", Some("md"));
            fs::write(&file, "x").unwrap();
            let dir = unique_temp_path("support", "dir", None);
            fs::create_dir_all(dir.join("nested")).unwrap();
            fs::write(dir.join("nested").join("f"), "x").unwrap();
            (file, dir)
        })
        .join()
        .unwrap();
        assert!(!file.exists(), "file left behind: {file:?}");
        assert!(!dir.exists(), "directory left behind: {dir:?}");
    }

    #[test]
    fn temp_paths_are_removed_even_when_the_test_panics() {
        let path = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        let seen = path.clone();
        let result = std::thread::spawn(move || {
            let file = unique_temp_path("support", "panic", None);
            fs::write(&file, "x").unwrap();
            *seen.lock().unwrap() = file;
            panic!("simulated test failure");
        })
        .join();
        assert!(result.is_err());
        let path = path.lock().unwrap().clone();
        assert!(!path.exists(), "left behind after a panic: {path:?}");
    }
}
