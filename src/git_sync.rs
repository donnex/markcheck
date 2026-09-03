use std::fs;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::model::{PendingSync, hash_bytes};

/// How long to wait between automatic push retries after a
/// `CommittedNotPushed` outcome, so a still-down network doesn't get
/// hammered with retries but connectivity returning is still noticed
/// without requiring another checklist edit.
const PUSH_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Timeout for local git plumbing commands (`status`, `ls-files`,
/// `hash-object`, `rev-parse`, `read-tree`, `update-index`, `commit`,
/// `symbolic-ref`, `update-ref`, `diff`, `show`) — all normally instant, no
/// network involved, so a generous-but-bounded cap catches a genuinely
/// stuck process (a hanging commit hook, a wedged filesystem) without ever
/// being a realistic limit under normal operation.
const PLUMBING_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for `git push` specifically — network-bound, so it needs
/// meaningfully longer than the plumbing commands above.
const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `cmd` to completion, killing it and returning a
/// [`io::ErrorKind::TimedOut`] error if it hasn't finished within
/// `timeout`. `Command::output()` has no timeout of its own — it blocks
/// until the child exits, however long that takes — so without this, a
/// hung git subprocess (broken SSH, a stuck credential helper, a hanging
/// commit hook, a wedged network transport) would block the sync worker
/// thread forever, and quitting markcheck while one is hung would orphan
/// the child process entirely (Rust neither kills child processes on drop
/// nor joins/kills detached threads on exit).
///
/// stdout/stderr are drained on their own threads for the whole run, not
/// just read after the child exits: `try_wait`-polling without doing this
/// risks the classic pipe-deadlock — if the child writes enough output to
/// fill the OS pipe buffer, it blocks on that write, `try_wait` never
/// returns, and nothing would ever unblock the child, if fake output
/// weren't already being drained in the background.
///
/// Takes an owned `Command` rather than `&mut Command` (unlike
/// `Command::output`) so call sites build it as a local variable first
/// instead of chaining off `Command::new` directly — a builder chain like
/// `Command::new("git").arg(...)` yields `&mut Command`, borrowing a
/// temporary, which can't be handed to a function expecting an owned one.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    // stdin is nulled rather than inherited: markcheck owns the terminal
    // while these run, and no plumbing command here has anything to read.
    // Hardening rather than a fix for an observed failure — an attempt to
    // demonstrate a credential-prompt hang under a PTY did *not* reproduce,
    // because `spawn_in_own_process_group` leaves the child in a background
    // process group where a terminal read fails instead of blocking. But
    // that outcome depends on process-group and signal details rather than
    // on anything stated here, and `git` seeing an immediate EOF is both
    // predictable and the conventional choice.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_in_own_process_group(cmd)?;
    wait_with_timeout(child, timeout, None)
}

/// Spawns `cmd` as the leader of a new process group (its own PID doubling
/// as the group ID) on Unix — plain on other platforms, where this is a
/// best-effort feature (see `kill_and_reap`). Matters because `git` itself
/// can spawn its own subprocesses (a credential helper, `ssh` for a remote
/// push, a commit hook's own children) that inherit the piped stdout/
/// stderr file descriptors this module sets up: killing only the direct
/// `git` child on timeout would leave those grandchildren running and
/// still holding those descriptors open, which would block
/// `wait_with_timeout`'s reader threads until the grandchildren happen to
/// exit on their own — precisely the hang this whole mechanism exists to
/// prevent. Killing the whole group (`kill_and_reap`) reaches all of them.
fn spawn_in_own_process_group(mut cmd: Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// Like `run_with_timeout`, but also writes `stdin_data` to the child's
/// stdin on its own thread before waiting — needed by `hash_object`, the
/// one call site that feeds git anything over stdin. Writing on a separate
/// thread (rather than writing then waiting, sequentially) matters for the
/// same reason draining stdout/stderr on their own threads does: a large
/// enough write could block on a full pipe buffer just as easily as a
/// large enough child-produced output could.
fn run_with_timeout_and_stdin(
    mut cmd: Command,
    timeout: Duration,
    stdin_data: &str,
) -> io::Result<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_in_own_process_group(cmd)?;
    let mut stdin_pipe = child.stdin.take().expect("stdin was requested as piped");
    let stdin_data = stdin_data.to_owned();
    // Reports completion through a channel rather than a `JoinHandle`, for
    // exactly the reason the stdout/stderr readers do — see
    // `wait_with_timeout`, which bounds how long it waits for this.
    let (stdin_done_tx, stdin_done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Errors ignored: a write failure here (e.g. the child exited
        // early) surfaces as a non-success exit status instead, which the
        // caller already checks.
        let _ = stdin_pipe.write_all(stdin_data.as_bytes());
        // `stdin_pipe` drops here, closing the write end so the child sees
        // EOF on its stdin rather than hanging waiting for more.
        drop(stdin_pipe);
        let _ = stdin_done_tx.send(());
    });
    wait_with_timeout(child, timeout, Some(stdin_done_rx))
}

/// How long to wait for the stdout/stderr reader threads to finish once
/// `child` itself is known to be gone (exited on its own, or just killed on
/// timeout) — deliberately much shorter than `PLUMBING_TIMEOUT`/
/// `PUSH_TIMEOUT`, since this is only ever bounding a *lingering
/// descendant* holding the pipe open, not real work. See `wait_with_timeout`
/// for why this exists at all.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Shared core of `run_with_timeout`/`run_with_timeout_and_stdin`: drains
/// `child`'s stdout/stderr on their own threads for the whole run (not just
/// after it exits — `try_wait`-polling without doing this risks the classic
/// pipe deadlock, where a child that fills the OS pipe buffer blocks on
/// that write and `try_wait` never returns, with nothing left to drain it),
/// polls `try_wait` until the child exits or `timeout` elapses, and kills
/// the child on timeout rather than merely giving up on waiting for it —
/// `Command::output()`/a bare `wait()` do neither, so a hung `git`
/// subprocess (broken SSH, a stuck credential helper, a hanging commit
/// hook, a wedged network transport) would otherwise block the sync worker
/// thread forever, and quitting markcheck while one is hung would orphan
/// the child process entirely (Rust neither kills child processes on drop
/// nor joins/kills detached threads on exit).
///
/// External review, round 8: the reader threads used to hand back their
/// buffer through a plain `JoinHandle`, joined unconditionally once `child`
/// was known to be gone — but `git` can spawn its own children (a
/// credential helper, `ssh`, a hook's own children) that inherit the piped
/// stdout/stderr file descriptors, and a hook that backgrounds one of those
/// without closing them leaves the pipe's write end open even after `git`
/// itself has exited. `read_to_end()` in the reader thread then blocks
/// forever waiting for an EOF that only the lingering descendant is
/// preventing, and an unconditional `.join()` waits right along with it —
/// hanging the whole operation *after* the timeout above should have
/// already bounded it. The reader threads now send their buffer through a
/// channel instead, so both exit paths below can bound how long they wait
/// for it (`PIPE_DRAIN_GRACE`) rather than joining unconditionally: if a
/// descendant is still holding the pipe, `kill_and_reap` is called again
/// (safe even though `child` itself is already reaped — a second `wait()`
/// on an already-reaped PID just errors, ignored the same way every other
/// best-effort cleanup in this module is) — it still reaches the
/// descendant, since `spawn_in_own_process_group` put it in the same
/// process group as `child`.
fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
    stdin_done: Option<mpsc::Receiver<()>>,
) -> io::Result<Output> {
    let mut stdout_pipe = child.stdout.take().expect("stdout was requested as piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was requested as piped");
    let (stdout_tx, stdout_rx) = mpsc::channel();
    let (stderr_tx, stderr_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });
    // One bounded teardown once the child is gone: wait for the stdin
    // writer, then drain both pipes, all inside a *single* `PIPE_DRAIN_GRACE`
    // budget rather than one each — three separate grace periods would
    // silently treble the bound this constant exists to impose.
    //
    // Deliberately does **not** kill the process group to force an EOF,
    // which is what it used to do. Deep round 2: by the time this runs the
    // child has always been reaped — `try_wait` reaps it on the success
    // path, `kill_and_reap` on the timeout path — so its PID is no longer
    // ours and the OS is free to recycle it. `kill -KILL -- -<pid>` on a
    // recycled PID signals an unrelated process group, a far worse outcome
    // than the truncated output it was avoiding, and it killed a descendant
    // of a command that had *succeeded*. Nothing is lost on the timeout path
    // (the group was killed moments earlier anyway); on the success path a
    // lingering descendant just means returning whatever arrived within the
    // budget. The reader and writer threads are detached and exit on their
    // own once it finally closes the pipes.
    let finish = |stdin_done: &Option<mpsc::Receiver<()>>| -> (Vec<u8>, Vec<u8>) {
        let deadline = Instant::now() + PIPE_DRAIN_GRACE;
        let left = || deadline.saturating_duration_since(Instant::now());
        if let Some(rx) = stdin_done {
            let _ = rx.recv_timeout(left());
        }
        let stdout = stdout_rx.recv_timeout(left()).unwrap_or_default();
        let stderr = stderr_rx.recv_timeout(left()).unwrap_or_default();
        (stdout, stderr)
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            finish(&stdin_done);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git command timed out after {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (stdout, stderr) = finish(&stdin_done);
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Kills `child` and reaps it (`wait`, discarding the result) so it never
/// lingers as a zombie process — `kill` alone only sends the signal, the
/// exit status still has to be collected for the OS to release the
/// process table entry.
fn kill_and_reap(child: &mut Child) {
    // Kill the whole process group, not just `child` itself — see
    // `spawn_in_own_process_group`'s doc comment for why a grandchild
    // process (git spawning ssh, a hook spawning its own children) would
    // otherwise survive and keep our piped stdout/stderr open. `kill`'s
    // negative-PID form targets a process *group* rather than a single
    // process; shelling out to the `kill` utility rather than adding a
    // dependency for the raw syscall, consistent with this project's
    // existing preference for std-only solutions where practical. Best
    // effort and Unix-only (a no-op on other platforms, where `child.id()`
    // is not a process group either way) — falls through to the plain
    // single-process `child.kill()` below regardless, which is the only
    // option at all on a platform without process groups.
    #[cfg(unix)]
    {
        // All three streams explicitly nulled. `status()` would otherwise
        // *inherit* markcheck's, and markcheck owns the terminal — in raw
        // mode, on the alternate screen. `/bin/kill` writes to stderr when
        // the target group is already gone (`/bin/kill: (-1234): No such
        // process`, verified), which would land in the middle of the
        // rendered UI, where ratatui's diff renderer won't necessarily
        // repaint over it. That race is narrow, but the rule is general and
        // free: **any** `Command` spawned while the TUI owns the terminal
        // must set its stdio explicitly, as `main.rs`'s `open_link` does.
        let _ = Command::new("kill")
            .args(["-KILL", "--"])
            .arg(format!("-{}", child.id()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A named point inside a sync where a test can interleave a real
/// concurrent git process.
///
/// Every review of this module has made the same observation: its race tests
/// construct the repository state *before* invoking the function, which pins
/// the resulting invariant but never exercises the actual check → race → use
/// ordering. These points close that gap. In production every one of them
/// compiles to nothing (`race_point` has an empty `cfg(not(test))` body), so
/// this costs the shipped binary literally nothing.
///
/// Hooks are **thread-local** on purpose. Tests run in parallel in one
/// process, and they drive `run_sync`/`catch_up_push` directly on their own
/// thread, so a thread-local registry gives each test an isolated view; a
/// global one would have tests firing each other's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RacePoint {
    /// After the unpushed-history guard has cleared the captured tip, and
    /// before anything acts on that verdict.
    AfterHistoryValidation,
    /// Immediately before a commit is built from the temporary index.
    BeforeCommit,
    /// Immediately before the real index is realigned to the new commit.
    BeforeIndexAlignment,
    /// Immediately before `git push` runs.
    BeforePush,
}

#[cfg(test)]
thread_local! {
    static RACE_HOOKS: std::cell::RefCell<
        std::collections::HashMap<RacePoint, Box<dyn Fn()>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Runs whatever this thread has installed at `point`, if anything.
#[cfg(test)]
fn race_point(point: RacePoint) {
    // The hook is taken out of the map while it runs, so a hook that itself
    // reaches a sync point can't recurse into itself.
    let hook = RACE_HOOKS.with(|h| h.borrow_mut().remove(&point));
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(test))]
#[inline(always)]
fn race_point(_point: RacePoint) {}

/// Builds a `git` invocation rooted at `repo_dir`, with the ambient git
/// environment neutralised.
///
/// Deep rounds 2, round 2. markcheck inherits its environment, and git reads
/// several variables that redirect which repository a command operates on.
/// Confirmed against real git: with `GIT_DIR` exported, `git log -1` run
/// inside repository *a* reports *b*'s HEAD — refs and the object database
/// come from the other repository while the work tree stays *a*, so
/// markcheck would resolve the tip from one repository and commit and push
/// into another. With `GIT_INDEX_FILE` exported, `ls-files --stage` for a
/// perfectly tracked checklist returns nothing (so it is reported untracked)
/// while `status` reports `D `/`??` — which the `??` prefix check does not
/// even catch — and `align_real_index_entry` would write into the stray
/// index.
///
/// None of that is exotic: git hooks set both, `git rebase --exec` and
/// `git bisect run` inherit them, and exporting `GIT_DIR`/`GIT_WORK_TREE`
/// for a bare dotfiles repository is a widely-used pattern.
///
/// Only variables that would make git act on a *different repository than
/// the one containing the checklist* are removed. `GIT_CEILING_DIRECTORIES`
/// is deliberately left alone: at worst it stops discovery, so git-sync
/// simply does not activate, which is safe. Author/committer identity and
/// config overrides are left alone too — those are the user's to set, and
/// they do not change which repository is written.
fn git_command(repo_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

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
    /// otherwise never trigger another sync attempt on its own. `commit` is
    /// the specific commit SHA that failed to push — `retry_push_if_due`
    /// retries *that commit*, not the file content that produced it, so a
    /// retry can never turn into a brand new commit built from stale
    /// content if something else has moved `HEAD` in the meantime (see
    /// `retry_commit`).
    CommittedNotPushed { message: String, commit: String },
    /// A push retry (`retry_commit`) gave up because the commit it was
    /// retrying is no longer `HEAD` — something else committed on top since
    /// the failed push, so replaying this specific retry would be pointless
    /// at best. Whatever superseded it gets its own sync opportunity through
    /// the normal request path, so nothing is lost.
    ///
    /// A dedicated variant rather than reusing `Skipped`: the two mean
    /// genuinely different things to the retry state (`Skipped` must leave a
    /// still-valid retry armed; this one must clear it), and disambiguating
    /// them used to need an out-of-band `last_spawn_was_retry` flag (long since
    /// removed) read at
    /// `poll` time. Carrying it in the outcome is what lets `apply_outcome`
    /// be a total `match` — which is the point, since the missing arm in the
    /// old one is exactly how the `Failed` retry storm survived.
    RetryAbandoned,
    /// `git status`/`commit` failed (nothing was committed); the message is
    /// the first line of the failing command's stderr.
    Failed(String),
    /// A **push retry** (`retry_commit`) failed outright — as opposed to
    /// `Failed`, which is a *content* sync failing. The two are separated
    /// because they say opposite things about the armed retry, and
    /// `apply_outcome` needs to treat them differently: a retry's own
    /// failure must restart its backoff (or a permanently failing retry
    /// fires on every frame), while a content sync's failure must leave the
    /// backoff alone (or repeated unrelated failures push the retry's turn
    /// out indefinitely and a pushable commit never goes out).
    ///
    /// Produced by mapping `retry_commit`'s `Failed` in `spawn_retry`, so
    /// there is one place the distinction is made rather than a flag read
    /// back at `poll` time — the same reasoning that made `RetryAbandoned`
    /// its own variant instead of an out-of-band `last_spawn_was_retry`.
    RetryFailed(String),
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
    /// Set when the most recently completed attempt committed locally but
    /// failed to push; cleared on `Synced`. Carries the *commit SHA* to
    /// retry pushing plus when that attempt finished, so `retry_push_if_due`
    /// can back off between attempts instead of hammering a still-down
    /// remote. Deliberately not the file content that produced the commit
    /// (see `SyncOutcome::CommittedNotPushed`/`retry_commit`) — replaying
    /// content through the generic commit-or-skip machinery could build a
    /// *new* commit from stale content if `HEAD` had moved on in the
    /// meantime, silently reverting whatever superseded it.
    retry: Option<(String, Instant)>,
    /// The content hash of the *latest* `PendingSync` this `GitSync` has
    /// ever been asked to sync — updated on every `request()` call,
    /// including ones that coalesce into `pending` rather than spawning
    /// immediately. Shared with the background thread (`run_sync` reads it
    /// live, not a snapshot taken at spawn time) so a sync in flight can
    /// tell "the file changed because a newer request already supersedes
    /// me, which will correct things right after I finish" apart from "the
    /// file changed because of something outside git-sync entirely, which
    /// nothing will ever correct" — see `run_sync`'s staleness check.
    latest_requested_hash: Arc<Mutex<[u8; 32]>>,
}

/// Locks `latest_requested_hash`, tolerating a poisoned mutex rather than
/// propagating the panic. The guarded value is a plain `[u8; 32]` — a
/// content digest, overwritten wholesale on every write — so there is no
/// multi-field invariant a panicking holder could have left half-updated,
/// which is the only thing poisoning is there to warn about. Tolerating it
/// matters because one of the two lock sites is on the **main** thread
/// (`GitSync::request`): an `unwrap()` there turns a background worker's
/// panic into a TUI crash, several frames after the fact and with no
/// connection to what the user was doing.
fn lock_hash(hash: &Mutex<[u8; 32]>) -> std::sync::MutexGuard<'_, [u8; 32]> {
    hash.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs a sync operation, converting a panic into a reportable
/// `SyncOutcome::Failed` instead of letting the worker thread die silently.
///
/// `GitSync.busy` is cleared **only** when `poll` receives an outcome, and
/// the receiver never reports `Disconnected` (the struct holds its own
/// sender clone), so a worker that panics before sending leaves `busy` stuck
/// `true` forever: every later `request` coalesces into `pending` and never
/// spawns, `retry_push_if_due` returns immediately, and quitting burns the
/// full `wait_for_git_sync` budget — git-sync silently, permanently dead
/// with nothing on screen to say so. Guaranteeing an outcome is always sent
/// keeps that failure loud and recoverable.
///
/// Not reachable by any known path — `run_sync` and `retry_commit` return
/// `Result`-shaped failures throughout, and the one `unwrap` they relied on
/// (a poisoned `latest_requested_hash`) is handled by `lock_hash` above.
/// This is the backstop for the unknown ones, so it has no test of its own;
/// forcing a panic through it would mean adding a panic to production code
/// purely to observe it.
fn run_reporting_panics(op: impl FnOnce() -> SyncOutcome) -> SyncOutcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)).unwrap_or(SyncOutcome::Failed(
        "git-sync: internal error (sync worker panicked)".to_string(),
    ))
}

impl GitSync {
    /// Confirms `file_path`'s directory is inside a git work tree and, if
    /// so, returns a `GitSync` ready to accept requests. `None` when it
    /// isn't (or `git` itself can't be run) — git-sync is a convenience
    /// feature, so this fails open rather than erroring out.
    pub fn detect(file_path: &Path) -> Option<GitSync> {
        let repo_dir = file_path.parent()?.to_path_buf();
        let mut cmd = git_command(&repo_dir);
        cmd.args(["rev-parse", "--is-inside-work-tree"]);
        let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
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
            retry: None,
            latest_requested_hash: Arc::new(Mutex::new(hash_bytes(b""))),
        })
    }

    /// Requests a commit+push for `sync.content` (the exact file content
    /// expected once the underlying change lands), labeled with
    /// `sync.description` (e.g. `Check "Restart service"`); the full commit
    /// message is built from the file name plus this description. Coalesced
    /// with any already-running sync per the `pending` rule above.
    ///
    /// Always records `sync.content_hash` as the latest-known request
    /// first — unconditionally, before deciding whether to coalesce or
    /// spawn — so a sync already in flight for an *older* request can see
    /// that a newer one now exists, even though that newer one won't
    /// itself start running until the current one finishes.
    pub fn request(&mut self, sync: PendingSync) {
        *lock_hash(&self.latest_requested_hash) = sync.content_hash;
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
        let latest_requested_hash = Arc::clone(&self.latest_requested_hash);
        std::thread::spawn(move || {
            let outcome = run_reporting_panics(|| {
                let message = commit_message(&file_path, &sync.description);
                run_sync(
                    &repo_dir,
                    &file_path,
                    &sync.content,
                    &message,
                    &latest_requested_hash,
                    sync.previous_content_hash,
                )
            });
            let _ = sender.send(outcome);
        });
    }

    /// Like `spawn`, but for retrying a push of an already-made commit
    /// (`retry_push_if_due`) rather than a fresh content-based sync request
    /// — see `retry_commit`.
    fn spawn_retry(&mut self, commit: String) {
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            // Everything this path can report as an outright failure is a
            // *retry's* failure, including a panic backstop — mapped once,
            // here, so `apply_outcome` never has to ask where it came from.
            let outcome = match run_reporting_panics(|| retry_commit(&repo_dir, &commit)) {
                SyncOutcome::Failed(msg) => SyncOutcome::RetryFailed(msg),
                other => other,
            };
            let _ = sender.send(outcome);
        });
    }

    /// Asks the background worker to push a commit an earlier session left
    /// local-only — see `catch_up_push`, which this is the only caller of.
    /// Called once by `main.rs` when git-sync activates, so a leftover
    /// unpushed commit gets a chance to go out without waiting for the user
    /// to make another checklist edit (a commit that already matches the
    /// desired content never triggers a `request` on its own).
    ///
    /// Deliberately **not** a `request`: this must never be able to create a
    /// commit, which is exactly what expressing it as one used to do.
    pub fn request_catch_up_push(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        let repo_dir = self.repo_dir.clone();
        let file_path = self.file_path.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let outcome = run_reporting_panics(|| catch_up_push(&repo_dir, &file_path));
            let _ = sender.send(outcome);
        });
    }

    /// Drains the channel non-blockingly; call once per frame regardless of
    /// input, like `FileWatcher::poll_changed`. Returns the outcome of a
    /// completed sync, if one just finished, updates the push-retry state
    /// from it (`apply_outcome`), and kicks off a queued request that
    /// arrived while busy.
    pub fn poll(&mut self) -> Option<SyncOutcome> {
        let outcome = self.receiver.try_recv().ok();
        if let Some(outcome) = &outcome {
            self.busy = false;
            self.apply_outcome(outcome);
            if let Some(sync) = self.pending.take() {
                self.spawn(sync);
            }
        }
        outcome
    }

    /// Folds one completed outcome into the push-retry state — the single
    /// place `self.retry` is written, and deliberately a **total** `match`
    /// with no catch-all arm, so adding a `SyncOutcome` variant is a compile
    /// error rather than a silent no-op.
    ///
    /// That totality is the point. Deep review, round 2: `Failed` used to
    /// fall into a catch-all that did nothing, which left an armed retry
    /// holding its *original* timestamp. `retry_push_if_due`'s backoff check
    /// then stayed permanently elapsed, so a retry spawned on every ~100ms
    /// frame forever — measured at 98 `git` subprocesses in 10 seconds, with
    /// no way out, since nothing in that state can produce a different
    /// outcome. This is the same defect round 7 fixed for the abandoned-retry
    /// case; it survived here because that fix patched one arm instead of
    /// closing the hole. Every arm below now either clears the retry or
    /// restarts its backoff.
    fn apply_outcome(&mut self, outcome: &SyncOutcome) {
        match outcome {
            // Pushed successfully — nothing left to retry.
            SyncOutcome::Synced => self.retry = None,
            // The commit is local-only; arm (or re-arm) the retry, with the
            // backoff starting from now.
            SyncOutcome::CommittedNotPushed { commit, .. } => {
                self.retry = Some((commit.clone(), Instant::now()));
            }
            // The retried commit is no longer HEAD — that specific retry is
            // pointless now, and whatever superseded it syncs on its own.
            SyncOutcome::RetryAbandoned => self.retry = None,
            // The retry itself failed: still unpushed, so keep it armed —
            // but restart the backoff, or a persistently failing retry fires
            // every frame.
            SyncOutcome::RetryFailed(_) => {
                if let Some((_, last_attempt)) = &mut self.retry {
                    *last_attempt = Instant::now();
                }
            }
            // A *content* sync failed. That says nothing about an armed
            // retry for an earlier commit, so its backoff is left exactly as
            // it is — deep round 1: restarting it here let repeated
            // unrelated failures (an unrelated-commits refusal on every
            // toggle, say) postpone a perfectly pushable commit forever.
            SyncOutcome::Failed(_) => {}
            // Ordinary no-ops from a content sync. They say nothing about an
            // unrelated earlier commit's retry, so leave it exactly as it is
            // — clearing here would strand a commit that still needs pushing.
            SyncOutcome::Skipped | SyncOutcome::SkippedUntracked => {}
        }
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
        let Some((commit, last_attempt)) = &self.retry else {
            return;
        };
        if now.saturating_duration_since(*last_attempt) < PUSH_RETRY_INTERVAL {
            return;
        }
        let commit = commit.clone();
        self.spawn_retry(commit);
    }

    /// Whether a sync is currently running or queued behind one that is.
    /// Used only when quitting, to decide whether it's worth waiting at all.
    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

/// Commit messages are kept to one line and capped here so a long task
/// title (the usual source of `change_desc`) can't produce an unwieldy
/// `git log` entry; the description is what gets cut, with a trailing `…`
/// marking the cut, and the file-name prefix is kept intact — unless the
/// file name is itself long enough to blow the cap on its own, in which
/// case it's cut too (see `MAX_FILE_NAME_LEN`).
const MAX_COMMIT_MESSAGE_LEN: usize = 80;

/// The smallest description budget worth leaving room for, so a truncated
/// message still says *something* about the change instead of degenerating
/// to a bare ellipsis.
const MIN_DESC_LEN: usize = 8;

/// How much of the file name the prefix may use. Without this bound,
/// `budget` below saturates to 0 for a file name of ~79 characters or more
/// and the result is `<prefix>…` — *longer* than `MAX_COMMIT_MESSAGE_LEN`,
/// which is the one property that constant exists to guarantee. Accounts
/// for the prefix's own `": "` plus a minimum description and its ellipsis.
const MAX_FILE_NAME_LEN: usize = MAX_COMMIT_MESSAGE_LEN - (2 + MIN_DESC_LEN + 1);

fn commit_message(file_path: &Path, change_desc: &str) -> String {
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checklist".to_string());
    let file_name = if file_name.chars().count() > MAX_FILE_NAME_LEN {
        let kept: String = file_name
            .chars()
            .take(MAX_FILE_NAME_LEN.saturating_sub(1))
            .collect();
        format!("{kept}\u{2026}")
    } else {
        file_name
    };
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
/// commit — *whenever something actually requests one*, which it just can
/// never bleed into this one. External review, round 5: that qualifier
/// matters — a passive file-watcher reload never requests one (see
/// `AppState::request_external_edit_sync`'s doc comment), so a request that
/// goes stale that way would otherwise never be corrected. See the
/// staleness check below (`latest_requested_hash`) for how this function
/// refuses rather than publishing a request once that's happened, without
/// weakening the guarantee in this paragraph.
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
    latest_requested_hash: &Mutex<[u8; 32]>,
    previous_content_hash: Option<[u8; 32]>,
) -> SyncOutcome {
    // All plumbing commands run from the repo root with a root-relative
    // path, sidestepping any ambiguity between CWD-relative and
    // repo-root-relative pathspec handling. Resolved first (before even
    // `status`) since every other check below needs it.
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
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
    // porcelain line to interpret: absent, `?? ` (untracked), or any other
    // two-letter code (a real change to commit). A leading `??` is taken as
    // untracked here because it is unambiguous, but **empty output is not
    // taken as "tracked and unchanged"** — see the index lookup below, which
    // is what actually settles trackedness. `status` is only trusted for the
    // question it can always answer: has anything changed.
    let mut cmd = git_command(repo_dir);
    cmd.args(["status", "--porcelain", "--"]).arg(file_path);
    let status = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) => output,
        Err(err) => return SyncOutcome::Failed(format!("git status failed: {err}")),
    };
    if !status.status.success() {
        return SyncOutcome::Failed(command_error("git status", &status));
    }
    if status.stdout.starts_with(b"??") {
        return SyncOutcome::SkippedUntracked;
    }

    // The path relative to the repo root (required by the plumbing commands
    // below, several of which don't share `status`/`commit`/`push`'s
    // CWD-relative pathspec handling) plus the file's tracked mode, in one
    // call. Safe to read from the real index now that repo_sync_blocked has
    // already confirmed there's no unmerged (conflicted) entry for it.
    //
    // Deliberately resolved **before** the empty-status short-circuit below.
    // Deep round 2: `git status` is not a reliable untrackedness oracle, and
    // is silent about an untracked file in two ordinary configurations —
    // `status.showUntrackedFiles=no`, and the checklist matching a
    // `.gitignore` rule (both confirmed). Either one produced empty output,
    // which the short-circuit read as "nothing to do" and reported as
    // `Skipped` — silently, forever, which is exactly the "git-sync can
    // never do anything and looks like a bug" case `SkippedUntracked` exists
    // to surface. The index is the authority on whether a path is tracked,
    // so trackedness is settled here first and `status` is left to answer
    // only the question it can: has anything changed.
    let (mode, relpath) = match index_entry(repo_dir, file_path) {
        Ok(Some(entry)) => entry,
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if status.stdout.is_empty() {
        return SyncOutcome::Skipped;
    }

    // Refuse if the branch already has unpushed history that touches
    // something other than this file, rather than publishing it as a side
    // effect — checked before *both* branches below that can push (see
    // `unpushed_history`'s doc comment for why plain
    // ahead-of-upstream isn't the right test here — several of markcheck's
    // own commits can legitimately stack up while offline). External
    // review, round 7: this used to run only right before building a new
    // commit, on the assumption that the `blob_bytes_at`-matches fast path
    // below is always "markcheck's own prior commit, always legitimate to
    // push" — false whenever an unrelated commit lands on top of that
    // prior commit without touching the checklist file at all: the
    // checklist's blob at `HEAD` still matches `expected_content` (the
    // unrelated commit never touched it), so the fast path would push
    // `HEAD` — and the unrelated commit riding along with it — without
    // this check ever running. Reachable whenever a request queued
    // earlier gets processed after both a newer, not-yet-committed edit
    // has landed on disk (so `status` above is non-empty, reaching this
    // far) *and* an unrelated commit has landed on the branch — see the
    // fast path's own comment below for why `status` can be non-empty
    // even once this older request is already satisfied at `HEAD`.
    //
    // Resolve the branch tip **once**, and ask every question below about
    // that exact commit — this guard, the already-committed fast path, the
    // push, and the parent a new commit is built on.
    //
    // External review, round 10: these used to re-resolve `HEAD`
    // independently at each step, so this guard validated the range ending
    // at one commit while the fast path pushed whatever `HEAD` had become by
    // the time it ran, a few subprocesses later. A commit landing in that
    // window was published without ever having been checked. Capturing the
    // SHA closes it outright rather than narrowing it: `push` targets an
    // explicit SHA, so the ancestry it can publish is exactly the range that
    // was validated, no matter what `HEAD` does afterwards — no re-check and
    // no retry path needed. `resolve_parent` rather than `current_head`
    // because only it tells "no commits yet" apart from a failed lookup,
    // which the commit path below depends on.
    let tip = match resolve_parent(&repo_root) {
        Ok(tip) => tip,
        Err(err) => return SyncOutcome::Failed(err),
    };

    if let Some(refusal) = unpushed_history_blocks(&repo_root, tip.as_deref(), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);

    // If HEAD already holds exactly this content, the commit half of this
    // request was already satisfied by an earlier sync (e.g. it sat
    // coalesced behind one that committed the same or newer content) —
    // even though `status` above is non-empty because of someone else's
    // still-uncommitted change to the file. That's not the same as the
    // request being *fully* satisfied, though: if the commit hasn't reached
    // upstream yet (a prior push failed), there's still a push worth
    // retrying — see `ahead_of_upstream`'s doc comment for why this matters.
    //
    // Both the content check and the push name `tip` — the same SHA the
    // guard above validated — so this path can only ever publish history
    // that was actually checked. It used to read the blob at the symbolic
    // `HEAD` and then separately re-resolve `HEAD` for the push, which are
    // not guaranteed to be the same commit.
    //
    // A `tip` of `None` (no commits yet) simply isn't this path: there is no
    // committed blob to match, so it falls through to build the first
    // commit. That also subsumes the old unresolvable-`HEAD` failure here,
    // which existed because `current_head(...).unwrap_or_default()` produced
    // `""`, and `push` formats that into the refspec `<remote> :<branch>` —
    // git's *delete a remote ref* form, which succeeds and reports `Synced`
    // while the branch is gone. `push` still rejects an empty SHA outright.
    if let Some(tip) = tip.as_deref()
        && blob_bytes_at(&repo_root, tip, &relpath).as_deref() == Some(expected_content.as_bytes())
    {
        if ahead_of_upstream(&repo_root, tip) {
            return push(repo_dir, tip);
        }
        return SyncOutcome::Skipped;
    }

    // `expected_content` is a snapshot captured whenever the request was
    // queued — possibly well before this background thread actually gets
    // to it (coalesced behind a slow push, a busy worker). External
    // review, round 5: nothing above re-confirms that snapshot is still
    // accounted for by the file's *current* revision before committing it.
    //
    // A naive "disk must equal expected_content" check is too strict,
    // though: several of markcheck's own toggles can legitimately land on
    // disk in quick succession (each one queuing its own request) before
    // the *first* request's background thread ever gets scheduled — by
    // the time it runs, disk already reflects a *later*, already-queued
    // request, not an unrelated external change. That's fine (the later
    // request's own sync will commit that content momentarily); it's not
    // the case this check needs to catch.
    //
    // What actually matters: does the file's current content correspond
    // to *some* request git-sync knows about — either this one, or a newer
    // one already queued to run next — or is it something entirely
    // outside that system (an edit not made through markcheck's own `e`,
    // or a deletion)? `latest_requested_hash` is updated on every
    // `request()` call, including coalesced ones, so it always reflects
    // the newest content git-sync has ever been asked to sync. If disk
    // matches that, whatever's there is already accounted for (either by
    // this commit or a following one); if it doesn't, the change is
    // unaccounted for and must not be silently published. The passive
    // file-watcher reload deliberately never queues a request of its own
    // (see "Isolated commits" below), so without this check, nothing would
    // ever correct the record for that case. `fs::read` failing (the file
    // was deleted) is always unaccounted for, regardless of
    // `latest_requested_hash` — committing `expected_content` regardless
    // would otherwise silently resurrect a file the user just deleted.
    match fs::read(file_path) {
        Ok(bytes) if hash_bytes(&bytes) == *lock_hash(latest_requested_hash) => {}
        _ => {
            return SyncOutcome::Failed(
                "git-sync: file changed since this request was queued; edit or toggle again to sync the current content"
                    .to_string(),
            );
        }
    }

    // Refuse if committing the working tree would drop a staged version of
    // the checklist that exists nowhere else. Checked here, after the fast
    // path (which never builds a commit, so it can't lose anything) and
    // before any commit is constructed.
    let head_blob = tip
        .as_deref()
        .and_then(|t| blob_at(&repo_root, t, &relpath));
    if staged_target_would_be_lost(
        &repo_root,
        &relpath,
        head_blob.as_deref(),
        previous_content_hash,
    ) {
        return SyncOutcome::Failed(STAGED_TARGET_REFUSAL.to_string());
    }

    let blob = match hash_object(&repo_root, expected_content) {
        Ok(sha) => sha,
        Err(err) => return SyncOutcome::Failed(err),
    };

    // The same tip captured before the guards ran — not a fresh resolution,
    // so the commit is built on exactly the history that was validated.
    // `commit_via_temp_index` re-checks `HEAD == parent` immediately before
    // committing and refuses if anything moved in between.
    let parent = tip;
    let created_commit =
        match commit_via_temp_index(&repo_root, &parent, &mode, &blob, &relpath, message) {
            Ok(sha) => sha,
            Err(err) => return SyncOutcome::Failed(err),
        };

    push_if_head_unchanged(repo_dir, &repo_root, &created_commit)
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
fn ahead_of_upstream(repo_root: &Path, tip: &str) -> bool {
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
fn commits_ahead_of_upstream(repo_root: &Path, tip: &str) -> Option<u64> {
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--count", &format!("@{{u}}..{tip}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether the branch already has unpushed commits ahead of upstream that
/// touch something *other* than `relpath` — checked in `run_sync` before
/// either of its two paths that can push (building a new commit, or the
/// `blob_bytes_at`-already-matches fast path's push of the existing `HEAD`),
/// never inside `retry_commit`'s own timed retry (a stale retry is instead
/// abandoned outright — see its doc comment — rather than re-checked
/// against this). Deliberately not a plain ahead-of-upstream check:
/// multiple markcheck commits can legitimately stack up while
/// offline (a user toggling several tasks before connectivity returns —
/// see `repeated_requests_against_an_unreachable_remote_never_deadlock_or_lose_edits`),
/// and none of those are "unrelated work" to refuse over — only commits
/// touching something else are. `range_has_unrelated_commits` checks each
/// commit in `@{u}..HEAD` individually, which is what tells the two apart
/// correctly — see its own doc comment for why a single net diff isn't
/// enough.
///
/// Deliberately **fails closed** (`false`, i.e. "don't refuse") whenever
/// the check itself can't be answered — no upstream configured, no commits
/// yet (a repo's very first sync), or any other `git` failure — because
/// refusing is the consequential action here, not pushing. Failing open
/// the way `ahead_of_upstream` does would wrongly block every first-ever
/// commit to a fresh repository and every sync where upstream tracking
/// simply isn't set up.
/// What the unpushed-history check could establish about `tip` — three
/// genuinely different answers that a plain `bool` used to collapse into
/// two.
#[derive(Debug, PartialEq, Eq)]
enum UnpushedHistory {
    /// No upstream is configured, so there is no range to compare against.
    /// **Not** a safety failure: the branch simply has no publication target
    /// yet (a fresh repository, or one where `git push -u` was never run),
    /// and `push` refuses on its own with a specific message. Committing may
    /// proceed; publishing can't happen anyway.
    NoUpstream,
    /// Every unpushed commit touches only the checklist path.
    Safe,
    /// At least one unpushed commit touches something else.
    ContainsUnrelated,
}

/// Whether the unpushed range ending at `tip` contains commits touching
/// anything but `relpath` — the guard that keeps a checklist toggle from
/// publishing unrelated local work, since an explicit-SHA push still sends
/// the commit's whole ancestry.
///
/// `tip` is the caller's captured branch tip, never the symbolic `HEAD`, so
/// the range checked is exactly the range that will be published. `None`
/// means the branch has no commits at all, so there is nothing unpushed for
/// anything to be unrelated to.
///
/// External review, round 10: this returned a `bool` built with
/// `.unwrap_or(false)`, so *any* git failure — a `rev-list` timeout on a
/// large history, a transiently unhealthy object database — became
/// indistinguishable from "verified safe" and the caller went on to push.
/// The asymmetry matters: a false refusal costs the user a retry, while a
/// false clearance publishes someone else's commits. The reason it was
/// written that way is real, though, and is why this isn't simply a
/// `Result<bool, _>`: `@{u}` is legitimately unresolvable when no upstream
/// exists, and failing closed on that would block every first-ever commit to
/// a fresh repository. Separating that case out is what lets the genuine
/// failures fail closed without taking the legitimate one down with them.
fn unpushed_history(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
) -> Result<UnpushedHistory, String> {
    let Some(tip) = tip else {
        return Ok(UnpushedHistory::Safe);
    };
    let Some(upstream) = resolve_upstream(repo_root)? else {
        return Ok(UnpushedHistory::NoUpstream);
    };
    match range_has_unrelated_commits(repo_root, Some(&upstream), tip, relpath)? {
        true => Ok(UnpushedHistory::ContainsUnrelated),
        false => Ok(UnpushedHistory::Safe),
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
fn resolve_upstream(repo_root: &Path) -> Result<Option<String>, String> {
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

/// The refusal message shared by both push-capable paths.
const UNRELATED_COMMITS_REFUSAL: &str =
    "git-sync: branch has unpushed commits unrelated to this change; push them manually first";

/// Maps an `unpushed_history` answer to "may this path continue?", refusing
/// on both an unrelated commit and an unanswerable check.
fn unpushed_history_blocks(
    repo_root: &Path,
    tip: Option<&str>,
    relpath: &str,
) -> Option<SyncOutcome> {
    match unpushed_history(repo_root, tip, relpath) {
        Ok(UnpushedHistory::Safe) | Ok(UnpushedHistory::NoUpstream) => None,
        Ok(UnpushedHistory::ContainsUnrelated) => {
            Some(SyncOutcome::Failed(UNRELATED_COMMITS_REFUSAL.to_string()))
        }
        Err(err) => Some(SyncOutcome::Failed(format!(
            "git-sync: could not verify the branch has no unrelated unpushed commits ({err}); \
             not publishing"
        ))),
    }
}

/// Whether any commit in `base..tip` — or, when `base` is `None`, in the
/// full ancestry of `tip` (the root-commit case) — touches a path other
/// than `relpath`. Checked **per commit**, not as a single net diff between
/// `base` and `tip`: external review, round 8, and a self-review finding of
/// the same shape in `verify_commit_scope` below — a commit that touches an
/// unrelated path followed by a later commit that reverts that exact change
/// nets out to zero in a `base..tip` tree diff, so a naive check sees
/// nothing wrong, even though both commits are still in the range and still
/// get published (they're ancestors of `tip`, and an explicit-SHA push
/// sends a commit's whole ancestry regardless of what any single commit's
/// net effect on the tree looks like).
///
/// Two subprocesses total, regardless of how many commits are in the range.
/// `git rev-list --parents <range>` gives every commit and its parent count
/// in one call; a commit with more than one parent (a merge) is treated as
/// unrelated unconditionally — refusing outright rather than attempting a
/// combined-diff interpretation of what a merge itself touched, which has
/// genuine edge cases (conflict resolutions, content changed only during the
/// merge) that could misjudge it either way. The remaining (single-parent or
/// root) commits then go to **one** `git diff-tree --root ... --stdin` for
/// the whole range. `--root` is required for a root commit to show anything
/// at all (confirmed empirically: without it, `diff-tree` on a zero-parent
/// commit prints nothing) and is a no-op for an ordinary commit (still diffs
/// against its own first parent).
///
/// Deep review, round 2: this used to spawn one `diff-tree` **per commit**,
/// each through `run_with_timeout`'s subprocess plus two reader threads —
/// on every sync request, over `@{u}..HEAD`, a range that grows without
/// bound while the remote is unreachable (and markcheck's design
/// deliberately lets its own commits stack up while offline). Measured at
/// 200 unpushed commits: 0.503s and ~400 thread spawns per toggle, versus
/// 0.017s for the batched form. Both shapes are "any" predicates over the
/// same range, so only the order of evaluation changes, not the verdict.
///
/// Returns `Err` (rather than failing open or closed itself) on any `git`
/// failure, leaving that decision to each caller: `branch_has_unrelated_
/// unpushed_commits` fails closed (`false`) on `Err`, matching its existing
/// behavior; `verify_commit_scope` propagates it as a hard sync failure,
/// also matching its existing behavior.
fn range_has_unrelated_commits(
    repo_root: &Path,
    base: Option<&str>,
    tip: &str,
    relpath: &str,
) -> Result<bool, String> {
    let range = match base {
        Some(base) => format!("{base}..{tip}"),
        None => tip.to_string(),
    };
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", &range]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git rev-list failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git rev-list", &output));
    }
    // Pass 1: the commit list, plus merge detection from the parent count.
    // A merge short-circuits before any diff is needed at all.
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut shas = String::new();
    for line in listing.lines() {
        let mut tokens = line.split_whitespace();
        let Some(sha) = tokens.next() else { continue };
        if tokens.count() > 1 {
            return Ok(true);
        }
        shas.push_str(sha);
        shas.push('\n');
    }
    if shas.is_empty() {
        return Ok(false);
    }

    // Pass 2: one `diff-tree` for the whole range, fed the SHA list on
    // stdin, rather than one subprocess per commit.
    //
    // `--no-commit-id` matters beyond tidiness: without it the output
    // interleaves commit ids with paths in the same NUL-separated stream,
    // and telling them apart would mean guessing that a 40-hex field is an
    // id — which a file legitimately named 40 hex characters would defeat.
    // With it, every field is a path, so there is nothing to disambiguate.
    let mut diff_cmd = git_command(repo_root);
    diff_cmd.args([
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        "--stdin",
    ]);
    let diff_output = run_with_timeout_and_stdin(diff_cmd, PLUMBING_TIMEOUT, &shas)
        .map_err(|err| format!("git diff-tree failed: {err}"))?;
    if !diff_output.status.success() {
        return Err(command_error("git diff-tree", &diff_output));
    }
    Ok(diff_output
        .stdout
        .split(|&b| b == 0)
        .any(|path| !path.is_empty() && path != relpath.as_bytes()))
}

/// Resolves `(remote, upstream-branch-ref)` for the current branch via its
/// `branch.<name>.remote`/`branch.<name>.merge` config — the same two keys
/// several tests already set up manually (via `-u` push or explicit `git
/// config` calls) — rather than parsing `@{u}`'s abbreviated form, which
/// would need guessing where the remote name ends and a branch name that
/// itself contains `/` begins. `None` when either key is unset (no
/// upstream configured at all), or the branch itself can't be resolved
/// (detached `HEAD`).
fn upstream_parts(repo_root: &Path) -> Option<(String, String)> {
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

/// Pushes `expected_commit` — explicitly, as `<remote>
/// <expected_commit>:<upstream-branch>` rather than a bare `git push` —
/// translating the result into a `SyncOutcome`. A failure here is always
/// `CommittedNotPushed`, never `Failed`, with one carve-out below: by the
/// time this is called, either a commit was just made or `HEAD` was already
/// confirmed to hold the desired content — either way, a local commit
/// exists and the only thing to retry is the push itself.
///
/// The carve-out is an **empty** `expected_commit`, which is rejected
/// outright as `Failed` (the one case where no local commit can be claimed,
/// since there's no SHA naming one). This is defence in depth, not a
/// reachable path: every caller resolves a real SHA first. It exists because
/// the consequence of getting here with `""` is severe and silent — the
/// refspec below would become `<remote> :<branch>`, git's *delete a remote
/// ref* form, which succeeds and reports `Synced` while deleting the branch.
/// A caller that regressed into passing an unresolved SHA (as `run_sync`'s
/// fast path once did, via `unwrap_or_default()`) must fail loudly rather
/// than delete anything.
///
/// External review, round 5: both of this function's callers
/// (`push_if_head_unchanged`, `retry_commit`) verify `HEAD ==
/// expected_commit` immediately beforehand, but that check and the actual
/// push used to be two separate steps regardless — a bare `git push`
/// sends whatever the local branch tip has become by the time it runs, so
/// a commit landing in the gap between the check and the push would still
/// ride along. Targeting `expected_commit` explicitly closes this rather
/// than merely re-narrowing it: the push can never publish more than that
/// commit's own ancestry, no matter what the local branch has become by
/// push-time.
///
/// When `upstream_parts` can't resolve a remote/branch to target — no
/// upstream tracking configured for this branch — the push is **refused**
/// rather than falling back to a bare `git push`. Deep review, reproduced
/// empirically: that fallback was justified on the premise that a bare push
/// "already reports a clear, git-native error" in this situation, which is
/// false for an ordinary configuration. With a remote added but `-u` never
/// used, and `push.default = current`, a bare push resolves a destination
/// and **succeeds**, publishing the whole branch tip — every unpushed
/// commit, not just `expected_commit`. That is exactly what targeting the
/// commit explicitly exists to prevent, and it composes badly with
/// the unrelated-history guard, which independently fails
/// *closed* in the same configuration (its `@{u}..HEAD` range can't be
/// resolved either, so it declines to refuse): both guards are off at once,
/// and an unrelated local commit gets published by a checklist toggle.
/// Refusing keeps the commit safely local, reuses the existing retry
/// machinery, and gives the user a one-off action to take.
fn push(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    race_point(RacePoint::BeforePush);
    if expected_commit.is_empty() {
        return SyncOutcome::Failed("git-sync: refusing to push an unresolved commit".to_string());
    }
    let Some((remote, branch_ref)) = upstream_parts(repo_dir) else {
        return SyncOutcome::CommittedNotPushed {
            message: "git-sync: no upstream configured for this branch; \
                      run `git push -u` once"
                .to_string(),
            commit: expected_commit.to_string(),
        };
    };
    let mut cmd = git_command(repo_dir);
    cmd.args(["push", &remote, &format!("{expected_commit}:{branch_ref}")]);
    let output = run_with_timeout(cmd, PUSH_TIMEOUT);
    match output {
        Ok(output) if output.status.success() => SyncOutcome::Synced,
        Ok(output) => SyncOutcome::CommittedNotPushed {
            message: command_error("git push", &output),
            commit: expected_commit.to_string(),
        },
        Err(err) => SyncOutcome::CommittedNotPushed {
            message: format!("git push failed: {err}"),
            commit: expected_commit.to_string(),
        },
    }
}

/// Pushes `expected_commit` (the commit `run_sync` just made and verified),
/// but only if `HEAD` still equals it — external review, round 4:
/// the unrelated-history guard (`unpushed_history`) only ever ran as a snapshot
/// *before* the commit, so a commit landing on the branch after
/// verification but before this point (another markcheck instance, a
/// human, an IDE) would otherwise get pushed right alongside markcheck's
/// own, unnoticed. Unlike `retry_commit`'s silent `Skipped` on the same
/// mismatch (correct there — it's abandoning a *retry* of an
/// already-reported failure), a first attempt at a freshly-made commit
/// must surface something rather than going quiet, so a mismatch here is
/// reported as `CommittedNotPushed` instead: nothing is lost (the commit
/// is safely local either way) and this reuses the existing automatic
/// retry machinery (`GitSync::poll`/`retry_push_if_due` → `retry_commit`,
/// which performs the same check again once due).
fn push_if_head_unchanged(repo_dir: &Path, repo_root: &Path, expected_commit: &str) -> SyncOutcome {
    if current_head(repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::CommittedNotPushed {
            message: "git-sync: repository changed after commit".to_string(),
            commit: expected_commit.to_string(),
        };
    }
    push(repo_dir, expected_commit)
}

/// Re-attempts pushing a specific commit that previously failed to push
/// (`GitSync::retry_push_if_due`), rather than replaying the file content
/// that produced it through the generic commit-or-skip logic in `run_sync`.
/// That distinction is the whole point: if `expected_commit` is no longer
/// `HEAD` (something else — most plausibly another concurrent markcheck/
/// git-sync process — committed on top since the failed push), silently
/// abandoning the retry is strictly safer than `run_sync` would be here,
/// since `run_sync` would build a *new* commit from the stale content,
/// reverting whatever superseded it. Whatever superseded `expected_commit`
/// gets its own sync opportunity through the normal request path, so
/// nothing is lost by giving up on this specific stale retry.
fn retry_commit(repo_dir: &Path, expected_commit: &str) -> SyncOutcome {
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };
    if current_head(&repo_root).as_deref() != Some(expected_commit) {
        return SyncOutcome::RetryAbandoned;
    }
    push(repo_dir, expected_commit)
}

/// Pushes a commit an earlier session left local-only, and **never builds a
/// commit of its own** — the startup counterpart to `retry_commit`'s timed
/// retry, run once by `main.rs` when git-sync activates.
///
/// Deep review, round 3, reproduced end-to-end. Startup used to express this
/// as an ordinary `GitSync::request` carrying the file's *current disk
/// content* as `expected_content`, described as `Catch up a pending push`.
/// That defeats every guard `run_sync` has, by construction: `status` is
/// non-empty, `HEAD`'s blob differs, and the staleness check compares disk
/// against `latest_requested_hash` — which that same request had just set to
/// that same content. So a checklist with ordinary uncommitted edits (made
/// in an editor, no markcheck involvement) was committed *and pushed* purely
/// by opening the file and quitting, under a message describing a push
/// catch-up rather than the change it actually published. Publishing is not
/// undoable the way a local commit is, and nothing about opening a viewer
/// should publish anything.
///
/// So this path only ever pushes. It also fails **closed** throughout —
/// unlike `run_sync`, which fails open in several places because it runs
/// after a change the user just made, where silence would hide a problem.
/// Nothing has happened here, so every unanswerable question means "do
/// nothing, quietly": no upstream, no commits, an unreadable count, all
/// return `Skipped`. The one thing worth reporting is an untracked file,
/// which `run_sync` would have reported at startup before this change and
/// which means git-sync can never do anything at all for this file.
fn catch_up_push(repo_dir: &Path, file_path: &Path) -> SyncOutcome {
    let mut cmd = git_command(repo_dir);
    cmd.args(["rev-parse", "--show-toplevel"]);
    let repo_root = match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => return SyncOutcome::Failed(command_error("git rev-parse", &output)),
        Err(err) => return SyncOutcome::Failed(format!("git rev-parse failed: {err}")),
    };
    if let Some(reason) = repo_sync_blocked(&repo_root) {
        return SyncOutcome::Failed(reason);
    }
    // Capture the tip **before** validating anything, and push that exact
    // SHA. External review, round 10: this used to validate the range
    // ending at the symbolic `HEAD` and then separately re-resolve `HEAD`
    // for the push, so an unrelated commit landing between those two
    // subprocesses was published without ever having been checked — the
    // same defect as `run_sync`'s fast path, in the one path that runs
    // unprompted at startup. Naming the SHA closes it: `push` targets an
    // explicit commit, so what it can publish is exactly what was validated.
    let Some(head) = current_head(&repo_root) else {
        return SyncOutcome::Skipped;
    };
    // Positively-known unpushed commits only — see `commits_ahead_of_upstream`.
    if commits_ahead_of_upstream(&repo_root, &head).is_none_or(|ahead| ahead == 0) {
        return SyncOutcome::Skipped;
    }
    let relpath = match index_entry(repo_dir, file_path) {
        Ok(Some((_mode, relpath))) => relpath,
        Ok(None) => return SyncOutcome::SkippedUntracked,
        Err(err) => return SyncOutcome::Failed(err),
    };
    // The same refusal `run_sync` makes: an explicit-SHA push still sends the
    // commit's whole ancestry, so unrelated unpushed work must not ride along.
    if let Some(refusal) = unpushed_history_blocks(&repo_root, Some(&head), &relpath) {
        return refusal;
    }
    race_point(RacePoint::AfterHistoryValidation);
    push(repo_dir, &head)
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
/// the real index is never read, and the only write it ever makes is
/// `align_real_index_entry`'s single-path realignment after a successful
/// commit (see its own doc comment for why that's needed) — every other
/// path in the real index, staged or not, is left completely alone.
fn commit_via_temp_index(
    repo_root: &Path,
    parent: &Option<String>,
    mode: &str,
    blob: &str,
    relpath: &str,
    message: &str,
) -> Result<String, String> {
    let git_dir =
        git_dir(repo_root).ok_or_else(|| "git-sync: could not resolve git-dir".to_string())?;
    let temp_index = git_dir.join(format!(
        "markcheck-index-{}-{:x}",
        std::process::id(),
        crate::writer::random_suffix()
    ));
    // What the real index holds for this path *before* any of the work
    // below runs — the baseline `align_real_index_entry` compares against so
    // it never overwrites something the user staged in the meantime.
    let index_before = index_blob(repo_root, relpath);
    let result = (|| {
        if let Some(head) = parent {
            populate_temp_index(repo_root, &temp_index, head)?;
        }
        stage_into_temp_index(repo_root, &temp_index, mode, blob, relpath)?;
        if &current_head(repo_root) != parent {
            return Err("git-sync: repository changed during sync, will retry".to_string());
        }
        // repo_sync_blocked (in run_sync) is only a snapshot taken before
        // any of the above ran; re-checking here narrows (not eliminates —
        // same residual-race tradeoff as the HEAD check just above) the
        // window in which a merge/rebase/etc. could have started since,
        // which would otherwise let this land as a merge-completing commit
        // instead of refusing outright.
        if let Some(reason) = repo_sync_blocked(repo_root) {
            return Err(reason);
        }
        race_point(RacePoint::BeforeCommit);
        let created_commit = commit_temp_index(
            repo_root,
            &temp_index,
            message,
            parent,
            relpath,
            blob,
            PLUMBING_TIMEOUT,
        )?;
        verify_commit_scope(repo_root, parent, relpath, &created_commit)?;
        race_point(RacePoint::BeforeIndexAlignment);
        align_real_index_entry(repo_root, mode, blob, relpath, index_before.as_deref());
        Ok(created_commit)
    })();
    let _ = std::fs::remove_file(&temp_index);
    // git's index-writing plumbing (`read-tree`/`update-index`/`commit`)
    // stages its write through `<index>.lock` (the full filename with a
    // literal `.lock` suffix appended, not a path-extension replacement —
    // `temp_index`'s own generated name never has an extension to begin
    // with), renaming it into place on success. A subprocess killed
    // mid-write by `run_with_timeout` can leave that lock file behind
    // without ever reaching the rename — the index-file cleanup just above
    // wouldn't catch it, since the target index itself may never have
    // existed at all in that case.
    let mut lock_file = temp_index.into_os_string();
    lock_file.push(".lock");
    let _ = std::fs::remove_file(lock_file);
    result
}

/// `GIT_INDEX_FILE=<temp_index> git read-tree <parent>`: seeds the
/// temporary index with `parent`'s tree, so every path other than the one
/// about to be replaced commits exactly as `parent` had it — never the real
/// index's (possibly unrelated-staged-content-holding) state.
fn populate_temp_index(repo_root: &Path, temp_index: &Path, parent: &str) -> Result<(), String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["read-tree", parent]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
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
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index).args([
        "update-index",
        "--add",
        "--cacheinfo",
        mode,
        blob,
        relpath,
    ]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git update-index failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git update-index", &output));
    }
    Ok(())
}

/// `git update-index --add --cacheinfo`, run against the *real* index (no
/// `GIT_INDEX_FILE` override) — called once, right after a commit succeeds,
/// to set the real index's entry for `relpath` to the blob/mode just
/// committed. Building the commit through a separate temporary index (see
/// `stage_into_temp_index`) keeps unrelated staged content from riding
/// along, but it also means the real index's own entry for `relpath` is
/// never advanced as a side effect the way a normal `git commit -- <path>`
/// would leave it — external review, round 9: reproduced empirically
/// against a clean repo with no staged content at all. Before any sync,
/// the real index matches `HEAD` for `relpath`; once the temp-index commit
/// moves `HEAD` forward without touching the real index, that path's real
/// entry is left pointing at the *old* blob — permanently, from the very
/// first toggle. From then on `git status` shows the file as both staged
/// (stale index vs. new `HEAD`) and not staged (current working tree vs.
/// that same stale index), recurring on every subsequent toggle, since
/// nothing else ever corrects it. This call is the fix: it realigns only
/// `relpath`'s own entry, mirroring exactly what committing that one path
/// normally leaves behind — every other real-index entry (e.g. an
/// unrelated file the user `git add`ed) is left completely untouched, so
/// `run_sync_never_includes_an_unrelated_staged_file`'s guarantee still
/// holds. Best-effort: the commit and any push it enables have already
/// succeeded by the time this runs, so a failure here (this is local
/// bookkeeping only, not part of the commit's correctness) is not worth
/// reporting as a sync failure.
/// External review, round 10: this write used to be unconditional, which
/// makes it the one place the real index is mutated without first checking
/// that the entry still belongs to markcheck. If the user stages a *newer*
/// version of the checklist while the background sync is running (`git add`
/// from another terminal, or an IDE doing it for them), overwriting the
/// entry silently discards their staged version. The working tree is
/// untouched either way, so this is staging loss rather than content loss —
/// but it is exactly the kind of unrelated state the temporary-index design
/// exists to protect, and the target path shouldn't be the one exception
/// that ignores it. `expected_entry` is the blob the real index held when
/// this sync began: matching means markcheck is still realigning its own
/// entry, and any mismatch means someone else has staged something since
/// and it is theirs to keep.
fn align_real_index_entry(
    repo_root: &Path,
    mode: &str,
    blob: &str,
    relpath: &str,
    expected_entry: Option<&str>,
) {
    if index_blob(repo_root, relpath).as_deref() != expected_entry {
        return;
    }
    let mut cmd = git_command(repo_root);
    cmd.args(["update-index", "--add", "--cacheinfo", mode, blob, relpath]);
    let _ = run_with_timeout(cmd, PLUMBING_TIMEOUT);
}

/// The refusal message for a checklist whose staged version would be lost.
const STAGED_TARGET_REFUSAL: &str = "git-sync: the checklist has staged changes that differ from the file on disk; \
     committing would drop them — commit or unstage them first";

/// Whether committing the working tree would discard a staged snapshot of
/// the checklist that exists nowhere else.
///
/// External review, round 11, reproduced end-to-end. The commit is built
/// from the working tree, and `align_real_index_entry` afterwards points the
/// real index at it — so if the index was holding a *different* version of
/// the checklist, that version is neither committed nor still staged. It
/// survives only as a dangling blob, reachable through no ordinary git
/// workflow. Verified: a staged blob's unique content was absent from the
/// resulting commit and unreferenced by anything afterwards.
///
/// This is deliberately **not** "refuse whenever the target is staged".
/// Staged content equal to what markcheck loaded is wholly contained in what
/// is about to be committed — the toggle is the only difference — so nothing
/// is lost, and refusing there would break two ordinary workflows for no
/// safety gain: staging the checklist before running markcheck, and `git
/// add`ing a brand-new checklist that has never been committed. Only a
/// staged snapshot that differs from the pre-write working tree is at risk,
/// and that is exactly what this returns true for.
///
/// Fails **closed**: if the staged bytes can't be read, or the request
/// carries no pre-write hash to compare against, safety can't be
/// established and the sync refuses — the same rule `unpushed_history`
/// follows.
fn staged_target_would_be_lost(
    repo_root: &Path,
    relpath: &str,
    head_blob: Option<&str>,
    previous_content_hash: Option<[u8; 32]>,
) -> bool {
    let index = index_blob(repo_root, relpath);
    if index.as_deref() == head_blob {
        return false; // nothing staged for this path
    }
    let Some(previous) = previous_content_hash else {
        return true;
    };
    match staged_bytes(repo_root, relpath) {
        Some(bytes) => hash_bytes(&bytes) != previous,
        None => true,
    }
}

/// The checklist's staged content — stage 0 of the **real** index (`git show
/// :<path>`), not `HEAD`'s. `None` when there is no staged entry or the read
/// fails.
fn staged_bytes(repo_root: &Path, relpath: &str) -> Option<Vec<u8>> {
    let mut cmd = git_command(repo_root);
    cmd.args(["show", &format!(":{relpath}")]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}

/// The blob SHA the **real** index currently holds for `relpath`, or `None`
/// when it has no entry for it (or the lookup fails). `git ls-files --stage`
/// prints `<mode> <object> <stage>\t<path>`, so the object is the second
/// whitespace-separated field of the part before the tab; `-z` keeps a path
/// containing a newline or a quotable character intact, as `index_entry`
/// already relies on.
fn index_blob(repo_root: &Path, relpath: &str) -> Option<String> {
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

/// `GIT_INDEX_FILE=<temp_index> git commit -m <message>`: commits the
/// temporary index's tree against the repository's real `HEAD`/branch —
/// normal commit machinery (hooks, `commit.gpgsign`, HEAD's own locked
/// read-and-update), just fed from the temporary index instead of the real
/// one. Returns the new commit's SHA (resolved once, immediately after
/// success) — the single source of truth for "the commit markcheck just
/// made" that `verify_commit_scope`/`undo_commit` operate on, rather than
/// each re-resolving `HEAD` independently at a later point in time (which
/// would let a commit landing in between be mistaken for part of this
/// one).
///
/// `timeout` is a parameter (mirroring `run_with_timeout`'s own explicit
/// design) rather than reading `PLUMBING_TIMEOUT` directly, so tests can
/// inject a short one — needed to exercise the reconciliation below
/// deterministically via a hook that sleeps past it.
///
/// External review, round 4: `git commit` can run a `post-commit` hook —
/// which only ever runs *after* the ref update, `git commit`'s own
/// mutation, is already durable — as its last step before the process
/// exits. If that hook (or the commit machinery itself, past the point of
/// no return) is still running when `timeout` expires, `run_with_timeout`
/// kills the whole process group, and a naive caller would report the
/// commit as failed even though it had already succeeded. On a timeout
/// specifically (never on an ordinary nonzero exit, which never leaves
/// `HEAD` in doubt the way a killed process can), `HEAD` is re-resolved: if
/// it moved past `parent`, the commit is treated as having succeeded —
/// returning that new `HEAD` exactly as the success path would, so the
/// normal `verify_commit_scope` pipeline still runs against it (a hook
/// that also misbehaved during that same timed-out run is still caught and
/// undone normally). If `HEAD` didn't move, the commit genuinely never
/// happened and the original timeout is reported.
///
/// External review, round 9: `HEAD` moving past `parent` is not by itself
/// proof that *our* commit is what moved it — another process can
/// legitimately commit something else entirely during the same window
/// (most plausibly while our own commit is genuinely still blocked in a
/// slow hook, not actually done yet). Adopting that unrelated commit as
/// `created_commit` would be worse than reporting a false failure: the
/// scope check below would correctly flag it as out-of-scope (it never
/// touched `relpath`) and undo it — and `undo_commit`'s compare-and-swap
/// would succeed, because we handed it the *right* SHA for the *wrong*
/// reason, rewinding a real, unrelated commit off the branch. `relpath`
/// and `blob` (the exact content this call staged) are threaded through
/// so the timeout branch can positively confirm ownership before trusting
/// it, rather than inferring ownership from ref movement alone.
///
/// Ownership is confirmed by **three** checks, not one, because a content
/// check and a lineage check each catch a case the other misses:
///
/// * `HEAD` moved past `parent` at all — the original round-4 check.
/// * `relpath` at the new `HEAD` is exactly `blob`. Catches an unrelated
///   commit made directly on `parent` (our own commit still blocked in a
///   slow hook): it never touched `relpath`, so the blob there is still
///   the old one.
/// * the new `HEAD`'s parents are exactly `parent` (one parent equal to
///   it, or none at all when `parent` is `None` and this is a root
///   commit). Catches the mirror case the blob check alone cannot: *our*
///   commit landed **and** an unrelated commit landed on top of it without
///   touching `relpath`, which leaves our blob in place at `HEAD` and so
///   passes the content check while still naming the wrong SHA.
///
/// A commit that landed but got raced this way is reported as a failure
/// rather than adopted. That's a false negative — the commit really is
/// there — but it is the safe direction: `run_sync` returns `Failed`, the
/// next request retries from fresh state, and nothing is rewound.
fn commit_temp_index(
    repo_root: &Path,
    temp_index: &Path,
    message: &str,
    parent: &Option<String>,
    relpath: &str,
    blob: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.env("GIT_INDEX_FILE", temp_index)
        .args(["commit", "-m", message]);
    match run_with_timeout(cmd, timeout) {
        Ok(output) if output.status.success() => current_head(repo_root)
            .ok_or_else(|| "git-sync: could not resolve HEAD after commit".to_string()),
        Ok(output) => Err(command_error("git commit", &output)),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => match current_head(repo_root) {
            Some(head)
                if Some(&head) != parent.as_ref()
                    && blob_at(repo_root, &head, relpath).as_deref() == Some(blob)
                    && descends_directly_from(repo_root, &head, parent) =>
            {
                Ok(head)
            }
            _ => Err(format!("git commit failed: {err}")),
        },
        Err(err) => Err(format!("git commit failed: {err}")),
    }
}

/// Runs after `commit_temp_index` succeeds (hooks having already executed
/// against the temporary index) and enforces the invariant a `pre-commit`
/// hook can otherwise silently break: a `git add`/formatter/`lint-staged`/
/// "stage everything" hook inherits the same `GIT_INDEX_FILE` as the rest
/// of this commit, and can stage more into it — defeating the whole point
/// of the temporary-index design (see `commit_via_temp_index`'s doc
/// comment), which exists specifically so an unrelated staged file can
/// never ride along into a markcheck commit. Checks `parent..created_commit`
/// — the exact commit `commit_temp_index` just made, captured once by its
/// caller — rather than re-resolving `HEAD` here: a commit landing after
/// `created_commit` was captured must never be mistaken for (or silently
/// swept into) markcheck's own commit. `range_has_unrelated_commits` (not
/// a single net diff — see its own doc comment for why: a hook that makes
/// two nested commits where a later one reverts an earlier one's unrelated
/// change would net out to zero in a plain diff, even though both extra
/// commits are still in history and still get pushed) decides whether
/// anything beyond `relpath` changed; if so, the commit is undone
/// (`undo_commit`) and an error reported — a hook that adds a nested commit
/// of its own is undone the same way, since resetting the branch ref back
/// to `parent` discards the whole chain regardless of how many commits are
/// in it.
fn verify_commit_scope(
    repo_root: &Path,
    parent: &Option<String>,
    relpath: &str,
    created_commit: &str,
) -> Result<(), String> {
    // A verification that *cannot be performed* is not the same as one that
    // passed, and it leaves the commit sitting on the branch. Deep rounds 2,
    // round 3: this used to be a bare `?`, so the user saw only the raw git
    // failure ("git rev-list: fatal: ...") with nothing to say a commit had
    // been made and kept — while the neighbouring undo-failed arm is careful
    // to name exactly that. The commit is deliberately *not* undone here:
    // whether it is in scope is precisely what could not be established, and
    // rewinding a possibly-fine commit is the worse guess.
    let has_unrelated =
        match range_has_unrelated_commits(repo_root, parent.as_deref(), created_commit, relpath) {
            Ok(has_unrelated) => has_unrelated,
            Err(err) => {
                return Err(format!(
                    "git-sync: could not verify what commit {created_commit} changed ({err}); \
                 it was made and left on the branch — check it before pushing"
                ));
            }
        };
    if !has_unrelated {
        return Ok(());
    }
    match undo_commit(repo_root, created_commit) {
        Ok(()) => Err(
            "git-sync: a commit hook modified files beyond the checklist; sync aborted".to_string(),
        ),
        Err(undo_err) => Err(format!(
            "git-sync: a commit hook modified files beyond the checklist, and undoing the \
             commit failed ({undo_err}); commit {created_commit} may need manual cleanup"
        )),
    }
}

/// Moves the branch ref back to `parent` (or deletes it entirely, for a
/// root commit with `parent: None`) — the undo side of
/// `verify_commit_scope`. Never touches the working tree or the real
/// index, same as everything else in the temp-index commit path.
///
/// Both forms are compare-and-swapped against `created_commit` — `git
/// update-ref`'s optional expected-old-value argument — rather than an
/// unconditional move/delete: if the branch has advanced past
/// `created_commit` (another process committed on top before this undo
/// ran), the update-ref itself fails instead of silently rewinding that
/// concurrent commit out of the branch's history.
///
/// **It rewinds exactly one commit — markcheck's own — and never the
/// captured `parent`.** Deep round 3, reproduced with the race harness: this
/// used to reset the branch straight back to `parent`, which discards
/// *everything* between. A commit landing in the window between
/// `commit_via_temp_index`'s `HEAD == parent` re-check and `git commit`
/// itself becomes an **ancestor** of `created_commit`, so the
/// compare-and-swap above still passes — it only guards against the branch
/// moving *past* our commit — and the reset dropped that foreign commit off
/// the branch entirely. Observed directly: a concurrent `unrelated` commit
/// left the branch reading `init` alone, surviving only as a dangling
/// object.
///
/// The graph cannot distinguish that from a `pre-commit` hook making nested
/// commits of its own, and it does not need to: markcheck did not create
/// those either. The module's rule is that markcheck never moves `HEAD`
/// across a commit it did not make, and resetting to `created_commit`'s own
/// first parent is what actually honours it. The cost is that hook-made
/// commits now survive an aborted sync instead of being swept away with it —
/// which is the right trade, and the user is told (`verify_commit_scope`'s
/// error names the commit), and the next sync's unpushed-history guard
/// refuses with a clear message until they deal with it.
fn undo_commit(repo_root: &Path, created_commit: &str) -> Result<(), String> {
    // The commit's *own* first parent, not the parent captured before the
    // commit ran — see above.
    let mut cmd = git_command(repo_root);
    cmd.args(["rev-list", "--parents", "-n", "1", created_commit]);
    let output = run_with_timeout(cmd, PLUMBING_TIMEOUT)
        .map_err(|err| format!("git rev-list failed: {err}"))?;
    if !output.status.success() {
        return Err(command_error("git rev-list", &output));
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let first_parent = listing
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .map(str::to_string);

    let mut cmd = git_command(repo_root);
    match &first_parent {
        Some(sha) => {
            cmd.args(["update-ref", "HEAD", sha, created_commit]);
        }
        // A root commit has no parent to move back to, so the branch ref is
        // deleted outright, returning to the "no commits yet" state.
        None => {
            let branch_ref = current_branch_ref(repo_root).ok_or_else(|| {
                "git-sync: could not resolve branch ref to undo root commit".to_string()
            })?;
            cmd.args(["update-ref", "-d", &branch_ref, created_commit]);
        }
    }
    match run_with_timeout(cmd, PLUMBING_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(command_error("git update-ref", &output)),
        Err(err) => Err(format!("git update-ref failed: {err}")),
    }
}

/// The branch `HEAD` symbolically points at (e.g. `refs/heads/main`),
/// resolved regardless of whether that branch has any commits yet — `git
/// init` makes `HEAD` a symref to the default branch immediately, before
/// the first commit exists, so this works the same before and after the
/// root-commit case `undo_commit` needs it for.
fn current_branch_ref(repo_root: &Path) -> Option<String> {
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
fn git_dir(repo_root: &Path) -> Option<PathBuf> {
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

/// Refuses to sync while the repository is in a state where an automatic
/// commit would be actively harmful: a merge/cherry-pick/revert/bisect/
/// rebase in progress (each recorded by git as a marker file or directory
/// under the git-dir), a detached `HEAD` (a commit made there is one `git
/// checkout` away from becoming unreachable, with no branch pointing at
/// it), or an unresolved conflict anywhere in the repository (not just the
/// target path — any of these mean `HEAD`/the index currently represent
/// in-progress work a background checklist commit must never interfere
/// with, silently "resolving" a conflict or completing someone else's
/// merge). Checked at the top of `run_sync`, before any other work, and
/// again by `commit_via_temp_index` immediately before the actual `git
/// commit` call — a snapshot check either time, not a held lock, so one of
/// these markers could still appear in the (now much narrower) gap between
/// the second check and `git commit` itself actually running; see
/// `commit_via_temp_index`'s doc comment for why that residual window is
/// accepted rather than eliminated.
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
    let mut symbolic_ref_cmd = git_command(repo_root);
    symbolic_ref_cmd.args(["symbolic-ref", "-q", "HEAD"]);
    let on_a_branch = run_with_timeout(symbolic_ref_cmd, PLUMBING_TIMEOUT)
        .is_ok_and(|output| output.status.success());
    if !on_a_branch {
        return Some("git-sync: repository is in a detached HEAD state".to_string());
    }
    let mut ls_files_cmd = git_command(repo_root);
    ls_files_cmd.args(["ls-files", "-u"]);
    let unmerged = run_with_timeout(ls_files_cmd, PLUMBING_TIMEOUT);
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
fn index_entry(repo_dir: &Path, file_path: &Path) -> Result<Option<(String, String)>, String> {
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
fn blob_bytes_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<Vec<u8>> {
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
fn blob_at(repo_root: &Path, commit: &str, relpath: &str) -> Option<String> {
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
fn descends_directly_from(repo_root: &Path, commit: &str, parent: &Option<String>) -> bool {
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
/// tree or index, returning its blob SHA.
fn hash_object(repo_root: &Path, content: &str) -> Result<String, String> {
    let mut cmd = git_command(repo_root);
    cmd.args(["hash-object", "-w", "--stdin"]);
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
fn current_head(repo_root: &Path) -> Option<String> {
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
fn resolve_parent(repo_root: &Path) -> Result<Option<String>, String> {
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
    const HOOK_RACE_TIMEOUT: Duration = Duration::from_secs(2);

    /// Sleep for a hook that must outlive `HOOK_RACE_TIMEOUT`. The process
    /// group is killed on timeout, so this never actually elapses.
    const HOOK_RACE_SLEEP: &str = "30";

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git command failed to run");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// Trimmed stdout of a `git` command, for asserting on repository state.
    fn git_stdout(dir: &Path, args: &[&str]) -> String {
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
    fn init_repo_with_remote_on_a_topic_branch() -> (PathBuf, PathBuf, String) {
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

    /// Like `init_repo_with_remote`, but the tracked file is named
    /// `file_name` instead of the fixed `tracked.md` — for exercising the
    /// git plumbing path (`index_entry`/`blob_bytes_at`/`stage_into_temp_index`)
    /// against unusual filenames.
    fn init_repo_with_remote_named(file_name: &str) -> (PathBuf, PathBuf) {
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

    /// A `latest_requested_hash` for a direct `run_sync` call representing
    /// "nobody else has requested anything since" — i.e. the ordinary,
    /// non-racing case most tests want, where the only known request is
    /// this call's own `content`.
    fn no_race(content: &str) -> Mutex<[u8; 32]> {
        Mutex::new(hash_bytes(content.as_bytes()))
    }

    /// The pre-write content hash for a test whose staged checklist matches
    /// the content being synced — the ordinary "staged, nothing lost" case,
    /// which the guard must let through.
    fn staged_matches(content: &str) -> Option<[u8; 32]> {
        Some(hash_bytes(content.as_bytes()))
    }

    fn pending_sync(content: &str, description: &str) -> PendingSync {
        PendingSync {
            content: content.to_string(),
            content_hash: hash_bytes(content.as_bytes()),
            // These tests drive `GitSync`'s request/coalescing machinery, not
            // the staged-target guard; none of them stage the checklist, so
            // the guard short-circuits before this is consulted.
            previous_content_hash: None,
            description: description.to_string(),
        }
    }

    #[test]
    fn detect_finds_a_repo() {
        let work = init_repo_without_remote();
        assert!(GitSync::detect(&work.join("tracked.md")).is_some());
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_poisoned_latest_hash_does_not_take_down_the_main_thread() {
        // Deep review: `request` runs on the main (TUI) thread and takes the
        // same lock a background worker holds. An `unwrap()` there turns any
        // worker panic into a TUI crash, several frames later and with no
        // connection to what the user was doing. The guarded value is a
        // plain digest with no invariant to protect, so a poisoned lock is
        // recovered rather than propagated.
        let work = init_repo_without_remote();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let hash = Arc::clone(&sync.latest_requested_hash);
        let poisoner = std::thread::spawn(move || {
            let _guard = hash.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(poisoner.join().is_err(), "the poisoning thread must panic");
        assert!(sync.latest_requested_hash.is_poisoned());

        // Must not panic, and must still record the request.
        sync.request(pending_sync("- [x] one\n", "after poisoning"));
        assert_eq!(
            *lock_hash(&sync.latest_requested_hash),
            hash_bytes(b"- [x] one\n")
        );

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
            run_sync(
                &work,
                &untracked,
                "- [ ] new\n",
                "should not commit",
                &no_race("- [ ] new\n"),
                None,
            ),
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
            run_sync(
                &work,
                &work.join("tracked.md"),
                "- [ ] one\n",
                "no changes",
                &no_race("- [ ] one\n"),
                None,
            ),
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
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
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
    fn run_sync_leaves_the_real_index_matching_head_after_a_commit() {
        // External review, round 9: reproduced empirically against a
        // freshly cloned repo with no staged content at all -- before this
        // fix, the temp-index commit advanced HEAD for `tracked.md` without
        // ever advancing the real index's own entry for it, so `git
        // status` permanently showed the file as both staged (stale index
        // vs. new HEAD) and not staged (current working tree vs. that same
        // stale index), starting from the very first sync.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let status = Command::new("git")
            .current_dir(&work)
            .args(["status", "--porcelain", "--", "tracked.md"])
            .output()
            .unwrap();
        assert!(
            status.stdout.is_empty(),
            "real index must match HEAD and the working tree after a commit: {:?}",
            String::from_utf8_lossy(&status.stdout)
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
            &no_race("- [x] one\n"),
            None,
        );
        // The commit itself succeeded (nothing was lost); only the push
        // failed, which is why this is `CommittedNotPushed` rather than
        // `Failed` — see the `SyncOutcome` doc comments.
        assert!(matches!(outcome, SyncOutcome::CommittedNotPushed { .. }));

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
    fn commit_via_temp_index_refuses_when_repo_becomes_blocked_after_the_head_check() {
        // External review: repo_sync_blocked's check in run_sync is only a
        // snapshot taken before any of the temp-index work below it runs;
        // a merge/rebase/etc. starting afterward but before the actual
        // `git commit` call would otherwise slip through undetected. This
        // exercises commit_via_temp_index's own re-check directly (a
        // marker present at call time), which narrows — not eliminates,
        // same as the HEAD race above — that window.
        let work = init_repo_without_remote();
        let parent = current_head(&work);
        fs::write(work.join(".git").join("MERGE_HEAD"), "deadbeef\n").unwrap();

        let blob = hash_object(&work, "- [x] one\n").unwrap();
        let result = commit_via_temp_index(
            &work,
            &parent,
            "100644",
            &blob,
            "tracked.md",
            "should not commit",
        );
        assert!(
            result.as_ref().is_err_and(|e| e.contains("merge")),
            "{result:?}"
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
    fn verify_commit_scope_checks_the_captured_commit_not_whatever_head_is_now() {
        // External review, round 4: the old implementation re-resolved
        // `current_head` inside verify_commit_scope itself, so a commit
        // landing after the one being verified (but before this function
        // ran) could be mistaken for part of it. Passing the exact SHA in
        // means a concurrent commit on top must be ignored entirely,
        // whatever it touches.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        // Stands in for markcheck's own commit: touches only tracked.md.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        // A concurrent commit lands on top, touching an unrelated file —
        // if verify_commit_scope re-resolved HEAD instead of using
        // `created_commit`, this would wrongly look like markcheck's own
        // hook had modified `other.md`.
        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "must never touch the concurrent commit sitting on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn verify_commit_scope_names_the_stray_commit_when_the_undo_itself_fails() {
        // The most alarming message in this module -- an out-of-scope commit
        // that could *not* be rolled back, so it's still sitting on the
        // branch and named for manual cleanup -- and it had no test. Forced
        // deterministically by making `undo_commit`'s compare-and-swap fail:
        // an unrelated commit lands on top of the one being verified, so
        // update-ref refuses (correctly -- rewinding would take the
        // concurrent commit with it), and verify_commit_scope has to report
        // a violation it couldn't undo rather than one it did.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        // Stands in for a commit a hook expanded beyond the checklist.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        fs::write(work.join("other.md"), "hook-added\n").unwrap();
        run(&work, &["add", "tracked.md", "other.md"]);
        run(&work, &["commit", "-q", "-m", "checklist plus hook spill"]);
        let created_commit = current_head(&work).unwrap();

        // Something else commits on top, so the CAS-guarded undo must refuse.
        fs::write(work.join("third.md"), "concurrent\n").unwrap();
        run(&work, &["add", "third.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &created_commit);

        let err = result.expect_err("an out-of-scope commit must be reported");
        assert!(
            err.contains("undoing the commit failed"),
            "must say the rollback itself failed: {err}"
        );
        assert!(
            err.contains(&created_commit),
            "must name the stray commit for manual cleanup: {err}"
        );
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "nothing may be rewound when the undo refuses"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn undo_commit_refuses_to_rewind_a_commit_that_landed_after_the_one_being_undone() {
        // External review, round 4: the old undo_commit did a bare
        // `update-ref HEAD <parent>` with no compare-and-swap, so a commit
        // landing on top of the one being undone (between the commit and
        // the undo) would be silently rewound along with it. The
        // CAS-guarded update-ref must refuse instead.
        let work = init_repo_without_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "to be undone"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = undo_commit(&work, &created_commit);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "the concurrent commit must not be rewound"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn undo_commit_refuses_to_delete_a_root_commit_that_already_has_a_child() {
        // Same CAS guarantee as the test above, for the root-commit
        // (`parent: None`) undo path, which deletes the branch ref outright
        // instead of moving it.
        let work = unique_dir("repo-root-commit-cas").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        run(&work, &["commit", "-q", "-m", "root commit to be undone"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);
        let concurrent_commit = current_head(&work).unwrap();

        let result = undo_commit(&work, &created_commit);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(concurrent_commit),
            "the branch ref must not be deleted out from under the concurrent commit"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn push_if_head_unchanged_refuses_to_push_a_commit_that_landed_after_verification() {
        // External review, round 4: the unrelated-history guard
        // only ever runs as a snapshot before markcheck's own commit — a
        // commit landing on the branch after that commit was made and
        // verified, but before the push actually runs, would otherwise get
        // pushed right alongside it, unnoticed.
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "concurrent\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "concurrent change"]);

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, commit }
                if message.contains("repository changed after commit") && commit == &created_commit),
            "{outcome:?}"
        );

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote_log.stdout).trim(),
            "init",
            "neither the markcheck commit nor the concurrent one must reach the remote"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn push_if_head_unchanged_pushes_normally_when_nothing_raced_it() {
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "markcheck commit"]);
        let created_commit = current_head(&work).unwrap();

        let outcome = push_if_head_unchanged(&work, &work, &created_commit);
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&remote_log.stdout).contains("markcheck commit"),
            "{}",
            String::from_utf8_lossy(&remote_log.stdout)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn push_refuses_an_empty_commit_instead_of_deleting_the_remote_branch() {
        // Deep review, critical finding. `<remote> :<branch>` is git's
        // *delete a remote ref* refspec, so an empty `expected_commit`
        // formatted into `{expected_commit}:{branch_ref}` doesn't fail — it
        // deletes the branch and exits 0, which `push` would then report as
        // `Synced`. Reproduced directly against git before the fix. The
        // reachable route was `run_sync`'s fast path, which used to write
        // `current_head(...).unwrap_or_default()`; this test guards `push`
        // itself, so no future caller can reintroduce it from a different
        // direction.
        let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();

        let outcome = push(&work, "");

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unresolved commit")),
            "{outcome:?}"
        );
        let branches = Command::new("git")
            .current_dir(&remote)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&branches.stdout);
        assert!(
            branches.lines().any(|b| b == branch),
            "the remote branch must still exist: {branches:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_commits_and_pushes_on_a_non_default_branch() {
        // Every other git-sync test runs on `main`, which a bare remote
        // refuses to delete out from under itself — masking any refspec
        // mistake. A topic branch has no such protection and is the shape
        // of every real branch this project's own workflow uses.
        let (work, remote, branch) = init_repo_with_remote_on_a_topic_branch();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");

        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
            SyncOutcome::Synced
        );

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", &format!("{branch}:tracked.md")])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&show.stdout),
            "- [x] one\n",
            "the topic branch must carry the change, not be deleted or left behind"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_never_publishes_unrelated_work_when_no_upstream_is_configured() {
        // Deep review, reproduced empirically. Two fail-safe decisions that
        // are each locally reasonable compose into the exact hazard
        // `unpushed_history` exists to prevent:
        //
        //   * that guard resolves `@{u}..HEAD`, which *fails* with no
        //     upstream configured, and it deliberately fails closed
        //     ("don't refuse") so a fresh repo's first commit isn't blocked;
        //   * `push` used to fall back to a bare `git push` for the same
        //     "no upstream" case, justified on the premise that a bare push
        //     reports a clear error.
        //
        // With `push.default = current` a bare push resolves a destination
        // and succeeds, so both guards were off at once and an unrelated
        // local commit rode out with the checklist change. `push` now
        // refuses instead.
        let root = unique_dir("repo-no-upstream");
        let remote = root.join("remote.git");
        let work = root.join("work");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&work).unwrap();
        run(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);
        run(&work, &["config", "push.default", "current"]);
        fs::write(work.join("tracked.md"), "- [ ] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        run(&work, &["commit", "-q", "-m", "init"]);
        // A remote, but deliberately no `-u`: `branch.main.remote` and
        // `branch.main.merge` stay unset, so `upstream_parts` is `None` and
        // `@{u}` can't be resolved.
        run(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        assert!(
            upstream_parts(&work).is_none(),
            "test setup: no upstream must be configured"
        );

        // The user's own unrelated work-in-progress commit.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::CommittedNotPushed { message, .. }
                if message.contains("no upstream configured")),
            "{outcome:?}"
        );
        // Nothing reached the remote at all -- it has no branches yet.
        let branches = Command::new("git")
            .current_dir(&remote)
            .args(["branch", "--format=%(refname:short)"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the unrelated commit must never reach the remote: {:?}",
            String::from_utf8_lossy(&branches.stdout)
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn push_never_sends_more_than_the_explicitly_targeted_commit() {
        // External review, round 5: push_if_head_unchanged/retry_commit
        // both verify HEAD == expected_commit immediately before calling
        // push, but that check and the push itself used to be two separate
        // steps regardless -- a commit landing in between would still ride
        // along, since a bare `git push` sends whatever the local branch
        // tip has become by push-time. Proving the fix directly (push an
        // older commit explicitly, with a newer one already sitting on top
        // locally) is far more deterministic than trying to time a race
        // into that now-vanishingly-small window -- the same reasoning
        // round 4's own verify_commit_scope/undo_commit tests already use.
        let (work, remote) = init_repo_with_remote();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "older commit"]);
        let older_commit = current_head(&work).unwrap();

        fs::write(work.join("other.md"), "newer\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "newer commit"]);

        let outcome = push(&work, &older_commit);
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            !subjects.contains("newer commit"),
            "must never publish anything beyond the targeted commit: {subjects}"
        );
        assert!(subjects.contains("older commit"), "{subjects}");

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
        sync.request(pending_sync("- [x] one\n", "first"));
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        sync.request(pending_sync("- [x] one\n- [x] two\n", "second"));
        fs::write(
            work.join("tracked.md"),
            "- [x] one\n- [x] two\n- [x] three\n",
        )
        .unwrap();
        sync.request(pending_sync("- [x] one\n- [x] two\n- [x] three\n", "third"));
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
            matches!(
                outcomes.first(),
                Some(SyncOutcome::CommittedNotPushed { .. })
            ),
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
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((commit, last_attempt));

        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL - Duration::from_millis(1));

        assert!(!sync.busy, "backoff interval hasn't elapsed yet");
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_noops_while_busy() {
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.busy = true;
        sync.retry = Some((commit, Instant::now() - PUSH_RETRY_INTERVAL));

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

        // A prior sync attempt already committed locally but failed to push
        // -- simulate that directly (a plain commit, not via run_sync) to
        // keep this test focused on retry_push_if_due's own timing logic,
        // not commit creation.
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let commit = current_head(&work).unwrap();

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((commit, last_attempt));

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
            matches!(outcome, Some(SyncOutcome::CommittedNotPushed { .. })),
            "still no remote to push to, so this retry fails the same way: {outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_push_if_due_clears_the_retry_when_head_has_moved_on() {
        // External review, round 7: retry_commit correctly abandons a
        // stale retry (Skipped) when HEAD has moved past the commit being
        // retried, but poll() used to leave `retry` armed regardless --
        // its Instant never refreshed, so the backoff check stays
        // permanently elapsed and a fresh (equally-abandoned) attempt
        // would otherwise fire on every subsequent call, not just every
        // PUSH_RETRY_INTERVAL. Confirms both halves: the retry state is
        // actually cleared, and a follow-up call is a genuine no-op.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let stale_commit = current_head(&work).unwrap();

        // Something else supersedes it before the retry fires.
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();
        run(&work, &["commit", "-q", "-am", "newer commit"]);

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now();
        sync.retry = Some((stale_commit, last_attempt));

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
        assert_eq!(outcome, Some(SyncOutcome::RetryAbandoned), "{outcome:?}");
        assert!(
            sync.retry.is_none(),
            "the stale retry must be cleared, not left armed forever"
        );

        // And retry_push_if_due is now a genuine no-op -- no second spawn.
        sync.retry_push_if_due(last_attempt + PUSH_RETRY_INTERVAL * 2);
        assert!(!sync.busy, "nothing left to retry");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn apply_outcome_covers_every_variant_s_effect_on_the_retry_state() {
        // `apply_outcome` is the single writer of `self.retry` and is a
        // total match on purpose. This is the matrix that pins each arm --
        // `Failed` being the cell that was missing, and the reason a
        // permanently-failing retry used to fire on every frame.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let armed = |sync: &mut GitSync, at: Instant| sync.retry = Some((commit.clone(), at));

        // Synced: nothing left to push.
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::Synced);
        assert!(sync.retry.is_none(), "Synced clears the retry");

        // RetryAbandoned: that specific commit is superseded.
        armed(&mut sync, Instant::now());
        sync.apply_outcome(&SyncOutcome::RetryAbandoned);
        assert!(sync.retry.is_none(), "an abandoned retry clears it");

        // CommittedNotPushed: (re)arms, with the backoff restarted.
        let old = Instant::now() - PUSH_RETRY_INTERVAL * 4;
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::CommittedNotPushed {
            message: "nope".to_string(),
            commit: commit.clone(),
        });
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > old, "CommittedNotPushed restarts the backoff");

        // RetryFailed: the retry's *own* failure. Stays armed (the commit is
        // still unpushed) and restarts the backoff, so it can't fire again on
        // the very next frame.
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::RetryFailed("boom".to_string()));
        let (_, at) = sync
            .retry
            .clone()
            .expect("RetryFailed keeps the retry armed");
        assert!(
            at > old,
            "RetryFailed must restart the backoff, not leave it stale"
        );

        // Failed: a *content* sync's failure. It says nothing about an armed
        // retry for an earlier commit, so the backoff must be left alone --
        // restarting it here let repeated unrelated failures starve a
        // perfectly pushable commit.
        armed(&mut sync, old);
        sync.apply_outcome(&SyncOutcome::Failed("boom".to_string()));
        let (_, at) = sync.retry.clone().expect("Failed keeps the retry armed");
        assert_eq!(at, old, "Failed must leave the retry's backoff alone");

        // Ordinary no-ops say nothing about an unrelated armed retry.
        for outcome in [SyncOutcome::Skipped, SyncOutcome::SkippedUntracked] {
            armed(&mut sync, old);
            sync.apply_outcome(&outcome);
            let (_, at) = sync.retry.clone().expect("left armed");
            assert_eq!(at, old, "{outcome:?} must leave an unrelated retry alone");
        }

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_unrelated_content_failure_does_not_starve_an_armed_retry() {
        // Deep round 1. `apply_outcome`'s `Failed` arm restarted the backoff
        // for *any* failure, but only a failure of the retry itself says
        // anything about the retry. A content sync that keeps failing for
        // its own reasons — an unrelated-commits refusal on every toggle,
        // say — pushed the armed retry's clock forward each time, so a
        // commit that was perfectly pushable never got its turn.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();

        let due = Instant::now() - PUSH_RETRY_INTERVAL * 4;
        sync.retry = Some((commit.clone(), due));
        sync.apply_outcome(&SyncOutcome::Failed("unrelated content sync".to_string()));

        let (_, at) = sync.retry.clone().expect("still armed");
        assert_eq!(
            at, due,
            "a content sync's failure must leave the retry's backoff alone"
        );

        // The retry's *own* failure is the one that must restart it, or a
        // permanently failing retry fires on every frame.
        sync.retry = Some((commit, due));
        sync.apply_outcome(&SyncOutcome::RetryFailed("retry blew up".to_string()));
        let (_, at) = sync.retry.clone().expect("still armed");
        assert!(at > due, "a failed retry restarts its own backoff");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_failed_retry_does_not_fire_again_on_the_very_next_frame() {
        // Deep review, round 2, reproduced against the real binary: once the
        // repository becomes unresolvable, `retry_commit` returns `Failed`.
        // That arm used to leave the retry's timestamp untouched, so the
        // backoff check stayed permanently elapsed and a retry spawned on
        // every ~100ms event-loop frame -- 98 `git` subprocesses in 10
        // seconds, indefinitely, with no outcome able to break the loop.
        let work = init_repo_without_remote();
        let commit = current_head(&work).unwrap();
        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        let last_attempt = Instant::now() - PUSH_RETRY_INTERVAL * 2;
        sync.retry = Some((commit, last_attempt));

        // A retry comes due and fails (the repo is gone).
        sync.retry_push_if_due(Instant::now());
        assert!(sync.busy, "a retry should have spawned");
        sync.busy = false;
        // `spawn_retry` maps a retry's failure to `RetryFailed`, which is
        // the arm that owns the backoff restart.
        sync.apply_outcome(&SyncOutcome::RetryFailed(
            "git rev-parse: not a git repository".to_string(),
        ));

        // The whole point: the next frame must be a no-op, not another spawn.
        sync.retry_push_if_due(Instant::now());
        assert!(
            !sync.busy,
            "a failed retry must respect the backoff instead of firing every frame"
        );
        assert!(
            sync.retry.is_some(),
            "the commit is still unpushed, so the retry stays armed for later"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_commit_abandons_silently_when_head_has_moved_on() {
        // The actual regression test for the stale-retry bug: a retry must
        // never rebuild a commit from old content when something else has
        // moved HEAD past the commit it was trying to push -- it must give
        // up on this specific retry instead, leaving whatever superseded it
        // alone.
        let work = init_repo_without_remote();
        let file_path = work.join("tracked.md");
        let stale_commit = current_head(&work).unwrap();

        // Something else advances HEAD past the commit this retry is stale
        // for -- standing in for a concurrent markcheck/git-sync process,
        // or any other actor, committing in between.
        fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
        run(&work, &["commit", "-q", "-am", "newer content"]);
        let newer_commit = current_head(&work).unwrap();
        assert_ne!(stale_commit, newer_commit);

        let outcome = retry_commit(&work, &stale_commit);

        assert_eq!(outcome, SyncOutcome::RetryAbandoned);
        assert_eq!(
            current_head(&work).unwrap(),
            newer_commit,
            "the newer commit must be left completely alone"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n- [x] two\n",
            "no stale commit was rebuilt on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn retry_commit_pushes_when_head_still_matches() {
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);
        let commit = current_head(&work).unwrap();

        let outcome = retry_commit(&work, &commit);

        assert_eq!(outcome, SyncOutcome::Synced);
        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] one\n");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn request_and_poll_roundtrip_reports_synced() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();

        let mut sync = GitSync::detect(&work.join("tracked.md")).unwrap();
        sync.request(pending_sync("- [x] one\n", "Check \"one\""));

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

        sync.request(pending_sync("- [x] one\n", "first"));
        assert!(sync.busy, "first request should mark the worker busy");
        // A second `request` while busy doesn't spawn a second concurrent
        // thread (two `git commit`/`push` runs on the same repo could race
        // on the index/HEAD) — it queues instead.
        sync.request(pending_sync("- [x] one\n", "second"));
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
    fn run_sync_refuses_to_commit_content_that_went_stale_before_the_worker_ran() {
        // External review, round 5: an earlier version of this test asserted
        // the opposite of what's checked here — that committing exactly the
        // queued snapshot, even once the working tree has more written to
        // it, was correct, on the theory that "a later sync picks up the
        // rest." That's false whenever nothing ever triggers a later sync —
        // a plain file-watcher reload (unlike the explicit `e`-editor
        // return path) never queues one, on purpose (see "Isolated
        // commits" below). A request that's gone stale must now be
        // refused rather than published, without ever absorbing B into
        // this commit either.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"A\"");

        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();

        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] A\n",
            &message,
            &no_race("- [x] A\n"),
            None,
        );
        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("file changed since this request was queued")),
            "{outcome:?}"
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "init",
            "nothing must be committed at all -- neither the stale snapshot nor a mix with B"
        );

        // B is untouched on disk -- refusing never resurrects, reverts, or
        // otherwise rewrites the working tree.
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] A\n- [x] B\n"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_to_recommit_a_file_deleted_after_the_request_was_queued() {
        // External review, round 5: the same staleness gap, but for a
        // deletion -- git status still sees a deleted-in-worktree tracked
        // file (not `??`, not empty) as something to proceed on, so without
        // this check the stale queued content would be committed via the
        // temp index regardless, resurrecting a file the user just deleted
        // locally.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        let message = commit_message(&file_path, "Check \"one\"");

        fs::remove_file(&file_path).unwrap();

        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );
        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("file changed since this request was queued")),
            "{outcome:?}"
        );

        let log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "init",
            "the deleted file must not be resurrected in a commit"
        );
        assert!(!file_path.exists(), "the deletion must be left alone");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_recovers_on_the_next_request_after_a_stale_refusal() {
        // A refusal must be a one-shot skip of this specific stale request,
        // not a stuck state -- the very next sync, carrying the file's
        // actual current content, must succeed normally.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");

        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
        let stale_message = commit_message(&file_path, "Check \"A\"");
        assert!(matches!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n",
                &stale_message,
                &no_race("- [x] A\n"),
                None,
            ),
            SyncOutcome::Failed(_)
        ));

        let fresh_message = commit_message(&file_path, "Edited in vim");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] A\n- [x] B\n",
            &fresh_message,
            &no_race("- [x] A\n- [x] B\n"),
            None,
        );
        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");

        let show = Command::new("git")
            .current_dir(&remote)
            .args(["show", "HEAD:tracked.md"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout), "- [x] A\n- [x] B\n");

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
            run_sync(
                &work,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
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

    #[cfg(unix)]
    #[test]
    fn run_sync_undoes_the_commit_when_a_hook_stages_extra_files() {
        // External review: the temp-index rewrite closes the *staged
        // file* contamination path (the test above), but a normal `git
        // commit` still runs the repository's commit hooks, and a hook
        // that itself runs `git add` inherits the same `GIT_INDEX_FILE` --
        // so it can stage extra files into the *temporary* index instead,
        // defeating the same guarantee through a different door. This is
        // that door: a real, executable `pre-commit` hook that stages an
        // unrelated file, verifying markcheck detects it and undoes the
        // whole commit rather than letting it stand (let alone push it).
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("hook modified files beyond the checklist")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            parent,
            "the commit must be fully undone, not just refused going forward"
        );
        let head_has_other = Command::new("git")
            .current_dir(&work)
            .args(["cat-file", "-e", "HEAD:other.md"])
            .status()
            .unwrap()
            .success();
        assert!(
            !head_has_other,
            "the hook-staged file must never reach a reachable commit"
        );
        let real_index = Command::new("git")
            .current_dir(&work)
            .args(["ls-files", "other.md"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&real_index.stdout).is_empty(),
            "the real index must stay untouched by the hook too"
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

    #[cfg(unix)]
    #[test]
    fn run_sync_undoes_a_hook_violated_root_commit_by_deleting_the_branch_ref() {
        // The root-commit case (parent: None) needs its own undo path --
        // there's no prior SHA to move the branch ref back to, so
        // undo_commit must resolve the branch's symref and delete it
        // entirely instead, returning to the pre-commit "no commits yet"
        // state.
        use std::os::unix::fs::PermissionsExt;

        let work = unique_dir("repo-root-commit-hook").join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q", "-b", "main"]);
        run(&work, &["config", "user.email", "test@example.com"]);
        run(&work, &["config", "user.name", "test"]);
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            "#!/bin/sh\necho unrelated > other.md\ngit add other.md\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        // Staged (so index_entry can read it) but never committed -- a
        // fresh repo's first sync, with no HEAD yet at all.
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        run(&work, &["add", "tracked.md"]);

        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            staged_matches("- [x] one\n"),
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("hook modified files beyond the checklist")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work),
            None,
            "the repository must be back to having no commits at all"
        );
        assert_eq!(
            current_branch_ref(&work).as_deref(),
            Some("refs/heads/main"),
            "HEAD must still symbolically resolve to the branch, just with no commit on it yet"
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
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] resolved\n",
            "should not commit",
            &no_race("- [x] resolved\n"),
            None,
        );
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
        // The disk already holds "A\nB\n" before either sync runs, standing
        // in for a rapid double-toggle: the first request (content "A") was
        // queued, then a second toggle wrote "A\nB\n" and queued its own
        // request before the first one's worker got scheduled. That second
        // request is why `latest` is set to "A\nB\n" for both calls here --
        // git-sync already knows about it (it's `pending`, about to be
        // spawned the moment the first finishes), so the first request's
        // own staleness check must not refuse just because disk has moved
        // past its own content.
        let (work, remote) = init_repo_with_remote();
        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] A\n- [x] B\n").unwrap();
        let latest = no_race("- [x] A\n- [x] B\n");

        let first_message = commit_message(&file_path, "Check \"A\"");
        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n",
                &first_message,
                &latest,
                None
            ),
            SyncOutcome::Synced
        );

        let second_message = commit_message(&file_path, "Check \"B\"");
        assert_eq!(
            run_sync(
                &work,
                &file_path,
                "- [x] A\n- [x] B\n",
                &second_message,
                &latest,
                None,
            ),
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
            run_sync(
                &work,
                &file_path,
                "- [ ] one\n",
                "already committed",
                &no_race("- [ ] one\n"),
                None,
            ),
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

        let outcome = run_sync(
            &work,
            &file_path,
            "- [ ] one\n",
            "already committed",
            &no_race("- [ ] one\n"),
            None,
        );
        assert!(
            matches!(outcome, SyncOutcome::CommittedNotPushed { .. }),
            "must attempt (and report failure of) the push, not silently skip: {outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_when_branch_has_unrelated_unpushed_commits() {
        // External review: markcheck's own commit scope is carefully
        // restricted (temp index, hook-scope enforcement), but `git push`
        // sends the whole branch -- so an unrelated local commit made
        // outside markcheck would get published right alongside the
        // checklist change unless sync refuses outright first.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits unrelated to this change")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "no checklist commit should have been created on top"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n",
            "the on-disk edit itself is untouched, just not synced yet"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_the_fast_path_push_when_branch_has_unrelated_unpushed_commits() {
        // External review, round 7: the unrelated-unpushed-commits guard
        // used to run only right before building a *new* commit, on the
        // assumption that the blob_bytes_at-matches fast path is always
        // "markcheck's own prior commit, always legitimate to push." That's
        // false whenever an unrelated commit lands on top without touching
        // the checklist file at all: the checklist's blob at HEAD still
        // matches an older, already-satisfied request's expected_content,
        // so the fast path would push HEAD -- and the unrelated commit
        // riding along with it.
        //
        // Reaching the fast path (rather than the earlier `git status`
        // empty-check, or the new-commit path) needs `expected_content` to
        // match HEAD while the *working tree* currently holds something
        // else -- exactly the fast path's own doc comment scenario ("status
        // is non-empty because of someone else's still-uncommitted
        // change"): a request queued earlier (content "one", now already
        // satisfied at HEAD) executes after both a newer, not-yet-committed
        // edit lands on disk *and* an unrelated commit lands on the branch.
        let (work, remote) = init_repo_with_remote(); // origin/main = A, pushed.

        // A1: a checklist commit that's ahead of upstream (standing in for
        // one whose push failed -- the fast path doesn't care why it's
        // unpushed, only that it is).
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "Check \"one\""]);

        // A2: an unrelated commit on top that never touches the checklist.
        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);
        let head_before = current_head(&work).unwrap();

        // A newer edit sits on disk, uncommitted -- makes `git status`
        // non-empty, so the stale "one"-content request below reaches the
        // fast path instead of the earlier empty-status Skip.
        fs::write(work.join("tracked.md"), "- [x] one\n- [x] two\n").unwrap();

        let file_path = work.join("tracked.md");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            "Catch up a pending push",
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits unrelated to this change")),
            "{outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "nothing should have been committed or pushed"
        );

        let remote_log = Command::new("git")
            .current_dir(&remote)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&remote_log.stdout);
        assert!(
            !subjects.contains("unrelated local commit"),
            "the unrelated commit must never reach the remote: {subjects}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_when_an_unrelated_commit_is_reverted_before_the_checklist_commit() {
        // External review, round 8, empirically confirmed: a plain net
        // tree diff between upstream and HEAD nets an unrelated commit and
        // its own revert out to zero, even though both commits are still
        // in the unpushed range and still get published (they're ancestors
        // of the checklist commit, and an explicit-SHA push sends a
        // commit's whole ancestry regardless of any single commit's net
        // effect on the tree). Reproduced directly: `git diff --name-only`
        // between the pre-unrelated-commit state and HEAD here shows only
        // tracked.md, despite `other.md` genuinely appearing and
        // disappearing in between.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

        fs::write(work.join("other.md"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated local commit"]);

        run(&work, &["rm", "-q", "other.md"]);
        run(
            &work,
            &["commit", "-q", "-m", "revert unrelated local commit"],
        );
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let message = commit_message(&file_path, "Check \"one\"");
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &message,
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits unrelated to this change")),
            "the reverted commit must still count as unrelated history: {outcome:?}"
        );
        assert_eq!(
            current_head(&work).unwrap(),
            head_before,
            "no checklist commit should have been created on top"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn range_has_unrelated_commits_reads_a_whole_batched_range_correctly() {
        // The per-commit `diff-tree` calls are now one batched `--stdin`
        // call, so the parser sees every commit's paths in a single NUL
        // stream instead of one clean response per commit. Exercise it over
        // a range deliberately mixing shapes: checklist-only commits, a
        // multi-file commit, and a root commit (via the `base: None` call
        // shape `verify_commit_scope` uses).
        let work = init_repo_without_remote();

        // A run of checklist-only commits -- nothing unrelated yet.
        let root = current_head(&work).unwrap();
        for n in 1..=5 {
            fs::write(work.join("tracked.md"), format!("- [x] {n}\n")).unwrap();
            run(&work, &["commit", "-q", "-am", &format!("check {n}")]);
        }
        let checklist_only_tip = current_head(&work).unwrap();
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&root), &checklist_only_tip, "tracked.md"),
            Ok(false),
            "a long run of checklist-only commits is not unrelated work"
        );

        // The root commit itself only touched tracked.md, so the full
        // ancestry (base: None, which needs `--root` to show anything) is
        // still clean.
        assert_eq!(
            range_has_unrelated_commits(&work, None, &checklist_only_tip, "tracked.md"),
            Ok(false),
            "the root commit must be diffed, and it only touched the checklist"
        );

        // One commit touching two paths anywhere in the range flips it.
        fs::write(work.join("tracked.md"), "- [x] six\n").unwrap();
        fs::write(work.join("other.md"), "unrelated\n").unwrap();
        run(&work, &["add", "tracked.md", "other.md"]);
        run(&work, &["commit", "-q", "-m", "two files at once"]);
        let mixed_tip = current_head(&work).unwrap();
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&root), &mixed_tip, "tracked.md"),
            Ok(true),
            "a multi-file commit anywhere in the batch must be caught"
        );

        // And an empty range short-circuits before the batched call.
        assert_eq!(
            range_has_unrelated_commits(&work, Some(&mixed_tip), &mixed_tip, "tracked.md"),
            Ok(false),
            "an empty range has nothing to diff"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_refuses_when_a_merge_commit_is_in_the_unpushed_range() {
        // A merge commit's own diff-tree needs a combined-diff
        // interpretation that's genuinely ambiguous to check safely against
        // a single path (conflict resolutions, content changed only during
        // the merge) -- treated as unrelated unconditionally instead, even
        // when the merge itself is trivial and touches nothing but the
        // checklist.
        let (work, _remote) = init_repo_with_remote(); // origin/main = A, pushed.

        run(&work, &["checkout", "-q", "-b", "side"]);
        fs::write(work.join("other.md"), "side\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "side change"]);
        run(&work, &["checkout", "-q", "main"]);
        run(
            &work,
            &["merge", "-q", "--no-ff", "side", "-m", "merge side"],
        );
        let head_before = current_head(&work).unwrap();

        let file_path = work.join("tracked.md");
        fs::write(&file_path, "- [x] one\n").unwrap();
        let outcome = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            "already committed",
            &no_race("- [x] one\n"),
            None,
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("unpushed commits unrelated to this change")),
            "a merge commit in range must refuse, even a clean one: {outcome:?}"
        );
        assert_eq!(current_head(&work).unwrap(), head_before);

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn verify_commit_scope_catches_a_revert_cancelled_unrelated_change() {
        // The same net-diff blind spot as branch_has_unrelated_unpushed_
        // commits, but for verify_commit_scope's own parent..created_commit
        // range -- a hook that made two nested commits where the second
        // reverts the first's unrelated change would net out to zero in a
        // plain diff, even though both extra commits are still in history
        // and still get pushed. Simulated directly here rather than through
        // an actual hook, since the mechanism being tested is
        // range_has_unrelated_commits itself, not hook plumbing.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();

        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist change"]);

        fs::write(work.join("other.md"), "unrelated\n").unwrap();
        run(&work, &["add", "other.md"]);
        run(&work, &["commit", "-q", "-m", "unrelated nested commit"]);

        run(&work, &["rm", "-q", "other.md"]);
        run(
            &work,
            &["commit", "-q", "-m", "revert unrelated nested commit"],
        );
        let created_commit = current_head(&work).unwrap();

        let result =
            verify_commit_scope(&work, &Some(parent.clone()), "tracked.md", &created_commit);
        assert!(
            result.is_err_and(|e| e.contains("hook modified files beyond the checklist")),
            "the revert-cancelled unrelated commit must still be caught"
        );
        // Deep round 3 changed what "undo" means here. It used to reset
        // straight back to `parent`, sweeping away every commit in between —
        // but markcheck did not create those either, and the same reset is
        // what discarded a genuinely concurrent commit that landed in the
        // window before `git commit` (see `undo_commit`). Only markcheck's
        // own commit is rewound now; the hook's nested commits survive, are
        // named in the reported error, and the next sync's unpushed-history
        // guard refuses until the user deals with them.
        let nested = git_stdout(&work, &["rev-parse", &format!("{created_commit}^")]);
        assert_eq!(
            current_head(&work).unwrap(),
            nested,
            "exactly one commit — markcheck's own — is rewound"
        );
        assert_ne!(
            current_head(&work).unwrap(),
            parent,
            "the commits markcheck did not create are deliberately left in place"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_does_not_refuse_for_markchecks_own_stacked_unpushed_commits() {
        // The refusal above must not fire just because markcheck's own
        // earlier commits (from toggling while offline) are what's ahead of
        // upstream -- only genuinely unrelated history should be refused.
        let work = init_repo_without_remote();
        run(&work, &["remote", "add", "origin", "/does/not/exist.git"]);
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let file_path = work.join("tracked.md");

        // First edit: commits locally (via run_sync's own temp-index path),
        // push fails against the unreachable remote -- exactly the
        // "markcheck's own commit is what's ahead" state this must allow.
        fs::write(&file_path, "- [x] one\n").unwrap();
        let first = run_sync(
            &work,
            &file_path,
            "- [x] one\n",
            &commit_message(&file_path, "Check \"one\""),
            &no_race("- [x] one\n"),
            None,
        );
        assert!(matches!(first, SyncOutcome::CommittedNotPushed { .. }));

        // Second, different edit: must still be allowed to commit, not
        // refused just because the branch is now ahead of upstream by
        // markcheck's own first commit.
        fs::write(&file_path, "- [x] one\n- [x] two\n").unwrap();
        let second = run_sync(
            &work,
            &file_path,
            "- [x] one\n- [x] two\n",
            &commit_message(&file_path, "Check \"two\""),
            &no_race("- [x] one\n- [x] two\n"),
            None,
        );
        assert!(
            matches!(second, SyncOutcome::CommittedNotPushed { .. }),
            "must still commit (and only then fail to push), not refuse: {second:?}"
        );
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "- [x] one\n- [x] two\n"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_works_when_the_file_is_in_a_repo_subdirectory() {
        // `repo_dir` (the file's own parent) isn't the repo root here,
        // exercising the plumbing commands' repo-root-relative path
        // handling (`index_entry`/`blob_bytes_at`/`stage_blob`) rather than the
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
            run_sync(
                &sub,
                &file_path,
                "- [x] one\n",
                &message,
                &no_race("- [x] one\n"),
                None,
            ),
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
    fn run_sync_handles_filenames_with_spaces_dashes_colons_and_unicode() {
        // External review: the git plumbing path (`index_entry`/
        // `blob_bytes_at`/`stage_into_temp_index`) was never exercised against
        // anything but a plain `tracked.md`. None of these need `--`
        // protection to matter in practice (every command that takes a
        // pathspec already uses it — see `run_sync`'s own doc comment) but
        // are exactly the filenames most likely to break naive text
        // parsing of `git` output, like the `ls-files --stage` line this
        // module parses in `index_entry`. Confirmed against the pre-fix,
        // newline-delimited version of `index_entry` locally: it actually
        // failed on the *unicode* case first (not embedded-tab/newline as
        // expected) — `git ls-files --stage` C-quotes/escapes non-ASCII
        // bytes by default without `-z`, so `relpath` came back as that
        // quoted-and-escaped literal string rather than the real path,
        // silently committing to the wrong index entry instead of erroring
        // (this sync test's mismatch was `HEAD:<real name>` still showing
        // the old content). The embedded-tab and embedded-newline cases
        // are still worth keeping per the review's own callout: `-z` is
        // what avoids the quoting *and* is what makes a raw newline in the
        // record safe to split on, even though neither case reaches the
        // ambiguous `split_once('\t')` call in a way that would actually
        // break it — see `index_entry`'s doc comment for why a tab in the
        // path is harmless there regardless.
        for file_name in [
            "with space.md",
            "-leading-dash.md",
            "colon:name.md",
            "unicode-\u{2705}\u{65e5}\u{672c}.md",
            "tab\tname.md",
            "newline\nname.md",
        ] {
            let (work, remote) = init_repo_with_remote_named(file_name);
            let file_path = work.join(file_name);
            fs::write(&file_path, "- [x] one\n").unwrap();
            let message = commit_message(&file_path, "Check \"one\"");

            assert_eq!(
                run_sync(
                    &work,
                    &file_path,
                    "- [x] one\n",
                    &message,
                    &no_race("- [x] one\n"),
                    None,
                ),
                SyncOutcome::Synced,
                "file name: {file_name:?}"
            );

            let show = Command::new("git")
                .current_dir(&remote)
                .args(["show", &format!("HEAD:{file_name}")])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&show.stdout),
                "- [x] one\n",
                "file name: {file_name:?}"
            );

            fs::remove_dir_all(work.parent().unwrap()).ok();
        }
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
        let blob = hash_object(&work, "- [ ] one\n").unwrap();
        populate_temp_index(&work, &temp_index, &head).unwrap();
        assert!(
            commit_temp_index(
                &work,
                &temp_index,
                "empty",
                &Some(head.clone()),
                "tracked.md",
                &blob,
                PLUMBING_TIMEOUT
            )
            .is_err()
        );
        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn commit_temp_index_reconciles_when_a_post_commit_hook_outlives_the_timeout() {
        // External review, round 4: a `post-commit` hook only ever runs
        // *after* the ref update is already durable. If `run_with_timeout`
        // kills the process group while that hook is still running, the
        // commit itself already succeeded — reporting it as failed would be
        // wrong. A short injected timeout plus a hook that outlives it
        // reproduces this deterministically, without needing a real
        // multi-second wait against `PLUMBING_TIMEOUT` itself.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-timeout");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "reconciled despite timeout",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        let new_head = current_head(&work);
        assert!(
            new_head.is_some() && new_head != Some(parent),
            "the commit itself must have actually landed: {new_head:?}"
        );
        assert_eq!(
            result.as_deref().ok(),
            new_head.as_deref(),
            "a timeout after the ref already moved must report success, not a false failure: {result:?}"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn commit_temp_index_still_fails_when_the_commit_never_lands() {
        // Companion to the test above: a `pre-commit` hook runs *before*
        // the ref update, so a timeout while it's still sleeping means the
        // commit genuinely never happened — this must still report a
        // failure, not paper over a real one.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(&hook_path, "#!/bin/sh\nsleep 2\n").unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-never-lands");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not land",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            Duration::from_millis(300),
        );
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            current_head(&work),
            Some(parent),
            "the commit must never have happened"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_an_unrelated_commit_after_a_timeout() {
        // External review, round 9: HEAD moving past `parent` after a
        // timeout used to be treated as proof "our commit succeeded" --
        // but HEAD can move because something else entirely committed
        // during the same window, not because our own git commit actually
        // landed. A pre-commit hook that (after unsetting the inherited
        // GIT_INDEX_FILE, standing in for a genuinely separate process
        // rather than our own temp-index machinery) commits something
        // unrelated to the *real* branch, then sleeps past the timeout,
        // means our own temp-index commit never actually happens (blocked
        // in pre-commit) -- HEAD moves, but not because of us.
        //
        // `--no-verify` on the nested commit is load-bearing, not tidiness:
        // without it that commit runs `pre-commit` again, which commits
        // again, recursively, so nothing ever lands, HEAD never moves, and
        // the test neither reproduces the scenario nor exercises the
        // ownership check (it would pass against the pre-fix code purely on
        // the older `HEAD != parent` half). Verified by hand: with
        // `--no-verify` HEAD does move to the unrelated commit while
        // `HEAD:tracked.md` stays the *old* blob -- exactly the state the
        // blob check exists to reject.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated concurrent commit'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-unrelated");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "should not adopt the unrelated commit",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        // Asserting the *message*, not just `is_err`, so this can't quietly
        // start passing again for the old reason (the commit never landing
        // at all) rather than for the ownership check rejecting it.
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the unrelated commit as its own: {result:?}"
        );
        let head_after = current_head(&work).unwrap();
        assert_ne!(
            head_after, parent,
            "the unrelated commit must still exist, untouched"
        );
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("unrelated concurrent commit"),
            "{}",
            String::from_utf8_lossy(&log.stdout)
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[cfg(unix)]
    #[test]
    fn commit_temp_index_does_not_adopt_a_commit_that_landed_on_top_of_its_own() {
        // The mirror of the test above, and the case a content check alone
        // cannot catch: *our* commit lands, and an unrelated commit lands on
        // top of it before HEAD is re-resolved. That unrelated commit never
        // touched `relpath`, so `blob_at(HEAD, relpath)` is still exactly
        // our blob -- the blob check passes -- yet HEAD names the wrong
        // commit. Adopting it would hand `verify_commit_scope` a SHA whose
        // range contains the unrelated commit, and `undo_commit`'s
        // compare-and-swap would then succeed and rewind *both* off the
        // branch. The lineage check (`descends_directly_from`) is what
        // rejects it.
        //
        // A `post-commit` hook is the natural driver: it only ever runs once
        // the ref update is already durable, so our own commit really has
        // landed by the time it commits on top. `--no-verify` on the nested
        // commit avoids the hook recursing into itself, and the `read-tree
        // HEAD` first is what makes this the *mirror* case rather than a
        // second copy of the test above: the real index still holds the
        // pre-sync `tracked.md` (nothing has run `align_real_index_entry`
        // at this point), so committing it as-is would revert our blob and
        // the content check would reject the commit for the wrong reason.
        use std::os::unix::fs::PermissionsExt;

        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let hooks_dir = work.join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-commit");
        fs::write(
            &hook_path,
            format!(
                "#!/bin/sh\n\
                 env -u GIT_INDEX_FILE git read-tree HEAD\n\
                 echo unrelated > other.md\n\
                 env -u GIT_INDEX_FILE git add other.md\n\
                 env -u GIT_INDEX_FILE git commit -q --no-verify -m 'unrelated commit on top'\n\
                 sleep {HOOK_RACE_SLEEP}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).unwrap();

        let temp_index = work.join(".git").join("scratch-test-index-on-top");
        populate_temp_index(&work, &temp_index, &parent).unwrap();
        let blob = hash_object(&work, "- [x] one\n").unwrap();
        stage_into_temp_index(&work, &temp_index, "100644", &blob, "tracked.md").unwrap();

        let result = commit_temp_index(
            &work,
            &temp_index,
            "ours, then raced",
            &Some(parent.clone()),
            "tracked.md",
            &blob,
            HOOK_RACE_TIMEOUT,
        );

        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("git commit failed")),
            "must not adopt the commit that landed on top of ours: {result:?}"
        );
        // Both commits are still there, untouched -- refusing must never
        // rewind anything, least of all the unrelated commit.
        let log = Command::new("git")
            .current_dir(&work)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("unrelated commit on top"), "{subjects}");
        assert!(subjects.contains("ours, then raced"), "{subjects}");
        // The blob check alone would have passed here: our content really is
        // what HEAD holds for the checklist path.
        assert_eq!(
            blob_at(&work, "HEAD", "tracked.md").as_deref(),
            Some(blob.as_str()),
            "test setup: the content check must be the one that would pass"
        );

        let _ = fs::remove_file(&temp_index);
        fs::remove_dir_all(work.parent().unwrap()).ok();
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
    fn commit_message_stays_within_the_cap_for_a_very_long_file_name() {
        // Deep review: the file-name prefix used to be kept whole
        // unconditionally, so a long enough name saturated the description
        // budget to 0 and produced `<prefix>…` -- longer than
        // MAX_COMMIT_MESSAGE_LEN, the one thing the constant guarantees.
        for name_len in [MAX_FILE_NAME_LEN - 1, MAX_FILE_NAME_LEN, 79, 100, 200] {
            let name = format!("{}.md", "x".repeat(name_len));
            let message = commit_message(
                Path::new(&format!("/a/b/{name}")),
                "Check \"a task with a reasonably long title\"",
            );
            assert!(
                message.chars().count() <= MAX_COMMIT_MESSAGE_LEN,
                "name_len {name_len}: {} chars: {message:?}",
                message.chars().count()
            );
            assert!(
                message.contains(": "),
                "the prefix separator survives: {message:?}"
            );
        }
    }

    #[test]
    fn commit_message_keeps_an_ordinary_file_name_intact() {
        // The clamp above must only ever engage for pathological names --
        // a normal checklist file name is untouched.
        let message = commit_message(Path::new("/a/b/deployment-runbook.md"), "Check \"x\"");
        assert!(message.starts_with("deployment-runbook.md: "), "{message}");
    }

    #[test]
    fn commit_message_leaves_a_short_description_untruncated() {
        let message = commit_message(Path::new("/a/b/checklist.md"), "Check \"short\"");
        assert_eq!(message, "checklist.md: Check \"short\"");
        assert!(!message.contains('\u{2026}'));
    }

    #[test]
    fn run_with_timeout_returns_normal_output_for_a_fast_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hello"]);
        let output = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn run_with_timeout_reports_a_nonzero_exit_and_captures_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo oops >&2; exit 1"]);
        let output = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(!output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "oops");
    }

    #[test]
    fn run_with_timeout_and_stdin_writes_and_the_command_reads_it_back() {
        let cmd = Command::new("cat");
        let output =
            run_with_timeout_and_stdin(cmd, Duration::from_secs(5), "hello via stdin").unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello via stdin");
    }

    #[test]
    fn run_with_timeout_kills_a_process_that_exceeds_the_deadline() {
        // Proves the child is actually killed, not merely given up on:
        // rather than just timing the call (which only shows the *wait*
        // stopped, not that the process itself was terminated), the child
        // is told to touch a marker file after a delay well past the
        // timeout -- if it's genuinely killed, that marker must never
        // appear even after waiting past when it would have.
        let marker = crate::test_support::unique_temp_path("git-sync-timeout", "marker", None);
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 5 && touch {}",
                shell_words::quote(marker.to_str().unwrap())
            ),
        ]);

        let started = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(150));
        let elapsed = started.elapsed();

        assert!(
            matches!(&result, Err(err) if err.kind() == io::ErrorKind::TimedOut),
            "{result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must return promptly once the deadline passes, not wait for the child: {elapsed:?}"
        );

        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !marker.exists(),
            "the child must have been killed, not merely abandoned -- \
             it was still alive it would have created this marker by now"
        );

        fs::remove_file(&marker).ok();
    }

    #[test]
    fn run_with_timeout_kills_a_process_writing_enough_output_to_fill_a_pipe() {
        // Regression guard for the pipe-deadlock hazard `wait_with_timeout`'s
        // doc comment describes: without draining stdout on its own thread
        // for the whole run, a child writing more than the OS pipe buffer
        // (~64KB on Linux) would block on that write, `try_wait` would
        // never return, and this call would hang regardless of `timeout`.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "yes | head -c 5000000; sleep 5"]);
        let started = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(200));
        assert!(
            matches!(&result, Err(err) if err.kind() == io::ErrorKind::TimedOut),
            "{result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must not deadlock on a full pipe buffer: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_does_not_hang_on_a_descendant_that_outlives_the_direct_child() {
        // External review, round 8: the direct child can exit successfully
        // while a descendant it spawned (e.g. a hook backgrounding a
        // process without closing inherited fds) still holds the piped
        // stdout/stderr open -- read_to_end() in the reader thread then
        // blocks forever waiting for an EOF only the descendant is
        // preventing, and an unconditional `.join()` on that thread hangs
        // right along with it, well past this call's own timeout. `sleep 5
        // &` backgrounds a descendant that inherits the shell's piped
        // stdout and shares its process group (so the fix's group-kill can
        // actually reach it); the shell itself exits 0 almost immediately.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5 & exit 0"]);
        let started = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(150));
        assert!(
            result.is_ok(),
            "the direct child exited successfully: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must not block on a descendant that outlives the direct child: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_and_stdin_does_not_hang_when_a_descendant_holds_stdin() {
        // Deep round 2, the mirror of round 8's stdout/stderr fix on the
        // other pipe. `git` can hand the inherited stdin read end to a
        // descendant; if one holds it open after `git` exits, nothing drains
        // the write and `write_all` never returns. The old unconditional
        // `join()` on that thread then blocked forever — well past this
        // call's own timeout, on the *success* path where no group kill ever
        // came to free it.
        //
        // Getting a descendant to *actually* hold stdin takes care: POSIX
        // shells assign `/dev/null` to a background job's stdin, so the
        // obvious `sleep 5 &` inherits nothing and the write sails through
        // (verified — it does not reproduce). Duplicating the descriptor
        // first and redirecting from the copy is an explicit redirection,
        // which overrides that default; measured blocking the write for the
        // full 5s. The payload is far larger than a pipe buffer so the write
        // genuinely blocks rather than fitting in one go.
        let payload = "x".repeat(1024 * 1024);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec 3<&0; sleep 5 <&3 & exit 0"]);

        let started = Instant::now();
        let result = run_with_timeout_and_stdin(cmd, Duration::from_millis(150), &payload);

        assert!(
            result.is_ok(),
            "the direct child exited successfully: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "must not block on a descendant holding stdin: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_successful_command_does_not_kill_a_descendant_it_no_longer_owns() {
        // Deep round 2. The pipe drain used to kill the process group when a
        // reader hadn't finished within the grace period. But by then the
        // child has always been reaped — `try_wait` on success — so its PID
        // is no longer ours and the OS may have recycled it; the group kill
        // could land on an unrelated process group. It also killed a
        // descendant of a command that had *succeeded*, which markcheck has
        // no business doing.
        //
        // `sleep 3` outlives PIPE_DRAIN_GRACE (2s) while holding the
        // inherited stdout, so it is exactly the case that used to trigger
        // the kill; the marker proves it ran to completion instead.
        let dir = unique_dir("descendant-survives");
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("descendant-finished");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "(sleep 3; touch {}) & exit 0",
            marker.to_str().unwrap()
        ));

        let started = Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(150));
        assert!(result.is_ok(), "the direct child exited successfully");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must stay bounded: {:?}",
            started.elapsed()
        );

        // Give the descendant time to finish on its own.
        std::thread::sleep(Duration::from_secs(4).saturating_sub(started.elapsed()));
        assert!(
            marker.exists(),
            "a descendant of a successful command must not be killed"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_untracked_checklist_is_reported_even_when_git_status_stays_silent() {
        // Deep round 2. `git status --porcelain -- <file>` prints nothing for
        // an untracked file in two ordinary configurations, so the
        // empty-output short-circuit reported `Skipped` and git-sync sat
        // silent forever — the exact "looks like a bug" case
        // `SkippedUntracked` was introduced to surface. Both confirmed
        // against real git before this test was written.
        for (label, configure) in [
            (
                "status.showUntrackedFiles=no",
                &["config", "status.showUntrackedFiles", "no"][..],
            ),
            (
                "gitignored",
                &["config", "core.excludesFile", ".ignore"][..],
            ),
        ] {
            let (work, _remote) = init_repo_with_remote();
            run(&work, configure);
            if label == "gitignored" {
                fs::write(work.join(".ignore"), "untracked.md\n").unwrap();
            }
            let untracked = work.join("untracked.md");
            fs::write(&untracked, "- [ ] new\n").unwrap();

            // Precondition: status really is silent about it here.
            assert_eq!(
                git_stdout(&work, &["status", "--porcelain", "--", "untracked.md"]),
                "",
                "{label}: test setup expects git status to say nothing"
            );

            assert_eq!(
                run_sync(
                    &work,
                    &untracked,
                    "- [ ] new\n",
                    "should not commit",
                    &no_race("- [ ] new\n"),
                    None,
                ),
                SyncOutcome::SkippedUntracked,
                "{label}: an untracked checklist must still be reported"
            );

            fs::remove_dir_all(work.parent().unwrap()).ok();
        }
    }

    /// Installs a hook at `point` for this thread, removing it on drop so a
    /// test can never leak one into whatever runs next on the same thread.
    struct RaceHook;

    impl RaceHook {
        fn at(point: RacePoint, hook: impl Fn() + 'static) -> RaceHook {
            RACE_HOOKS.with(|h| h.borrow_mut().insert(point, Box::new(hook)));
            RaceHook
        }
    }

    impl Drop for RaceHook {
        fn drop(&mut self) {
            RACE_HOOKS.with(|h| h.borrow_mut().clear());
        }
    }

    fn racer_sha_probe(work: &Path) -> String {
        fs::read_to_string(work.join(".racer-sha")).unwrap_or_default()
    }

    /// Commits an unrelated file in `work` — the concurrent writer every
    /// interleaving test below races against.
    fn commit_unrelated(work: &Path, name: &str) -> String {
        fs::write(work.join(name), "unrelated\n").unwrap();
        run(work, &["add", name]);
        run(work, &["commit", "-q", "-m", "unrelated"]);
        git_stdout(work, &["rev-parse", "HEAD"])
    }

    // --- Genuine check-then-race-then-use interleavings (round 3) ---

    #[test]
    fn an_unrelated_commit_landing_after_validation_is_not_published_by_the_fast_path() {
        // The ordering every previous review asked for and none of the tests
        // actually exercised: the guard clears the branch, *then* an
        // unrelated commit lands, *then* the push runs. Before the
        // captured-tip work this published the unrelated commit; the hook
        // makes that ordering deterministic instead of hoping for a race.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        // Commit the content but leave it unpushed, so the branch is ahead
        // of upstream and `HEAD` already holds exactly what is being synced.
        fs::write(&file, content).unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let validated = git_stdout(&work, &["rev-parse", "HEAD"]);
        // A further uncommitted edit keeps `git status` non-empty, which is
        // what carries the request past the nothing-to-do short-circuit and
        // into the already-committed fast path.
        fs::write(&file, "- [x] one\n- [ ] two\n").unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
            commit_unrelated(&racer, "other.txt");
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "main"]),
            validated,
            "the remote must hold exactly the validated tip, not the commit that raced in"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_unrelated_commit_landing_after_validation_is_not_published_by_catch_up() {
        // Same interleaving through the startup path, which runs unprompted
        // and so is the one where publishing something unchecked is worst.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let validated = git_stdout(&work, &["rev-parse", "HEAD"]);

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::AfterHistoryValidation, move || {
            commit_unrelated(&racer, "other.txt");
        });

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["rev-parse", "main"]),
            validated,
            "catch-up must publish only the tip it validated"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_commit_landing_before_ours_is_detected_rather_than_built_on_stale_state() {
        // HEAD moves between the guards and the commit itself. The temp
        // index was seeded from the old parent, so committing anyway would
        // produce a tree that silently reverts the commit that raced in.
        let (work, _remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        fs::write(&file, content).unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
            let sha = commit_unrelated(&racer, "other.txt");
            fs::write(racer.join(".racer-sha"), &sha).unwrap();
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        // The commit is aborted either way; what matters is that the commit
        // markcheck did *not* make is still on the branch afterwards.
        assert!(
            matches!(&outcome, SyncOutcome::Failed(_)),
            "the sync must not succeed on a superseded parent: {outcome:?}"
        );
        let unrelated = racer_sha_probe(&work);
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "unrelated\ninit",
            "the commit that raced in must still be on the branch"
        );
        assert_eq!(
            git_stdout(&work, &["rev-parse", "HEAD"]),
            unrelated,
            "HEAD must be the concurrent commit, not rewound past it"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_version_staged_during_the_sync_survives_index_alignment() {
        // Round 10's guard, finally tested as the race it defends against
        // rather than by pre-staging before the call.
        let (work, _remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let content = "- [x] one\n";
        fs::write(&file, content).unwrap();

        let racer = work.clone();
        let _hook = RaceHook::at(RacePoint::BeforeIndexAlignment, move || {
            fs::write(racer.join("tracked.md"), "- [ ] staged mid-sync\n").unwrap();
            run(&racer, &["add", "tracked.md"]);
        });

        let outcome = run_sync(
            &work,
            &file,
            content,
            &commit_message(&file, "Check \"one\""),
            &no_race(content),
            None,
        );

        assert!(
            matches!(
                outcome,
                SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            staged_bytes(&work, "tracked.md").as_deref(),
            Some(&b"- [ ] staged mid-sync\n"[..]),
            "a version staged during the sync must not be overwritten by the realignment"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn git_invocations_ignore_an_ambient_git_environment() {
        // Deep rounds 2, round 2. markcheck inherits its environment, and a
        // handful of git variables redirect which *repository* a command
        // acts on. This test has two halves: the first proves the hazard is
        // real against actual git, the second proves every variable that
        // causes it is explicitly removed.
        //
        // The ambient case can't be reproduced by setting the variable on
        // the process, because `std::env::set_var` is process-global and
        // would race every other test in this binary — and setting it on the
        // built `Command` instead would simply override the removal being
        // tested. So the hazard is demonstrated on a deliberately poisoned
        // command, and the defence structurally.
        let root = unique_dir("ambient-env");
        let (a, b) = (root.join("a"), root.join("b"));
        for dir in [&a, &b] {
            fs::create_dir_all(dir).unwrap();
            run(dir, &["init", "-q", "-b", "main"]);
            run(dir, &["config", "user.email", "test@example.com"]);
            run(dir, &["config", "user.name", "test"]);
            fs::write(dir.join("f.md"), "x\n").unwrap();
            run(dir, &["add", "f.md"]);
        }
        run(&a, &["commit", "-q", "-m", "a init"]);
        run(&b, &["commit", "-q", "-m", "b init"]);

        // The hazard: GIT_DIR makes a command run *inside* a report b's HEAD,
        // so refs and objects come from the wrong repository entirely.
        let mut poisoned = Command::new("git");
        poisoned
            .current_dir(&a)
            .env("GIT_DIR", b.join(".git"))
            .args(["log", "-1", "--format=%s"]);
        let out = run_with_timeout(poisoned, PLUMBING_TIMEOUT).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "b init",
            "test premise: an ambient GIT_DIR really does redirect the repository"
        );

        // The defence: every variable that can do that is removed.
        let cmd = git_command(&a);
        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
        ] {
            assert!(
                removed.iter().any(|k| k == var),
                "{var} must be neutralised on every git invocation"
            );
        }
        // Identity and discovery limits are deliberately *not* touched.
        for var in ["GIT_AUTHOR_NAME", "GIT_CEILING_DIRECTORIES"] {
            assert!(
                !removed.iter().any(|k| k == var),
                "{var} is the user's to set and must be left alone"
            );
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unverifiable_commit_scope_says_the_commit_was_left_behind() {
        // Deep rounds 2, round 3. When the scope check itself fails, the
        // commit has already been made and stays on the branch. The old bare
        // `?` reported only the raw git error, so the user was told the sync
        // failed with no hint that a commit now exists — unlike the
        // undo-failed path, which names it.
        let work = init_repo_without_remote();
        let parent = current_head(&work).unwrap();
        let bogus = "0".repeat(40);

        let result = verify_commit_scope(&work, &Some(parent), "tracked.md", &bogus);

        let err = result.expect_err("an unresolvable commit cannot be verified");
        assert!(
            err.contains(&bogus) && err.contains("left on the branch"),
            "the message must name the commit and say it survived: {err}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Two concurrent syncs in one repository (deep rounds 2, round 4) ---

    #[test]
    fn a_second_sync_committing_mid_flight_cannot_revert_the_first() {
        // The realistic multi-process shape: two markcheck instances open on
        // *different* checklists in the *same* repository. The write lock
        // serialises writes per file, and `GitSync` serialises syncs within
        // one process — neither does anything across processes, so two
        // `run_sync` calls really can interleave here.
        //
        // The danger is that both seed a temp index from the same tip: the
        // loser's tree then reverts the winner's file. The racer here is a
        // complete second `run_sync`, not a bare `git commit`, so the whole
        // pipeline participates.
        let (work, _remote) = init_repo_with_remote();
        let a = work.join("a.md");
        let b = work.join("b.md");
        fs::write(&a, "- [ ] a\n").unwrap();
        fs::write(&b, "- [ ] b\n").unwrap();
        run(&work, &["add", "a.md", "b.md"]);
        run(&work, &["commit", "-q", "-m", "add both checklists"]);
        run(&work, &["push", "-q", "origin", "main"]);

        // The other instance finishes its whole sync while ours is mid-commit.
        let racer_dir = work.clone();
        let racer_a = a.clone();
        let _hook = RaceHook::at(RacePoint::BeforeCommit, move || {
            fs::write(&racer_a, "- [x] a\n").unwrap();
            let outcome = run_sync(
                &racer_dir,
                &racer_a,
                "- [x] a\n",
                "a.md: Check \"a\"",
                &no_race("- [x] a\n"),
                None,
            );
            assert!(
                matches!(
                    outcome,
                    SyncOutcome::Synced | SyncOutcome::CommittedNotPushed { .. }
                ),
                "the other instance's sync should land: {outcome:?}"
            );
        });

        fs::write(&b, "- [x] b\n").unwrap();
        let outcome = run_sync(
            &work,
            &b,
            "- [x] b\n",
            "b.md: Check \"b\"",
            &no_race("- [x] b\n"),
            None,
        );

        // Ours must not succeed on a tree seeded from the superseded tip.
        assert!(
            matches!(&outcome, SyncOutcome::Failed(_)),
            "the losing sync must refuse, not commit a reverting tree: {outcome:?}"
        );
        // And the winner's work must be intact, both in history and on disk.
        assert_eq!(
            git_stdout(&work, &["show", "HEAD:a.md"]),
            "- [x] a",
            "the other instance's commit must survive untouched"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "-1", "main"]),
            "a.md: Check \"a\"",
            "and must still be the branch tip"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Hostile repository shapes (deep rounds 2, round 2) ---

    #[test]
    fn run_sync_works_when_the_checklist_lives_in_a_subdirectory() {
        // `repo_dir` is the checklist's own parent, so for anything but a
        // root-level checklist it differs from `repo_root`. Several plumbing
        // commands run in one and take root-relative paths from the other.
        let (work, remote) = init_repo_with_remote();
        let sub = work.join("docs").join("runbooks");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("list.md");
        fs::write(&file, "- [ ] one\n").unwrap();
        run(&work, &["add", "docs/runbooks/list.md"]);
        run(&work, &["commit", "-q", "-m", "add nested checklist"]);
        run(&work, &["push", "-q", "origin", "main"]);

        let expected = "- [x] one\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &sub,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "HEAD:docs/runbooks/list.md"]),
            expected.trim_end(),
            "the nested path must be committed at its repo-root-relative location"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_works_inside_a_linked_worktree() {
        // A linked worktree's `.git` is a *file*, and `rev-parse --git-dir`
        // points at `.git/worktrees/<name>` — which is where its MERGE_HEAD
        // and the temp index have to live, not the main repo's git dir.
        let (work, remote) = init_repo_with_remote();
        let linked = work.parent().unwrap().join("linked");
        run(
            &work,
            &[
                "worktree",
                "add",
                "-b",
                "wt",
                linked.to_str().unwrap(),
                "main",
            ],
        );
        run(&linked, &["config", "user.email", "test@example.com"]);
        run(&linked, &["config", "user.name", "test"]);
        run(&linked, &["push", "-q", "-u", "origin", "wt"]);
        assert!(
            linked.join(".git").is_file(),
            "test setup: a linked worktree's .git is a file"
        );

        let file = linked.join("tracked.md");
        let expected = "- [x] one\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &linked,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "wt:tracked.md"]),
            expected.trim_end()
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn detect_and_sync_cope_with_a_shallow_clone() {
        // A shallow clone has grafted history: `rev-list` over a range that
        // reaches the graft boundary behaves differently, and the
        // unrelated-history guard walks exactly such a range.
        let (origin, remote) = init_repo_with_remote();
        for n in 1..=3 {
            fs::write(origin.join("tracked.md"), format!("- [ ] {n}\n")).unwrap();
            run(&origin, &["commit", "-q", "-am", &format!("edit {n}")]);
        }
        run(&origin, &["push", "-q", "origin", "main"]);

        let shallow = origin.parent().unwrap().join("shallow");
        run(
            origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                remote.to_str().unwrap(),
                shallow.to_str().unwrap(),
            ],
        );
        run(&shallow, &["config", "user.email", "test@example.com"]);
        run(&shallow, &["config", "user.name", "test"]);

        let file = shallow.join("tracked.md");
        let expected = "- [x] shallow\n";
        fs::write(&file, expected).unwrap();
        let outcome = run_sync(
            &shallow,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            None,
        );

        assert_eq!(
            outcome,
            SyncOutcome::Synced,
            "a shallow clone must still sync: {outcome:?}"
        );

        fs::remove_dir_all(origin.parent().unwrap()).ok();
    }

    // --- Captured-tip invariant (round 10) ---

    #[test]
    fn a_head_that_moves_after_the_tip_is_captured_cannot_broaden_the_push() {
        // External review, round 10. `run_sync`'s fast path and
        // `catch_up_push` both used to validate the unpushed range ending at
        // the *symbolic* `HEAD`, then separately re-resolve `HEAD` to get a
        // SHA for the push. Those are two different subprocesses, so a
        // commit landing between them was published having never been
        // checked. Both now capture the tip once and name that SHA at every
        // step.
        //
        // The interleaving is reproduced deterministically by moving `HEAD`
        // inside the window rather than by racing a real thread: what the
        // fix guarantees is that the *validated* SHA and the *pushed* SHA
        // are the same commit, which is exactly what this asserts.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "checklist"]);
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        // The guard passes for that tip: its unpushed range is checklist-only.
        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::Safe)
        );

        // Now an unrelated commit lands — exactly what could happen between
        // the two subprocesses in the old code.
        fs::write(work.join("other.txt"), "unrelated\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);
        let moved = git_stdout(&work, &["rev-parse", "HEAD"]);
        assert_ne!(moved, tip, "test setup: HEAD must have moved");

        // Pushing the captured tip publishes exactly the validated range.
        assert_eq!(push(&work, &tip), SyncOutcome::Synced);
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "checklist\ninit",
            "the unrelated commit must not reach the remote"
        );

        // And had the moved `HEAD` been what the guard was asked about, it
        // would have refused — which is what makes checking one commit and
        // pushing another unsafe in the first place.
        assert_eq!(
            unpushed_history(&work, Some(&moved), "tracked.md"),
            Ok(UnpushedHistory::ContainsUnrelated)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_tipless_branch_has_no_unrelated_unpushed_commits() {
        // `None` means the branch has no commits at all, so there is nothing
        // unpushed for anything to be unrelated to. Previously this question
        // was asked about the symbolic `HEAD`, which simply errored on an
        // empty repository and fell through to the same answer by accident.
        let work = init_repo_without_remote();
        assert_eq!(
            unpushed_history(&work, None, "x.md"),
            Ok(UnpushedHistory::Safe)
        );
        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Staged-target guard (round 11) ---

    #[test]
    fn run_sync_refuses_when_a_staged_checklist_version_would_be_dropped() {
        // External review, round 11, reproduced end-to-end before this
        // guard: the commit is built from the working tree and the real
        // index is realigned to it afterwards, so a staged snapshot that
        // differs from the working tree is neither committed nor still
        // staged — it survives only as a dangling blob, reachable through no
        // ordinary git workflow.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");

        // The user stages a version carrying work that exists only in the index...
        fs::write(&file, "- [ ] one\n- [ ] IMPORTANT\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let staged = index_blob(&work, "tracked.md").unwrap();

        // ...then keeps editing, so the working tree no longer has it.
        let pre_write = "- [ ] one\n- [ ] later\n";
        fs::write(&file, pre_write).unwrap();

        // markcheck toggles a task, writing the post-toggle content to disk.
        let expected = "- [x] one\n- [ ] later\n";
        fs::write(&file, expected).unwrap();

        let outcome = run_sync(
            &work,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            staged_matches(pre_write),
        );

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("staged changes")),
            "{outcome:?}"
        );
        assert_eq!(
            index_blob(&work, "tracked.md").as_deref(),
            Some(staged.as_str()),
            "the staged version must still be staged"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "init",
            "no commit may be built"
        );
        assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn run_sync_proceeds_when_the_staged_checklist_matches_the_working_tree() {
        // The deliberately permitted case: staging the checklist and *then*
        // toggling loses nothing, because everything staged is already
        // contained in the content about to be committed. Refusing here
        // would break an ordinary workflow for no safety gain — which is why
        // this guard is not "refuse whenever the target is staged".
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");

        let pre_write = "- [ ] one\n- [ ] extra\n";
        fs::write(&file, pre_write).unwrap();
        run(&work, &["add", "tracked.md"]);

        // An unrelated staged file must survive this untouched, as always.
        fs::write(work.join("other.txt"), "unrelated staged\n").unwrap();
        run(&work, &["add", "other.txt"]);
        let other_staged = index_blob(&work, "other.txt");

        let expected = "- [x] one\n- [ ] extra\n";
        fs::write(&file, expected).unwrap();

        let outcome = run_sync(
            &work,
            &file,
            expected,
            &commit_message(&file, "Check \"one\""),
            &no_race(expected),
            staged_matches(pre_write),
        );

        assert_eq!(outcome, SyncOutcome::Synced, "{outcome:?}");
        assert_eq!(
            git_stdout(&remote, &["show", "HEAD:tracked.md"]),
            expected.trim_end(),
            "the commit carries exactly the toggled content, staged work included"
        );
        assert_eq!(
            index_blob(&work, "other.txt"),
            other_staged,
            "an unrelated staged file is still untouched"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn staged_target_guard_fails_closed_without_a_pre_write_hash() {
        // Safety can't be established without something to compare the
        // staged bytes against, so the guard refuses — the same rule
        // `unpushed_history` follows for an unanswerable check.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [ ] one\n- [ ] staged\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let head = git_stdout(&work, &["rev-parse", "HEAD:tracked.md"]);

        assert!(
            staged_target_would_be_lost(&work, "tracked.md", Some(&head), None),
            "no pre-write hash means the staged version cannot be cleared as safe"
        );
        // ...and an unstaged path is never at risk regardless.
        assert!(!staged_target_would_be_lost(
            &work,
            "other-untouched.md",
            None,
            None
        ));

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Real-index realignment (round 10) ---

    #[test]
    fn align_real_index_entry_leaves_a_newer_staged_version_alone() {
        // External review, round 10: this write was unconditional, so a
        // `git add` of the checklist from another terminal (or an IDE) while
        // the background sync ran had its staged version silently replaced.
        // The working tree is untouched either way, so it is staging loss
        // rather than content loss — but it is exactly the unrelated state
        // the temporary-index design exists to protect.
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");
        assert!(before.is_some(), "test setup: the file is tracked");

        // The user stages a newer version while the sync is in flight.
        fs::write(work.join("tracked.md"), "- [x] staged by the user\n").unwrap();
        run(&work, &["add", "tracked.md"]);
        let user_staged = index_blob(&work, "tracked.md").unwrap();
        assert_ne!(Some(&user_staged), before.as_ref(), "test setup");

        // markcheck finishes and would realign to its own committed blob.
        let committed = hash_object(&work, "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(user_staged),
            "the user's staged version must survive untouched"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn align_real_index_entry_still_realigns_its_own_untouched_entry() {
        // The case the realignment exists for: nothing else staged the path,
        // so the entry is markcheck's to advance. Without this, `git status`
        // shows the file as both staged and unstaged after every toggle.
        let (work, _remote) = init_repo_with_remote();
        let before = index_blob(&work, "tracked.md");

        let committed = hash_object(&work, "- [x] committed by markcheck\n").unwrap();
        align_real_index_entry(&work, "100644", &committed, "tracked.md", before.as_deref());

        assert_eq!(
            index_blob(&work, "tracked.md"),
            Some(committed),
            "an untouched entry is still advanced to the committed blob"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Unpushed-history check: three answers, not two (round 10) ---

    #[test]
    fn an_unanswerable_history_check_refuses_instead_of_permitting_a_push() {
        // External review, round 10: this check was `.unwrap_or(false)`, so
        // any git failure — a `rev-list` timeout on a large history, a
        // transiently unhealthy object database — read exactly like
        // "verified safe" and the caller went on to push. A false refusal
        // costs a retry; a false clearance publishes someone else's commits.
        let (work, _remote) = init_repo_with_remote();
        // A tip that doesn't exist: `rev-list` fails, so the question
        // genuinely cannot be answered.
        let bogus = "0".repeat(40);

        assert!(
            unpushed_history(&work, Some(&bogus), "tracked.md").is_err(),
            "an unresolvable range must surface as an error, not a verdict"
        );
        let refusal = unpushed_history_blocks(&work, Some(&bogus), "tracked.md");
        assert!(
            matches!(&refusal, Some(SyncOutcome::Failed(msg)) if msg.contains("could not verify")),
            "the caller must refuse to publish: {refusal:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_branch_without_an_upstream_is_not_a_verification_failure() {
        // The legitimate case that stops this failing closed everywhere: a
        // fresh repository has nothing to compare against, and refusing
        // there would block every first-ever commit. Committing proceeds;
        // `push` declines separately, with its own specific message.
        let work = init_repo_without_remote();
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::NoUpstream)
        );
        assert!(unpushed_history_blocks(&work, Some(&tip), "tracked.md").is_none());

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_configured_but_unfetched_upstream_still_counts_as_no_upstream() {
        // Regression for a real mistake made while fixing the above: the
        // no-upstream case was first detected via `upstream_parts`, which
        // reads `branch.<name>.remote`/`.merge` *config*. Config can say an
        // upstream exists while `@{u}` still doesn't resolve — a remote
        // added but never fetched, or those keys set by hand — and treating
        // that as an unanswerable check turned three passing tests into hard
        // sync failures. Resolving `@{u}` itself is what tells them apart.
        let work = init_repo_without_remote();
        run(&work, &["config", "branch.main.remote", "origin"]);
        run(&work, &["config", "branch.main.merge", "refs/heads/main"]);
        let tip = git_stdout(&work, &["rev-parse", "HEAD"]);

        assert!(
            upstream_parts(&work).is_some(),
            "test setup: the config claims an upstream exists"
        );
        assert_eq!(
            resolve_upstream(&work),
            Ok(None),
            "but the tracking ref does not resolve"
        );
        assert_eq!(
            unpushed_history(&work, Some(&tip), "tracked.md"),
            Ok(UnpushedHistory::NoUpstream)
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // --- Startup catch-up push (`catch_up_push`) ---

    #[test]
    fn catch_up_push_never_commits_uncommitted_working_tree_changes() {
        // Deep review, round 3, reproduced end-to-end against a real repo
        // and bare remote before this fix. Startup expressed the catch-up
        // as an ordinary content request carrying the file's *current disk
        // bytes*, which satisfies every `run_sync` guard by construction --
        // so merely opening a checklist that had ordinary uncommitted
        // editor changes committed *and pushed* them, under the message
        // `Catch up a pending push`. Opening a viewer must publish nothing.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        let edited = "- [x] edited outside markcheck\n";
        fs::write(&file, edited).unwrap();

        let outcome = catch_up_push(&work, &file);

        assert_eq!(
            outcome,
            SyncOutcome::Skipped,
            "nothing was ahead of upstream, so there was nothing to do"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "init",
            "no commit may be created"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "init",
            "nothing may reach the remote"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            edited,
            "the uncommitted edit stays exactly as the user left it"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_pushes_a_local_only_commit_without_committing_a_dirty_tree() {
        // The case the startup hook exists for -- a prior session committed
        // but quit before its push landed -- combined with the case above,
        // to pin both halves at once: the existing commit goes out, and the
        // unrelated uncommitted edit sitting next to it does not.
        let (work, remote) = init_repo_with_remote();
        let file = work.join("tracked.md");
        fs::write(&file, "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "tracked.md: Check \"one\""]);
        // ... and then the user keeps editing, without committing.
        let dirty = "- [x] one\n- [ ] two\n";
        fs::write(&file, dirty).unwrap();

        let outcome = catch_up_push(&work, &file);

        assert_eq!(outcome, SyncOutcome::Synced);
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "tracked.md: Check \"one\"\ninit",
            "the local-only commit reached the remote"
        );
        assert_eq!(
            git_stdout(&work, &["log", "--format=%s", "main"]),
            "tracked.md: Check \"one\"\ninit",
            "and no second commit was created for the dirty tree"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), dirty);

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_is_a_no_op_when_everything_is_already_pushed() {
        let (work, remote) = init_repo_with_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );
        assert_eq!(git_stdout(&remote, &["log", "--format=%s", "main"]), "init");

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_stays_silent_when_no_upstream_is_configured() {
        // Fails closed, unlike `ahead_of_upstream`: nothing has happened
        // here, so an unanswerable question means "do nothing, quietly"
        // rather than nagging about the missing upstream on every launch.
        let work = init_repo_without_remote();

        assert_eq!(
            catch_up_push(&work, &work.join("tracked.md")),
            SyncOutcome::Skipped
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_refuses_when_unrelated_commits_are_unpushed() {
        // An explicit-SHA push still sends the commit's whole ancestry, so
        // the same refusal `run_sync` makes has to apply here too.
        let (work, remote) = init_repo_with_remote();
        fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "unrelated"]);

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg)
                if msg.contains("unrelated to this change")),
            "{outcome:?}"
        );
        assert_eq!(
            git_stdout(&remote, &["log", "--format=%s", "main"]),
            "init",
            "the unrelated commit must not be published"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_reports_an_untracked_checklist() {
        // Preserves what the old startup request reported: git-sync being
        // permanently unable to do anything for this file is worth saying.
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("other.txt"), "unrelated work\n").unwrap();
        run(&work, &["add", "other.txt"]);
        run(&work, &["commit", "-q", "-m", "ahead"]);
        let untracked = work.join("not-tracked.md");
        fs::write(&untracked, "- [ ] one\n").unwrap();

        assert_eq!(
            catch_up_push(&work, &untracked),
            SyncOutcome::SkippedUntracked
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn catch_up_push_refuses_while_the_repository_is_mid_merge() {
        let (work, _remote) = init_repo_with_remote();
        fs::write(work.join("tracked.md"), "- [x] one\n").unwrap();
        run(&work, &["commit", "-q", "-am", "ahead"]);
        let git_dir = work.join(".git");
        fs::write(git_dir.join("MERGE_HEAD"), "deadbeef\n").unwrap();

        let outcome = catch_up_push(&work, &work.join("tracked.md"));

        assert!(
            matches!(&outcome, SyncOutcome::Failed(msg) if msg.contains("a merge in progress")),
            "{outcome:?}"
        );

        fs::remove_dir_all(work.parent().unwrap()).ok();
    }
}
