//! Asking git questions about the repository.
//!
//! Every read-only lookup the sync depends on: what is in the index, what a
//! blob is at some commit, where `HEAD` points, what the upstream is, and
//! whether a branch still exists on the remote. These answer questions;
//! nothing here decides anything or changes the repository.
//!
//! Several return deliberately three-way answers (`Result<Option<_>, _>`,
//! `RemoteBranch`) because "could not tell" is not the same as "no", and
//! collapsing the two is the defect class this module has repeatedly been
//! bitten by.

use std::path::{Path, PathBuf};

use super::process::{
    PLUMBING_TIMEOUT, PUSH_TIMEOUT, command_error, git_command, run_with_timeout,
    run_with_timeout_and_stdin,
};

/// Whether `HEAD` is ahead of its upstream tracking branch — i.e. there's a
/// local commit not yet on the remote. Used to tell "already fully synced"
/// apart from "committed locally, but a previous push attempt failed" when
/// the file's content already matches `HEAD` (see `run_sync`): without this
/// distinction, a push failure followed by no further checklist edits would
/// leave the commit local-only forever, since nothing else would ever
/// re-trigger a push for content that's already committed.
///
/// Fails open (`true`, i.e. "assume there's something to push") when the
/// check itself can't be answered — no upstream configured, detached-ish
/// tracking state, or any other `git rev-list` failure — so a push is
/// attempted (and its real failure reason reported) rather than the sync
/// going quiet with no explanation.
pub(super) fn ahead_of_upstream(repo_root: &Path, tip: &str) -> bool {
    commits_ahead_of_upstream(repo_root, tip).unwrap_or(1) > 0
}
/// How many commits `HEAD` is ahead of its upstream, or `None` when the
/// question can't be answered at all (no upstream configured, detached
/// `HEAD`, any other `git rev-list` failure).
///
/// Split out of `ahead_of_upstream` so the two callers can choose opposite
/// failure directions from the same one subprocess. `ahead_of_upstream`
/// folds `None` into "assume yes" because it runs *after* a change the user
/// just made, where going quiet would hide a real problem.
/// `catch_up_push` needs the opposite: it runs unprompted at startup, so it
/// must act only on a positively-known unpushed commit and stay silent
/// otherwise, rather than nagging about a missing upstream on every launch.
pub(super) fn commits_ahead_of_upstream(repo_root: &Path, tip: &str) -> Option<u64> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--count", &format!("@{{u}}..{tip}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}
/// Whether `branch_ref` still exists on `remote`, asked of the remote itself
/// rather than of the local tracking ref.
///
/// **The local tracking ref is not evidence the branch still exists.**
/// `refs/remotes/<remote>/<branch>` is a cache, updated only by a fetch, so
/// after somebody deletes the branch upstream it keeps pointing at the last
/// commit this clone saw and `@{u}` keeps resolving happily. Pushing an
/// explicit `<sha>:refs/heads/<branch>` refspec at that point is not a
/// rejected non-fast-forward — there is nothing to be non-fast-forward
/// *against* — so git simply creates the branch again. Confirmed against a
/// real remote: `* [new branch]`.
///
/// That is unattended republication of a branch somebody deliberately
/// removed, triggered by ticking a checkbox, and it is the same hazard the
/// refuse-without-an-upstream rule and the explicit-SHA refspec exist to
/// prevent — arriving by a route neither of them watches. External review of
/// `8a405dd`.
///
/// Three-way rather than a bool, because "could not ask" is a different
/// answer from "not there" and collapsing the two is the defect class this
/// module keeps relearning (see `UnpushedHistory`, `LockOutcome`,
/// `parse_first_parent`).
pub(super) enum RemoteBranch {
    Present,
    Absent,
    /// The remote could not be consulted at all — offline, credentials,
    /// transport failure.
    Unknown,
}
/// Costs one extra network round trip on the push path, which already spends
/// one; nothing is asked of the network when there is nothing to push, since
/// the callers reach here only with a commit in hand.
pub(super) fn remote_branch_state(repo_dir: &Path, remote: &str, branch_ref: &str) -> RemoteBranch {
    let mut cmd = git_command(repo_dir);
    cmd.args(["ls-remote", "--heads", remote, branch_ref]);
    match run_with_timeout(cmd, PUSH_TIMEOUT) {
        Ok(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
                RemoteBranch::Absent
            } else {
                RemoteBranch::Present
            }
        }
        _ => RemoteBranch::Unknown,
    }
}
/// The commit the branch's upstream tracking ref points at. `Ok(None)` when
/// there is no resolvable upstream — either none is configured, or one is
/// configured whose tracking ref doesn't exist locally yet (a remote added
/// but never fetched, or `branch.<name>.remote`/`.merge` set by hand).
/// Confirmed empirically: `git rev-parse --verify -q @{u}` exits **1** for
/// both, and 0 with the SHA when it resolves — the same exit-code
/// distinction `resolve_parent` relies on, so any *other* failure (128, a
/// timeout, a spawn error) becomes `Err` rather than being mistaken for
/// "no upstream".
///
/// Resolving to a concrete SHA also means `unpushed_history`'s range walk
/// names two real commits instead of the symbolic `@{u}`, matching the
/// captured-tip rule the rest of this module now follows: `upstream_parts`
/// reads the *config*, which can say an upstream exists while `@{u}` still
/// won't resolve — precisely the case that made an earlier version of this
/// check report a hard failure where "no upstream yet" was the truth.
pub(super) fn resolve_upstream(repo_root: &Path) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--verify", "-q", "@{u}"]);
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => Err(command_error("git rev-parse", &output)),
        Err(err) => Err(format!("git rev-parse failed: {err}")),
    }
}
/// Resolves `(remote, upstream-branch-ref)` for the current branch via its
/// `branch.<name>.remote`/`branch.<name>.merge` config — the same two keys
/// several tests already set up manually (via `-u` push or explicit `git
/// config` calls) — rather than parsing `@{u}`'s abbreviated form, which
/// would need guessing where the remote name ends and a branch name that
/// itself contains `/` begins. `None` when either key is unset (no
/// upstream configured at all), or the branch itself can't be resolved
/// (detached `HEAD`).
pub(super) fn upstream_parts(repo_root: &Path) -> Option<(String, String)> {
    let branch_ref = current_branch_ref(repo_root)?;
    let branch = branch_ref.strip_prefix("refs/heads/")?;
    let remote = git_config(repo_root, &format!("branch.{branch}.remote"))?;
    let merge_ref = git_config(repo_root, &format!("branch.{branch}.merge"))?;
    Some((remote, merge_ref))
}
/// `git config --get <key>`, trimmed — `None` on any failure (key unset,
/// not a repo, `git` itself failing to run).
fn git_config(repo_root: &Path, key: &str) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["config", "--get", key]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// The blob SHA the **real** index currently holds for `relpath`, or `None`
/// when it has no entry for it (or the lookup fails). `git ls-files --stage`
/// prints `<mode> <object> <stage>\t<path>`, so the object is the second
/// whitespace-separated field of the part before the tab; `-z` keeps a path
/// containing a newline or a quotable character intact, as `index_entry`
/// already relies on.
pub(super) fn index_blob(repo_root: &Path, relpath: &str) -> Option<String> {
    let mut cmd = git_command(repo_root);
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
/// The first parent in a `git rev-list --parents -n 1 <commit>` listing:
/// `Some(None)` for a root commit, `Some(Some(sha))` otherwise, and `None`
/// when the output says nothing at all.
///
/// That last distinction is the whole point, and its absence was a real
/// defect. `undo_commit` used `.lines().next().unwrap_or_default()`, so an
/// **empty** listing produced no parent — indistinguishable from a genuine
/// root commit, whose undo path *deletes the branch ref* rather than moving
/// it back one commit. Deep rounds 3, round 1: this is the same ambiguous
/// `None` that `resolve_parent` exists to eliminate for `current_head`,
/// recurring here.
///
/// It is reachable, not theoretical. When output was piped, a drain that
/// timed out against a lingering descendant returned empty output while the
/// command's own exit status was still success; output now goes to a file,
/// which removes that particular route, but not the premise. A scratch file
/// that cannot be read back still yields empty output beside a successful
/// status, and `git` is under no obligation to have written anything.
/// Refusing to rewind is the safe answer either way; the commit stays and
/// the next sync reports on it.
pub(super) fn parse_first_parent(listing: &str) -> Option<Option<String>> {
    let mut fields = listing.split_whitespace();
    // The commit itself is always the first field when there is any output.
    fields.next()?;
    Some(fields.next().map(str::to_string))
}
/// The branch `HEAD` symbolically points at (e.g. `refs/heads/main`),
/// resolved regardless of whether that branch has any commits yet — `git
/// init` makes `HEAD` a symref to the default branch immediately, before
/// the first commit exists, so this works the same before and after the
/// root-commit case `undo_commit` needs it for.
pub(super) fn current_branch_ref(repo_root: &Path) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["symbolic-ref", "-q", "HEAD"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Absolute path to the repository's git directory. Resolved relative to
/// `repo_root` when `git` reports a relative one (its own behavior differs
/// depending on whether it's invoked from the work tree root or a
/// subdirectory) — `repo_sync_blocked` needs an absolute path to check for
/// marker files regardless of which one `git` happened to hand back.
pub(super) fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--git-dir"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}
/// The file's tracked mode and its path relative to the repo root, read from
/// the index in one call (`git ls-files --stage --full-name`). Only called
/// once `status` has already confirmed the path is tracked (not `??`).
pub(super) fn index_entry(
    repo_dir: &Path,
    file_path: &Path,
) -> Result<Option<(String, String)>, String> {
    let mut cmd = git_command(repo_dir);
    cmd.args(["ls-files", "--stage", "--full-name", "-z", "--"])
        .arg(file_path);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git ls-files failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git ls-files", &output));
    }
    // `-z` NUL-delimits records instead of newline-delimiting them, and
    // (unlike the default form) never C-quotes/escapes the path — needed
    // for a tracked path containing a literal newline, which would
    // otherwise either get truncated by a newline-based split or come back
    // quoted-and-escaped with no unquoting done here. Exactly one record is
    // expected, since exactly one path was queried; a tab within the path
    // itself is harmless to `split_once('\t')` since the single separator
    // tab is always the leftmost one (the `<mode> <object> <stage>` prefix
    // before it never itself contains a tab).
    // No record at all means the path simply isn't in the index — the file
    // is untracked. That is a normal answer, not a failure, and the caller
    // needs it separated from "the lookup itself went wrong": `git status`
    // cannot be trusted to report untrackedness on its own (see `run_sync`).
    let Some(record) = output.stdout.split(|&b| b == 0).find(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let record = String::from_utf8_lossy(record);
    let (info, path) = record
        .split_once('\t')
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    let mode = info
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("git ls-files: unexpected output {record:?}"))?;
    Ok(Some((mode.to_string(), path.to_string())))
}
/// `commit`'s committed bytes for `relpath` (repo-root-relative), or `None`
/// if the path has no entry at `commit` (e.g. staged but never committed)
/// or `git show` otherwise fails.
pub(super) fn blob_bytes_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!("{commit}:{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}
/// The blob SHA at `commit:relpath` (repo-root-relative), or `None` if that
/// path doesn't exist at `commit` or `git rev-parse` otherwise fails —
/// the object-identity counterpart to `blob_bytes_at`, which fetches content
/// *bytes*; this fetches just the SHA, which is all a comparison against
/// an already-known blob SHA needs (see `commit_temp_index`'s ownership
/// check on a timeout).
pub(super) fn blob_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", &format!("{commit}:{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Whether `commit` is a direct child of `parent` — exactly one parent,
/// equal to it — or, when `parent` is `None`, a root commit with no parents
/// at all. Anything else (including a merge, or a commit one further step
/// removed) is `false`, as is any `git` failure: this only ever *grants*
/// trust, so failing closed leaves the caller reporting a failure rather
/// than acting on an unverified SHA.
///
/// `git rev-list --parents -n 1 <commit>` prints `<sha> <parent>…` on one
/// line — the same shape (and the same `split_whitespace` parsing)
/// `range_has_unrelated_commits` already reads, just for a single commit.
pub(super) fn descends_directly_from(
    repo_root: &Path,
    commit: &str,
    parent: &Option<String>,
) -> bool {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", "-n", "1", commit]);
    let Ok(output) = run_with_timeout(cmd, PLUMBING_TIMEOUT) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().next() else {
        return false;
    };
    let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
    match parent {
        Some(expected) => parents == [expected.as_str()],
        None => parents.is_empty(),
    }
}
/// Writes `content` into the object database without touching the working
/// tree or index, returning its blob SHA — the blob a later commit stages.
pub(super) fn hash_object(
    repo_root: &Path,
    relpath: &str,
    content: &str,
) -> Result<String, String> {
    hash_object_inner(repo_root, relpath, content, true)
}
/// The blob SHA `content` *would* have at `relpath`, computed without
/// writing anything. Used to compare against what is already staged, where
/// writing a throwaway object on every check would only litter the
/// repository with unreferenced loose blobs.
pub(super) fn blob_sha_for(repo_root: &Path, relpath: &str, content: &str) -> Option<String> {
    hash_object_inner(repo_root, relpath, content, false).ok()
}
/// **`--path` is what makes this agree with git.** Without it, `hash-object`
/// hashes the bytes verbatim; with it, git applies whatever clean filter
/// that path is configured for — `core.autocrlf`, a `.gitattributes` `text`
/// setting — exactly as `git add` would.
///
/// Omitting it was a real defect, not a nicety. On a repository with
/// `core.autocrlf` and CRLF endings, the same two lines hash to
/// `508b5d62…` verbatim but `c83684de…` through the filter, and `git add`
/// stores the latter — so markcheck was committing a blob git itself would
/// never have written, publishing un-normalised content to everyone who
/// pulls it. It also broke `staged_target_would_be_lost`, which compared a
/// filtered blob against unfiltered bytes and so could never match, and
/// refused a sync that was in fact safe.
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
/// The current commit `HEAD` points at, or `None` for a branch with no
/// commits yet (so the next commit is created as a root commit).
///
/// This `None` is ambiguous — `git rev-parse HEAD`'s failure looks
/// identical (`output.status.success() == false`) whether the repository
/// genuinely has no commits yet, or the command failed for any other
/// reason (a timeout, a spawn failure, a corrupted ref) — confirmed
/// empirically: a real empty repo exits with code 128 and "fatal: ambiguous
/// argument 'HEAD': unknown revision", the same shape of failure
/// `run_with_timeout` itself produces for a genuine timeout. That ambiguity
/// is harmless at every call site *except* where `run_sync` first
/// establishes the parent for a possible new commit — see `resolve_parent`,
/// used there specifically instead of this function.
pub(super) fn current_head(repo_root: &Path) -> Option<String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "HEAD"]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
/// Like `current_head`, but distinguishes "no commits yet" from "the check
/// itself failed" — self-review finding: `run_sync` treats `parent: None`
/// as "build a root commit," which skips seeding the temporary index from
/// any existing tree (`populate_temp_index`). If `current_head` merely
/// failed *transiently* here (a wedged filesystem, a timeout on the
/// simplest possible git read) on an otherwise non-empty repository, the
/// resulting commit's tree would contain **only the checklist path** —
/// silently dropping every other tracked file — and `verify_commit_scope`
/// wouldn't catch it, since its own baseline is equally wrong under the
/// same bad assumption. `git rev-parse --verify -q HEAD` exits with status
/// **1** specifically for "no commits yet" (confirmed empirically, silent
/// thanks to `-q`) — distinctly from 128 or a timeout for any other
/// failure — so only that exact exit code is treated as `Ok(None)`;
/// anything else becomes `Err`, a hard sync failure rather than a silent
/// wrong assumption. Every other `current_head` call site keeps using that
/// function unchanged — traced through each one and confirmed the
/// ambiguity is harmless there (a transient failure only ever causes an
/// unnecessary refusal or retry, never a silent wrong action), so this
/// stays a narrow fix at the one genuinely dangerous call site rather than
/// a signature change rippling through the whole module.
pub(super) fn resolve_parent(repo_root: &Path) -> Result<Option<String>, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-parse", "--verify", "-q", "HEAD"]);
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => Err(command_error("git rev-parse", &output)),
        Err(err) => Err(format!("git rev-parse failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use std::fs;

    /// markcheck must stage the blob git itself would stage. Without
    /// `--path`, `hash-object` hashes the bytes verbatim and skips the clean
    /// filter, so markcheck published un-normalised content that `git add`
    /// would never have produced — everyone pulling the repository got it.
    #[test]
    fn a_staged_blob_matches_what_git_add_would_have_stored() {
        let work = init_repo_with_crlf_filter();
        let content = "- [x] one\r\n- [ ] two\r\n";

        let ours = hash_object(&work, "tracked.md", content).unwrap();

        // What git produces for the same content through its own porcelain.
        fs::write(work.join("tracked.md"), content).unwrap();
        run(&work, &["add", "tracked.md"]);
        let theirs = index_blob(&work, "tracked.md").unwrap();

        assert_eq!(
            ours, theirs,
            "the blob markcheck stages must be the one `git add` stores"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn index_entry_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-index-entry");
        fs::create_dir_all(&dir).unwrap();
        // Outside a repository `ls-files` itself fails, which is a genuine
        // error — distinct from the `Ok(None)` that means "this repo simply
        // has no index entry for that path".
        assert!(index_entry(&dir, &dir.join("nope.md")).is_err());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn index_entry_reports_an_absent_path_as_untracked_not_an_error() {
        // The other half of the three-way result: inside a healthy repo, a
        // path with no index entry is untracked. `run_sync` depends on that
        // distinction, since `git status` cannot be trusted to report
        // untrackedness on its own.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("untracked.md"), "- [ ] new\n").unwrap();

        assert_eq!(index_entry(&work, &work.join("untracked.md")), Ok(None));
        assert!(matches!(
            index_entry(&work, &work.join("tracked.md")),
            Ok(Some(_))
        ));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn hash_object_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-hash-object");
        fs::create_dir_all(&dir).unwrap();
        assert!(hash_object(&dir, "tracked.md", "content").is_err());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn descends_directly_from_matches_only_a_direct_child() {
        let work = init_repo_without_remote();
        let root = current_head(&work).unwrap();
        fs::write(work.join("other.md"), "second\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "second"]);
        let second = current_head(&work).unwrap();
        fs::write(work.join("third.md"), "third\n").unwrap();
        run(&work, &["add", "third.md"]);
        run(&work, &["commit", "-q", "-m", "third"]);
        let third = current_head(&work).unwrap();

        assert!(descends_directly_from(&work, &second, &Some(root.clone())));
        assert!(
            !descends_directly_from(&work, &third, &Some(root.clone())),
            "a grandchild is not a direct child"
        );
        assert!(
            descends_directly_from(&work, &root, &None),
            "a root commit has no parents, matching parent: None"
        );
        assert!(
            !descends_directly_from(&work, &second, &None),
            "a commit with a parent must not match the root-commit case"
        );
        assert!(
            !descends_directly_from(&work, "not-a-commit", &Some(root)),
            "fails closed when git can't answer"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn git_dir_resolves_to_an_absolute_path() {
        let work = init_repo_without_remote();
        let dir = git_dir(&work).unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with(".git"));
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn git_dir_resolves_correctly_from_a_subdirectory() {
        // `git rev-parse --git-dir` itself returns an already-absolute path
        // when run from a subdirectory of the work tree (unlike the
        // relative "./.git" it returns from the root) — exercises that
        // branch specifically, distinct from the join-with-repo_root
        // fallback the root-invocation test above exercises.
        let work = init_repo_without_remote();
        let sub = work.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let dir = git_dir(&sub).unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with(".git"));
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn git_dir_none_outside_a_repo() {
        let dir = unique_dir("not-a-repo-git-dir");
        fs::create_dir_all(&dir).unwrap();
        assert!(git_dir(&dir).is_none());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn resolve_parent_is_ok_none_for_a_genuinely_empty_repo() {
        // Self-review finding: current_head's None is ambiguous between
        // "no commits yet" and "the check itself failed" (both look like
        // `output.status.success() == false`). resolve_parent must
        // distinguish them -- confirmed here against a real empty repo,
        // where `git rev-parse --verify -q HEAD` exits 1 specifically.
        let work = unique_dir("repo-empty-resolve-parent").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);

        assert_eq!(resolve_parent(&work), Ok(None));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn resolve_parent_is_ok_some_for_a_normal_repo() {
        let work = init_repo_without_remote();
        let expected = current_head(&work).unwrap();

        assert_eq!(resolve_parent(&work), Ok(Some(expected)));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
    #[test]
    fn resolve_parent_is_err_outside_a_repo() {
        // Not "no commits yet" (exit 1) -- a completely different failure
        // shape (128, "not a git repository") -- must not be silently
        // treated the same as an empty repo.
        let dir = unique_dir("not-a-repo-resolve-parent");
        fs::create_dir_all(&dir).unwrap();
        assert!(resolve_parent(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn parse_first_parent_tells_a_root_commit_from_unreadable_output() {
        // Deep rounds 3, round 1. `undo_commit` collapsed both into "no
        // parent", and its no-parent path *deletes the branch ref* rather
        // than moving it back one commit. An empty listing is reachable:
        // output read back as empty alongside a successful exit status is
        // not a contradiction — see `parse_first_parent`'s own doc comment —
        // so an unreadable listing could have deleted the branch.
        assert_eq!(
            parse_first_parent(""),
            None,
            "no output at all must not read as a root commit"
        );
        assert_eq!(parse_first_parent("   \n"), None, "nor whitespace only");
        assert_eq!(
            parse_first_parent("abc123\n"),
            Some(None),
            "a lone commit really is a root commit"
        );
        assert_eq!(
            parse_first_parent("abc123 def456\n"),
            Some(Some("def456".to_string()))
        );
        assert_eq!(
            parse_first_parent("abc123 def456 789abc\n"),
            Some(Some("def456".to_string())),
            "a merge's *first* parent is the one to rewind to"
        );
    }
}
