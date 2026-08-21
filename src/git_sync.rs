use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;

use crate::model::PendingSync;

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
    /// matters, since a coalesced-over request's content already includes
    /// whatever the dropped one would have committed.
    pending: Option<PendingSync>,
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

    /// Requests a commit+push for `sync.content` (the exact file content
    /// expected once the underlying change lands), labeled with
    /// `sync.description` (e.g. `Check "Restart service"`); the full commit
    /// message is built from the file name plus this description. Coalesced
    /// with any already-running sync per the `pending` rule above.
    pub fn request(&mut self, sync: PendingSync) {
        if self.busy {
            self.pending = Some(sync);
            return;
        }
        self.spawn(sync);
    }

    fn spawn(&mut self, sync: PendingSync) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let message = commit_message(&file_path, &sync.description);
            let outcome = run_sync(&repo_dir, &file_path, &sync.content, &message);
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
            if let Some(sync) = self.pending.take() {
                self.spawn(sync);
            }
        }
        outcome
    }

    /// Whether a sync is currently running or queued behind one that is.
    /// Used only when quitting, to decide whether it's worth waiting at all.
    pub fn is_busy(&self) -> bool {
        self.busy
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
    // change_desc is usually `Verb "item text"`; if the cut lands inside the
    // quoted item text, the opening `"` would otherwise never be closed.
    if truncated.matches('"').count() % 2 == 1 {
        let shorter: String = change_desc.chars().take(budget.saturating_sub(1)).collect();
        format!("{prefix}{shorter}\u{2026}\"")
    } else {
        format!("{prefix}{truncated}\u{2026}")
    }
}

/// Runs the commit+push sequence synchronously; called from the background
/// thread spawned by `spawn`, kept as a free function so tests can drive it
/// directly without waiting on a thread.
///
/// The commit is built entirely from `expected_content` via plumbing rather
/// than `git commit`'s own path-based commit — despite appearances, `git
/// commit --only`/a plain pathspec always re-reads the file's *current
/// working-tree content* for the named path (it stages it fresh, ignoring
/// whatever's already in the index for that path), never a manually staged
/// index entry. That's exactly the hazard: if some other change (another
/// markcheck write, an external editor, anything) lands on disk between a
/// request being queued and this function actually running, the old
/// working-tree-reading commit would silently absorb it under a message
/// that only describes the *original* request. Building the commit from
/// `expected_content` instead — via `hash-object`/`update-index
/// --cacheinfo`/`write-tree`/`commit-tree`/`update-ref`, none of which touch
/// the working tree at all — makes the commit's content always match its
/// message exactly, regardless of what else is happening to the file
/// concurrently. A later, unrelated change still gets synced — as its own,
/// separately labeled commit, the next time `request` runs — it just can
/// never bleed into this one.
fn run_sync(
    repo_dir: &Path,
    file_path: &Path,
    expected_content: &str,
    message: &str,
) -> SyncOutcome {
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

    // The path relative to the repo root (required by the plumbing commands
    // below, several of which don't share `status`/`commit`/`push`'s
    // CWD-relative pathspec handling) plus the file's tracked mode, in one
    // call.
    let (mode, relpath) = match index_entry(repo_dir, file_path) {
        Ok(entry) => entry,
        Err(err) => return SyncOutcome::Failed(err),
    };

    // All plumbing commands below run from the repo root with a
    // root-relative path, sidestepping any ambiguity between CWD-relative
    // and repo-root-relative pathspec handling.
    let repo_root = match Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };

    // If HEAD already holds exactly this content, this request was already
    // satisfied by an earlier sync (e.g. it sat coalesced behind one that
    // committed the same or newer content) — nothing left to do, even
    // though `status` above is non-empty because of someone else's
    // still-uncommitted change to the file.
    if head_blob(&repo_root, &relpath).as_deref() == Some(expected_content.as_bytes()) {
        return SyncOutcome::Skipped;
    }

    let blob = match hash_object(&repo_root, expected_content) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };
    if let Err(err) = stage_blob(&repo_root, &mode, &blob, &relpath) {
        return SyncOutcome::Failed(err);
    }
    let tree = match write_tree(&repo_root) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };
    let parent = current_head(&repo_root);
    let commit = match commit_tree(&repo_root, &tree, parent.as_deref(), message) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };
    if let Err(err) = update_head(&repo_root, &commit) {
        return SyncOutcome::Failed(err);
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

/// The file's tracked mode and its path relative to the repo root, read from
/// the index in one call (`git ls-files --stage --full-name`). Only called
/// once `status` has already confirmed the path is tracked (not `??`).
fn index_entry(repo_dir: &Path, file_path: &Path) -> Result<(String, String), String> {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-files", "--stage", "--full-name", "--"])
        .arg(file_path)
        .output()
        .map_err(|err| format!("git ls-files failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git ls-files", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| "git ls-files: no index entry for file".to_string())?;
    let (info, path) = line
        .split_once('\t')
        .ok_or_else(|| format!("git ls-files: unexpected output {line:?}"))?;
    let mode = info
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("git ls-files: unexpected output {line:?}"))?;
    Ok((mode.to_string(), path.to_string()))
}

/// `HEAD`'s current committed bytes for `relpath` (repo-root-relative), or
/// `None` if the path has no HEAD entry yet (e.g. staged but never
/// committed) or `git show` otherwise fails.
fn head_blob(repo_root: &Path, relpath: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["show", &format!("HEAD:{relpath}")])
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Writes `content` into the object database without touching the working
/// tree or index, returning its blob SHA.
fn hash_object(repo_root: &Path, content: &str) -> Result<String, String> {
    let mut child = Command::new("git")
        .current_dir(repo_root)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("git hash-object failed: {err}"))?;
    child
        .stdin
        .take()
        .expect("stdin was requested as piped")
        .write_all(content.as_bytes())
        .map_err(|err| format!("git hash-object failed: {err}"))?;
    let output = child
        .wait_with_output()
        .map_err(|err| format!("git hash-object failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git hash-object", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Stages `blob` for `relpath` in the index, at `mode`, without touching any
/// other index entry or the working tree.
fn stage_blob(repo_root: &Path, mode: &str, blob: &str, relpath: &str) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["update-index", "--cacheinfo", mode, blob, relpath])
        .output()
        .map_err(|err| format!("git update-index failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-index", &output));
    }
    Ok(())
}

/// Writes the current index out as a tree object, returning its SHA.
fn write_tree(repo_root: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["write-tree"])
        .output()
        .map_err(|err| format!("git write-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git write-tree", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The current commit `HEAD` points at, or `None` for a branch with no
/// commits yet (so the next commit is created as a root commit).
fn current_head(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Creates a commit object from `tree` with `parent` (if any) and `message`,
/// without moving any ref — returns the new commit's SHA.
fn commit_tree(
    repo_root: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, String> {
    let mut args = vec!["commit-tree".to_string(), tree.to_string()];
    if let Some(parent) = parent {
        args.push("-p".to_string());
        args.push(parent.to_string());
    }
    args.push("-m".to_string());
    args.push(message.to_string());
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(&args)
        .output()
        .map_err(|err| format!("git commit-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git commit-tree", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Advances the current branch (via `HEAD`) to `commit` — the plumbing
/// equivalent of what `git commit` does at the ref level once its commit
/// object exists.
fn update_head(repo_root: &Path, commit: &str) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["update-ref", "HEAD", commit])
        .output()
        .map_err(|err| format!("git update-ref failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-ref", &output));
    }
    Ok(())
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
            run_sync(&work, &untracked, "- [ ] new\n", "should not commit"),
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
            run_sync(&work, &work.join("tracked.md"), "- [ ] one\n", "no changes"),
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

        assert_eq!(
            run_sync(&work, &file_path, "- [x] one\n", &message),
            SyncOutcome::Synced
        );

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

        let outcome = run_sync(
            &work,
            &work.join("tracked.md"),
            "- [x] one\n",
            "Check \"one\"",
        );
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
        sync.request(PendingSync {
            content: "- [x] one\n".to_string(),
            description: "first".to_string(),
        });
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        sync.request(PendingSync {
            content: "- [x] one\n- [x] two\n".to_string(),
            description: "second".to_string(),
        });
        fs::write(
            work.join("tracked.md"),
            "- [x] one\n- [x] two\n- [x] three\n",
        )
        .unwrap();
        sync.request(PendingSync {
            content: "- [x] one\n- [x] two\n- [x] three\n".to_string(),
            description: "third".to_string(),
        });
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
        sync.request(PendingSync {
            content: "- [x] one\n".to_string(),
            description: "Check \"one\"".to_string(),
        });

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

        sync.request(PendingSync {
            content: "- [x] one\n".to_string(),
            description: "first".to_string(),
        });
        assert!(sync.busy, "first request should mark the worker busy");
        // A second `request` while busy doesn't spawn a second concurrent
        // thread (two `git commit`/`push` runs on the same repo could race
        // on the index/HEAD) — it queues instead.
        sync.request(PendingSync {
            content: "- [x] one\n".to_string(),
            description: "second".to_string(),
        });
        assert_eq!(
            sync.pending.as_ref().map(|p| p.description.as_str()),
            Some("second")
        );

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
    fn run_sync_commits_exactly_the_expected_content_ignoring_concurrent_disk_changes() {
        // The core regression test for the reported git-sync race: even
        // though the working-tree file already has more written to it than
        // this request knows about — simulating an unrelated concurrent
        // write (another toggle, an external editor) landing between the
        // request being queued and the sync worker actually running — the
        // commit must contain only the content *this* request captured,
        // never a mix of the two silently attributed to this request's
        // message.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"A\"");

        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();

        assert_eq!(
            run_sync(&work, &file_path, "- [x] A\n", &message),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&show.stdout),
            "- [x] A\n",
            "commit must hold exactly the requested snapshot, not the unrelated concurrent write"
        );
        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "tracked.md: Check \"A\""
        );

        // B is still sitting on disk, uncommitted — not lost, just not part
        // of this commit; a later sync (its own request) picks it up.
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] A\n- [x] B\n"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_later_request_commits_the_concurrent_change_separately() {
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();

        let first_message = commit_message(&file_path, "Check \"A\"");
        assert_eq!(
            run_sync(&work, &file_path, "- [x] A\n", &first_message),
            SyncOutcome::Synced
        );

        let second_message = commit_message(&file_path, "Check \"B\"");
        assert_eq!(
            run_sync(&work, &file_path, "- [x] A\n- [x] B\n", &second_message),
            SyncOutcome::Synced
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("tracked.md: Check \"A\""));
        assert!(subjects.contains("tracked.md: Check \"B\""));

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_skips_when_expected_content_already_matches_head() {
        let work = init_repo_without_remote();
        let file_path = work.join("tracked.md");
        // Working tree has drifted further (an unrelated concurrent change)
        // so `git status` is non-empty, but this specific request's
        // expected content is already what's committed at HEAD — e.g. it
        // sat coalesced behind an earlier sync that already committed it.
        fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

        assert_eq!(
            run_sync(&work, &file_path, "- [ ] one\n", "already committed"),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_works_when_the_file_is_in_a_repo_subdirectory() {
        // `repo_dir` (the file's own parent) isn't the repo root here,
        // exercising the plumbing commands' repo-root-relative path
        // handling (`index_entry`/`head_blob`/`stage_blob`) rather than the
        // CWD-relative handling `status`/`push` rely on.
        let root = unique_dir("repo-nested");
        let remote = root.join("remote.git");
        let repo = root.join("repo");
        let sub = repo.join("checklists");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&sub).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "test"]);
        fs::write(sub.join("tracked.md"), "- [ ] one\n").unwrap();
        run(&repo, &["add", "checklists/tracked.md"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo, &["push", "-q", "-u", "origin", "main"]);

        let file_path = sub.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        assert_eq!(
            run_sync(&sub, &file_path, "- [x] one\n", &message),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:checklists/tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn index_entry_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-index-entry");
        fs::create_dir_all(&dir).unwrap();
        assert!(index_entry(&dir, &dir.join("nope.md")).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_object_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-hash-object");
        fs::create_dir_all(&dir).unwrap();
        assert!(hash_object(&dir, "content").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stage_blob_reports_failure_for_an_invalid_mode() {
        let work = init_repo_without_remote();
        let blob = hash_object(&work, "content").unwrap();
        assert!(stage_blob(&work, "not-a-mode", &blob, "tracked.md").is_err());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn write_tree_reports_failure_outside_a_git_repo() {
        let dir = unique_dir("not-a-repo-write-tree");
        fs::create_dir_all(&dir).unwrap();
        assert!(write_tree(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_tree_reports_failure_for_an_invalid_tree_sha() {
        let work = init_repo_without_remote();
        let bad_tree = "0".repeat(40);
        assert!(commit_tree(&work, &bad_tree, None, "msg").is_err());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn update_head_reports_failure_for_a_malformed_commit_id() {
        // `update-ref` accepts any well-formed-looking SHA without checking
        // the object actually exists (that's `fsck`'s job), so the failure
        // case to exercise is a value that isn't even object-id shaped.
        let work = init_repo_without_remote();
        assert!(update_head(&work, "not-a-commit-id").is_err());
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
        assert!(
            message.ends_with("\u{2026}\""),
            "closes the quote opened by the item text: {message:?}"
        );
        assert_eq!(message.matches('"').count(), 2);
    }

    #[test]
    fn commit_message_truncation_without_a_quote_gets_no_closing_quote() {
        let message = commit_message(
            Path::new("/a/b/checklist.md"),
            &format!(
                "Reset all tasks to not done and then some more {}",
                "x".repeat(50)
            ),
        );
        assert_eq!(message.chars().count(), 80);
        assert!(message.ends_with('\u{2026}'));
        assert!(!message.ends_with("\u{2026}\""));
    }

    #[test]
    fn commit_message_leaves_a_short_description_untruncated() {
        let message = commit_message(Path::new("/a/b/checklist.md"), "Check \"short\"");
        assert_eq!(message, "checklist.md: Check \"short\"");
        assert!(!message.contains('\u{2026}'));
    }
}
