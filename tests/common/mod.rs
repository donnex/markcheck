//! Shared helpers for the integration test binaries (`cli.rs`, `pty.rs`).
//! A `tests/common/mod.rs` (rather than `tests/common.rs`) is the standard
//! way to share code between `tests/` binaries without Cargo treating this
//! file as its own (empty) test target.

use std::ffi::OsString;
use std::fs;

#[path = "../../src/temp_test_support.rs"]
mod temp_test_support;

pub use temp_test_support::unique_temp_path;

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
