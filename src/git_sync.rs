use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::model::PendingSync;

/// How long to wait between automatic push retries after a
/// `CommittedNotPushed` outcome, so a still-down network doesn't get
/// hammered with retries but connectivity returning is still noticed
/// without requiring another checklist edit.
const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Result of one background commit+push attempt, delivered to the
/// main loop via [`GitSync::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Committed and pushed.
    Synced,
    /// Nothing to do: the file is already tracked, identical to what's
    /// already committed, *and* that commit has already reached upstream
    /// (see `ahead_of_upstream`) — not just committed locally. Not reported
    /// to the user — nothing meaningful happened, so silence here doesn't
    /// read as broken.
    Skipped,
    /// The file isn't tracked by git at all. Unlike `Skipped`, this *is*
    /// reported to the user (git-sync being silently unable to do anything,
    /// forever, looks exactly like a bug) — but it still never gets
    /// `git add`ed automatically; that stays the user's call.
    SkippedUntracked,
    /// The commit succeeded but `git push` failed (offline, auth, no
    /// upstream, etc.) — distinct from `Failed` because the recovery action
    /// differs: nothing was lost, and the fix is to retry the *push*, never
    /// to create another commit. `GitSync` retries this automatically (see
    /// `retry_push_if_due`) rather than waiting for the next checklist edit,
    /// since a local commit that already matches the desired content would
    /// otherwise never trigger another sync attempt on its own.
    CommittedNotPushed(String),
    /// `git status`/`commit` failed (nothing was committed); the message is
    /// the first line of the failing command's stderr.
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
    /// The content currently being committed/pushed on the background
    /// thread, if any — cloned aside in `spawn` (the original moves into
    /// the thread closure) so `poll` can recover it if the attempt ends in
    /// `CommittedNotPushed` and it needs remembering for a retry.
    in_flight: Option<PendingSync>,
    /// Set when the most recently completed attempt committed locally but
    /// failed to push; cleared on `Synced`. Carries the content to retry
    /// with plus when that attempt finished, so `retry_push_if_due` can
    /// back off between attempts instead of hammering a still-down remote.
    retry: Option<(PendingSync, Instant)>,
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
            in_flight: None,
            retry: None,
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
        self.in_flight = Some(sync.clone());
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
    /// request that arrived while busy. Also updates the push-retry state
    /// (see `retry_push_if_due`) from the outcome, since `Synced` clears a
    /// prior pending retry and `CommittedNotPushed` (re)arms one.
    pub fn poll(&mut self) -> Option<SyncOutcome> {
        let outcome = self.receiver.try_recv().ok();
        if let Some(outcome) = &outcome {
            self.busy = false;
            let attempted = self.in_flight.take();
            match outcome {
                SyncOutcome::Synced => self.retry = None,
                SyncOutcome::CommittedNotPushed(_) => {
                    if let Some(sync) = attempted {
                        self.retry = Some((sync, Instant::now()));
                    }
                }
                SyncOutcome::Skipped | SyncOutcome::SkippedUntracked | SyncOutcome::Failed(_) => {}
            }
            if let Some(sync) = self.pending.take() {
                self.spawn(sync);
            }
        }
        outcome
    }

    /// Re-attempts a previously failed push, if one is due: call once per
    /// frame alongside `poll`, passing the current time (a parameter
    /// instead of reading `Instant::now()` internally, matching
    /// `AppState::expire_status`'s pattern, so tests can simulate the
    /// backoff elapsing without an actual multi-second sleep). A commit
    /// that already matches the desired content never triggers `request`
    /// again on its own (there's nothing new to commit), so without this a
    /// push failure — network down, auth expired — could otherwise leave a
    /// commit sitting local-only indefinitely if the user makes no further
    /// checklist edits after connectivity returns. No-ops while `busy` or
    /// before `PUSH_RETRY_INTERVAL` has elapsed since the last attempt.
    pub fn retry_push_if_due(&mut self, now: Instant) {
        if self.busy {
            return;
        }
        let Some((sync, last_attempt)) = &self.retry else {
            return;
        };
        if now.saturating_duration_since(*last_attempt) < PUSH_RETRY_INTERVAL {
            return;
        }
        let sync = sync.clone();
        self.spawn(sync);
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
/// The commit is built entirely from `expected_content` rather than
/// re-reading the working tree at commit time — despite appearances, `git
/// commit --only`/a plain pathspec always re-reads the file's *current
/// working-tree content* for the named path (it stages it fresh, ignoring
/// whatever's already in the index for that path), never a manually staged
/// index entry. That's the hazard the exact-content approach closes: if
/// some other change (another markcheck write, an external editor,
/// anything) lands on disk between a request being queued and this function
/// actually running, a working-tree-reading commit would silently absorb it
/// under a message that only describes the *original* request. A later,
/// unrelated change still gets synced — as its own, separately labeled
/// commit, the next time `request` runs — it just can never bleed into this
/// one.
///
/// The commit itself is built via a **temporary index** (`GIT_INDEX_FILE`)
/// populated from `HEAD` plus exactly one replaced path, committed with a
/// normal `git commit` — not by hand-assembling tree/commit objects and
/// moving `HEAD` with `update-ref`, which an earlier version of this
/// function did. That manual-plumbing approach used the *real* repo index
/// for `write-tree`, which silently pulled in anything else the user had
/// staged (`git add`ed but not yet committed) into markcheck's commit, and
/// used a bare `update-ref HEAD <new>` with no compare-and-swap against a
/// concurrently-moving `HEAD`. A temporary index sidesteps the first
/// problem entirely (the real index is never read or written), and a
/// normal `git commit` sidesteps the second (it does its own HEAD read as
/// part of one locked operation, not a separate read-then-blindly-write
/// step on markcheck's side) while also running the repo's normal commit
/// hooks and honoring `commit.gpgsign`, neither of which `commit-tree`
/// ever did. `repo_sync_blocked` (called first, below) covers the one
/// thing a normal commit *doesn't* refuse on its own: committing while the
/// repository is mid-merge/rebase/cherry-pick/etc., which would otherwise
/// silently resolve a conflict or advance past unfinished work.
fn run_sync(
    repo_dir: &Path,
    file_path: &Path,
    expected_content: &str,
    message: &str,
) -> SyncOutcome {
    // All plumbing commands run from the repo root with a root-relative
    // path, sidestepping any ambiguity between CWD-relative and
    // repo-root-relative pathspec handling. Resolved first (before even
    // `status`) since every other check below needs it.
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

    // Refuse outright while the repository is in a state where an automatic
    // commit could do real damage (see repo_sync_blocked's own doc comment)
    // — checked before anything else, so a mid-merge repository never even
    // reaches the status check below.
    if let Some(reason) = repo_sync_blocked(&repo_root) {
        return SyncOutcome::Failed(reason);
    }

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
    // call. Safe to read from the real index now that repo_sync_blocked has
    // already confirmed there's no unmerged (conflicted) entry for it.
    let (mode, relpath) = match index_entry(repo_dir, file_path) {
        Ok(entry) => entry,
        Err(err) => return SyncOutcome::Failed(err),
    };

    // If HEAD already holds exactly this content, the commit half of this
    // request was already satisfied by an earlier sync (e.g. it sat
    // coalesced behind one that committed the same or newer content) —
    // even though `status` above is non-empty because of someone else's
    // still-uncommitted change to the file. That's not the same as the
    // request being *fully* satisfied, though: if the commit hasn't reached
    // upstream yet (a prior push failed), there's still a push worth
    // retrying — see `ahead_of_upstream`'s doc comment for why this matters.
    if head_blob(&repo_root, &relpath).as_deref() == Some(expected_content.as_bytes()) {
        if ahead_of_upstream(&repo_root) {
            return push(repo_dir);
        }
        return SyncOutcome::Skipped;
    }

    let blob = match hash_object(&repo_root, expected_content) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };

    let parent = current_head(&repo_root);
    let outcome = commit_via_temp_index(&repo_root, &parent, &mode, &blob, &relpath, message);
    if let Err(err) = outcome {
        return SyncOutcome::Failed(err);
    }

    push(repo_dir)
}

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
fn ahead_of_upstream(repo_root: &Path) -> bool {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(1)
                > 0
        }
        _ => true,
    }
}

/// Runs `git push`, translating the result into a `SyncOutcome`. A failure
/// here is always `CommittedNotPushed`, never `Failed`: by the time this is
/// called, either a commit was just made or `HEAD` was already confirmed to
/// hold the desired content — either way, a local commit exists and the
/// only thing to retry is the push itself.
fn push(repo_dir: &Path) -> SyncOutcome {
    let output = Command::new("git")
        .current_dir(repo_dir)
        .arg("push")
        .output();
    match output {
        Ok(output) if output.status.success() => SyncOutcome::Synced,
        Ok(output) => SyncOutcome::CommittedNotPushed(command_error("git push", &output)),
        Err(err) => SyncOutcome::CommittedNotPushed(format!("git push failed: {err}")),
    }
}

/// Populates a fresh temporary index from `parent` (or leaves it empty for
/// a repository with no commits yet), stages `blob` into it at `mode`/
/// `relpath`, re-confirms `HEAD` hasn't moved since `parent` was read (a
/// residual, narrowed version of the same race a bare `update-ref` had —
/// `git commit` below safely serializes its *own* HEAD update, but the
/// temporary index's *content* for every path other than `relpath` was
/// still only as fresh as `parent`; re-checking here catches the common
/// case rather than silently committing a tree that reverts a
/// concurrently-landed, unrelated commit), and commits it — normal `git
/// commit`, so hooks and `commit.gpgsign` apply, never `commit-tree`. The
/// temporary index file is always removed afterward, success or failure;
/// the real index is never read or written by any of this.
fn commit_via_temp_index(
    repo_root: &Path,
    parent: &Option<String>,
    mode: &str,
    blob: &str,
    relpath: &str,
    message: &str,
) -> Result<(), String> {
    let git_dir =
        git_dir(repo_root).ok_or_else(|| "git-sync: could not resolve git-dir".to_string())?;
    let temp_index = git_dir.join(format!(
        "markcheck-index-{}-{:x}",
        std::process::id(),
        crate::writer::random_suffix()
    ));
    let result = (|| {
        if let Some(head) = parent {
            populate_temp_index(repo_root, &temp_index, head)?;
        }
        stage_into_temp_index(repo_root, &temp_index, mode, blob, relpath)?;
        if &current_head(repo_root) != parent {
            return Err("git-sync: repository changed during sync, will retry".to_string());
        }
        commit_temp_index(repo_root, &temp_index, message)
    })();
    let _ = std::fs::remove_file(&temp_index);
    result
}

/// `GIT_INDEX_FILE=<temp_index> git read-tree <parent>`: seeds the
/// temporary index with `parent`'s tree, so every path other than the one
/// about to be replaced commits exactly as `parent` had it — never the real
/// index's (possibly unrelated-staged-content-holding) state.
fn populate_temp_index(repo_root: &Path, temp_index: &Path, parent: &str) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", temp_index)
        .args(["read-tree", parent])
        .output()
        .map_err(|err| format!("git read-tree failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git read-tree", &output));
    }
    Ok(())
}

/// `GIT_INDEX_FILE=<temp_index> git update-index --add --cacheinfo`: stages
/// `blob` for `relpath` at `mode` in the *temporary* index — `--add` since
/// the path may not already be present (a fresh repo's first commit, or a
/// file staged but never yet committed) — without touching the real index
/// or the working tree at all.
fn stage_into_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", temp_index)
        .args(["update-index", "--add", "--cacheinfo", mode, blob, relpath])
        .output()
        .map_err(|err| format!("git update-index failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-index", &output));
    }
    Ok(())
}

/// `GIT_INDEX_FILE=<temp_index> git commit -m <message>`: commits the
/// temporary index's tree against the repository's real `HEAD`/branch —
/// normal commit machinery (hooks, `commit.gpgsign`, HEAD's own locked
/// read-and-update), just fed from the temporary index instead of the real
/// one.
fn commit_temp_index(repo_root: &Path, temp_index: &Path, message: &str) -> Result<(), String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .env("GIT_INDEX_FILE", temp_index)
        .args(["commit", "-m", message])
        .output()
        .map_err(|err| format!("git commit failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git commit", &output));
    }
    Ok(())
}

/// Absolute path to the repository's git directory. Resolved relative to
/// `repo_root` when `git` reports a relative one (its own behavior differs
/// depending on whether it's invoked from the work tree root or a
/// subdirectory) — `repo_sync_blocked` needs an absolute path to check for
/// marker files regardless of which one `git` happened to hand back.
fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
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

/// Refuses to sync while the repository is in a state where an automatic
/// commit would be actively harmful: a merge/cherry-pick/revert/bisect/
/// rebase in progress (each recorded by git as a marker file or directory
/// under the git-dir), a detached `HEAD` (a commit made there is one `git
/// checkout` away from becoming unreachable, with no branch pointing at
/// it), or an unresolved conflict anywhere in the repository (not just the
/// target path — any of these mean `HEAD`/the index currently represent
/// in-progress work a background checklist commit must never interfere
/// with, silently "resolving" a conflict or completing someone else's
/// merge). Checked once at the top of `run_sync`, before any other work.
fn repo_sync_blocked(repo_root: &Path) -> Option<String> {
    let git_dir = git_dir(repo_root)?;
    for (marker, label) in [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("BISECT_LOG", "a bisect"),
    ] {
        if git_dir.join(marker).exists() {
            return Some(format!("git-sync: repository has {label} in progress"));
        }
    }
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        return Some("git-sync: repository has a rebase in progress".to_string());
    }
    let on_a_branch = Command::new("git")
        .current_dir(repo_root)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !on_a_branch {
        return Some("git-sync: repository is in a detached HEAD state".to_string());
    }
    let unmerged = Command::new("git")
        .current_dir(repo_root)
        .args(["ls-files", "-u"])
        .output();
    match unmerged {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some("git-sync: repository has unresolved merge conflicts".to_string())
        }
        _ => None,
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
        // The commit itself succeeded (nothing was lost); only the push
        // failed, which is why this is `CommittedNotPushed` rather than
        // `Failed` — see the `SyncOutcome` doc comments.
        assert!(matches!(outcome, SyncOutcome::CommittedNotPushed(_)));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn commit_via_temp_index_refuses_when_head_moved_since_parent_was_read() {
        // Narrows (doesn't need to eliminate — see run_sync's doc comment)
        // the residual race a bare `update-ref HEAD` used to have with no
        // compare-and-swap at all: if some other commit landed after
        // `parent` was captured but before the temp index is actually
        // committed, the temp index's content for every path *other* than
        // the one being replaced would silently be stale (based on
        // `parent`, not the real current HEAD) — better to refuse and let
        // the next sync attempt retry from fresh state than to commit a
        // tree that quietly reverts the intervening change.
        let work = init_repo_without_remote();
        let stale_parent = current_head(&work);

        // A commit lands that `commit_via_temp_index`'s caller doesn't know
        // about yet — simulating a concurrent `git commit`/pull/rebase
        // racing the background sync thread.
        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let blob = hash_object(&work, "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &stale_parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("changed during sync")),
            "{result:?}"
        );

        // Nothing committed on top of the concurrent change; no leftover
        // temp index file.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "concurrent change\ninit"
        );
        let leftover = fs::read_dir(work.join(".git")).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("markcheck-index-")
        });
        assert!(!leftover, "temp index file was not cleaned up");

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
        // The very first attempt already had a real change committed, so
        // pushing against the unreachable remote always fails — but the
        // commit itself always succeeds, hence `CommittedNotPushed` rather
        // than `Failed` (never Skipped either).
        assert!(
            matches!(outcomes.first(), Some(SyncOutcome::CommittedNotPushed(_))),
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
    fn retry_push_if_due_noops_when_nothing_is_pending() {
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.retry_push_if_due(Instant::now());
        assert!(!sync.busy, "nothing pending, nothing to retry");
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_before_the_backoff_interval_elapses() {
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((
            PendingSync {
                content: "- [x] one\n".to_string(),
                description: "retry".to_string(),
            },
            last_attempt,
        ));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL - Duration::from_millis(1));

        assert!(!sync.busy, "backoff interval hasn't elapsed yet");
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_while_busy() {
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.busy = true;
        sync.retry = Some((
            PendingSync {
                content: "- [x] one\n".to_string(),
                description: "retry".to_string(),
            },
            Instant::now() - PUSH_RETRY_INTERVAL,
        ));

        // A no-op here just means "doesn't spawn a second concurrent
        // attempt while one's already running" -- there's nothing else to
        // observe from outside, so this mainly guards against a panic or a
        // stray thread spawn while `busy` is set for an unrelated reason.
        sync.retry_push_if_due(Instant::now());

        assert!(sync.busy);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_retries_once_the_backoff_interval_elapses() {
        // External review: the whole point of automatic push retry is
        // closing the gap where a user makes no further checklist edits
        // after a push failure -- this drives that path directly, passing
        // a synthetic `now` (matching AppState::expire_status's pattern)
        // instead of a real 30-second sleep.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let last_attempt = Instant::now();
        sync.retry = Some((
            PendingSync {
                content: "- [x] one\n".to_string(),
                description: "retry".to_string(),
            },
            last_attempt,
        ));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL);
        assert!(
            sync.busy,
            "the backoff interval elapsed, a retry should have spawned"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = None;
        while Instant::now() < deadline {
            if let Some(o) = sync.poll() {
                outcome = Some(o);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(outcome, Some(SyncOutcome::CommittedNotPushed(_))),
            "still no remote to push to, so this retry fails the same way: {outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
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
    fn run_sync_never_includes_an_unrelated_staged_file() {
        // External review, empirically reproduced against the prior
        // implementation before this fix: `write-tree` against the *real*
        // index serialized everything in it, not just the checklist path —
        // so a file the user had `git add`ed but not yet committed
        // silently rode along inside markcheck's commit, under a message
        // describing only the checklist change, then got pushed. The
        // temp-index rewrite must never see the real index's staged
        // content at all.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        fs::write(work.join("secret.env"), "SECRET_API_KEY=xyz123\n").unwrap();
        run(&work, &["add", "secret.env"]);

        let message = commit_message(&file_path, "Check \"one\"");
        assert_eq!(
            run_sync(&work, &file_path, "- [x] one\n", &message),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "--stat", "HEAD"])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&show.stdout);
        assert!(
            !stat.contains("secret.env"),
            "the unrelated staged file must never appear in the pushed commit: {stat}"
        );

        // The real index must be completely untouched — secret.env is
        // still exactly as the user staged it, ready for their own commit.
        let index = Command::new("git")
            .current_dir(&work)
            .args(["ls-files", "--stage", "secret.env"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&index.stdout).is_empty(),
            "secret.env must remain staged in the real index"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_during_an_unresolved_merge_conflict() {
        // External review, empirically reproduced against the prior
        // implementation before this fix: with a genuine unresolved merge
        // conflict on the checklist file, the plumbing sequence silently
        // collapsed the 3-stage conflict entry to markcheck's own expected
        // content, wrote a normal (non-merge) commit, and advanced HEAD
        // past the in-progress merge — while `MERGE_HEAD` and the
        // conflict-marked working tree were left behind, exactly as if the
        // merge had never been dealt with. run_sync must now refuse
        // outright instead, leaving the merge exactly as the user left it.
        let work = init_repo_without_remote();
        run(&work, &["checkout", "-q", "-b", "branch-a"]);
        fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
        run(&work, &["commit", "-q", "-am", "a"]);
        run(&work, &["checkout", "-q", "main"]);
        fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
        run(&work, &["commit", "-q", "-am", "b"]);
        let head_before_merge_attempt = current_head(&work).unwrap();
        let _ = Command::new("git")
            .current_dir(&work)
            .args(["merge", "-q", "branch-a"])
            .output();
        assert!(
            work.join(".git").join("MERGE_HEAD").exists(),
            "test setup: real conflict expected"
        );

        let file_path = work.join("tracked.md");
        let outcome = run_sync(&work, &file_path, "- [x] resolved\n", "should not commit");
        assert!(matches!(outcome, SyncOutcome::Failed(_)), "{outcome:?}");

        // Nothing about the in-progress merge was touched: HEAD never
        // moved, MERGE_HEAD still marks the merge as unfinished, and the
        // conflict markers are still in the working tree.
        assert_eq!(current_head(&work).unwrap(), head_before_merge_attempt);
        assert!(work.join(".git").join("MERGE_HEAD").exists());
        let contents = fs::read_to_string(&file_path).unwrap();
        assert!(
            contents.contains("<<<<<<<"),
            "conflict markers must still be present, untouched: {contents:?}"
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
    fn run_sync_skips_when_expected_content_already_matches_head_and_upstream() {
        // A remote is required here (unlike most `run_sync` tests): a
        // truly `Skipped` result now requires HEAD to both match the
        // expected content *and* already be pushed — see `ahead_of_upstream`.
        let (work, _remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        // Working tree has drifted further (an unrelated concurrent change)
        // so `git status` is non-empty, but this specific request's
        // expected content is already what's committed at HEAD *and*
        // already pushed (by `init_repo_with_remote`'s own setup) — e.g. it
        // sat coalesced behind an earlier sync that already committed and
        // pushed it.
        fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

        assert_eq!(
            run_sync(&work, &file_path, "- [ ] one\n", "already committed"),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_retries_the_push_when_expected_content_matches_head_but_is_unpushed() {
        // External review: HEAD matching the expected content used to be
        // treated as proof the request was fully satisfied, even when the
        // matching commit had never actually reached upstream (e.g. a prior
        // push failed). That silently stranded the commit local-only
        // forever unless another checklist edit happened to come along.
        // `run_sync` must instead retry the push in that case.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [ ] one\n- [x] two\n").unwrap();

        let outcome = run_sync(&work, &file_path, "- [ ] one\n", "already committed");
        assert!(
            matches!(outcome, SyncOutcome::CommittedNotPushed(_)),
            "must attempt (and report failure of) the push, not silently skip: {outcome:?}"
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
    fn stage_into_temp_index_reports_failure_for_an_invalid_mode() {
        let work = init_repo_without_remote();
        let blob = hash_object(&work, "content").unwrap();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(
            stage_into_temp_index(&work, &temp_index, "not-a-mode", &blob, "tracked.md").is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn populate_temp_index_reports_failure_for_an_unresolvable_parent() {
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        assert!(populate_temp_index(&work, &temp_index, "not-a-commit-id").is_err());
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn commit_temp_index_reports_failure_for_a_tree_unchanged_from_head() {
        // A temp index populated straight from HEAD, with nothing further
        // staged into it, produces the exact same tree HEAD already has —
        // the same "empty commit refused" behavior normal `git commit`
        // always has, exercised here through the temp-index path
        // specifically.
        let work = init_repo_without_remote();
        let temp_index = work.join(".git").join("scratch-test-index");
        let head = current_head(&work).unwrap();
        populate_temp_index(&work, &temp_index, &head).unwrap();
        assert!(commit_temp_index(&work, &temp_index, "empty").is_err());
        let _ = fs::remove_file(&temp_index);
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
    fn repo_sync_blocked_is_none_for_a_clean_repo() {
        let work = init_repo_without_remote();
        assert_eq!(repo_sync_blocked(&work), None);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repo_sync_blocked_detects_a_merge_in_progress() {
        let work = init_repo_without_remote();
        // MERGE_HEAD's presence alone is what's checked — no need to drive
        // an actual conflicted merge to exercise this specific marker file.
        fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("merge")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repo_sync_blocked_detects_a_rebase_in_progress() {
        let work = init_repo_without_remote();
        fs::create_dir_all(work.join(".git").join("rebase-merge")).unwrap();
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("rebase")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repo_sync_blocked_detects_detached_head() {
        let work = init_repo_without_remote();
        let head = current_head(&work).unwrap();
        run(&work, &["checkout", "-q", &head]);
        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("detached")),
            "{reason:?}"
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repo_sync_blocked_detects_a_real_merge_conflict() {
        // End-to-end: a genuine conflicted merge must be blocked — this is
        // the exact scenario a prior version of run_sync would have
        // silently "resolved" (see the regression test on run_sync below
        // for the full empirically-reproduced failure mode this replaces).
        // MERGE_HEAD's presence is what actually catches it here (checked
        // before the unmerged-index scan), which is fine: any of the gate's
        // checks blocking is the invariant that matters.
        let work = init_repo_without_remote();
        run(&work, &["checkout", "-q", "-b", "branch-a"]);
        fs::write(work.join("tracked.md"), "- [x] a\n").unwrap();
        run(&work, &["commit", "-q", "-am", "a"]);
        run(&work, &["checkout", "-q", "main"]);
        fs::write(work.join("tracked.md"), "- [x] b\n").unwrap();
        run(&work, &["commit", "-q", "-am", "b"]);
        let _ = Command::new("git")
            .current_dir(&work)
            .args(["merge", "-q", "branch-a"])
            .output();

        assert!(repo_sync_blocked(&work).is_some());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn repo_sync_blocked_detects_unmerged_entries_without_a_merge_marker() {
        // Isolates the ls-files -u check specifically: unmerged (stage
        // 1/2/3) index entries can exist without MERGE_HEAD too (e.g. after
        // `git checkout -m`, or a conflicted `stash pop`) — inject one
        // directly via update-index --index-info rather than relying on a
        // specific porcelain command to produce it. The conflicted path
        // (other.md) is deliberately *not* the file markcheck is syncing
        // (tracked.md) — the gate must look at the whole repository, not
        // just the target path, per the review's own recommendation.
        let work = init_repo_without_remote();
        fs::write(work.join("other.md"), "base\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "add other.md"]);

        let base = hash_object(&work, "base\n").unwrap();
        let ours = hash_object(&work, "ours\n").unwrap();
        let theirs = hash_object(&work, "theirs\n").unwrap();
        let index_info = format!(
            "100644 {base} 1\tother.md\n100644 {ours} 2\tother.md\n100644 {theirs} 3\tother.md\n"
        );
        let mut child = Command::new("git")
            .current_dir(&work)
            .args(["update-index", "--index-info"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(index_info.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(!work.join(".git").join("MERGE_HEAD").exists());

        let reason = repo_sync_blocked(&work);
        assert!(
            reason.as_ref().is_some_and(|r| r.contains("conflict")),
            "{reason:?}"
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
