// Included by both `build.rs` and `git_sync/process.rs`, so the build's own
// `git` calls and git sync's strip the same variables.

/// The variables that would make git act on a different repository than the
/// one asked about (hooks and `git rebase --exec` export `GIT_DIR` and
/// `GIT_INDEX_FILE`, for example).
pub(crate) const REDIRECTING_GIT_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];
