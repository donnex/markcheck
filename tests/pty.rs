//! End-to-end tests that drive the compiled binary under a pseudo-terminal.
//! These exercise `main.rs`'s terminal wiring — the event loop, key
//! dispatch, editor suspend/restore, and reset — which unit tests can't
//! reach headless. Assertions are behavioral (clean exit, file contents)
//! rather than screen-scraping, to avoid ANSI/timing flakiness. Run under
//! `cargo llvm-cov` the spawned binary is instrumented, so this also lifts
//! `main.rs` coverage.
//!
//! Unix-only, matching the project's own platform support (Linux/macOS —
//! see README's Installation section) and CI (`check.yml` runs on
//! `ubuntu-latest` only).
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

mod common;

/// These `git_sync_*` tests each spawn a subprocess with its own
/// background commit+push thread and then poll a bare remote for it to
/// land. Run concurrently (the default, like every other test in this
/// file), they compete with each other for CPU on top of whatever else the
/// machine is doing, which can push the background thread past the poll
/// deadline below on a busy runner. Held for a whole test's duration, this
/// serializes just these against each other; `into_inner` recovers
/// the guard even if an earlier holder panicked, since there's no shared
/// data here to have been corrupted — only mutual exclusion matters.
static GIT_SYNC_TEST_LOCK: Mutex<()> = Mutex::new(());

fn git_sync_test_guard() -> std::sync::MutexGuard<'static, ()> {
    GIT_SYNC_TEST_LOCK
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

/// One step of a scripted interaction: either a keypress (paced with a short
/// fixed delay — plain sequential keys have no observable side effect worth
/// polling for before the *next* one lands), or a poll on the target
/// markdown file's content for a condition a preceding key is expected to
/// have caused — used before any step, notably `Key("q")`, that
/// depends on that condition already holding, e.g. an edit having landed or
/// a toggle having been written back.
enum Step {
    Key(&'static str),
    WaitForFile(fn(&str) -> bool),
    /// Simulates another program (e.g. an editor) writing the file while
    /// the binary is running: writes `contents` with a bumped mtime
    /// (avoiding coarse-mtime-resolution flakiness, mirroring
    /// `app_tests.rs`'s `touch_with_new_mtime`), then pauses well past the
    /// watcher's poll cycle so the reload has landed before the next step.
    ExternalWrite(&'static str),
    /// Simulates the file being deleted out from under the running binary,
    /// then pauses for the watcher to detect it.
    ExternalDelete,
}

/// Pacing delay between plain keys: generous enough for the 100ms event loop
/// to read, process, and redraw before the next one arrives.
const KEY_PACING_MS: u64 = 80;

/// Spawns the binary in a PTY, runs the steps, waits for it to exit, and
/// returns whether it exited successfully. A background thread drains the
/// PTY output the whole time — without it the master's buffer fills and the
/// child blocks on its next draw and never sees the quit key.
fn drive(md_path: &PathBuf, env: &[(&str, &str)], steps: &[Step]) -> bool {
    drive_args(md_path, &[], env, steps)
}

/// Like `drive` but with extra CLI flags before the file argument.
fn drive_args(md_path: &PathBuf, args: &[&str], env: &[(&str, &str)], steps: &[Step]) -> bool {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_markcheck"));
    cmd.arg("--no-nerd-font");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(md_path);
    // Isolate from a real config file that might exist on the machine
    // running the tests — std::env::temp_dir() is a directory that
    // exists but never has a markcheck/config.toml under it, so this is
    // equivalent to no config file being present. A test that wants to
    // exercise the config file overrides XDG_CONFIG_HOME itself below.
    cmd.env("XDG_CONFIG_HOME", std::env::temp_dir());
    for (k, v) in env {
        cmd.env(k, v);
    }
    run_with_pty(cmd, md_path, steps)
}

/// Like `drive_args`, but for `--new`: passes `--new <path>` instead of the
/// positional FILE argument, since the two conflict on the CLI. `md_path`
/// must not already exist yet — markcheck is expected to create it.
fn drive_new(md_path: &PathBuf, steps: &[Step]) -> bool {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_markcheck"));
    cmd.arg("--no-nerd-font");
    cmd.arg("--new");
    cmd.arg(md_path);
    cmd.env("XDG_CONFIG_HOME", std::env::temp_dir());
    run_with_pty(cmd, md_path, steps)
}

fn run_with_pty(cmd: CommandBuilder, md_path: &Path, steps: &[Step]) -> bool {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let drain = thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
        String::from_utf8_lossy(&sink).into_owned()
    });

    {
        let mut writer = pair.master.take_writer().unwrap();
        thread::sleep(Duration::from_millis(600)); // first draw
        for step in steps {
            match step {
                Step::Key(k) => {
                    writer.write_all(k.as_bytes()).unwrap();
                    writer.flush().unwrap();
                    thread::sleep(Duration::from_millis(KEY_PACING_MS));
                }
                Step::WaitForFile(pred) => {
                    assert!(
                        wait_for_file(md_path, pred),
                        "timed out waiting for the expected file content"
                    );
                }
                Step::ExternalWrite(contents) => {
                    std::fs::write(md_path, contents).unwrap();
                    let metadata = std::fs::metadata(md_path).unwrap();
                    let new_mtime = metadata.modified().unwrap() + Duration::from_secs(1);
                    std::fs::File::open(md_path)
                        .unwrap()
                        .set_modified(new_mtime)
                        .unwrap();
                    thread::sleep(Duration::from_millis(300));
                }
                Step::ExternalDelete => {
                    std::fs::remove_file(md_path).unwrap();
                    thread::sleep(Duration::from_millis(300));
                }
            }
        }
    } // writer dropped

    let status = child.wait().unwrap();
    drop(pair.master);
    let output = drain.join().unwrap_or_default();
    if !status.success() {
        // Surface the child's output (stderr is on the same PTY) so a
        // failure is diagnosable rather than a bare "not successful".
        let tail: String = output
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        eprintln!("[pty] child exited unsuccessfully; output tail:\n{tail}");
    }
    status.success()
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
            Step::Key(" "), // attempted toggle while deleted: must not crash
            Step::ExternalWrite("## Work\n\n- [ ] `alpha`\n- [ ] `beta`\n"),
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

#[test]
fn auto_copy_navigation_exits_clean() {
    let path = unique_path("autocopy");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] plain item\n");

    // With --auto-copy, navigating emits clipboard writes (OSC 52 to the
    // PTY when arboard is unavailable) and must not crash, including on the
    // item with no code candidate. l navigates, q quits.
    let ok = drive_args(
        &path,
        &["--auto-copy"],
        &[],
        &[Step::Key("l"), Step::Key("h"), Step::Key("q")],
    );
    assert!(ok, "binary should exit successfully with --auto-copy");

    std::fs::remove_file(&path).ok();
}

#[test]
fn config_file_sets_auto_copy_default_without_the_flag() {
    // A config file's `auto_copy = true` must reach the same runtime
    // behavior as passing --auto-copy, with no flag on the command line.
    let path = unique_path("autocopy-cfg");
    write_file(&path, "## Work\n\n- [ ] `alpha`\n- [ ] plain item\n");

    let xdg_config_home = unique_path("autocopy-cfg-xdg");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        "auto_copy = true\n",
    )
    .unwrap();

    let ok = drive_args(
        &path,
        &[],
        &[("XDG_CONFIG_HOME", xdg_config_home.to_str().unwrap())],
        &[Step::Key("l"), Step::Key("h"), Step::Key("q")],
    );
    assert!(
        ok,
        "binary should exit successfully with auto_copy set via config"
    );

    std::fs::remove_file(&path).ok();
    std::fs::remove_dir_all(&xdg_config_home).ok();
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

#[test]
fn git_sync_flag_commits_and_pushes_after_toggle() {
    // `--git-sync` should commit and push the file's change after a
    // toggle, when the file lives in a git repo — end-to-end through
    // `main.rs`'s wiring (flag parsing, `GitSync::detect`, and the poll/
    // request loop), which the `git_sync.rs` unit tests can't reach since
    // they drive `GitSync` directly rather than through the compiled binary.
    let _guard = git_sync_test_guard();
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
        let out = std::process::Command::new("git")
            .current_dir(&remote)
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
fn git_sync_paths_config_auto_activates_without_the_flag() {
    // git_sync_paths shares main.rs's flag-merge logic with the other config
    // keys, but it's additive (OR'd with --git-sync) rather than a plain
    // CLI-overrides-config default, and only auto_copy's config path had PTY
    // coverage before this. A path prefix match must turn git-sync on with
    // no --git-sync on the command line at all.
    let _guard = git_sync_test_guard();
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
        let out = std::process::Command::new("git")
            .current_dir(&remote)
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

#[test]
#[cfg(unix)]
fn git_sync_paths_config_matches_through_a_symlinked_prefix() {
    // git_sync_paths is compared against the file's *canonicalized* path
    // (main.rs resolves symlinks before this check), but the configured
    // prefix itself used to be compared as written — so a prefix reached via
    // a symlink never matched, even though the file genuinely lives under
    // it. Regression test: configure the prefix as a symlink to the real
    // work dir; sync must still auto-activate.
    let _guard = git_sync_test_guard();
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
        let out = std::process::Command::new("git")
            .current_dir(&remote)
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
    let _guard = git_sync_test_guard();
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
    #[cfg(unix)]
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
        let out = std::process::Command::new("git")
            .current_dir(&remote)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if subject != "init" {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    // The program name is the fake editor's own (temp-dir, so variable-length)
    // path, so mirror git_sync's truncate-with-ellipsis rule here rather than
    // asserting a fixed string.
    let program = editor.to_str().unwrap();
    let prefix = "checklist.md: Edited in ";
    let full = format!("{prefix}{program}");
    let expected = if full.chars().count() <= 80 {
        full
    } else {
        let budget = 80 - prefix.chars().count() - 1;
        let truncated: String = program.chars().take(budget).collect();
        format!("{prefix}{truncated}\u{2026}")
    };
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
    #[cfg(unix)]
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

    // Give a would-be (wrong) commit+push every chance to land before
    // asserting it didn't: poll briefly, then confirm the log is still
    // exactly the one "init" commit.
    thread::sleep(Duration::from_millis(500));
    let out = std::process::Command::new("git")
        .current_dir(&remote)
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
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

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_markcheck"));
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.arg(&path);
    cmd.env("XDG_CONFIG_HOME", std::env::temp_dir());
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let drain = thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = reader.read_to_end(&mut sink);
        sink
    });
    {
        let mut writer = pair.master.take_writer().unwrap();
        thread::sleep(Duration::from_millis(600));
        writer.write_all(b" ").unwrap(); // toggle: triggers a sync attempt
        writer.flush().unwrap();
        thread::sleep(Duration::from_millis(800)); // let the background sync poll land
        writer.write_all(b"q").unwrap();
        writer.flush().unwrap();
    }
    let status = child.wait().unwrap();
    drop(pair.master);
    let raw = drain.join().unwrap_or_default();
    assert!(status.success(), "binary should exit successfully");

    let text = String::from_utf8_lossy(&raw);
    let visible: String = text
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    assert!(
        visible.contains("Git sync skipped: file is not tracked in git"),
        "expected the untracked-file message in the rendered output: {visible:?}"
    );

    let status_out = std::process::Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status_out.stdout).contains("?? checklist.md"),
        "the file must still never be added"
    );

    std::fs::remove_dir_all(&root).ok();
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
    let _guard = git_sync_test_guard();
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
    // Point origin at a path that will never resolve, so the first sync
    // attempt's push fails deterministically and fast (no real network
    // hang), while still recording origin as this branch's upstream so
    // `ahead_of_upstream` (used by the retry-fast-path and the startup
    // catch-up push) has something to compare against.
    run_git(
        &work,
        &["remote", "add", "origin", broken_remote.to_str().unwrap()],
    );
    run_git(&work, &["config", "branch.main.remote", "origin"]);
    run_git(&work, &["config", "branch.main.merge", "refs/heads/main"]);

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_markcheck"));
    cmd.arg("--no-nerd-font");
    cmd.arg("--git-sync");
    cmd.arg(&path);
    cmd.env("XDG_CONFIG_HOME", std::env::temp_dir());
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut all = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    all.extend_from_slice(&buf[..n]);
                    let visible: String = String::from_utf8_lossy(&all)
                        .chars()
                        .filter(|c| c.is_ascii_graphic() || *c == ' ')
                        .collect();
                    let _ = tx.send(visible);
                }
                Err(_) => break,
            }
        }
        all
    });

    let visible_contains = |rx: &std::sync::mpsc::Receiver<String>, needle: &str| -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = String::new();
        while Instant::now() < deadline {
            while let Ok(v) = rx.try_recv() {
                last = v;
            }
            if last.contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    };

    let mut writer = pair.master.take_writer().unwrap();
    thread::sleep(Duration::from_millis(600));
    writer.write_all(b" ").unwrap(); // toggle: commits locally, push fails
    writer.flush().unwrap();

    assert!(
        visible_contains(&rx, "push failed, will retry"),
        "the first push failure must be reported, proving CommittedNotPushed is handled"
    );

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
        let out = std::process::Command::new("git")
            .current_dir(&remote)
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

    writer.write_all(b"q").unwrap();
    writer.flush().unwrap();
    let status = child.wait().unwrap();
    drop(pair.master);
    let _ = drain.join();
    assert!(status.success(), "binary should exit successfully");

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
