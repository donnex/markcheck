//! Running `git` as a subprocess, bounded and isolated.
//!
//! The layer everything else in `git_sync` sits on: how a command is built
//! (`git_command`, which strips the ambient git environment), how its output
//! is captured (scratch files, never pipes — see `run_with_optional_stdin`),
//! and how one that will not finish is killed. Depends on nothing else in
//! the module.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Timeout for local git plumbing commands (`status`, `ls-files`,
/// `hash-object`, `rev-parse`, `read-tree`, `update-index`, `commit`,
/// `symbolic-ref`, `update-ref`, `diff`, `show`) — all normally instant, no
/// network involved, so a generous-but-bounded cap catches a genuinely
/// stuck process (a hanging commit hook, a wedged filesystem) without ever
/// being a realistic limit under normal operation.
pub(super) const PLUMBING_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for `git push` specifically — network-bound, so it needs
/// meaningfully longer than the plumbing commands above.
pub(super) const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Scratch files backing one subprocess run's stdout, stderr, and (when the
/// command is fed anything) stdin. Removed on drop, on every path — success,
/// failure, timeout, or an early `?` return.
///
/// **In the temp directory, never beside the checklist**, for the same
/// reason `writer::WriteLock`'s file is: anything written inside the user's
/// repository can be swept into a commit by a `git add -A` hook, which
/// `verify_commit_scope` would then correctly abort the sync over.
struct ScratchFiles {
    paths: Vec<PathBuf>,
}
impl ScratchFiles {
    fn new() -> Self {
        ScratchFiles { paths: Vec::new() }
    }

    /// A fresh, empty file owned by this run. `create_new` so an existing
    /// name is never clobbered or reused, and 0600 on unix because git's
    /// output routinely names branches and absolute repository paths, which
    /// have no business being world-readable in a shared temp directory.
    fn create(&mut self, purpose: &str) -> io::Result<(PathBuf, fs::File)> {
        let path = std::env::temp_dir().join(format!(
            "markcheck-git-{purpose}-{}-{:x}",
            std::process::id(),
            crate::writer::random_suffix()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        self.paths.push(path.clone());
        Ok((path, file))
    }
}
impl Drop for ScratchFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}
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
/// Takes an owned `Command` rather than `&mut Command` (unlike
/// `Command::output`) so call sites build it as a local variable first
/// instead of chaining off `Command::new` directly — a builder chain like
/// `Command::new("git").arg(...)` yields `&mut Command`, borrowing a
/// temporary, which can't be handed to a function expecting an owned one.
pub(super) fn run_with_timeout(cmd: Command, timeout: Duration) -> io::Result<Output> {
    run_with_optional_stdin(cmd, timeout, None)
}
/// Like `run_with_timeout`, but feeds `stdin_data` to the child — needed by
/// `hash_object`, the one call site that gives git anything on stdin.
pub(super) fn run_with_timeout_and_stdin(
    cmd: Command,
    timeout: Duration,
    stdin_data: &str,
) -> io::Result<Output> {
    run_with_optional_stdin(cmd, timeout, Some(stdin_data))
}
/// Shared core of both: redirect the child's three streams to scratch
/// **files**, poll until it exits or the timeout expires, then read the
/// output back.
///
/// **Files rather than pipes, and that is the whole point.** The previous
/// version piped stdout/stderr and drained each on its own detached thread,
/// with a third thread writing stdin. Threads were unavoidable there
/// because a pipe has a small fixed buffer: a child that outproduces it
/// blocks mid-write, `try_wait` never returns, and nothing arrives to
/// unblock it unless something is draining concurrently — the classic pipe
/// deadlock.
///
/// That design produced three separate defects across earlier reviews, all
/// the same shape. `git` hands its descriptors to whatever it spawns — a
/// credential helper, `ssh`, a commit hook and *its* children — so a
/// descendant outliving `git` itself keeps the pipes open, and a thread
/// blocked in `read_to_end` waits for an EOF only that descendant can send.
/// Round 8 fixed the stdout/stderr hang, deep round 2 the mirror-image
/// stdin hang, and pass 5 measured what the bounded-wait fix left behind:
/// two threads and two descriptors leaked per call, held for as long as the
/// descendant lives, at +44 threads over 20 calls.
///
/// A file has no buffer to fill, so the child never blocks on a write and
/// nothing has to drain concurrently. No reader threads, no writer thread,
/// no drain budget, and a lingering descendant is simply harmless: it goes
/// on writing to an unlinked inode that the OS reclaims when it exits. This
/// removes the entire class rather than the latest instance of it — the
/// three fixes above are all subsumed here, which is why it is worth
/// touching code this carefully tuned.
///
/// Failing to create a scratch file returns the error rather than falling
/// back to pipes or discarding output: git-sync reports a failed sync,
/// which is the fail-closed answer this module takes everywhere else.
fn run_with_optional_stdin(
    mut cmd: Command,
    timeout: Duration,
    stdin_data: Option<&str>,
) -> io::Result<Output> {
    let mut scratch = ScratchFiles::new();

    match stdin_data {
        Some(data) => {
            let (path, mut file) = scratch.create("stdin")?;
            file.write_all(data.as_bytes())?;
            file.sync_all()?;
            drop(file);
            // Reopened read-only: the child needs to read what was just
            // written, from the start.
            cmd.stdin(Stdio::from(fs::File::open(&path)?));
        }
        // Nulled rather than inherited: markcheck owns the terminal while
        // these run, and no command here has anything to read. `git` seeing
        // an immediate EOF is the predictable, conventional choice — a
        // credential helper cannot sit waiting on a prompt nobody will
        // answer.
        None => {
            cmd.stdin(Stdio::null());
        }
    }

    let (stdout_path, stdout_file) = scratch.create("stdout")?;
    let (stderr_path, stderr_file) = scratch.create("stderr")?;
    cmd.stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    let mut child = spawn_in_own_process_group(cmd)?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git command timed out after {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // Read after the child is known to be gone. A descendant still holding
    // the descriptors may append after this point; that output is missed,
    // exactly as it was missed when the drain budget expired before, but
    // now it costs nothing to miss it.
    Ok(Output {
        status,
        stdout: fs::read(&stdout_path).unwrap_or_default(),
        stderr: fs::read(&stderr_path).unwrap_or_default(),
    })
}
/// Spawns `cmd` as the leader of a new process group (its own PID doubling
/// as the group ID) on Unix — plain on other platforms, where this is a
/// best-effort feature (see `kill_and_reap`). Matters because `git` itself
/// can spawn its own subprocesses (a credential helper, `ssh` for a remote
/// push, a commit hook's own children): killing only the direct `git` child
/// on timeout would leave those grandchildren running indefinitely, still
/// doing whatever wedged the command in the first place. Killing the whole
/// group (`kill_and_reap`) reaches all of them.
///
/// This used to matter for a second reason — those grandchildren inherited
/// the piped stdout/stderr and could hang the reader threads — which no
/// longer applies now that output goes to files. Reaping the whole group on
/// timeout is still the right thing on its own merits.
fn spawn_in_own_process_group(mut cmd: Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
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
pub(super) fn git_command(repo_dir: &Path) -> Command {
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
/// The first line of a failed command's stderr, prefixed with which command
/// produced it — short enough for the single-line sticky status bar.
pub(super) fn command_error(step: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first_line = stderr.lines().next().unwrap_or("unknown error").trim();
    format!("{step}: {first_line}")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

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
        // Regression guard for the deadlock that made this module use
        // pipes-plus-threads in the first place: a child writing more than
        // the OS pipe buffer (~64KB on Linux) blocks mid-write, `try_wait`
        // never returns, and the call hangs regardless of `timeout` unless
        // something drains concurrently. Redirecting to a file is what makes
        // that impossible now — a file has no buffer to fill — so this keeps
        // guarding the same hazard against a different implementation. Five
        // million bytes is far past any pipe buffer.
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
    /// A descendant outliving the direct child must now cost **nothing**.
    ///
    /// This measured a real leak when output was piped: the two reader
    /// threads stayed blocked in `read_to_end` waiting for an EOF only the
    /// descendant could send, holding a descriptor each — +2 per call, +44
    /// over 20 calls, for as long as the descendant lived. The test then
    /// pinned that the cost was at least *bounded* by that lifetime.
    ///
    /// Redirecting to scratch files removed the threads outright, so the
    /// assertion is now the stronger one: run the exact case that used to
    /// leak, twenty times, while the descendants are all still alive, and
    /// the thread count must not move at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lingering_descendant_costs_nothing() {
        let threads = || {
            std::fs::read_dir("/proc/self/task")
                .map(|d| d.count())
                .unwrap_or(0)
        };

        // Warm up first: the first calls create lazily-spawned runtime
        // threads that never go away, which would otherwise be counted as
        // leakage from the calls under test.
        for _ in 0..3 {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exit 0"]);
            let _ = run_with_timeout(cmd, Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_millis(300));
        let baseline = threads();

        // Each shell exits immediately — the success path, where no group
        // kill happens — while leaving a descendant that holds the inherited
        // stdout and stderr for far longer than the whole loop takes.
        for _ in 0..20 {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30 & exit 0"]);
            let result = run_with_timeout(cmd, Duration::from_millis(500));
            assert!(result.is_ok(), "the direct child exits successfully");
        }

        // A small tolerance rather than strict equality, because `cargo test`
        // runs these in parallel and an unrelated test spawning a worker
        // between the two samples would otherwise fail this for something
        // that has nothing to do with the code under test. It still
        // discriminates the regression by a wide margin: the piped design
        // leaked two threads per call, so these twenty calls would sit ~40
        // above the baseline rather than within a handful of it.
        let after = threads();
        let grown = after.saturating_sub(baseline);
        assert!(
            grown < 10,
            "20 calls whose descendants are all still holding the inherited \
             descriptors must not cost threads: baseline {baseline}, now \
             {after} (+{grown}); the piped design leaked about 40 here"
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
        // `sleep 3` holds the inherited stdout well past the point the old
        // drain would have given up and killed the group, so it is exactly
        // the case that used to trigger the kill; the marker proves it ran
        // to completion instead. Still worth keeping now that output goes to
        // a file and nothing waits on a drain at all: the property under
        // test is that a *succeeded* command never kills what it spawned,
        // which no implementation should be free to break.
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
}
