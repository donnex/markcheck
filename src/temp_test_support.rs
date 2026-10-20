//! Unique temp paths for tests, removed when the test ends. Compiled into
//! both the crate's unit tests (through `test_support`) and the integration
//! test binaries (through `tests/common`, by `#[path]`), so there is one
//! implementation rather than two kept in step by hand (#143). It must use
//! nothing but `std`, since `tests/` can't see the crate's `cfg(test)` code.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
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

fn remove_path(path: &Path) {
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
/// the caller so leftovers are traceable; `hint` tells a caller's paths
/// apart; `ext` adds a suffix.
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
