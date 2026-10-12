//! Running `git` as a subprocess: environment isolation, output captured to
//! scratch files, and a timeout that kills the whole process group.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Cap for local plumbing commands, which are normally instant.
pub(super) const PLUMBING_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
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

    Ok(Output {
        status,
        stdout: fs::read(&stdout_path).unwrap_or_default(),
        stderr: fs::read(&stderr_path).unwrap_or_default(),
    })
}
/// Its own process group, so a timeout can kill git's descendants too.
fn spawn_in_own_process_group(mut cmd: Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}
fn kill_and_reap(child: &mut Child) {
    #[cfg(unix)]
    {
        // Nulled: `kill` complains on stderr if the group is gone, and markcheck
        // owns the terminal.
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
/// Strips the variables that would make git act on a different repository
/// than the one holding the checklist (hooks and `git rebase --exec` export
/// `GIT_DIR` and `GIT_INDEX_FILE`, for example).
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
/// A [`git_command`] whose pathspecs match literally, so `notes[1].md` cannot
/// match `notes1.md`. Not set in `git_command` because hooks run by
/// `git commit` inherit the environment and may rely on globs.
pub(super) fn literal_pathspec_command(repo_dir: &Path) -> Command {
    let mut cmd = git_command(repo_dir);
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
        // The marker is touched after the timeout: it appearing would mean the
        // child was given up on, not killed.
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
        // Five million bytes is far past any pipe buffer.
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
    /// A descendant outliving the direct child must not leak threads.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lingering_descendant_costs_nothing() {
        let threads = || {
            std::fs::read_dir("/proc/self/task")
                .map(|d| d.count())
                .unwrap_or(0)
        };

        // Warm up first: lazily-spawned runtime threads would count as leaks.
        for _ in 0..3 {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exit 0"]);
            let _ = run_with_timeout(cmd, Duration::from_millis(500));
        }
        std::thread::sleep(Duration::from_millis(300));
        let baseline = threads();

        for _ in 0..20 {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30 & exit 0"]);
            let result = run_with_timeout(cmd, Duration::from_millis(500));
            assert!(result.is_ok(), "the direct child exits successfully");
        }

        // A tolerance because tests run in parallel; a leak of two threads per
        // call would still sit ~40 above baseline.
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
        // `sleep 5 &` backgrounds a descendant that inherits the shell's stdout.
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
        // A background job's stdin defaults to /dev/null, so the descriptor is
        // duplicated first to make the descendant really hold it. The payload is
        // larger than a pipe buffer.
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
        // A command that succeeded must never kill what it spawned.
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

        std::thread::sleep(Duration::from_secs(4).saturating_sub(started.elapsed()));
        assert!(
            marker.exists(),
            "a descendant of a successful command must not be killed"
        );

        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn literal_pathspec_command_matches_literally_whatever_the_ambient_setting() {
        // Checked structurally: setting the variable on the process would race
        // other tests.
        let cmd = literal_pathspec_command(Path::new("."));
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("GIT_LITERAL_PATHSPECS".to_string(), Some("1".to_string()))));
        for var in [
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
            "GIT_DIR",
        ] {
            assert!(
                envs.contains(&(var.to_string(), None)),
                "{var} must be removed"
            );
        }
        assert!(
            !git_command(Path::new("."))
                .get_envs()
                .any(|(k, _)| k == "GIT_LITERAL_PATHSPECS")
        );
    }
    #[test]
    fn git_invocations_ignore_an_ambient_git_environment() {
        // The first half shows the hazard against real git, the second that the
        // defence removes every variable. Structural because `set_var` would race
        // other tests.
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
        for var in ["GIT_AUTHOR_NAME", "GIT_CEILING_DIRECTORIES"] {
            assert!(
                !removed.iter().any(|k| k == var),
                "{var} is the user's to set and must be left alone"
            );
        }

        fs::remove_dir_all(&root).ok();
    }
}
