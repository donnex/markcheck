//! `#[cfg(test)]` helpers shared by the `git_sync` submodules' test modules.
//!
//! Repository fixtures and the race harness live here rather than in any one
//! submodule, so tests can sit beside the code they exercise without each
//! file rebuilding its own `git init` boilerplate. Distinct from the
//! crate-level `crate::test_support`, which only provides unique temp paths.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::*;

pub(super) fn unique_dir(name_hint: &str) -> PathBuf {
    crate::test_support::unique_temp_path("git-sync", name_hint, None)
}
pub(super) fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("git command failed to run");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}
/// Trimmed stdout of a `git` command, for asserting on repository state.
pub(super) fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git command failed to run");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
/// Like `init_repo_with_remote`, but the checklist lives on a **topic**
/// branch while the remote's own `HEAD` stays on `main`. Every other
/// git-sync test works on `main`, which is exactly the branch a bare
/// remote protects (`receive.denyDeleteCurrent` refuses to delete its
/// current branch by default) — so a refspec-level mistake that deletes
/// or clobbers the target ref is invisible there and only observable on
/// a branch like this one, which is also the shape of every real topic
/// branch. Returns `(work, remote, branch)`.
pub(super) fn init_repo_with_remote_on_a_topic_branch() -> (PathBuf, PathBuf, String) {
    let root = unique_dir("repo-topic-branch");
    let remote = root.join("remote.git");
    let work = root.join("work");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&work).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    run(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&work, &["push", "-q", "-u", "origin", "main"]);
    run(&work, &["checkout", "-q", "-b", "topic"]);
    run(&work, &["push", "-q", "-u", "origin", "topic"]);
    (work, remote, "topic".to_string())
}
/// A repo with one committed file (`tracked.md`), ready for tests to
/// dirty and sync. `origin` is a bare remote already set as upstream, so
/// `git push` (no explicit remote/branch args) has somewhere to go.
pub(super) fn init_repo_with_remote() -> (PathBuf, PathBuf) {
    let root = unique_dir("repo");
    let remote = root.join("remote.git");
    let work = root.join("work");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&work).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    run(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&work, &["push", "-q", "-u", "origin", "main"]);
    (work, remote)
}
/// Like `init_repo_with_remote`, but the tracked file is named
/// `file_name` instead of the fixed `tracked.md` — for exercising the
/// git plumbing path (`index_entry`/`blob_bytes_at`/`stage_into_temp_index`)
/// against unusual filenames.
pub(super) fn init_repo_with_remote_named(file_name: &str) -> (PathBuf, PathBuf) {
    let root = unique_dir("repo-named");
    let remote = root.join("remote.git");
    let work = root.join("work");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&work).unwrap();
    run(&remote, &["init", "--bare", "-q", "-b", "main"]);
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    fs::write(work.join(file_name), "- [ ] one\n").unwrap();
    run(&work, &["add", "--", file_name]);
    run(&work, &["commit", "-q", "-m", "init"]);
    run(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&work, &["push", "-q", "-u", "origin", "main"]);
    (work, remote)
}
/// Nested under its own unique root (like `init_repo_with_remote`), not
/// returned directly from `unique_dir` — otherwise `work.parent()` would
/// be the shared system temp directory itself, and every test's cleanup
/// (`remove_dir_all(work.parent().unwrap())`) would attempt to wipe it.
/// The checklist's staged content — stage 0 of the **real** index (`git
/// show :<path>`), not `HEAD`'s. `None` when there is no staged entry or
/// the read fails.
///
/// A test helper rather than production code: the staged-target guard
/// used to read these bytes to compare them against a digest of the
/// working tree, which is exactly the comparison that broke under a
/// clean filter. It now compares blob SHAs git computed for the path, so
/// nothing in production needs the bytes — only the tests asserting a
/// staged version survived a race still do.
pub(super) fn staged_bytes(repo_root: &Path, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!(":{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}
pub(super) fn init_repo_without_remote() -> PathBuf {
    let work = unique_dir("repo-no-remote").join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    work
}
pub(super) fn init_repo_with_crlf_filter() -> PathBuf {
    let work = unique_dir("repo-autocrlf").join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    run(&work, &["config", "core.autocrlf", "input"]);
    fs::write(work.join("tracked.md"), "- [ ] one\r\n").unwrap();
    run(&work, &["add", "tracked.md"]);
    run(&work, &["commit", "-q", "-m", "init"]);
    work
}
/// A `latest_requested_hash` for a direct `run_sync` call representing
/// "nobody else has requested anything since" — i.e. the ordinary,
/// non-racing case most tests want, where the only known request is
/// this call's own `content`.
pub(super) fn no_race(content: &str) -> Mutex<[u8; 32]> {
    Mutex::new(hash_bytes(content.as_bytes()))
}
/// The pre-write content hash for a test whose staged checklist matches
/// the content being synced — the ordinary "staged, nothing lost" case,
/// which the guard must let through.
/// The pre-write content a request carries — what the staged-target
/// guard compares the index against, now in git's terms rather than as a
/// digest (see `hash_object_inner`).
pub(super) fn staged_matches(content: &str) -> Option<&str> {
    Some(content)
}
pub(super) fn pending_sync(content: &str, description: &str) -> PendingSync {
    PendingSync {
        content: content.to_string(),
        content_hash: hash_bytes(content.as_bytes()),
        // These tests drive `GitSync`'s request/coalescing machinery, not
        // the staged-target guard; none of them stage the checklist, so
        // the guard short-circuits before this is consulted.
        previous_content: None,
        description: description.to_string(),
    }
}
/// Installs a hook at `point` for this thread, removing it on drop so a
/// test can never leak one into whatever runs next on the same thread.
pub(super) struct RaceHook;
impl RaceHook {
    pub(super) fn at(point: RacePoint, hook: impl Fn() + 'static) -> RaceHook {
        RACE_HOOKS.with(|h| h.borrow_mut().insert(point, Box::new(hook)));
        RaceHook
    }
}
impl Drop for RaceHook {
    fn drop(&mut self) {
        RACE_HOOKS.with(|h| h.borrow_mut().clear());
    }
}
pub(super) fn racer_sha_probe(work: &Path) -> String {
    fs::read_to_string(work.join(".racer-sha")).unwrap_or_default()
}
/// Commits an unrelated file in `work` — the concurrent writer every
/// interleaving test below races against.
pub(super) fn commit_unrelated(work: &Path, name: &str) -> String {
    fs::write(work.join(name), "unrelated\n").unwrap();
    run(work, &["add", name]);
    run(work, &["commit", "-q", "-m", "unrelated"]);
    git_stdout(work, &["rev-parse", "HEAD"])
}
/// Whether `sha` is still reachable from the branch.
pub(super) fn still_on_branch(work: &Path, sha: &str) -> bool {
    Command::new("git")
        .current_dir(work)
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// Whether `sha` is reachable from the remote's branch — i.e. it has
/// been published.
pub(super) fn reached_the_remote(remote: &Path, sha: &str) -> bool {
    Command::new("git")
        .current_dir(remote)
        .args(["merge-base", "--is-ancestor", sha, "main"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Injected `commit_temp_index` timeout for the two tests whose hook has
/// to complete a real nested `git commit` *before* the timeout fires and
/// `HEAD` is re-resolved.
///
/// Deliberately much larger than the 300ms the other timeout tests use:
/// those only need their hook to reach its `sleep`, while these need it
/// to finish actual git work first. At 300ms that was a coin flip under
/// parallel test load — the nested commit hadn't landed when `HEAD` was
/// re-resolved, so the ownership check correctly adopted markcheck's own
/// commit (the right answer for that interleaving) and the test failed
/// asserting the other one. Reproduced by running the suite six ways
/// concurrently: roughly half the runs failed.
///
/// The hooks sleep far longer than this, so the timeout — not the sleep
/// — is still what ends the run, and the timeout branch is still what's
/// under test.
pub(super) const HOOK_RACE_TIMEOUT: Duration = Duration::from_secs(2);
/// Sleep for a hook that must outlive `HOOK_RACE_TIMEOUT`. The process
/// group is killed on timeout, so this never actually elapses.
pub(super) const HOOK_RACE_SLEEP: &str = "30";
