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
fn a_timed_out_command_gets_sigterm_before_sigkill() {
    // The trap only runs on a catchable signal, so the marker proves SIGTERM
    // arrived first.
    let marker = crate::test_support::unique_temp_path("git-sync-sigterm", "marker", None);
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!(
            "trap 'touch {}; exit 1' TERM; sleep 5 & wait",
            shell_words::quote(marker.to_str().unwrap())
        ),
    ]);

    let started = Instant::now();
    let result = run_with_timeout(cmd, Duration::from_millis(150));

    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(
        !err.to_string().contains("force-killed"),
        "a command that exits on SIGTERM was not force-killed: {err}"
    );
    assert!(marker.exists(), "the child must have received SIGTERM");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must not wait out the grace period once the child exits: {:?}",
        started.elapsed()
    );
    fs::remove_file(&marker).ok();
}
#[test]
fn a_command_ignoring_sigterm_is_force_killed_with_a_lock_hint() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "trap '' TERM; sleep 5"]);

    let started = Instant::now();
    let result = run_with_timeout(cmd, Duration::from_millis(150));
    let elapsed = started.elapsed();

    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(
        err.to_string().contains(".git/index.lock"),
        "a force-kill can leave a lock behind and must say how to clear it: {err}"
    );
    assert!(
        elapsed >= KILL_GRACE && elapsed < KILL_GRACE + Duration::from_secs(2),
        "SIGKILL must follow once the grace period runs out: {elapsed:?}"
    );
}
#[test]
fn git_command_disables_optional_locks() {
    let cmd = git_command(Path::new("."));
    assert!(
        cmd.get_envs()
            .any(|(k, v)| k == "GIT_OPTIONAL_LOCKS" && v == Some(std::ffi::OsStr::new("0"))),
        "a background git status must not take .git/index.lock"
    );
    assert!(
        literal_pathspec_command(Path::new("."))
            .get_envs()
            .any(|(k, v)| k == "GIT_OPTIONAL_LOCKS" && v == Some(std::ffi::OsStr::new("0")))
    );
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

    let mut poisoned = crate::test_support::git();
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

#[test]
fn failure_reason_prefers_a_hooks_last_line_over_its_opening_banner() {
    let stderr =
        "[INFO] Initializing environment\nlint....Failed\n\ntracked.md:3 heading level skipped\n";
    assert_eq!(
        failure_reason(stderr),
        Some("tracked.md:3 heading level skipped")
    );
}
#[test]
fn failure_reason_prefers_gits_own_error_line() {
    let stderr = "warning: something minor\nfatal: not a git repository\nhint: try this\n";
    assert_eq!(failure_reason(stderr), Some("fatal: not a git repository"));
}
#[test]
fn failure_reason_names_a_refused_push_rather_than_its_destination() {
    let stderr = "To /srv/remote.git\n ! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs to '/srv/remote.git'\n";
    assert_eq!(
        failure_reason(stderr),
        Some("! [rejected]        main -> main (non-fast-forward)")
    );
}
#[test]
fn failure_reason_keeps_a_remote_hooks_own_message() {
    let stderr = "remote: \nremote: branch is protected\nTo /srv/remote.git\n ! [remote rejected] main -> main (pre-receive hook declined)\nerror: failed to push some refs\n";
    assert_eq!(failure_reason(stderr), Some("remote: branch is protected"));
}
#[test]
fn failure_reason_finds_nothing_when_the_text_says_nothing() {
    assert_eq!(failure_reason("\n  \nhint: nothing useful\n"), None);
}
fn failed_output(stdout: &str, stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt as _;
    Output {
        status: std::process::ExitStatus::from_raw(1 << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}
#[test]
fn command_error_explains_a_lease_rejection_in_plain_words() {
    let stderr = "To /srv/remote.git\n ! [rejected]        main -> main (stale info)\nerror: failed to push some refs to '/srv/remote.git'\n";
    assert_eq!(
        command_error("git push", &failed_output("", stderr)),
        "git push: the remote branch changed while pushing (stale info)"
    );
}
#[test]
fn command_error_falls_back_to_stdout_when_stderr_is_empty() {
    let stdout = "On branch main\nnothing to commit, working tree clean\n";
    assert_eq!(
        command_error("git commit", &failed_output(stdout, "")),
        "git commit: nothing to commit, working tree clean"
    );
}
#[test]
fn command_error_prefers_stderr_over_stdout() {
    assert_eq!(
        command_error(
            "git commit",
            &failed_output("On branch main\n", "fatal: bad config\n")
        ),
        "git commit: fatal: bad config"
    );
}
#[test]
fn command_error_says_unknown_only_when_neither_stream_explains() {
    assert_eq!(
        command_error("git commit", &failed_output("\n", "hint: x\n")),
        "git commit: unknown error"
    );
}
