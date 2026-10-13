//! Shared helpers for the integration test binaries (`cli.rs`, `pty.rs`).
//! A `tests/common/mod.rs` (rather than `tests/common.rs`) is the standard
//! way to share code between `tests/` binaries without Cargo treating this
//! file as its own (empty) test target.

use std::cell::RefCell;
use std::ffi::OsString;
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

/// Removed from every process a test spawns: they would point git at a
/// repository other than the fixture's (a hook running the suite exports
/// `GIT_DIR` and `GIT_INDEX_FILE`) or inject config. The first eight mirror
/// `git_command` in `src/git_sync/process.rs`.
pub const AMBIENT_GIT_VARS: [&str; 10] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Set on every process a test spawns, alongside removing
/// [`AMBIENT_GIT_VARS`], so it sees nothing of the machine running the
/// tests: a fresh, empty `HOME` and `XDG_CONFIG_HOME` (no markcheck config
/// file, real or planted in the shared temp dir, and no global git ignore or
/// attributes), and no global or system git config (signing,
/// `core.hooksPath`, templates). A test exercising the config file overrides
/// `XDG_CONFIG_HOME` afterwards.
pub fn isolated_env(module: &str) -> Vec<(&'static str, OsString)> {
    let home = unique_temp_path(module, "home", None);
    fs::create_dir_all(&home).unwrap();
    vec![
        ("HOME", home.clone().into()),
        ("XDG_CONFIG_HOME", home.into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
    ]
}
