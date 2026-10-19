//! End-to-end tests that drive the compiled binary under a pseudo-terminal.
//! These exercise `main.rs`'s terminal wiring — the event loop, key
//! dispatch, editor suspend/restore, and reset — which unit tests can't
//! reach headless. Assertions are behavioral (clean exit, file contents)
//! rather than screen-scraping wherever that is possible, to avoid
//! ANSI/timing flakiness. The exception is a message whose *only* observable
//! effect is what the user sees — the hard-link warning below — where
//! scraping is the assertion rather than a shortcut around one; those match
//! a short, stable substring so terminal width and styling can change
//! without breaking them. Run under
//! `cargo llvm-cov` the spawned binary is instrumented, so this also lifts
//! `main.rs` coverage.
//!
//! Nothing here synchronizes on a fixed sleep: every wait polls for an
//! observable condition (the alternate screen being entered, text on the
//! screen, the file's content, the child's exit) up to a deadline, and a
//! deadline that passes fails the test rather than hanging it.
//!
//! Unix-only, like the crate itself (Linux/macOS — see README's
//! Installation section); CI (`check.yml`) runs on `ubuntu-latest` only.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

mod common;

/// Every PTY test spawns a real subprocess, and the `git_sync_*` ones each
/// have their own background commit+push thread on top. Run concurrently on
/// a busy runner, they starved that background thread badly enough to blow
/// even a 90s poll deadline, while the git operations themselves stay fast
/// (~19ms) — pure self-contention. So each [`Session`] holds this for as long
/// as its binary runs, serializing just this file's tests; the unit tests,
/// in their own test binaries, keep full parallelism. `into_inner` recovers
/// the guard even if an earlier holder panicked, since there's no shared
/// data here to have been corrupted — only mutual exclusion matters.
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

fn pty_test_guard() -> MutexGuard<'static, ()> {
    PTY_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_path(hint: &str) -> PathBuf {
    common::unique_temp_path("pty", hint, Some("md"))
}

fn write_file(path: &PathBuf, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

/// Polls `path`'s content every 20ms until `pred` matches it, up to 2s —
/// mirroring `watcher.rs`'s own `wait_until` test helper. Waiting for
/// the actual on-disk condition instead of guessing a fixed delay means the
/// test only ever waits as long as it truly needs to, and isn't a fixed
/// guess that could be too short under CI load.
fn wait_for_file(path: &Path, mut pred: impl FnMut(&str) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(path)
            && pred(&contents)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// One step of a scripted interaction: either a keypress, or a wait for a
/// condition a preceding step is expected to have caused — on the target
/// markdown file's content or on the screen — used before any step, notably
/// `Key("q")`, that depends on that condition already holding.
enum Step {
    Key(&'static str),
    WaitForFile(fn(&str) -> bool),
    /// Waits for text to be on screen, see [`Session::wait_for_screen`].
    WaitForScreen(&'static str),
    /// Simulates another program (e.g. an editor) writing the file while
    /// the binary is running, with a bumped mtime (avoiding
    /// coarse-mtime-resolution flakiness, mirroring `app_tests.rs`'s
    /// `touch_with_new_mtime`). Follow it with a `WaitForScreen` on the
    /// reload's status message.
    ExternalWrite(&'static str),
    /// Simulates the file being deleted out from under the running binary.
    /// Follow it with a `WaitForScreen` on the deletion's status message.
    ExternalDelete,
}

/// Pacing between keys, so two keys never arrive in one read — an `Esc`
/// followed straight away by another key would otherwise parse as one
/// Alt-chord. Pacing input only: nothing waits on it to have taken effect.
const KEY_PACING_MS: u64 = 80;

/// How long anything drawn is given to appear, starting with the first frame.
const SCREEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the binary is given to exit once the script is done. Generous
/// next to `main.rs`'s own 5s wait for a final git sync, but bounded: a
/// binary that never quits fails the test instead of hanging CI.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The binary under test, cut off from the desktop session so a copy
/// can't overwrite the developer's real clipboard: arboard fails and the
/// OSC 52 fallback lands in the pseudo-terminal instead.
fn markcheck_command() -> CommandBuilder {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_markcheck"));
    isolate(&mut cmd);
    cmd
}

/// Isolates a spawned process per [`common::isolated_env`] and from the
/// desktop session.
fn isolate(cmd: &mut CommandBuilder) {
    cmd.env_remove("DISPLAY");
    cmd.env_remove("WAYLAND_DISPLAY");
    for var in common::AMBIENT_GIT_VARS {
        cmd.env_remove(var);
    }
    for (key, value) in common::isolated_env("pty") {
        cmd.env(key, value);
    }
}

/// A fixture `git` command in `dir`, isolated like the binary under test.
fn git(dir: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir);
    for var in common::AMBIENT_GIT_VARS {
        cmd.env_remove(var);
    }
    cmd.envs(common::isolated_env("pty"));
    cmd
}

const ROWS: u16 = 30;
const COLS: u16 = 100;

/// What the terminal shows once `raw` has been drawn, one line per row.
/// ratatui only redraws the cells that changed between frames, so a message
/// is rarely one contiguous run in the byte stream; replaying the cursor
/// moves onto a grid is what makes it matchable as plain text. Covers what
/// the binary and a shell emit: printable characters (each one cell wide),
/// `\r`/`\n`, cursor positioning, screen/line erasure, and entering or
/// leaving the alternate screen (both treated as a clear). Every other
/// escape sequence is skipped.
fn render_screen(raw: &str) -> String {
    let (rows, cols) = (usize::from(ROWS), usize::from(COLS));
    let mut grid = vec![vec![' '; cols]; rows];
    let (mut row, mut col) = (0usize, 0usize);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut action = None;
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            action = Some(c);
                            break;
                        }
                        params.push(c);
                    }
                    let nums: Vec<usize> = params
                        .split(';')
                        .map(|n| n.trim_start_matches('?').parse().unwrap_or(0))
                        .collect();
                    let first = nums.first().copied().unwrap_or(0);
                    match action {
                        Some('H') | Some('f') => {
                            row = first.max(1) - 1;
                            col = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                        }
                        Some('G') => col = first.max(1) - 1,
                        Some('C') => col += first.max(1),
                        Some('J') if first == 2 => grid.iter_mut().for_each(|r| r.fill(' ')),
                        Some('K') if row < rows => grid[row][col.min(cols)..].fill(' '),
                        Some('h') | Some('l') if params == "?1049" => {
                            grid.iter_mut().for_each(|r| r.fill(' '))
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => col = 0,
            '\n' => row += 1,
            c if c.is_control() => {}
            c => {
                if row < rows && col < cols {
                    grid[row][col] = c;
                }
                col += 1;
            }
        }
    }
    grid.iter()
        .map(|r| r.iter().collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The binary running under a pseudo-terminal. A background thread drains
/// the PTY output the whole time — without it the master's buffer fills and
/// the child blocks on its next draw and never sees the quit key — into a
/// buffer the waits below poll. Dropping a session that never reached
/// [`Session::finish`] (a failed assertion mid-script) kills the child, so
/// it can't outlive the test.
struct Session {
    child: Box<dyn Child + Send + Sync>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    drained: Receiver<()>,
    _guard: MutexGuard<'static, ()>,
}

impl Session {
    /// Spawns `cmd` and waits for its first frame: entering the alternate
    /// screen comes right after raw mode is on, so keys sent from then on
    /// reach the event loop rather than the terminal's line editing.
    fn spawn(cmd: CommandBuilder) -> Session {
        let guard = pty_test_guard();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let (done, drained) = channel();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let _ = done.send(());
        });
        let writer = pair.master.take_writer().unwrap();

        let session = Session {
            child,
            master: Some(pair.master),
            writer: Some(writer),
            output,
            drained,
            _guard: guard,
        };
        session.wait_until(SCREEN_TIMEOUT, "the first frame", |raw| {
            raw.contains("\x1b[?1049h")
        });
        session
    }

    fn raw_output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Polls the raw output until `pred` holds, panicking with the output's
    /// tail once `timeout` passes.
    fn wait_until(&self, timeout: Duration, what: &str, mut pred: impl FnMut(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        loop {
            let raw = self.raw_output();
            if pred(&raw) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}; screen was:\n{}", tail(&raw));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for `needle` to be on screen, per [`render_screen`].
    fn wait_for_screen(&self, needle: &str) {
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let screen = render_screen(&self.raw_output());
            if screen.contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {needle:?} on screen; screen was:\n{screen}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn key(&mut self, key: &str) {
        let writer = self
            .writer
            .as_mut()
            .expect("keys are only sent before finish");
        writer.write_all(key.as_bytes()).unwrap();
        writer.flush().unwrap();
        thread::sleep(Duration::from_millis(KEY_PACING_MS));
    }

    fn run(&mut self, md_path: &Path, steps: &[Step]) {
        for step in steps {
            match step {
                Step::Key(k) => self.key(k),
                Step::WaitForFile(pred) => {
                    assert!(
                        wait_for_file(md_path, pred),
                        "timed out waiting for the expected file content"
                    );
                }
                Step::WaitForScreen(needle) => self.wait_for_screen(needle),
                Step::ExternalWrite(contents) => {
                    std::fs::write(md_path, contents).unwrap();
                    let metadata = std::fs::metadata(md_path).unwrap();
                    let new_mtime = metadata.modified().unwrap() + Duration::from_secs(1);
                    std::fs::File::open(md_path)
                        .unwrap()
                        .set_modified(new_mtime)
                        .unwrap();
                }
                Step::ExternalDelete => std::fs::remove_file(md_path).unwrap(),
            }
        }
    }

    /// Waits up to [`EXIT_TIMEOUT`] for the binary to exit, killing it and
    /// failing the test if it doesn't, then returns whether it exited
    /// successfully and everything it drew. See [`Session::finish_within`].
    fn finish(self) -> (bool, String) {
        self.finish_within(EXIT_TIMEOUT)
    }

    fn finish_within(mut self, timeout: Duration) -> (bool, String) {
        self.writer = None;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!(
                    "the binary did not exit within {timeout:?}; screen was:\n{}",
                    tail(&self.raw_output())
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.master = None;
        // The drain ends at EOF once nothing holds the terminal open, which
        // a stray grandchild (an opener the binary launched) could delay.
        // Whatever arrived by the deadline is all the tests look at anyway.
        let _ = self.drained.recv_timeout(Duration::from_secs(5));
        let output = self.raw_output();
        if !status.success() {
            // Surface the child's output (stderr is on the same PTY) so a
            // failure is diagnosable rather than a bare "not successful".
            eprintln!(
                "[pty] child exited unsuccessfully; output tail:\n{}",
                tail(&output)
            );
        }
        (status.success(), output)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn tail(output: &str) -> String {
    let skip = output.chars().count().saturating_sub(400);
    output.chars().skip(skip).collect()
}

/// Spawns the binary in a PTY, runs the steps, waits for it to exit, and
/// returns whether it exited successfully.
fn drive(md_path: &PathBuf, env: &[(&str, &str)], steps: &[Step]) -> bool {
    drive_args(md_path, &[], env, steps)
}

/// Like `drive` but with extra CLI flags before the file argument.
fn drive_args(md_path: &PathBuf, args: &[&str], env: &[(&str, &str)], steps: &[Step]) -> bool {
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(md_path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    run_with_pty(cmd, md_path, steps)
}

/// Like `drive_args`, but for `--new`: passes `--new <path>` instead of the
/// positional FILE argument, since the two conflict on the CLI. `md_path`
/// must not already exist yet — markcheck is expected to create it.
fn drive_new(md_path: &PathBuf, steps: &[Step]) -> bool {
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--new");
    cmd.arg(md_path);
    run_with_pty(cmd, md_path, steps)
}

fn run_with_pty(cmd: CommandBuilder, md_path: &Path, steps: &[Step]) -> bool {
    run_with_pty_capturing(cmd, md_path, steps).0
}

/// As `run_with_pty`, but also hands back everything the binary drew to the
/// terminal, for the few tests that need to assert on what the user actually
/// saw rather than only on the file and the exit status.
fn run_with_pty_capturing(cmd: CommandBuilder, md_path: &Path, steps: &[Step]) -> (bool, String) {
    let mut session = Session::spawn(cmd);
    session.run(md_path, steps);
    session.finish()
}

/// The harness's own deadline: a binary that is never told to quit must
/// fail the test promptly instead of hanging it, and be killed rather than
/// left running.
#[test]
#[should_panic(expected = "did not exit within")]
fn a_binary_that_never_quits_fails_the_test_instead_of_hanging_it() {
    let path = unique_path("never-quits");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg(&path);
    Session::spawn(cmd).finish_within(Duration::from_millis(500));
}

/// A main-thread panic must leave the terminal usable and its message
/// readable: printed after the alternate screen is left, not onto it.
#[test]
fn a_main_thread_panic_prints_its_message_on_the_restored_screen() {
    let path = unique_path("panic-hook");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg(&path);
    cmd.env("MARKCHECK_TEST_PANIC", "1");
    let (ok, output) = Session::spawn(cmd).finish();

    assert!(!ok, "a panic must exit unsuccessfully");
    let left = output
        .find("\x1b[?1049l")
        .unwrap_or_else(|| panic!("the alternate screen was never left; output:\n{output}"));
    let message = output
        .find("forced test panic")
        .unwrap_or_else(|| panic!("the panic message never reached the terminal:\n{output}"));
    assert!(
        message > left,
        "the panic message was printed onto the alternate screen:\n{output}"
    );
    assert!(
        output[left..].contains("Please report this at"),
        "the report pointer is missing:\n{output}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn toggle_then_quit_persists_and_exits_clean() {
    let path = unique_path("toggle");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    // Space toggles the current item, q quits.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `alpha`"),
        "first item toggled: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn undo_then_redo_persist_to_the_file() {
    let path = unique_path("undo");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    // Space toggles alpha done; u undoes it (back to not-done); Ctrl-R redoes
    // it (done again); q quits. The file must reflect the final redo.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("u"),
            Step::WaitForFile(|s| s.contains("[ ] `alpha`")),
            Step::Key("\x12"), // Ctrl-R
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `alpha`"),
        "redo re-applies the toggle: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn editor_flow_reloads_after_edit() {
    let path = unique_path("editor");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");

    // Fake $EDITOR: appends a task to the file it's given, then exits.
    let editor = unique_path("fakeeditor");
    let editor = editor.with_extension("sh");
    write_file(
        &editor,
        "#!/bin/sh\nprintf -- '- [ ] `beta`\\n' >> \"$1\"\n",
    );
    let mut perms = std::fs::metadata(&editor).unwrap().permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor, perms).unwrap();
    }

    // e opens the editor (which appends beta), then q quits.
    let ok = drive(
        &path,
        &[("EDITOR", editor.to_str().unwrap())],
        &[
            Step::Key("e"),
            Step::WaitForFile(|s| s.contains("`beta`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully after editing");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("`beta`"),
        "editor's append is present: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&editor).ok();
}

#[test]
fn editor_spawn_failure_does_not_crash_or_change_the_file() {
    // $EDITOR pointing at a nonexistent binary must surface a clear error
    // (`main.rs`'s `Err(err)` branch) rather than crashing or hanging
    // the TUI. No screen-scraping (per this module's own convention) — the
    // behavioral proof is that the app survives and quits cleanly with the
    // file untouched, since the (nonexistent) editor never ran.
    let path = unique_path("editorspawnfail");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");
    let missing_editor = unique_path("does-not-exist");

    let ok = drive(
        &path,
        &[("EDITOR", missing_editor.to_str().unwrap())],
        &[Step::Key("e"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully after a failed spawn");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        contents, "## Work\n\n- [ ] `alpha`\n",
        "file untouched — the missing editor never ran: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn editor_non_zero_exit_does_not_crash_or_change_the_file() {
    // An editor that exits non-zero (`main.rs`'s `Ok(status) if
    // !status.success()` branch) must also surface an error, not crash.
    let path = unique_path("editorexitfail");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");

    let editor = unique_path("fakeeditorfail");
    let editor = editor.with_extension("sh");
    write_file(&editor, "#!/bin/sh\nexit 1\n");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&editor).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor, perms).unwrap();
    }

    let ok = drive(
        &path,
        &[("EDITOR", editor.to_str().unwrap())],
        &[Step::Key("e"), Step::Key("q")],
    );
    assert!(
        ok,
        "binary should exit successfully after a non-zero editor exit"
    );

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        contents, "## Work\n\n- [ ] `alpha`\n",
        "file untouched — the editor exited before writing anything: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&editor).ok();
}

#[test]
fn reset_flow_clears_all_done() {
    let path = unique_path("reset");
    write_file(&path, "## Work\n\n- [x] `alpha`\n- [x] `beta`\n");

    // R opens the confirm prompt, y confirms the reset, q quits.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("R"),
            Step::Key("y"),
            Step::WaitForFile(|s| !s.contains("[x]")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully after reset");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(!contents.contains("[x]"), "all tasks reset: {contents:?}");
    assert!(
        contents.contains("[ ] `alpha`"),
        "items still present: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn advance_across_lists_exits_clean() {
    let path = unique_path("advance");
    write_file(
        &path,
        "## One\n\n- [ ] `a`\n\n## Two\n\n- [ ] `b`\n- [ ] `c`\n",
    );

    // n cycles search matches, but there's no active search here, so it's a
    // no-op; l walks forward, crossing into the next list at the boundary
    // (list-jumping is Shift-L/Shift-H, sub-section jumping is }/{). Neither
    // should crash; q quits.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("n"),
            Step::Key("l"),
            Step::Key("l"),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    std::fs::remove_file(&path).ok();
}

#[test]
fn live_reload_picks_up_an_external_edit() {
    // Exercises the real watcher.poll_changed() -> reload_if_changed() wiring
    // in main.rs's event loop, not just app.rs's own direct-call unit tests.
    // `G` (jump to the last item) only reaches `beta` if the reload actually
    // happened — otherwise the app still thinks there's a single-item list
    // and `G`/toggle would land on `alpha` instead.
    let path = unique_path("livereload");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");

    let ok = drive(
        &path,
        &[],
        &[
            Step::ExternalWrite("## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n"),
            Step::WaitForScreen("Reloaded: 1 added"),
            Step::Key("G"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x]")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `beta`") && contents.contains("[ ] `alpha`"),
        "reload picked up beta and G landed on it, not the stale alpha: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn file_deletion_blocks_writes_then_reloads_when_restored() {
    // Exercises the real watcher-driven deletion/restoration path through
    // main.rs, not just app.rs's own direct-call unit tests: the toggle
    // attempted while deleted must not crash or resurrect the file, and the
    // later restore-and-toggle must land on the newly-added item, proving
    // the app picked the restored content back up.
    let path = unique_path("deletion");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");

    let ok = drive(
        &path,
        &[],
        &[
            Step::ExternalDelete,
            Step::WaitForScreen("File was deleted"),
            Step::Key(" "), // attempted toggle while deleted: must not crash
            Step::ExternalWrite("## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n"),
            Step::WaitForScreen("File restored"),
            Step::Key("G"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x]")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `beta`") && contents.contains("[ ] `alpha`"),
        "restored file was reloaded and the toggle landed on the new item: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn opens_through_a_symlink_writes_through_to_the_target() {
    // Exercises the real startup canonicalization -> write-through ->
    // watch-the-target's-parent-directory chain end to end, as main.rs
    // actually wires it — writer.rs/watcher.rs each only unit-test symlink
    // behavior in isolation.
    //
    // Two items, not one: toggling only `alpha` must not complete the whole
    // list, which would otherwise swap `q` for the reset-before-quit prompt
    // (unrelated to this test) instead of exiting.
    let target = unique_path("symlink-target");
    write_file(&target, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    let link = unique_path("symlink-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let ok = drive(
        &link,
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink itself must survive the write"
    );
    let contents = std::fs::read_to_string(&target).unwrap();
    assert!(
        contents.contains("[x] `alpha`"),
        "the write went through to the real target: {contents:?}"
    );

    std::fs::remove_file(&link).ok();
    std::fs::remove_file(&target).ok();
}

#[test]
fn no_mouse_flag_starts_and_exits_cleanly() {
    let path = unique_path("no-mouse");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");

    let ok = drive_args(&path, &["--no-mouse"], &[], &[Step::Key("q")]);
    assert!(ok, "binary should exit successfully with --no-mouse");

    std::fs::remove_file(&path).ok();
}

#[test]
fn mouse_wheel_and_click_do_not_crash() {
    // main.rs's Event::Mouse dispatch (ScrollUp/ScrollDown ->
    // handle_scroll_up/down, Down(Left) -> handle_left_click) has no PTY
    // coverage otherwise. Raw SGR mouse sequences (ESC [ < Cb ; Cx ; Cy
    // M/m — wheel up is Cb 64, wheel down 65, plain left button 0) can be
    // written straight into the PTY like any other input, no real terminal
    // required. Coordinates are just inside the 100x30 PTY, not aimed at any
    // specific on-screen target — this is a crash/hang smoke test, not a
    // click-precision one (that's covered headlessly in ui/mod.rs's tests).
    let path = unique_path("mouse");
    write_file(
        &path,
        "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n- [ ] `gamma`\n",
    );

    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("\x1b[<64;40;15M"), // wheel up
            Step::Key("\x1b[<65;40;15M"), // wheel down
            Step::Key("\x1b[<0;40;15M"),  // left button down
            Step::Key("\x1b[<0;40;15m"),  // left button up
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully after mouse input");

    std::fs::remove_file(&path).ok();
}

#[test]
fn bracketed_paste_on_the_checklist_does_not_run_its_characters() {
    // A paste arrives wrapped in ESC [ 200~ … ESC [ 201~ once bracketed paste
    // is on; unwrapped, its spaces would toggle tasks and its `q` would quit.
    let path = unique_path("paste");
    let original = "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n";
    write_file(&path, original);

    let ok = drive(
        &path,
        &[],
        &[Step::Key("\x1b[200~s x q R y\r\x1b[201~"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully after a paste");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    std::fs::remove_file(&path).ok();
}

#[test]
fn start_key_persists_started_marker() {
    let path = unique_path("started");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    // s marks the current task started (writes [/] but does not advance),
    // q quits (section not complete, since started != done).
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("s"),
            Step::WaitForFile(|s| s.contains("[/] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[/] `alpha`"),
        "first task marked started: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn startup_selects_first_undone_item() {
    let path = unique_path("startup");
    // First item already done; startup should land on the first *undone*
    // item (beta), not item 0.
    write_file(
        &path,
        "## Work\n\n- [x] `alpha`\n- [ ] `beta`\n- [ ] `gamma`\n",
    );

    // Space toggles the selected item; if the cursor started on beta it
    // becomes done while alpha stays done. q quits (section not all done).
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `beta`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `alpha`"),
        "alpha stays done — cursor did not start on it: {contents:?}"
    );
    assert!(
        contents.contains("[x] `beta`"),
        "beta toggled — cursor started on the first undone item: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn quit_when_all_done_prompts_reset_then_resets() {
    let path = unique_path("quitreset");
    write_file(&path, "## Work\n\n- [x] `alpha`\n- [x] `beta`\n");

    // Everything is done: q opens the quit-reset prompt (does not quit),
    // y resets all tasks and quits.
    let ok = drive(&path, &[], &[Step::Key("q"), Step::Key("y")]);
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        !contents.contains("[x]"),
        "quit-reset reset all tasks: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

/// The OSC 52 clipboard write for `alpha`, which is what an auto-copy of the
/// `alpha` card emits in the PTY, where no system clipboard is reachable.
const ALPHA_OSC52: &str = "\x1b]52;c;YWxwaGE=\x07";

const AUTO_COPY_CHECKLIST: &str = "## Work\n\n- [ ] `alpha`\n- [ ] plain item\n";

/// Navigates off the `alpha` card onto one with no code and back, then
/// quits, returning the exit status and everything drawn to the terminal.
fn drive_auto_copy_navigation(
    path: &PathBuf,
    args: &[&str],
    env: &[(&str, &str)],
    copies: bool,
) -> (bool, String) {
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let steps: &[Step] = if copies {
        &[
            Step::Key("l"),
            Step::Key("h"),
            Step::WaitForScreen("Sent to clipboard"),
            Step::Key("q"),
        ]
    } else {
        &[Step::Key("l"), Step::Key("h"), Step::Key("q")]
    };
    run_with_pty_capturing(cmd, path, steps)
}

#[test]
fn auto_copy_flag_copies_the_card_navigated_onto() {
    let path = unique_path("autocopy");
    write_file(&path, AUTO_COPY_CHECKLIST);

    // Landing on the no-code card must not crash; landing back on `alpha`
    // must copy it.
    let (ok, output) = drive_auto_copy_navigation(&path, &["--auto-copy"], &[], true);
    assert!(ok, "binary should exit successfully with --auto-copy");
    assert!(
        output.contains(ALPHA_OSC52),
        "--auto-copy did not copy `alpha`; output tail:\n{}",
        tail(&output)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn navigation_copies_nothing_without_auto_copy() {
    let path = unique_path("no-autocopy");
    write_file(&path, AUTO_COPY_CHECKLIST);

    let (ok, output) = drive_auto_copy_navigation(&path, &[], &[], false);
    assert!(ok, "binary should exit successfully");
    assert!(
        !output.contains("\x1b]52;"),
        "navigation copied without auto-copy enabled:\n{}",
        tail(&output)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn config_file_sets_auto_copy_default_without_the_flag() {
    // A config file's `auto_copy = true` must reach the same runtime
    // behavior as passing --auto-copy, with no flag on the command line.
    let path = unique_path("autocopy-cfg");
    write_file(&path, AUTO_COPY_CHECKLIST);

    let xdg_config_home = unique_path("autocopy-cfg-xdg");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        "auto_copy = true\n",
    )
    .unwrap();

    let (ok, output) = drive_auto_copy_navigation(
        &path,
        &[],
        &[("XDG_CONFIG_HOME", xdg_config_home.to_str().unwrap())],
        true,
    );
    assert!(
        ok,
        "binary should exit successfully with auto_copy set via config"
    );
    assert!(
        output.contains(ALPHA_OSC52),
        "the config's auto_copy did not copy `alpha`; output tail:\n{}",
        tail(&output)
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&xdg_config_home).ok();
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = git(dir).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

#[test]
fn git_sync_flag_commits_and_pushes_after_toggle() {
    // `--git-sync` should commit and push the file's change after a
    // toggle, when the file lives in a git repo — end-to-end through
    // `main.rs`'s wiring (flag parsing, `GitSync::detect`, and the poll/
    // request loop), which the `git_sync.rs` unit tests can't reach since
    // they drive `GitSync` directly rather than through the compiled binary.
    let root = unique_path("gitsync");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    // Two items, not one: toggling only `alpha` must not complete the whole
    // list, which would otherwise swap `q` for the reset-before-quit prompt
    // (unrelated to this test) instead of exiting.
    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    let ok = drive_args(
        &path,
        &["--git-sync"],
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully with --git-sync");

    // The commit+push happens on a background thread that can outlive the
    // child process (detached, not joined on quit), so poll the
    // bare remote rather than assuming it's already landed.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut subject = String::new();
    while Instant::now() < deadline {
        let out = git(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject != "init" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(subject, "checklist.md: Check \"alpha\"");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_sync_children_cannot_reach_markcheck_s_terminal() {
    // A credential, host-key or pinentry prompt opens /dev/tty directly, so
    // a hook doing the same stands in for all of them. Under the PTY the
    // terminal exists; git sync's children must still be unable to open it.
    let root = unique_path("gitsync-no-tty");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);

    let marker = root.join("tty-result");
    let hook = work.join(".git/hooks/pre-commit");
    write_file(
        &hook,
        &format!(
            "#!/bin/sh\nif (: < /dev/tty) 2>/dev/null; then r=opened; else r=refused; fi\necho $r > '{}'\n",
            marker.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let ok = drive_args(
        &path,
        &["--git-sync"],
        &[],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully with --git-sync");

    let deadline = Instant::now() + Duration::from_secs(90);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    let result = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        result.trim(),
        "refused",
        "a git-sync child must have no controlling terminal"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A repository whose `pre-commit` hook records its PID in `hook-pid`
/// beside the work tree, then runs `body`, so a test can catch a sync
/// mid-commit. Returns the checklist and the PID file.
fn repo_with_a_slow_hook(hint: &str, body: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = unique_path(hint);
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);
    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);

    let pid_file = root.join("hook-pid");
    let hook = work.join(".git/hooks/pre-commit");
    write_file(
        &hook,
        &format!(
            "#!/bin/sh\necho $$ > '{}.tmp'\nmv '{0}.tmp' '{0}'\n{body}\n",
            pid_file.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (root, path, pid_file)
}

fn git_sync_session(path: &Path) -> Session {
    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.args(["--git-sync-delay", "0"]);
    cmd.arg(path);
    Session::spawn(cmd)
}

/// Waits for the hook to have written its PID, then returns it.
fn wait_for_hook_pid(pid_file: &Path) -> String {
    let deadline = Instant::now() + SCREEN_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(pid) = std::fs::read_to_string(pid_file) {
            return pid.trim().to_string();
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("the pre-commit hook never started");
}

/// Whether `pid` is still running, polled for a while first, since a killed
/// hook's parent may take a moment to be reaped.
fn still_running_after_a_while(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        let alive = match stat {
            Ok(stat) => !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z'),
            Err(_) => false,
        };
        if !alive {
            return false;
        }
        if Instant::now() >= deadline {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn temp_indexes_left(work: &Path) -> Vec<String> {
    std::fs::read_dir(work.join(".git"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("markcheck-index-"))
        .collect()
}

#[test]
fn quitting_during_a_sync_says_it_waits_and_quits_once_it_lands() {
    let (root, path, pid_file) = repo_with_a_slow_hook("gitsync-quit-wait", "sleep 2");
    let work = path.parent().unwrap().to_path_buf();

    let mut session = git_sync_session(&path);
    session.key(" ");
    wait_for_hook_pid(&pid_file);
    session.key("q");
    session.wait_for_screen("Waiting for git sync");
    let (ok, _) = session.finish();
    assert!(ok, "binary should exit successfully once the sync lands");

    let out = git(&work)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "checklist.md: Check \"alpha\"",
        "the waited-for sync must have committed"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_second_q_abandons_the_sync_and_leaves_nothing_behind() {
    let (root, path, pid_file) = repo_with_a_slow_hook("gitsync-abandon", "sleep 30");
    let work = path.parent().unwrap().to_path_buf();

    let mut session = git_sync_session(&path);
    session.key(" ");
    let hook_pid = wait_for_hook_pid(&pid_file);
    session.key("q");
    session.wait_for_screen("press q again to abandon");
    session.key("q");
    let (ok, output) = session.finish_within(Duration::from_secs(60));
    assert!(ok, "binary should exit successfully after abandoning");

    assert!(
        !still_running_after_a_while(&hook_pid),
        "the hook must not outlive markcheck"
    );
    assert_eq!(temp_indexes_left(&work), Vec::<String>::new());
    let tail = output.rsplit("\x1b[?1049l").next().unwrap_or_default();
    assert!(
        tail.contains("markcheck: git sync"),
        "the abandoned sync is reported after the TUI closes:\n{tail}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn sigterm_mid_sync_quits_cleanly_and_leaves_nothing_behind() {
    let (root, path, pid_file) = repo_with_a_slow_hook("gitsync-sigterm", "sleep 30");
    let work = path.parent().unwrap().to_path_buf();

    let mut session = git_sync_session(&path);
    session.key(" ");
    let hook_pid = wait_for_hook_pid(&pid_file);
    let markcheck_pid = session.child.process_id().unwrap();
    let status = std::process::Command::new("kill")
        .args(["-TERM", &markcheck_pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let (ok, output) = session.finish_within(Duration::from_secs(60));
    assert!(ok, "SIGTERM must take the normal quit path");

    assert!(output.contains("\x1b[?1049l"), "the terminal is restored");
    assert!(
        !still_running_after_a_while(&hook_pid),
        "the hook must not outlive markcheck"
    );
    assert_eq!(temp_indexes_left(&work), Vec::<String>::new());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_burst_of_toggles_waits_then_becomes_one_commit_on_quit() {
    // The default quiet period holds the sync; the title bar says so, and
    // quitting sends it at once as one commit.
    let (root, path, _pid_file) = repo_with_a_slow_hook("gitsync-batch", "true");
    let work = path.parent().unwrap().to_path_buf();

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.arg(&path);
    let mut session = Session::spawn(cmd);
    session.key(" ");
    session.key("s");
    session.wait_for_screen("git · pending");
    session.key("q");
    let (ok, _) = session.finish();
    assert!(ok, "binary should exit successfully");

    let out = git(&work)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "checklist.md: 2 changes"
    );
    let count = git(&work)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn quitting_right_after_a_toggle_whose_sync_fails_says_so_after_the_tui_closes() {
    // The final toggle's sync runs during the exit wait, after the TUI's own
    // error line can no longer be seen, so its failure must reach stderr. The
    // hook sleeps first so the sync is still in flight when `q` lands.
    let root = unique_path("gitsync-exit-report");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);

    let hook = work.join(".git/hooks/pre-commit");
    write_file(
        &hook,
        "#!/bin/sh\nsleep 1\necho 'pre-commit hook rejected this' >&2\nexit 1\n",
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.arg(&path);
    let (ok, output) = run_with_pty_capturing(
        cmd,
        &path,
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );

    assert!(ok, "binary should still exit successfully");
    let tail = output.rsplit("\x1b[?1049l").next().unwrap_or_default();
    assert!(
        tail.contains("markcheck: git sync failed:") && tail.contains("not committed"),
        "the failure must be printed after the TUI closes; output after it was:\n{tail}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_sync_does_not_commit_uncommitted_changes_just_because_the_file_was_opened() {
    // Deep review, round 3. Startup asks git-sync to catch up a pending
    // push; that used to be expressed as an ordinary *content* request
    // carrying the file's current disk bytes, which satisfies every
    // `run_sync` guard by construction. So opening a checklist that had
    // ordinary uncommitted editor changes committed and pushed them --
    // no toggle, no keypress but the quit -- under the message
    // `Catch up a pending push`, which doesn't describe them at all.
    //
    // The unit tests around `catch_up_push` can't cover this: the function
    // didn't exist before the fix, so only driving the real binary
    // exercises the startup path that actually regressed.
    let root = unique_path("gitsync-open-only");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    // The user edits the checklist in an editor and leaves it uncommitted —
    // ordinary work in progress, nothing to do with markcheck.
    let edited = "## Work\n\n- [x] `alpha`\n- [ ] `beta`\n";
    write_file(&path, edited);

    // Open it and quit. Both tasks stay incomplete, so `q` exits directly
    // rather than raising the reset-before-quit prompt.
    let ok = drive_args(&path, &["--git-sync"], &[], &[Step::Key("q")]);
    assert!(ok, "binary should exit successfully with --git-sync");

    // The catch-up runs on a background thread the quit path waits on, so
    // with the binary gone a would-be commit has already landed if it was
    // ever going to.
    for dir in [&work, &remote] {
        let out = git(dir)
            .args(["log", "--format=%s", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "init",
            "opening a checklist must never commit or publish anything, in {dir:?}"
        );
    }

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        edited,
        "the uncommitted edit stays exactly as the user left it"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_sync_paths_config_auto_activates_without_the_flag() {
    // git_sync_paths shares main.rs's flag-merge logic with the other config
    // keys, but it's additive (OR'd with --git-sync) rather than a plain
    // CLI-overrides-config default, and only auto_copy's config path had PTY
    // coverage before this. A path prefix match must turn git-sync on with
    // no --git-sync on the command line at all.
    let root = unique_path("gitsync-cfg");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    let xdg_config_home = root.join("xdg-config");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        format!("git_sync_paths = [{:?}]\n", work.to_str().unwrap()),
    )
    .unwrap();

    let ok = drive_args(
        &path,
        &[], // no --git-sync flag: the config path prefix must be enough
        &[("XDG_CONFIG_HOME", xdg_config_home.to_str().unwrap())],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut subject = String::new();
    while Instant::now() < deadline {
        let out = git(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject != "init" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        subject, "checklist.md: Check \"alpha\"",
        "git_sync_paths must auto-activate the sync with no --git-sync flag"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A repository nested inside `notes/` that arrived from elsewhere: its
/// config runs `uploadpack` on any `ls-remote`, its hook runs on any commit,
/// and an unpushed commit invites the startup push. Returns the checklist,
/// the `notes/` prefix and the two marker files they would create.
fn nested_repository_with_hostile_config(
    hint: &str,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = unique_path(hint);
    let notes = root.join("notes");
    let nested = notes.join("imported");
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    run_git(&notes, &["init", "-q", "-b", "main"]);
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&nested, &["init", "-q", "-b", "main"]);
    run_git(&nested, &["config", "user.email", "test@example.com"]);
    run_git(&nested, &["config", "user.name", "test"]);
    let path = nested.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&nested, &["add", "checklist.md"]);
    run_git(&nested, &["commit", "-q", "-m", "init"]);
    run_git(
        &nested,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&nested, &["push", "-q", "-u", "origin", "main"]);
    write_file(&path, "## Work\n\n- [/] `alpha`\n- [ ] `beta`\n");
    run_git(
        &nested,
        &[
            "commit",
            "-q",
            "-am",
            "checklist.md: Start",
            "-m",
            "Synced-by: markcheck",
        ],
    );

    let fetched = root.join("uploadpack-ran");
    let committed = root.join("hook-ran");
    run_git(
        &nested,
        &[
            "config",
            "remote.origin.uploadpack",
            &format!("touch '{}'; git-upload-pack", fetched.display()),
        ],
    );
    let hook = nested.join(".git/hooks/pre-commit");
    write_file(
        &hook,
        &format!("#!/bin/sh\ntouch '{}'\n", committed.display()),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (root, path, notes, fetched, committed)
}

fn config_home_with_git_sync_path(root: &Path, entry: &str) -> PathBuf {
    let xdg_config_home = root.join("xdg-config");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        format!("git_sync_paths = [{entry:?}]\n"),
    )
    .unwrap();
    xdg_config_home
}

#[test]
fn a_plain_git_sync_paths_entry_never_runs_git_in_a_nested_repository() {
    let (root, path, notes, fetched, committed) =
        nested_repository_with_hostile_config("gitsync-nested");
    let xdg = config_home_with_git_sync_path(&root, notes.to_str().unwrap());

    let ok = drive_args(
        &path,
        &[],
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
        &[
            Step::WaitForScreen("nested inside git_sync_paths"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");
    assert!(
        !fetched.exists(),
        "no ls-remote may run in the nested repository"
    );
    assert!(
        !committed.exists(),
        "no commit may run in the nested repository"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_double_star_git_sync_paths_entry_covers_a_nested_repository() {
    let (root, path, notes, _fetched, committed) =
        nested_repository_with_hostile_config("gitsync-nested-star");
    let entry = format!("{}/**", notes.display());
    let xdg = config_home_with_git_sync_path(&root, &entry);

    let ok = drive_args(
        &path,
        &[],
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");
    assert!(
        committed.exists(),
        "the opted-in nested repository is synced"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_sync_paths_config_matches_through_a_symlinked_prefix() {
    // git_sync_paths is compared against the file's *canonicalized* path
    // (main.rs resolves symlinks before this check), but the configured
    // prefix itself used to be compared as written — so a prefix reached via
    // a symlink never matched, even though the file genuinely lives under
    // it. Regression test: configure the prefix as a symlink to the real
    // work dir; sync must still auto-activate.
    let root = unique_path("gitsync-symlink-cfg");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    let alias = root.join("work-alias");
    std::os::unix::fs::symlink(&work, &alias).unwrap();

    let xdg_config_home = root.join("xdg-config");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        format!("git_sync_paths = [{:?}]\n", alias.to_str().unwrap()),
    )
    .unwrap();

    let ok = drive_args(
        &path,
        &[], // no --git-sync flag: the (symlinked) config path prefix must be enough
        &[("XDG_CONFIG_HOME", xdg_config_home.to_str().unwrap())],
        &[
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut subject = String::new();
    while Instant::now() < deadline {
        let out = git(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject != "init" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        subject, "checklist.md: Check \"alpha\"",
        "a symlinked git_sync_paths prefix must still auto-activate the sync"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn git_sync_commits_an_editor_edit_without_a_further_toggle() {
    // A manual edit via `e` used to sit uncommitted until (or be missed
    // entirely, if there wasn't) some later markcheck-driven toggle — the
    // editor-reload path never queued a git-sync request the way
    // commit_write does. Regression test: edit only, no toggle at all, and
    // the edit must still land in the remote's log.
    let root = unique_path("gitsync-editor");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    // Fake $EDITOR: appends a task to the file it's given, then exits.
    let editor = unique_path("fakeeditor-gitsync");
    let editor = editor.with_extension("sh");
    write_file(
        &editor,
        "#!/bin/sh\nprintf -- '- [ ] `beta`\\n' >> \"$1\"\n",
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&editor).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor, perms).unwrap();
    }

    let ok = drive_args(
        &path,
        &["--git-sync"],
        &[("EDITOR", editor.to_str().unwrap())],
        &[
            Step::Key("e"),
            Step::WaitForFile(|s| s.contains("`beta`")),
            Step::Key("q"), // no toggle at all — the edit alone must sync
        ],
    );
    assert!(ok, "binary should exit successfully");

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut subject = String::new();
    while Instant::now() < deadline {
        let out = git(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject != "init" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // Only the editor's file name goes into the message, never its directory.
    let program = editor.file_name().unwrap().to_str().unwrap();
    let expected = format!("checklist.md: Edited in {program}");
    assert_eq!(
        subject, expected,
        "the editor edit must be committed and pushed with no toggle at all"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_file(&editor).ok();
}

#[test]
fn git_sync_makes_no_commit_when_the_editor_touches_the_file_without_changing_it() {
    // request_external_edit_sync() fires whenever reload_if_changed() reports
    // a real reload (its mtime/size check), which is coarser than "the
    // content actually differs" — many editors rewrite the file on save even
    // with no changes, bumping its mtime. The commit itself is still gated
    // on run_sync's own `git status --porcelain` check, which is
    // content-based (git hashes the blob), not mtime-based — so a no-op
    // save must queue a sync request but produce no commit at all.
    let root = unique_path("gitsync-editor-noop");
    let remote = root.join("remote.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    // Fake $EDITOR: touches the file's mtime forward without touching its
    // content at all — a "no-op save," which some real editors do too.
    let editor = unique_path("fakeeditor-gitsync-noop");
    let editor = editor.with_extension("sh");
    write_file(&editor, "#!/bin/sh\ntouch -d '+2 seconds' \"$1\"\n");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&editor).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&editor, perms).unwrap();
    }

    let ok = drive_args(
        &path,
        &["--git-sync"],
        &[("EDITOR", editor.to_str().unwrap())],
        &[Step::Key("e"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully");

    // The binary has exited, and its exit path waits for an in-flight sync
    // to settle, so a would-be (wrong) commit+push has already landed by
    // now if it was ever going to.
    let out = git(&remote).args(["log", "--format=%s"]).output().unwrap();
    let log = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        log.trim(),
        "init",
        "a no-op save must not produce any commit: {log:?}"
    );

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_file(&editor).ok();
}

#[test]
fn git_sync_reports_when_the_file_is_untracked() {
    // Unlike the other PTY tests, this one *does* inspect the rendered
    // screen text (ANSI-stripped) rather than staying purely behavioral —
    // there's no file-content or git-log side effect to check instead, since
    // the whole point is a status message that appears on screen and
    // nowhere else. A silent skip here used to be indistinguishable from
    // git-sync simply not working at all.
    let root = unique_path("gitsync-untracked");
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "test@example.com"]);
    run_git(&root, &["config", "user.name", "test"]);
    // Deliberately never `git add` this file.
    let path = root.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.arg(&path);
    let ok = run_with_pty(
        cmd,
        &path,
        &[
            Step::Key(" "), // toggle: triggers a sync attempt
            Step::WaitForScreen("Git sync skipped: file is not tracked in git"),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let status_out = git(&root).args(["status", "--porcelain"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&status_out.stdout).contains("?? checklist.md"),
        "the file must still never be added"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// `--git-sync` on a file outside any repository used to leave git-sync off
/// with no cue but a missing title-bar section, which isn't drawn at all on
/// a narrow terminal.
#[test]
fn git_sync_flag_outside_a_repo_says_why_it_is_off() {
    let root = unique_path("gitsync-no-repo");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    let ok = drive_args(
        &path,
        &["--git-sync"],
        &[],
        &[
            Step::WaitForScreen("Git sync off: the file is not in a git repository"),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    std::fs::remove_dir_all(&root).ok();
}

/// A `~/...` entry is expanded against `$HOME`, as a shell would, and one
/// that names nothing is reported instead of silently never matching.
#[test]
fn git_sync_paths_expands_tilde_and_reports_a_missing_entry() {
    let home = unique_path("gitsync-tilde-home");
    let work = home.join("checklists");
    std::fs::create_dir_all(&work).unwrap();
    run_git(&work, &["init", "-q", "-b", "main"]);
    // Never `git add`ed, so a toggle's "skipped" message proves git-sync is on
    // without needing a remote.
    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    let xdg_config_home = home.join("xdg-config");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        "git_sync_paths = [\"~/checklsts\", \"~/checklists\"]\n",
    )
    .unwrap();

    let ok = drive_args(
        &path,
        &[],
        &[
            ("HOME", home.to_str().unwrap()),
            ("XDG_CONFIG_HOME", xdg_config_home.to_str().unwrap()),
        ],
        &[
            Step::WaitForScreen("git_sync_paths: ~/checklsts does not exist"),
            Step::Key(" "),
            Step::WaitForScreen("Git sync skipped: file is not tracked in git"),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn git_sync_reports_push_failure_then_retries_and_succeeds() {
    // External review (twice): whether `main.rs` actually handles
    // `SyncOutcome::CommittedNotPushed` and actually calls
    // `retry_push_if_due` can't be answered by `git_sync.rs`'s unit tests,
    // which drive `GitSync` directly rather than through the compiled
    // binary and its main loop — exactly the gap that let two review
    // rounds in a row plausibly (if incorrectly) doubt that wiring exists.
    // This drives it for real: an initial toggle whose push fails (broken
    // remote), confirms the "will retry" message actually appears on
    // screen, then fixes the remote out-of-band and waits out a real
    // `PUSH_RETRY_INTERVAL` for the automatic retry to actually land the
    // push — proving both pieces of wiring, not just that the underlying
    // `GitSync` methods work in isolation.
    let root = unique_path("gitsync-retry");
    let remote = root.join("remote.git");
    let broken_remote = root.join("does-not-exist.git");
    let work = root.join("work");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    run_git(&work, &["init", "-q", "-b", "main"]);
    run_git(&work, &["config", "user.email", "test@example.com"]);
    run_git(&work, &["config", "user.name", "test"]);

    let path = work.join("checklist.md");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    run_git(&work, &["add", "checklist.md"]);
    run_git(&work, &["commit", "-q", "-m", "init"]);
    // Establish a *real* upstream first — `push -u` against the good remote,
    // which is what creates the remote-tracking ref. Setting only
    // `branch.main.remote`/`.merge` by hand is not equivalent: git-sync
    // resolves `@{u}` rather than reading the config (they disagree when the
    // tracking ref does not exist), and with no resolvable upstream it
    // refuses to publish at all — so the retry this test exists to prove
    // would never be reached.
    run_git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_git(&work, &["push", "-q", "-u", "origin", "main"]);

    // *Now* point origin at a path that will never resolve, so the first
    // sync attempt's push fails deterministically and fast (no real network
    // hang) while the upstream stays resolvable throughout.
    run_git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            broken_remote.to_str().unwrap(),
        ],
    );

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.args(["--git-sync-delay", "0"]);
    cmd.arg(&path);
    let mut session = Session::spawn(cmd);
    session.key(" "); // toggle: commits locally, push fails
    // Reported, proving CommittedNotPushed is handled.
    session.wait_for_screen("Committed, not pushed");

    // Fix the remote while the app is still running — the background
    // worker isn't touched, only the config it reads on its next attempt.
    run_git(
        &work,
        &["remote", "set-url", "origin", remote.to_str().unwrap()],
    );

    // Wait out a real PUSH_RETRY_INTERVAL for retry_push_if_due to fire on
    // its own — the actual behavior under test, not simulated.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut subject = String::new();
    while Instant::now() < deadline {
        let out = git(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject == "checklist.md: Check \"alpha\"" {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        subject, "checklist.md: Check \"alpha\"",
        "the automatic retry must land the push without any further user input"
    );

    session.key("q");
    let (ok, _) = session.finish();
    assert!(ok, "binary should exit successfully");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn search_jumps_to_task_then_toggles_it() {
    let path = unique_path("search");
    write_file(
        &path,
        "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n- [ ] `gamma`\n",
    );

    // `/` opens search; typing "gamma" jumps the cursor to that task; Enter
    // commits on it; Space toggles it done; q quits. If the search key path
    // works end-to-end, only gamma is checked.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("/"),
            Step::Key("gamma"),
            Step::Key("\r"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `gamma`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `gamma`"),
        "search jumped to gamma and toggled it: {contents:?}"
    );
    assert!(
        contents.contains("[ ] `alpha`") && contents.contains("[ ] `beta`"),
        "other tasks untouched: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn list_picker_jumps_to_filtered_task_then_toggles_it() {
    let path = unique_path("picker");
    write_file(
        &path,
        "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n- [ ] `gamma`\n",
    );

    // `T` opens the go-to-task overlay; typing "gamma" filters to it; Enter
    // jumps there; Space toggles it done; q quits. Only gamma should be checked.
    let ok = drive(
        &path,
        &[],
        &[
            Step::Key("T"),
            Step::Key("gamma"),
            Step::Key("\r"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `gamma`")),
            Step::Key("q"),
        ],
    );
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `gamma`"),
        "picker jumped to gamma and toggled it: {contents:?}"
    );
    assert!(
        contents.contains("[ ] `alpha`") && contents.contains("[ ] `beta`"),
        "other tasks untouched: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn open_link_spawns_the_configured_browser() {
    use std::os::unix::fs::PermissionsExt;

    let recorded = unique_path("linkopened");
    let browser = unique_path("fakebrowser");
    // A fake $BROWSER that records the URL it is handed.
    std::fs::write(
        &browser,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", recorded.display()),
    )
    .unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();

    let md = unique_path("linkmd");
    write_file(&md, "## Work\n\n- [ ] see [rb](https://example.com/rb)\n");

    // `o` opens the current card's link; the opener runs detached.
    let ok = drive_args(
        &md,
        &[],
        &[("BROWSER", browser.to_str().unwrap())],
        &[Step::Key("o"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully");

    // The opener is detached from the TUI's own lifetime, so its write to
    // `recorded` can land after the binary has already exited — poll for it
    // rather than guessing how long that takes.
    assert!(
        wait_for_file(&recorded, |s| !s.is_empty()),
        "timed out waiting for the detached opener to record the URL"
    );
    let got = std::fs::read_to_string(&recorded).unwrap_or_default();
    assert_eq!(
        got, "https://example.com/rb",
        "browser received the link URL"
    );

    std::fs::remove_file(&md).ok();
    std::fs::remove_file(&browser).ok();
    std::fs::remove_file(&recorded).ok();
}

#[test]
fn open_link_tries_each_browser_in_a_colon_list_with_url_placeholder() {
    use std::os::unix::fs::PermissionsExt;

    let recorded = unique_path("linklist");
    let browser = unique_path("fakebrowser-list");
    // Records its argument count and arguments, so a stray literal `%s` or a
    // doubled URL would show up.
    std::fs::write(
        &browser,
        format!(
            "#!/bin/sh\nprintf '%s|%s' \"$#\" \"$*\" > {}\n",
            recorded.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();

    let md = unique_path("linklistmd");
    write_file(&md, "## Work\n\n- [ ] see [rb](https://example.com/rb)\n");

    // The first entry doesn't exist, so the second must be tried.
    let value = format!(
        "/nonexistent/markcheck-browser:{} --tab %s",
        browser.display()
    );
    let ok = drive_args(
        &md,
        &[],
        &[("BROWSER", value.as_str())],
        &[Step::Key("o"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully");

    assert!(
        wait_for_file(&recorded, |s| !s.is_empty()),
        "timed out waiting for the second opener to record the URL"
    );
    let got = std::fs::read_to_string(&recorded).unwrap_or_default();
    assert_eq!(got, "2|--tab https://example.com/rb");

    std::fs::remove_file(&md).ok();
    std::fs::remove_file(&browser).ok();
    std::fs::remove_file(&recorded).ok();
}

#[test]
fn open_link_reports_an_opener_that_exits_with_an_error() {
    use std::os::unix::fs::PermissionsExt;

    let browser = unique_path("failingbrowser");
    std::fs::write(&browser, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();

    let md = unique_path("linkfailmd");
    write_file(&md, "## Work\n\n- [ ] see [rb](https://example.com/rb)\n");

    let ok = drive_args(
        &md,
        &[],
        &[("BROWSER", browser.to_str().unwrap())],
        &[
            Step::Key("o"),
            Step::WaitForScreen("exited with code 3"),
            Step::Key("q"),
        ],
    );
    assert!(
        ok,
        "the failed opener is reported, then the binary exits cleanly"
    );

    std::fs::remove_file(&md).ok();
    std::fs::remove_file(&browser).ok();
}

#[test]
fn open_link_without_a_browser_or_display_copies_the_link_instead() {
    let md = unique_path("linkheadlessmd");
    write_file(&md, "## Work\n\n- [ ] see [rb](https://example.com/rb)\n");

    let ok = drive_args(
        &md,
        &[],
        &[("BROWSER", ""), ("DISPLAY", ""), ("WAYLAND_DISPLAY", "")],
        &[
            Step::Key("o"),
            Step::WaitForScreen("No browser here"),
            Step::Key("q"),
        ],
    );
    assert!(
        ok,
        "the link is copied with a message, then the binary exits"
    );

    std::fs::remove_file(&md).ok();
}

#[test]
fn new_flag_creates_starter_checklist_and_opens_it() {
    let path = unique_path("new-flag").with_extension("md");
    assert!(!path.exists(), "test path must not already exist yet");

    // `--new` creates the file, then falls through into the normal
    // open-and-run flow: `q` quits it like any other file. The exact
    // title-derivation text is covered by scaffold.rs's own unit tests;
    // this only needs to prove the CLI wiring end-to-end.
    let ok = drive_new(&path, &[Step::Key("q")]);
    assert!(ok, "binary should exit successfully");

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.starts_with("# "),
        "starter file has a derived title: {contents:?}"
    );
    assert!(
        contents.matches("- [ ]").count() == 2,
        "starter file has two blank tasks: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

/// External review of `5c51d81`: writing a hard-linked checklist silently
/// turns one logical file into two, because the atomic rename gives the path
/// a new inode while the other name keeps the old one. markcheck warns
/// instead of refusing (see `writer::hard_link_count`), and the whole value
/// of that decision rests on the warning actually reaching the user — which
/// only the real binary, drawing to a real terminal, can demonstrate.
#[test]
fn a_hard_linked_checklist_warns_the_user_at_startup() {
    let path = unique_path("hardlink-warn");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");
    let alias = path.with_extension("alias.md");
    std::fs::remove_file(&alias).ok();
    std::fs::hard_link(&path, &alias).unwrap();

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg(&path);
    let (ok, screen) = run_with_pty_capturing(cmd, &path, &[Step::Key("q")]);

    assert!(ok, "binary should exit successfully");
    assert!(
        screen.contains("hard links"),
        "the user must be told before touching anything; screen was:\n{screen}"
    );

    std::fs::remove_file(&alias).ok();
    std::fs::remove_file(&path).ok();
}

/// The counterpart that stops the test above from passing for the wrong
/// reason: an ordinary checklist must not be labelled hard-linked.
#[test]
fn an_ordinary_checklist_is_not_warned_about() {
    let path = unique_path("no-hardlink-warn");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    let mut cmd = markcheck_command();
    cmd.arg("--no-nerd-font");
    cmd.arg(&path);
    let (ok, screen) = run_with_pty_capturing(cmd, &path, &[Step::Key("q")]);

    assert!(ok, "binary should exit successfully");
    assert!(
        !screen.contains("hard links"),
        "a single-linked file must not be warned about; screen was:\n{screen}"
    );

    std::fs::remove_file(&path).ok();
}

/// Ctrl-Z under a job-control shell: markcheck must stop (not exit), hand the
/// terminal back, and come back on `fg` responsive and with the edit the
/// shell made meanwhile loaded. The shell only reaches its `fg` once the
/// binary has stopped or exited, and `fg` fails if it exited, so a clean exit
/// after a further toggle proves a real suspend and resume.
#[test]
fn ctrl_z_suspends_to_the_shell_and_fg_resumes_with_edits_loaded() {
    let path = unique_path("suspend");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n");

    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(concat!(
        "set -m; \"$0\" --no-nerd-font \"$1\"; ",
        "printf -- '- [ ] `gamma`\\n' >> \"$1\"; ",
        "echo SUSPENDED-IN-SHELL; fg",
    ));
    cmd.arg(env!("CARGO_BIN_EXE_markcheck"));
    cmd.arg(&path);
    isolate(&mut cmd);
    let (ok, screen) = run_with_pty_capturing(
        cmd,
        &path,
        &[
            Step::Key("\x1a"), // Ctrl-Z
            Step::WaitForFile(|s| s.contains("`gamma`")),
            // Drawn only once the binary is back in the foreground and has
            // reloaded the shell's edit: before the suspend the file had no
            // `gamma`, and the shell writes it to the file, not the screen.
            Step::WaitForScreen("gamma"),
            Step::Key(" "),
            Step::WaitForFile(|s| s.contains("[x] `alpha`")),
            Step::Key("q"),
        ],
    );

    assert!(ok, "fg should resume the binary and it should exit cleanly");
    let after_suspend = screen
        .split_once("SUSPENDED-IN-SHELL")
        .unwrap_or_else(|| panic!("the shell never got the terminal; screen was:\n{screen}"))
        .1;
    assert!(
        after_suspend.contains("gamma"),
        "the shell's edit is shown after resuming; screen was:\n{after_suspend}"
    );
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains("[x] `alpha`") && contents.contains("`gamma`"),
        "toggle after resume keeps the shell's edit: {contents:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn a_hash_anchor_or_plus_line_opens_at_that_task() {
    let path = unique_path("start-at");
    write_file(
        &path,
        "## First\n\n- [ ] alpha\n- [ ] beta\n\n## Second section\n\n- [ ] gamma\n",
    );

    let mut cmd = markcheck_command();
    cmd.arg(format!("{}#second-section", path.display()));
    let mut session = Session::spawn(cmd);
    session.wait_for_screen("Opened at #second-section");
    session.wait_for_screen("gamma");
    session.key("q");
    assert!(session.finish().0);

    let mut cmd = markcheck_command();
    cmd.arg("+4");
    cmd.arg(&path);
    let mut session = Session::spawn(cmd);
    session.wait_for_screen("Opened at line 4");
    session.key("q");
    assert!(session.finish().0);

    let mut cmd = markcheck_command();
    cmd.arg(format!("{}#nowhere", path.display()));
    let mut session = Session::spawn(cmd);
    session.wait_for_screen("no list or task at #nowhere");
    session.key("q");
    assert!(session.finish().0);
}

#[test]
fn the_terminal_title_shows_progress_and_the_exit_line_says_where_it_stopped() {
    let path = unique_path("title-summary");
    write_file(
        &path,
        "# Night shift\n\n## Work\n\n- [ ] alpha\n- [ ] beta\n",
    );
    let mut cmd = markcheck_command();
    cmd.arg(&path);
    let mut session = Session::spawn(cmd);
    session.key(" ");
    session.wait_until(SCREEN_TIMEOUT, "the updated title", |raw| {
        raw.contains("\x1b]0;Night shift · 1/2 — markcheck\x07")
    });
    session.key("q");
    let (ok, output) = session.finish();
    assert!(ok);
    assert!(output.contains("\x1b[22;2t"), "the old title is saved");
    let tail = output.rsplit("\x1b[?1049l").next().unwrap_or_default();
    assert!(tail.contains("\x1b[23;2t"), "and restored on exit");
    assert!(
        tail.contains("Night shift: 1/2 done, stopped at \"beta\""),
        "the exit line comes after the TUI: {tail:?}"
    );
}
