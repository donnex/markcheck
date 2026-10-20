//! Read-only git queries. Several return three-way answers because "could
//! not tell" must not be confused with "no".

use std::path::{Path, PathBuf};

use super::process::{
    PLUMBING_TIMEOUT, PUSH_TIMEOUT, command_error, git_command, literal_pathspec_command,
    run_with_timeout, run_with_timeout_and_stdin,
};

/// Fails open (`true`), so a push is attempted and its real failure
/// reported rather than the sync going quiet.
pub(super) fn ahead_of_upstream(repo_root: &Path, tip: &str) -> bool {
    commits_ahead_of_upstream(repo_root, tip).unwrap_or(1) > 0
}
/// `None` when it can't be answered. Unlike `ahead_of_upstream`, the
/// startup catch-up push acts only on a positively-known answer.
pub(super) fn commits_ahead_of_upstream(repo_root: &Path, tip: &str) -> Option<u64> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--count", &format!("@{{u}}..{tip}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}
/// Whether a branch exists on the remote, asked of the remote itself: the
/// local tracking ref survives an upstream deletion, and pushing an explicit
/// refspec then silently recreates the branch.
pub(super) enum RemoteBranch {
    /// The SHA becomes the lease the push is made against.
    Present(String),
    Absent,
    Unknown,
}
/// `location` is a remote name or URL.
pub(super) fn remote_branch_state(
    repo_root: &Path,
    location: &str,
    branch_ref: &str,
) -> RemoteBranch {
    let mut cmd = git_command(repo_root);
    cmd.args(["ls-remote", "--heads", location, branch_ref]);
    match run_with_timeout(cmd, PUSH_TIMEOUT) {
        Ok(output) if output.status.success() => {
            exact_ref_sha(&String::from_utf8_lossy(&output.stdout), branch_ref)
                .map_or(RemoteBranch::Absent, RemoteBranch::Present)
        }
        _ => RemoteBranch::Unknown,
    }
}
/// `ls-remote` matches a pattern against ref tails, so asking for
/// `refs/heads/main` also lists `refs/heads/x/refs/heads/main`.
fn exact_ref_sha(listing: &str, branch_ref: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        (name == branch_ref).then(|| sha.to_string())
    })
}

/// `false` when unanswerable (e.g. `ancestor` was never fetched), so a
/// fast-forward is never assumed.
pub(super) fn is_ancestor(repo_root: &Path, ancestor: &str, descendant: &str) -> bool {
    let mut cmd = git_command(repo_root);
    cmd.args(["merge-base", "--is-ancestor", ancestor, descendant]);
    run_with_timeout(cmd, PLUMBING_TIMEOUT).is_ok_and(|out| out.status.success())
}
/// `Ok(None)` when no upstream resolves (exit 1); any other failure is
/// `Err`.
pub(super) fn resolve_upstream(repo_root: &Path) -> Result<Option<String>, String> {
    verify_rev(repo_root, "@{u}")
}
/// `rev-parse --verify -q <rev>`: `Ok(None)` on its exit 1 (the revision
/// doesn't resolve), `Err` on any other failure.
fn verify_rev(repo_root: &Path, rev: &str) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--verify", "-q", rev]);
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => Err(command_error("git rev-parse", &output)),
        Err(err) => Err(format!("git rev-parse failed: {err}")),
    }
}
/// Read from `branch.<name>.remote`/`.merge`, since a remote name can't be
/// split reliably from `@{u}` when the branch name contains `/`.
pub(super) fn upstream_parts(repo_root: &Path) -> Option<(String, String)> {
    let branch_ref = current_branch_ref(repo_root)?;
    let branch = branch_ref.strip_prefix("refs/heads/")?;
    let remote = git_config(repo_root, &format!("branch.{branch}.remote"))?;
    let merge_ref = git_config(repo_root, &format!("branch.{branch}.merge"))?;
    Some((remote, merge_ref))
}
/// Whether a plain `git push` on this branch would publish to `remote` under
/// the upstream's own name. `@{push}` applies `branch.<name>.pushRemote`,
/// `remote.pushDefault`, `push.default` and push refspecs exactly as `git push`
/// does, and fails to resolve wherever `git push` would refuse.
pub(super) fn push_goes_to_upstream(repo_root: &Path, remote: &str) -> bool {
    let Some(branch_ref) = current_branch_ref(repo_root) else {
        return false;
    };
    let Some(push_remote) = git_stdout_line(
        repo_root,
        &["for-each-ref", "--format=%(push:remotename)", &branch_ref],
    ) else {
        return false;
    };
    let tracking =
        |spec: &str| git_stdout_line(repo_root, &["rev-parse", "--symbolic-full-name", spec]);
    match (tracking("@{push}"), tracking("@{u}")) {
        (Some(push), Some(upstream)) => {
            push_remote == remote && !push.is_empty() && push == upstream
        }
        _ => false,
    }
}
/// The URL `git push <remote>` sends to: `remote.<name>.pushurl` when set,
/// which can name a different repository than the fetch `url`.
pub(super) fn push_url(repo_root: &Path, remote: &str) -> Option<String> {
    git_stdout_line(repo_root, &["remote", "get-url", "--push", remote])
}
/// `git <args>`'s trimmed stdout, or `None` when it fails or can't run.
fn git_stdout_line(repo_root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(args);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
fn git_config(repo_root: &Path, key: &str) -> Option<String> {
    git_stdout_line(repo_root, &["config", "--get", key])
}
/// The blob SHA the real index holds for `relpath`.
pub(super) fn index_blob(repo_root: &Path, relpath: &str) -> Option<String> {
    let mut cmd = literal_pathspec_command(repo_root);
    cmd.args(["ls-files", "--stage", "-z", "--"]).arg(relpath);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    let record = output.stdout.split(|&b| b == 0).find(|r| !r.is_empty())?;
    let record = String::from_utf8_lossy(record);
    let (info, _path) = record.split_once('\t')?;
    info.split_whitespace().nth(1).map(str::to_string)
}
/// `None` when the listing is empty, which must not be read as a root
/// commit: undoing a root commit deletes the branch ref.
pub(super) fn parse_first_parent(listing: &str) -> Option<Option<String>> {
    let mut fields = listing.split_whitespace();
    fields.next()?;
    Some(fields.next().map(str::to_string))
}
/// Works before the first commit, since `git init` already points `HEAD` at
/// the default branch.
pub(super) fn current_branch_ref(repo_root: &Path) -> Option<String> {
    git_stdout_line(repo_root, &["symbolic-ref", "-q", "HEAD"])
}
/// Absolute even when git reports a relative path.
pub(super) fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let path = PathBuf::from(git_stdout_line(repo_root, &["rev-parse", "--git-dir"])?);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}
pub(super) fn index_entry(
    repo_root: &Path,
    file_path: &Path,
) -> Result<Option<(String, String)>, String> {
    let mut cmd = literal_pathspec_command(repo_root);
    cmd.args(["ls-files", "--stage", "--full-name", "-z", "--"])
        .arg(file_path);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git ls-files failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git ls-files", &output));
    }
    // `-z` keeps paths with newlines or quotable characters intact. No record
    // means untracked, which is not an error.
    let mut records = output.stdout.split(|&b| b == 0).filter(|r| !r.is_empty());
    let Some(record) = records.next() else {
        return Ok(None);
    };
    // More than one record means the pathspec matched other files (#70).
    if records.next().is_some() {
        return Err(format!(
            "git ls-files: {} matched more than one tracked file",
            file_path.display()
        ));
    }
    // A lossy conversion would hand git a different, U+FFFD-mangled path to
    // stage, so a non-UTF-8 name is refused rather than synced (#8).
    let record = std::str::from_utf8(record).map_err(|_| {
        format!(
            "git sync: {} is not a UTF-8 file name, so it can't be synced",
            file_path.display()
        )
    })?;
    let (info, path) = record
        .split_once('\t')
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    let mode = info
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    Ok(Some((mode.to_string(), path.to_string())))
}
pub(super) fn blob_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<String> {
    git_stdout_line(repo_root, &["rev-parse", &format!("{commit}:{relpath}")])
}
pub(super) fn hash_object(
    repo_root: &Path,
    relpath: &str,
    content: &str,
) -> Result<String, String> {
    hash_object_inner(repo_root, relpath, content, true)
}
/// Computed without writing, to avoid littering loose blobs.
pub(super) fn blob_sha_for(repo_root: &Path, relpath: &str, content: &str) -> Option<String> {
    hash_object_inner(repo_root, relpath, content, false).ok()
}
/// `--path` applies the path's clean filter (`core.autocrlf`,
/// `.gitattributes`) exactly as `git add` would.
fn hash_object_inner(
    repo_root: &Path,
    relpath: &str,
    content: &str,
    write: bool,
) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.arg("hash-object");
    if write {
        cmd.arg("-w");
    }
    cmd.args(["--path", relpath, "--stdin"]);
    let output = run_with_timeout_and_stdin(cmd, PLUMBING_TIMEOUT, content)
        .map_err(|err| format!("git hash-object failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git hash-object", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// `None` is ambiguous between "no commits yet" and a failed check; see
/// `resolve_parent` where that matters.
pub(super) fn current_head(repo_root: &Path) -> Option<String> {
    git_stdout_line(repo_root, &["rev-parse", "HEAD"])
}
/// Exit 1 from `rev-parse --verify -q HEAD` means no commits yet; any other
/// failure is `Err`. Treating a transient failure as "no commits" would build
/// a root commit containing only the checklist.
pub(super) fn resolve_parent(repo_root: &Path) -> Result<Option<String>, String> {
    verify_rev(repo_root, "HEAD")
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
