//! Shared helpers for the integration test binaries (`cli.rs`, `pty.rs`).
//! A `tests/common/mod.rs` (rather than `tests/common.rs`) is the standard
//! way to share code between `tests/` binaries without Cargo treating this
//! file as its own (empty) test target.

use std::ffi::OsString;
use std::fs;

#[path = "../../src/temp_test_support.rs"]
mod temp_test_support;

#[path = "../../src/git_test_support.rs"]
mod git_test_support;

pub use git_test_support::ambient_git_vars;
pub use temp_test_support::unique_temp_path;

/// Set on every process a test spawns, alongside removing
/// [`ambient_git_vars`], so it sees nothing of the machine running the
/// tests: a fresh, empty `HOME` and `XDG_CONFIG_HOME` (no markcheck config
/// file, real or planted in the shared temp dir, and no global git ignore or
/// attributes), and no global or system git config (signing,
/// `core.hooksPath`, templates). A test exercising the config file overrides
/// `XDG_CONFIG_HOME` afterwards.
pub fn isolated_env(module: &str) -> Vec<(&'static str, OsString)> {
    let home = unique_temp_path(module, "home", None);
    fs::create_dir_all(&home).unwrap();
    let mut env = vec![
        ("HOME", home.clone().into()),
        ("XDG_CONFIG_HOME", home.into()),
    ];
    env.extend(
        git_test_support::NO_GLOBAL_GIT_CONFIG
            .into_iter()
            .map(|(key, value)| (key, value.into())),
    );
    env
}
