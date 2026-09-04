use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables that make `git` report a *different repository* than the one
/// this crate lives in — the same list, for the same reason, as
/// `git_command` in `src/git_sync.rs`. The version string must describe the
/// checkout being built, not whatever repository the ambient environment
/// happens to point at: a build run from inside a git hook, from
/// `git rebase --exec`, or from a shell exporting `GIT_DIR` for a bare
/// dotfiles repository would otherwise stamp another repository's SHA into
/// `--version`.
const REDIRECTING_GIT_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Trimmed stdout of a `git` command run against the crate's own directory,
/// or `None` if it can't be run, fails, or says nothing.
fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(manifest_dir).args(args);
    for var in REDIRECTING_GIT_VARS {
        cmd.env_remove(var);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Resolves one of git's own internal paths (`git rev-parse --git-path`) as
/// an absolute path. Asking git rather than assuming `.git/…` is what makes
/// this work in a linked worktree or a submodule, where `.git` is a *file*
/// and the real directory lives elsewhere — and it resolves per-worktree
/// versus common-directory placement correctly, which hand-built paths get
/// wrong.
fn git_path(manifest_dir: &Path, name: &str) -> Option<PathBuf> {
    let raw = git(manifest_dir, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    })
}

fn main() {
    let manifest_dir: PathBuf = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    let sha = git(&manifest_dir, &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MARKCHECK_GIT_SHA={sha}");

    // Rebuild when the commit actually changes. Getting this right takes
    // three watches, because the branch tip has three possible
    // representations and git moves between them freely.
    //
    // `HEAD` alone is not enough, and used to be all this did: committing on
    // the same branch rewrites the *branch ref*, not `HEAD` — confirmed —
    // so the stamped SHA silently went stale until some unrelated change
    // forced a rebuild. `HEAD` still has to be watched for branch switches
    // and for a detached `HEAD`, where there is no branch ref at all.
    //
    // Watching only the loose ref *file* is not enough either — external
    // review of `d885c88`. In a repository whose refs have been packed
    // (`git gc`, `git pack-refs`, a fresh clone) there is no loose file at
    // build time, so nothing was emitted for the branch; the next commit
    // then *creates* the loose ref, which cargo was not watching, and the
    // SHA went stale exactly as before. Confirmed against real git.
    //
    // So: watch the directory the loose ref would live in, which covers it
    // whether it exists yet or not, plus `packed-refs` for the case where the
    // tip moves while staying packed. Emitting a directory costs an
    // occasional extra rebuild when some *other* branch is created or
    // deleted, which is the right trade against reporting a version that
    // isn't the one built. Only existing paths are emitted: a missing
    // `rerun-if-changed` path makes cargo rebuild unconditionally. Outside a
    // repository (a source tarball) nothing is emitted at all, which is
    // right — there is no commit to go stale.
    let mut watched = Vec::new();
    if let Some(head) = git_path(&manifest_dir, "HEAD") {
        watched.push(head);
    }
    if let Some(packed) = git_path(&manifest_dir, "packed-refs") {
        watched.push(packed);
    }
    if let Some(branch_ref) = git(&manifest_dir, &["symbolic-ref", "-q", "HEAD"])
        .and_then(|name| git_path(&manifest_dir, &name))
    {
        // The directory, not the file: the file may not exist yet.
        if let Some(dir) = branch_ref.parent() {
            watched.push(dir.to_path_buf());
        }
    }
    for path in watched {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
