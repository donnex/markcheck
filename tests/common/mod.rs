//! Shared helpers for the integration test binaries (`cli.rs`, `pty.rs`).
//! A `tests/common/mod.rs` (rather than `tests/common.rs`) is the standard
//! way to share code between `tests/` binaries without Cargo treating this
//! file as its own (empty) test target.

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
    }
}

thread_local! {
    static CLEANUP: RefCell<Cleanup> = RefCell::default();
}

/// A unique path under the system temp dir, not created — callers that need
/// a real file or directory on disk do that themselves. Whatever ends up
/// there is removed when the calling test's thread ends. `module` names the
/// calling test binary (`"cli"` or `"pty"`), so leftover files from a failed
/// run are still traceable to their origin; `hint` further disambiguates
/// within that binary's own tests. `ext` appends a `.{ext}` suffix (e.g.
/// `"md"`) when the path names a file.
pub fn unique_temp_path(module: &str, hint: &str, ext: Option<&str>) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = format!("markcheck-{module}-{hint}-{pid}-{n}");
    let path = match ext {
        Some(ext) => std::env::temp_dir().join(format!("{name}.{ext}")),
        None => std::env::temp_dir().join(name),
    };
    CLEANUP.with(|c| c.borrow_mut().0.push(path.clone()));
    path
}
