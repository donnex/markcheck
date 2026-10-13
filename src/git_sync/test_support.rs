//! `#[cfg(test)]` repository fixtures and the race harness shared by the
//! `git_sync` submodules' tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::shared::{RACE_HOOKS, RacePoint};
use super::*;

pub(super) fn unique_dir(name_hint: &str) -> PathBuf {
    crate::test_support::unique_temp_path("git-sync", name_hint, None)
}
pub(super) fn run(dir: &Path, args: &[&str]) {
    let status = crate::test_support::git()
        .current_dir(dir)
        .args(args)
        .status()
        .expect("git command failed to run");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}
pub(super) fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = crate::test_support::git()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git command failed to run");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
/// The checklist on a topic branch: a bare remote refuses to delete its
/// current branch (`main`), which would hide a refspec mistake. Returns
/// `(work, remote, branch)`.
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
/// A repo with one committed `tracked.md` and a bare `origin` upstream.
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
/// Stage 0 of the real index for `relpath`.
pub(super) fn staged_bytes(repo_root: &Path, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!(":{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}
/// Nested under its own unique root, so a test removing `work.parent()`
/// never wipes the shared temp directory.
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
/// A `latest_requested_hash` where this call's `content` is the only
/// request.
pub(super) fn no_race(content: &str) -> Mutex<[u8; 32]> {
    Mutex::new(hash_bytes(content.as_bytes()))
}
pub(super) fn staged_matches(content: &str) -> Option<&str> {
    Some(content)
}
pub(super) fn pending_sync(content: &str, description: &str) -> PendingSync {
    PendingSync {
        content: content.to_string(),
        content_hash: hash_bytes(content.as_bytes()),
        previous_content: None,
        description: description.to_string(),
    }
}
/// Removed on drop, so a hook never leaks into the next test on the thread.
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
pub(super) fn commit_unrelated(work: &Path, name: &str) -> String {
    fs::write(work.join(name), "unrelated\n").unwrap();
    run(work, &["add", name]);
    run(work, &["commit", "-q", "-m", "unrelated"]);
    git_stdout(work, &["rev-parse", "HEAD"])
}
pub(super) fn still_on_branch(work: &Path, sha: &str) -> bool {
    crate::test_support::git()
        .current_dir(work)
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
pub(super) fn reached_the_remote(remote: &Path, sha: &str) -> bool {
    crate::test_support::git()
        .current_dir(remote)
        .args(["merge-base", "--is-ancestor", sha, "main"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Large enough for the hook's nested `git commit` to land before the
/// timeout under parallel test load; 300ms failed about half the time.
pub(super) const HOOK_RACE_TIMEOUT: Duration = Duration::from_secs(2);
/// Outlives `HOOK_RACE_TIMEOUT`; the group is killed first.
pub(super) const HOOK_RACE_SLEEP: &str = "30";
