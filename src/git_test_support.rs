//! The git environment every test cuts its processes off from. Compiled
//! into both the crate's unit tests (through `test_support`) and the
//! integration test binaries (through `tests/common`, by `#[path]`), and
//! built on the same `git_env.rs` that `git_command` strips, so a variable
//! added there reaches every test without a hand-kept copy. It must use
//! nothing but `std`, since `tests/` can't see the crate's `cfg(test)` code.

include!("git_env.rs");

/// The variables that inject config into every git command.
const CONFIG_INJECTING_GIT_VARS: [&str; 2] = ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"];

/// Set on every test's git: no global or system config (signing,
/// `core.hooksPath`, templates).
pub const NO_GLOBAL_GIT_CONFIG: [(&str, &str); 2] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// Removed from every process a test spawns: they would point git at a
/// repository other than the fixture's (a hook running the suite exports
/// `GIT_DIR` and `GIT_INDEX_FILE`) or inject config.
pub fn ambient_git_vars() -> impl Iterator<Item = &'static str> {
    REDIRECTING_GIT_VARS
        .into_iter()
        .chain(CONFIG_INJECTING_GIT_VARS)
}
