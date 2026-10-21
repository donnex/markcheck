//! `#[cfg(test)]` repository fixtures and the race harness shared by the
//! `git_sync` submodules' tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::shared::RacePoint;
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
/// What [`init_repo`] sets up: `file` committed with one open task in a
/// repository under its own unique root, plus what the fixture adds.
pub(super) struct RepoFixture<'a> {
    pub(super) hint: &'a str,
    pub(super) file: &'a str,
    /// A bare `origin` beside the work tree, with `main` pushed to it.
    pub(super) remote: bool,
    /// A branch checked out and pushed after `main`.
    pub(super) branch: Option<&'a str>,
    /// `core.autocrlf=input`, with the task committed CRLF-terminated.
    pub(super) autocrlf: bool,
}
impl Default for RepoFixture<'_> {
    fn default() -> Self {
        RepoFixture {
            hint: "repo",
            file: "tracked.md",
            remote: false,
            branch: None,
            autocrlf: false,
        }
    }
}
/// The work tree and, when asked for, its bare remote, both under one
/// unique root.
pub(super) fn init_repo(fixture: RepoFixture) -> (PathBuf, Option<PathBuf>) {
    let root = unique_dir(fixture.hint);
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["config", "user.email", "test@example.com"]);
    run(&work, &["config", "user.name", "test"]);
    let line_end = if fixture.autocrlf {
        run(&work, &["config", "core.autocrlf", "input"]);
        "\r\n"
    } else {
        "\n"
    };
    fs::write(work.join(fixture.file), format!("- [ ] one{line_end}")).unwrap();
    run(&work, &["add", "--", fixture.file]);
    run(&work, &["commit", "-q", "-m", "init"]);
    let remote = fixture.remote.then(|| {
        let remote = root.join("remote.git");
        fs::create_dir_all(&remote).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&work, &["push", "-q", "-u", "origin", "main"]);
        remote
    });
    if let Some(branch) = fixture.branch {
        run(&work, &["checkout", "-q", "-b", branch]);
        run(&work, &["push", "-q", "-u", "origin", branch]);
    }
    (work, remote)
}
fn init_repo_with(fixture: RepoFixture) -> (PathBuf, PathBuf) {
    let (work, remote) = init_repo(RepoFixture {
        remote: true,
        ..fixture
    });
    (work, remote.unwrap())
}
/// The checklist on a topic branch: a bare remote refuses to delete its
/// current branch (`main`), which would hide a refspec mistake. Returns
/// `(work, remote, branch)`.
pub(super) fn init_repo_with_remote_on_a_topic_branch() -> (PathBuf, PathBuf, String) {
    let (work, remote) = init_repo_with(RepoFixture {
        hint: "repo-topic-branch",
        branch: Some("topic"),
        ..RepoFixture::default()
    });
    (work, remote, "topic".to_string())
}
/// A repo with one committed `tracked.md` and a bare `origin` upstream.
pub(super) fn init_repo_with_remote() -> (PathBuf, PathBuf) {
    init_repo_with(RepoFixture::default())
}
pub(super) fn init_repo_with_remote_named(file_name: &str) -> (PathBuf, PathBuf) {
    init_repo_with(RepoFixture {
        hint: "repo-named",
        file: file_name,
        ..RepoFixture::default()
    })
}
/// Stage 0 of the real index for `relpath`.
pub(super) fn staged_bytes(repo_root: &Path, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!(":{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}
pub(super) fn init_repo_without_remote() -> PathBuf {
    init_repo(RepoFixture {
        hint: "repo-no-remote",
        ..RepoFixture::default()
    })
    .0
}
pub(super) fn init_repo_with_crlf_filter() -> PathBuf {
    init_repo(RepoFixture {
        hint: "repo-autocrlf",
        autocrlf: true,
        ..RepoFixture::default()
    })
    .0
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
        immediate: false,
        base: SyncBase::Edited { epoch: 0 },
    }
}
thread_local! {
    static RACE_HOOKS: std::cell::RefCell<
        std::collections::HashMap<RacePoint, Box<dyn Fn()>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

pub(super) fn race_point(point: RacePoint) {
    // Taken out while it runs, so a hook reaching a sync point can't recurse.
    let hook = RACE_HOOKS.with(|h| h.borrow_mut().remove(&point));
    if let Some(hook) = hook {
        hook();
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
impl GitSync {
    /// Polls until no sync is running, the requests a finishing one starts
    /// included, or `timeout` passes, and returns every outcome seen in
    /// order; [`GitSync::is_busy`] afterwards says whether it gave up.
    pub fn wait_for_outcomes(&mut self, timeout: Duration) -> Vec<SyncOutcome> {
        self.wait_for_outcomes_unless(timeout, || false)
    }
}
