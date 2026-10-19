//! Running `git` as a subprocess: environment isolation, output captured to
//! scratch files, a session of its own with no controlling terminal, and a
//! timeout that kills the whole process group.

use std::cell::RefCell;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Cap for local plumbing commands, which are normally instant.
pub(super) const PLUMBING_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap for the network-bound commands: `git push` and `git ls-remote`.
pub(super) const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Cap for `git commit`, which runs the repository's hooks and signing: a
/// hook framework's first-run setup or a signing key waiting for a touch can
/// take minutes.
pub(super) const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a timed-out command gets after `SIGTERM` before `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(1);
/// The git children a [`super::GitSync`]'s workers have running, so quitting
/// can stop them rather than leave them orphaned. A child is spawned, reaped
/// and signalled only under this lock, so [`Children::abandon`] can never
/// signal a process group whose leader was already reaped and its PID
/// recycled, nor miss a child spawned while it runs.
#[derive(Default)]
pub(super) struct Children {
    inner: Mutex<ChildrenInner>,
}
#[derive(Default)]
struct ChildrenInner {
    pids: Vec<u32>,
    /// Set by `abandon`: no further child may start.
    abandoned: bool,
}
thread_local! {
    /// The registry `run_with_timeout` uses on this thread: a worker's
    /// `GitSync`'s once [`track_children_on_this_thread`] ran, otherwise a
    /// private one nothing abandons.
    static CHILDREN: RefCell<Arc<Children>> = RefCell::new(Arc::default());
}
pub(super) fn track_children_on_this_thread(children: Arc<Children>) {
    CHILDREN.with(|current| *current.borrow_mut() = children);
}
impl Children {
    fn lock(&self) -> MutexGuard<'_, ChildrenInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn spawn(&self, cmd: Command) -> io::Result<Child> {
        let mut inner = self.lock();
        if inner.abandoned {
            return Err(io::Error::other("git sync was abandoned"));
        }
        let child = spawn_in_new_session(cmd)?;
        inner.pids.push(child.id());
        Ok(child)
    }
    fn try_wait(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        let mut inner = self.lock();
        let result = child.try_wait();
        if !matches!(result, Ok(None)) {
            inner.pids.retain(|&pid| pid != child.id());
        }
        result
    }
    /// Unregisters `child` before a blocking reap.
    fn forget(&self, child: &Child) {
        self.lock().pids.retain(|&pid| pid != child.id());
    }
    /// Stops every running child's process group with `signal` (`"-TERM"` or
    /// `"-KILL"`) and refuses new ones from now on. Returns whether any child
    /// was running.
    pub(super) fn abandon(&self, signal: &str) -> bool {
        let mut inner = self.lock();
        inner.abandoned = true;
        for &pid in &inner.pids {
            signal_group(pid, signal);
        }
        !inner.pids.is_empty()
    }
}
/// Scratch files for one run's stdio, removed on drop. Kept in the temp
/// directory: anything written inside the repository could be swept into a
/// commit by a hook.
struct ScratchFiles {
    paths: Vec<PathBuf>,
}
impl ScratchFiles {
    fn new() -> Self {
        ScratchFiles { paths: Vec::new() }
    }

    /// 0600, because git's output names branches and repository paths.
    fn create(&mut self, purpose: &str) -> io::Result<(PathBuf, fs::File)> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = std::env::temp_dir().join(format!(
            "markcheck-git-{purpose}-{}-{:x}",
            std::process::id(),
            crate::writer::random_suffix()
        ));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
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
/// `Command::output()` has no timeout, and a hung git would block the sync
/// worker forever.
pub(super) fn run_with_timeout(cmd: Command, timeout: Duration) -> io::Result<Output> {
    run_with_optional_stdin(cmd, timeout, None)
}
pub(super) fn run_with_timeout_and_stdin(
    cmd: Command,
    timeout: Duration,
    stdin_data: &str,
) -> io::Result<Output> {
    run_with_optional_stdin(cmd, timeout, Some(stdin_data))
}
/// Files rather than pipes: a descendant that outlives `git` (a credential
/// helper, `ssh`, a hook) can hold a pipe open and block the reader, but a
/// file has no buffer to fill and nothing has to drain it.
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
            cmd.stdin(Stdio::from(fs::File::open(&path)?));
        }
        // Nulled so a credential helper cannot wait on a prompt nobody will answer.
        None => {
            cmd.stdin(Stdio::null());
        }
    }

    let (stdout_path, stdout_file) = scratch.create("stdout")?;
    let (stderr_path, stderr_file) = scratch.create("stderr")?;
    cmd.stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    let children = CHILDREN.with(|current| Arc::clone(&current.borrow()));
    let mut child = children.spawn(cmd)?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = children.try_wait(&mut child)? {
            break status;
        }
        if Instant::now() >= deadline {
            let message = if terminate_and_reap(&children, &mut child, KILL_GRACE) {
                format!(
                    "git command timed out after {timeout:?} and was force-killed; \
                     if git now reports that index.lock exists, remove .git/index.lock"
                )
            } else {
                format!("git command timed out after {timeout:?}")
            };
            return Err(io::Error::new(io::ErrorKind::TimedOut, message));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    Ok(Output {
        status,
        stdout: fs::read(&stdout_path).unwrap_or_default(),
        stderr: fs::read(&stderr_path).unwrap_or_default(),
    })
}
/// A new session, which is also a new process group, so a timeout can kill
/// git's descendants too. With no controlling terminal, a prompt from git,
/// ssh or gpg fails at once instead of writing over the TUI and then waiting
/// for the timeout on input nobody can give.
fn spawn_in_new_session(mut cmd: Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec, where only
    // async-signal-safe calls are allowed; `setsid` is one and touches no
    // memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            // Fails only for a process group leader, which a freshly forked
            // child never is, so this branch can't be reached in a test.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}
/// `SIGTERM` first: git's signal handlers remove the lock files it holds in
/// the user's repository, which an uncatchable `SIGKILL` would leave behind.
/// The group is only signalled while the child is unreaped, since a reaped
/// child's PID, and so the group ID, can be recycled onto something unrelated.
/// Returns whether the child outlived the grace period and had to be killed.
fn terminate_and_reap(children: &Children, child: &mut Child, grace: Duration) -> bool {
    signal_group(child.id(), "-TERM");
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(children.try_wait(child), Ok(Some(_))) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    signal_group(child.id(), "-KILL");
    let _ = child.kill();
    children.forget(child);
    let _ = child.wait();
    true
}
fn signal_group(pid: u32, signal: &str) {
    // Nulled: `kill` complains on stderr if the group is gone, and
    // markcheck owns the terminal.
    let _ = Command::new("kill")
        .args([signal, "--"])
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
/// The variables that would make git act on a different repository than the
/// one holding the checklist (hooks and `git rebase --exec` export `GIT_DIR`
/// and `GIT_INDEX_FILE`, for example).
pub(crate) const REDIRECTING_GIT_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];
/// Strips [`REDIRECTING_GIT_VARS`]. `GIT_OPTIONAL_LOCKS=0` stops the
/// background `git status` from taking `.git/index.lock` to refresh the stat
/// cache, which would make the user's own git commands fail meanwhile.
pub(super) fn git_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for var in REDIRECTING_GIT_VARS {
        cmd.env_remove(var);
    }
    // Tests drive this in-process, so without it every commit markcheck
    // makes in a fixture would follow the developer's global git config.
    #[cfg(test)]
    crate::test_support::isolate_git(&mut cmd);
    cmd
}
/// A [`git_command`] whose pathspecs match literally, so `notes[1].md` cannot
/// match `notes1.md`. Not set in `git_command` because hooks run by
/// `git commit` inherit the environment and may rely on globs.
pub(super) fn literal_pathspec_command(repo_root: &Path) -> Command {
    let mut cmd = git_command(repo_root);
    for var in [
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    cmd
}
/// Falls back to stdout when stderr gives no reason: a refused
/// `git commit` ("nothing to commit") explains itself only on stdout.
pub(super) fn command_error(step: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reason = failure_reason(&stderr)
        .or_else(|| failure_reason(&stdout))
        .map(describe_reason)
        .unwrap_or_else(|| "unknown error".to_string());
    format!("{step}: {reason}")
}
/// A `--force-with-lease` rejection is reported by git as `(stale info)`,
/// which says nothing to someone who never ran the push themselves.
fn describe_reason(reason: &str) -> String {
    if reason.starts_with("! ") && reason.ends_with("(stale info)") {
        "the remote branch changed while pushing (stale info)".to_string()
    } else {
        reason.to_string()
    }
}
/// Picks the line that says why a command failed. Hook frameworks print
/// banners first and the failure last, and a refused push opens with its
/// `To <url>` destination, so the first line is rarely the reason.
fn failure_reason(text: &str) -> Option<&str> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:") && !line.starts_with("To "))
        .collect();
    let remote_message = lines.iter().find(|line| {
        line.strip_prefix("remote:")
            .is_some_and(|rest| !rest.trim().is_empty())
    });
    if let Some(rejection) = lines.iter().find(|line| line.starts_with("! ")) {
        return Some(match remote_message {
            Some(message) if rejection.contains("[remote rejected]") => message,
            _ => rejection,
        });
    }
    lines
        .iter()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .or(lines.last())
        .copied()
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
