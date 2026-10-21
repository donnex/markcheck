use std::path::{Path, PathBuf};
use std::process::Command;

// Removed as in `git_sync::process::git_command`, so `--version` describes
// this checkout.
include!("src/git_env.rs");

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

/// Asks git for its internal paths, which works in linked worktrees and
/// submodules where `.git` is a file.
fn git_path(manifest_dir: &Path, name: &str) -> Option<PathBuf> {
    let raw = git(manifest_dir, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    })
}

/// Whether the crate is the root of the work tree git answers for. Git walks
/// up from the crate's directory, so an unpacked source tarball inside some
/// other repository (a packaging repository, a monorepo, a dotfiles home)
/// would otherwise be stamped with that repository's commit.
fn is_own_checkout(manifest_dir: &Path) -> bool {
    let Some(toplevel) = git(manifest_dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (
        Path::new(&toplevel).canonicalize(),
        manifest_dir.canonicalize(),
    ) {
        (Ok(toplevel), Ok(manifest_dir)) => toplevel == manifest_dir,
        _ => false,
    }
}

fn main() {
    let manifest_dir: PathBuf = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    if !is_own_checkout(&manifest_dir) {
        println!("cargo:rustc-env=MARKCHECK_GIT_SHA=unknown");
        return;
    }

    let sha = git(&manifest_dir, &["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=MARKCHECK_GIT_SHA={sha}");

    // Rebuild when the commit changes: `HEAD` for branch switches and detached
    // HEAD, the directory the loose branch ref lives in (it may not exist yet
    // when refs are packed), and `packed-refs`. Only existing paths are
    // emitted, since a missing one makes cargo rebuild every time.
    let mut watched = Vec::new();
    if let Some(head) = git_path(&manifest_dir, "HEAD") {
        watched.push(head);
    }
    if let Some(packed) = git_path(&manifest_dir, "packed-refs") {
        watched.push(packed);
    }
    if let Some(dir) = git(&manifest_dir, &["symbolic-ref", "-q", "HEAD"])
        .and_then(|name| git_path(&manifest_dir, &name))
        .and_then(|branch_ref| branch_ref.parent().map(Path::to_path_buf))
    {
        watched.push(dir);
    }
    for path in watched {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
