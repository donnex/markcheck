use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;

/// Result of one background commit+push attempt, delivered to the
/// main loop via [`GitSync::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Committed and pushed.
    Synced,
    /// Nothing to do: the file is already tracked and identical to what's
    /// already committed. Not reported to the user — nothing meaningful
    /// happened, so silence here doesn't read as broken.
    Skipped,
    /// The file isn't tracked by git at all. Unlike `Skipped`, this *is*
    /// reported to the user (git-sync being silently unable to do anything,
    /// forever, looks exactly like a bug) — but it still never gets
    /// `git add`ed automatically; that stays the user's call.
    SkippedUntracked,
    /// `git status`/`commit`/`push` failed; the message is the first line
    /// of the failing command's stderr.
    Failed(String),
}

/// Drives commit+push for one file on a background thread, so a slow or
/// offline `git push` never blocks the UI. Mirrors `FileWatcher`:
/// a background worker feeds an `mpsc` channel that the main loop drains
/// non-blockingly once per frame via [`poll`](GitSync::poll).
pub struct GitSync {
    repo_dir: PathBuf,
    file_path: PathBuf,
    sender: mpsc::Sender<SyncOutcome>,
    receiver: mpsc::Receiver<SyncOutcome>,
    /// `true` while a background sync is running. A `request` that arrives
    /// while busy is coalesced into `pending` rather than spawning a second
    /// thread — two concurrent `git commit`/`push` runs on the same repo
    /// could race on the index/HEAD.
    busy: bool,
    /// The most recent request received while `busy`; only the latest
    /// matters, since every sync commits whatever is currently on disk
    /// rather than a specific diff.
    pending: Option<String>,
}

impl GitSync {
    /// Confirms `file_path`'s directory is inside a git work tree and, if
    /// so, returns a `GitSync` ready to accept requests. `None` when it
    /// isn't (or `git` itself can't be run) — git-sync is a convenience
    /// feature, so this fails open rather than erroring out.
    pub fn detect(file_path: &Path) -> Option<GitSync> {
        let repo_dir = file_path.parent()?.to_path_buf();
        let output = Command::new("git")
            .current_dir(&repo_dir)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        Some(GitSync {
            repo_dir,
            file_path: file_path.to_path_buf(),
            sender,
            receiver,
            busy: false,
            pending: None,
        })
    }

    /// Requests a commit+push with `change_desc` describing what changed
    /// (e.g. `Check "Restart service"`); the full commit message is built
    /// from the file name plus this description. Coalesced with any
    /// already-running sync per the `pending` rule above.
    pub fn request(&mut self, change_desc: String) {
        if self.busy {
            self.pending = Some(change_desc);
            return;
        }
        self.spawn(change_desc);
    }

    fn spawn(&mut self, change_desc: String) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let message = commit_message(&file_path, &change_desc);
            let outcome = run_sync(&repo_dir, &file_path, &message);
            let _ = sender.send(outcome);
        });
    }

    /// Drains the channel non-blockingly; call once per frame regardless of
    /// input, like `FileWatcher::poll_changed`. Returns the outcome of a
    /// completed sync, if one just finished, and kicks off a queued
    /// request that arrived while busy.
    pub fn poll(&mut self) -> Option<SyncOutcome> {
        let outcome = self.receiver.try_recv().ok();
        if outcome.is_some() {
            self.busy = false;
            if let Some(change_desc) = self.pending.take() {
                self.spawn(change_desc);
            }
        }
        outcome
    }
}

/// Commit messages are kept to one line and capped here so a long task
/// title (the usual source of `change_desc`) can't produce an unwieldy
/// `git log` entry; the file-name prefix is always kept intact and only
/// the description is cut, with a trailing `…` marking the cut.
const MAX_COMMIT_MESSAGE_LEN: usize = 80;

fn commit_message(file_path: &Path, change_desc: &str) -> String {
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checklist".to_string());
    let prefix = format!("{file_name}: ");
    let full = format!("{prefix}{change_desc}");
    if full.chars().count() <= MAX_COMMIT_MESSAGE_LEN {
        return full;
    }
    let budget = MAX_COMMIT_MESSAGE_LEN.saturating_sub(prefix.chars().count() + 1);
    let truncated: String = change_desc.chars().take(budget).collect();
    format!("{prefix}{truncated}\u{2026}")
}

/// Runs the commit+push sequence synchronously; called from the background
/// thread spawned by `spawn`, kept as a free function so tests can drive it
/// directly without waiting on a thread.
fn run_sync(repo_dir: &Path, file_path: &Path, message: &str) -> SyncOutcome {
    // Scoped to exactly this one file (`--`), so there's at most one
    // porcelain line to interpret: absent (nothing changed vs. HEAD),
    // `?? ` (untracked), or any other two-letter code (a real change to
    // commit). Distinguishing untracked from unchanged — rather than the
    // single `--untracked-files=no` check this used to be, which folded
    // both into the same silent no-op — is what lets an untracked file be
    // reported below instead of a sync that quietly never does anything.
    let status = match Command::new("git")
        .current_dir(repo_dir)
        .args(["status", "--porcelain", "--"])
        .arg(file_path)
        .output()
    {
        Ok(output) => output,
        Err(err) => return SyncOutcome::Failed(format!("git status failed: {err}")),
    };
    if !status.status.success() {
        return SyncOutcome::Failed(command_error("git status", &status));
    }
    if status.stdout.starts_with(b"??") {
        return SyncOutcome::SkippedUntracked;
    }
    if status.stdout.is_empty() {
        return SyncOutcome::Skipped;
    }

    // `--only` commits exactly this path's current working-tree content and
    // refuses outright if it isn't already tracked, so this can never stage
    // (let alone commit) a new file even if the check above were wrong.
    let commit = Command::new("git")
        .current_dir(repo_dir)
        .args(["commit", "--only", "-m", message, "--"])
        .arg(file_path)
        .output();
    match commit {
        Ok(output) if output.status.success() => {}
        Ok(output) => return SyncOutcome::Failed(command_error("git commit", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git commit failed: {err}")),
    }

    let push = Command::new("git")
        .current_dir(repo_dir)
        .arg("push")
        .output();
    match push {
        Ok(output) if output.status.success() => SyncOutcome::Synced,
        Ok(output) => SyncOutcome::Failed(command_error("git push", &output)),
        Err(err) => SyncOutcome::Failed(format!("git push failed: {err}")),
    }
}

/// The first line of a failed command's stderr, prefixed with which command
/// produced it — short enough for the single-line sticky status bar.
fn command_error(step: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first_line = stderr.lines().next().unwrap_or("unknown error").trim();
    format!("{step}: {first_line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    fn unique_dir(name_hint: &str) -> PathBuf {
        crate::test_support::unique_temp_path("git-sync", name_hint, None)
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git command failed to run");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// A repo with one committed file (`tracked.md`), ready for tests to
    /// dirty and sync. `origin` is a bare remote already set as upstream, so
    /// `git push` (no explicit remote/branch args) has somewhere to go.
    fn init_repo_with_remote() -> (PathBuf, PathBuf) {
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

    /// Nested under its own unique root (like `init_repo_with_remote`), not
    /// returned directly from `unique_dir` — otherwise `work.parent()` would
    /// be the shared system temp directory itself, and every test's cleanup
    /// (`remove_dir_all(work.parent().unwrap())`) would attempt to wipe it.
    fn init_repo_without_remote() -> PathBuf {
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

    #[test]
    fn detect_finds_a_repo() {
        let work = init_repo_without_remote();
        assert!(GitSync::detect(&work.join("tracked.md")).is_some());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn detect_none_outside_a_repo() {
        let dir = unique_dir("not-a-repo");
        fs::create_dir_all(&dir).unwrap();
        assert!(GitSync::detect(&dir.join("file.md")).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_sync_reports_an_untracked_file_without_adding_it() {
        let work = init_repo_without_remote();
        let untracked = work.join("untracked.md");
        fs::write(&untracked, "- [ ] new\n").unwrap();

        assert_eq!(
            run_sync(&work, &untracked, "should not commit"),
            SyncOutcome::SkippedUntracked
        );
        // Confirm it really never got added.
        let status = Command::new("git")
            .current_dir(&work)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("?? untracked.md"));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_skips_when_tracked_file_is_unchanged() {
        let work = init_repo_without_remote();
        assert_eq!(
            run_sync(&work, &work.join("tracked.md"), "no changes"),
            SyncOutcome::Skipped
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_commits_and_pushes_a_tracked_change() {
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(run_sync(&work, &file_path, &message), SyncOutcome::Synced);

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "tracked.md: Check \"one\""
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_reports_failure_when_push_has_no_remote() {
        let work = init_repo_without_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

        let outcome = run_sync(&work, &work.join("tracked.md"), "Check \"one\"");
        assert!(matches!(outcome, SyncOutcome::Failed(_)));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repeated_requests_against_an_unreachable_remote_never_deadlock_or_lose_edits() {
        // Simulates several toggles happening in quick succession while the
        // network is down. Pointing at a nonexistent path makes `git push`
        // fail fast and deterministically, in place of a real (slow,
        // OS-timeout-dependent) network hang.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        // Three edits fired in a row while `sync` is still busy with the
        // first. Each `request` call is itself synchronous and instant (it
        // only ever spawns a thread or sets `pending` — never runs `git`
        // inline), so none of this blocks regardless of how slow the
        // background attempt turns out to be.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        sync.request("first".to_string());
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        sync.request("second".to_string());
        fs::write(
            work.join("tracked.md"),
            "- [x] one\n- [x] two\n- [x] three\n",
        )
        .unwrap();
        sync.request("third".to_string());
        assert!(sync.busy, "still mid-flight on the first attempt");

        // Only ever one attempt in flight: "second"/"third" coalesce into a
        // single `pending` slot rather than queuing three separate threads.
        // Poll until it settles back to idle (busy clears once a completed
        // attempt has no queued follow-up) or the deadline passes.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcomes = Vec::new();
        loop {
            if let Some(outcome) = sync.poll() {
                outcomes.push(outcome);
            }
            if !sync.busy && !outcomes.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // At most two attempts ever ran (never three — "second" was always
        // overwritten by "third" in `pending` before it got its own turn),
        // and it settles back to idle rather than getting stuck busy.
        assert!(
            outcomes.len() <= 2,
            "at most 2 sync attempts, never one per request: {outcomes:?}"
        );
        assert!(!sync.busy, "must settle back to idle, not stuck busy");
        assert!(sync.pending.is_none(), "no request left dangling");
        // The very first attempt already had a real change to push, so it's
        // always a failure against the unreachable remote (never Skipped).
        assert!(
            matches!(outcomes.first(), Some(SyncOutcome::Failed(_))),
            "first attempt has a real, unpushed change: {outcomes:?}"
        );

        // Nothing was lost: every edit landed as a *local* commit regardless
        // of the push failing — `commit --only` always commits whatever is
        // on disk when it runs, so the accumulated edits end up in one or
        // two local commits (never zero), just not yet pushed.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(
            subjects.contains("tracked.md: first"),
            "first edit committed locally: {subjects}"
        );
        let final_contents = fs::read_to_string(work.join("tracked.md")).unwrap();
        assert!(
            final_contents.contains("three"),
            "the latest edit is on disk regardless of push failing: {final_contents}"
        );

        // The network "comes back": a plain `git push` from here (not via
        // `GitSync` — standing in for the *next* successful sync once
        // connectivity returns) must send every commit accumulated while it
        // was down, not just the latest, since `push` always sends the whole
        // unpushed range.
        let remote = init_repo_without_remote();
        run(
            &work,
            &["remote", "set-url", "origin", remote.to_str().unwrap()],
        );
        run(&work, &["push", "-q", "origin", "main:recovered"]);
        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "recovered"])
            .output()
            .unwrap();
        let remote_subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            remote_subjects.contains("tracked.md: first"),
            "catch-up push carries every commit made while offline: {remote_subjects}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
        fs::remove_dir_all(remote.parent().unwrap()).ok();
    }

    #[test]
    fn request_and_poll_roundtrip_reports_synced() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.request("Check \"one\"".to_string());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut result = None;
        while Instant::now() < deadline {
            if let Some(outcome) = sync.poll() {
                result = Some(outcome);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(result, Some(SyncOutcome::Synced));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_request_while_busy_is_coalesced_and_runs_after() {
        // The change is written *before* either request, so there's no race
        // between the background thread's `git status` check and a
        // concurrent write on this thread: "first" deterministically has
        // something to commit, and "second" (queued behind it) deterministically
        // finds nothing left to do once "first" has already committed it.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        sync.request("first".to_string());
        assert!(sync.busy, "first request should mark the worker busy");
        // A second `request` while busy doesn't spawn a second concurrent
        // thread (two `git commit`/`push` runs on the same repo could race
        // on the index/HEAD) — it queues instead.
        sync.request("second".to_string());
        assert_eq!(sync.pending.as_deref(), Some("second"));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcomes = Vec::new();
        while Instant::now() < deadline && outcomes.len() < 2 {
            if let Some(outcome) = sync.poll() {
                outcomes.push(outcome);
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert_eq!(outcomes, vec![SyncOutcome::Synced, SyncOutcome::Skipped]);

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "tracked.md: first"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn commit_message_prefixes_the_file_name() {
        assert_eq!(
            commit_message(Path::new("/a/b/checklist.md"), "Check \"x\""),
            "checklist.md: Check \"x\""
        );
    }

    #[test]
    fn commit_message_truncates_a_long_description() {
        let long_item = "x".repeat(100);
        let message = commit_message(
            Path::new("/a/b/checklist.md"),
            &format!("Check \"{long_item}\""),
        );
        assert_eq!(message.chars().count(), 80);
        assert!(message.starts_with("checklist.md: Check \"xxx"));
        assert!(message.ends_with('\u{2026}'));
    }

    #[test]
    fn commit_message_leaves_a_short_description_untruncated() {
        let message = commit_message(Path::new("/a/b/checklist.md"), "Check \"short\"");
        assert_eq!(message, "checklist.md: Check \"short\"");
        assert!(!message.contains('\u{2026}'));
    }
}
