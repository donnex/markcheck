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

    // Rebuild when the commit actually changes. Watching `HEAD` alone is not
    // enough, and used to be all this did: committing on the same branch
    // rewrites the *branch ref*, not `HEAD` — confirmed against real git —
    // so the stamped SHA silently went stale until some unrelated change
    // forced a rebuild. `HEAD` still has to be watched too, for branch
    // switches and for a detached `HEAD`, where there is no branch ref.
    //
    // Only paths that exist are emitted: a missing `rerun-if-changed` path
    // makes cargo rebuild unconditionally, and a packed ref has no loose
    // file to watch. Outside a repository (a source tarball) nothing is
    // emitted at all, which is right — there is no commit to go stale.
    let mut watched = Vec::new();
    if let Some(head) = git_path(&manifest_dir, "HEAD") {
        watched.push(head);
    }
    if let Some(branch_ref) = git(&manifest_dir, &["symbolic-ref", "-q", "HEAD"])
        .and_then(|name| git_path(&manifest_dir, &name))
    {
        watched.push(branch_ref);
    }
    for path in watched {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
