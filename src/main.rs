#[cfg(not(unix))]
compile_error!("markcheck supports Unix-like systems only");

mod app;
mod cli;
mod clipboard;
mod config;
mod effect;
mod git_sync;
mod launch;
mod model;
mod parser;
mod printable;
mod scaffold;
#[cfg(test)]
mod temp_test_support;
#[cfg(test)]
mod test_support;
mod ui;
mod watcher;
mod writer;

use std::io::{self, IsTerminal, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;

use cli::{Cli, Settings};
use model::{AppState, IconSet};
use watcher::ReloadTrigger;

struct TerminalGuard {
    mouse: bool,
    /// Whether the terminal title was saved at startup, to restore it.
    title: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(self.mouse);
        if self.title {
            // Pop the title saved at startup (xterm's title stack, which tmux
            // and most terminals keep).
            let _ = write!(io::stdout(), "\x1b[23;2t");
            let _ = io::stdout().flush();
        }
    }
}

fn restore_terminal(mouse: bool) {
    let _ = disable_raw_mode();
    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}

/// Restores the terminal before the default hook prints, so a main-thread
/// panic's message lands on the normal screen instead of being printed in
/// raw mode onto the alternate screen and wiped when `TerminalGuard` drops.
/// Other threads are left alone: git-sync's workers catch their own panics
/// (`run_reporting_panics`) while the TUI keeps running.
fn install_panic_hook() {
    let main_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_thread {
            restore_terminal(true);
            previous(info);
            eprintln!("markcheck crashed. Please report this at {ISSUES_URL}");
        } else {
            previous(info);
        }
    }));
}

const ISSUES_URL: &str = "https://github.com/donnex/markcheck/issues";

fn main() -> anyhow::Result<()> {
    let (args, start_line) = cli::take_start_line(std::env::args_os().collect());
    let cli = Cli::parse_from(args);
    if cli.generate_man {
        io::stdout().write_all(&cli::man_page()?)?;
        return Ok(());
    }
    if let Some(shell) = cli.generate_completions {
        io::stdout().write_all(&cli::completions(shell))?;
        return Ok(());
    }
    // A passed flag wins, then the config, then the default. A malformed
    // config is an error rather than a silent fallback; an unknown key is
    // only a warning, since it may come from a newer release.
    let config::Loaded {
        config,
        unknown_keys,
    } = config::load_for_run(
        cli.config.as_deref(),
        cli.no_config,
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;

    // Canonicalize up front: write-back would replace a symlink with a regular
    // file, and the watcher would watch the wrong directory. `--new` already
    // returns a canonical path.
    let mut start_at = start_line.map(model::StartAt::Line);
    let (file_path, created_new) = if let Some(new_path) = &cli.new {
        let created = scaffold::create_new_checklist(new_path).with_context(|| {
            format!(
                "cannot create {}",
                printable::printable_line(&new_path.display().to_string())
            )
        })?;
        (created, true)
    } else {
        // clap's `required_unless_present = "new"` guarantees this is `Some`
        // whenever `cli.new` is `None`.
        let mut file = cli
            .file
            .clone()
            .expect("clap requires FILE when --new is absent");
        if let Some((path, anchor)) = cli::split_anchor(&file) {
            file = path;
            start_at = Some(model::StartAt::Anchor(anchor));
        }
        let resolved = std::fs::canonicalize(&file).with_context(|| {
            format!(
                "cannot resolve path: {}",
                printable::printable_line(&file.display().to_string())
            )
        })?;
        (resolved, false)
    };
    let shown_path = printable::printable_line(&file_path.display().to_string());
    let document =
        parser::parse_document(file_path).with_context(|| format!("cannot read {shown_path}"))?;

    // The parser drops lists without checklist items, so no lists at all
    // means the file has nothing to work through.
    if document.lists.is_empty() {
        eprintln!(
            "No checklist items found in {shown_path}.\n\
             markcheck works through task items like `- [ ] step`; lists without checkboxes are skipped."
        );
        return Ok(());
    }

    // Checked before anything with side effects (e.g. git-sync's catch-up
    // push): without it, raw mode fails with a bare "No such device or
    // address" that names neither the file nor the cause.
    if !io::stdout().is_terminal() {
        anyhow::bail!(
            "cannot open {shown_path}: markcheck is interactive and needs a terminal (stdout is not a TTY)"
        );
    }

    // Warn *before* the first write, while quitting still costs nothing: a
    // toggle renames a new inode over this path and any hard-linked alias
    // silently stops tracking the checklist. See `writer::hard_link_count`
    // for why this warns rather than refuses.
    // Likewise a file owned by someone else: the save's new file is the
    // runner's, which can lock its owner out (see `writer::foreign_owner`).
    // A write-protected file is refused on save (`writer::write_back`), so say
    // so before the first keypress rather than at it.
    // Held until exit, so another session opening this checklist can tell.
    let (_session_marker, other_sessions) = writer::SessionMarker::register(&document.file_path);
    let read_only = writer::is_read_only(&document.file_path);
    let hard_links = writer::hard_link_count(&document.file_path);
    let foreign_owner = writer::foreign_owner(&document.file_path);

    let mut state = AppState::new(document);
    let settings = Settings::resolve(&cli, &config);
    if settings.nerd_font {
        state.icons = IconSet::nerd();
    }
    state.clipboard_primary = settings.primary;
    state.copy_route = cli::clipboard_choice(&cli, &config).route(cli::is_remote_session(
        std::env::var("SSH_CONNECTION").ok().as_deref(),
        std::env::var("SSH_TTY").ok().as_deref(),
    ));
    state.auto_copy = settings.auto_copy;
    // Pick the richest color representation the terminal supports, unless
    // `--color`/`color` forces one.
    state.palette = model::Palette::detect(settings.color);
    // A failed watch (e.g. inotify limits reached) only costs reload latency:
    // the trigger's timed recheck still runs, and the user is told.
    let (mut reload, watch_error) = ReloadTrigger::new(&state.document.file_path, Instant::now());

    // Git-sync activates only if requested and `GitSync::detect` finds a work
    // tree. Either half failing is reported rather than silently leaving it
    // off: the user asked for it.
    let request = launch::git_sync_request(
        cli.git_sync,
        cli.no_git_sync,
        config.git_sync_paths.as_deref(),
        &state.document.file_path,
        std::env::var("HOME").ok().as_deref(),
    );
    // `detect` runs only `rev-parse`, which runs no hooks or helpers, so the
    // scope check comes before any command a nested repository could hijack.
    let detected = request.requested.then(|| {
        git_sync::GitSync::detect(&state.document.file_path)
            .and_then(|sync| launch::git_sync_scope(&request, sync.repo_root()).map(|()| sync))
    });
    let detect_error = detected.as_ref().and_then(|d| d.as_ref().err()).cloned();
    let mut git_sync = detected.and_then(Result::ok);
    if let Some(sync) = git_sync.as_mut() {
        sync.set_delay(cli::git_sync_delay(&cli, &config));
    }
    // What no signal handler can catch (SIGKILL, the OOM killer, a power
    // loss) still leaves these behind.
    let leftovers = writer::sweep_interrupted_saves(&state.document.file_path)
        + git_sync
            .as_ref()
            .map_or(0, git_sync::GitSync::sweep_interrupted_syncs);
    let warnings = [
        config::unknown_keys_warning(&unknown_keys),
        launch::startup_warning(read_only, hard_links, foreign_owner),
        launch::git_sync_warning(&request.unresolved, detect_error.as_deref()),
        watch_error.map(|error| launch::watch_warning(&error.to_string())),
        launch::leftovers_warning(leftovers),
        launch::already_open_warning(&other_sessions),
        launch::unshown_warning(&state.document.unshown),
    ];
    let opened_at = start_at.as_ref().map(|start| state.open_at(start));
    let mut warnings = warnings.to_vec();
    let mut opened_note = None;
    match opened_at {
        Some(Err(reason)) => warnings.push(Some(reason)),
        Some(Ok(note)) => opened_note = Some(note),
        None => {}
    }
    match launch::startup_message(created_new, &warnings) {
        Some((message, true)) => state.set_error(message),
        Some((message, false)) => state.set_status(message),
        None => {
            if let Some(note) = opened_note {
                state.set_status(note);
            }
        }
    }
    // Drives the persistent `⇅ git` section in the title bar
    // (`render_title_bar` in `ui/mod.rs`) — set once here and never touched
    // again; `GitSyncState::last_at`/`pending` handle the per-sync timing,
    // this is just "is the feature on for this session at all".
    state.git_sync.active = git_sync.is_some();
    // A previous session may have quit before pushing its commit. This only
    // pushes; it never commits.
    if let Some(sync) = git_sync.as_mut() {
        sync.request_catch_up_push();
    }

    let stop = stop_on_signals().context("cannot install signal handlers")?;
    let mouse = settings.mouse;
    install_panic_hook();
    enable_raw_mode().context("cannot set up the terminal (markcheck is interactive)")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot set up the terminal (markcheck is interactive)")?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    let show_title = config.terminal_title.unwrap_or(true);
    if show_title {
        // Push the current title, restored by `TerminalGuard`.
        let _ = write!(io::stdout(), "\x1b[22;2t");
    }
    let guard = TerminalGuard {
        mouse,
        title: show_title,
    };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(
        &mut terminal,
        &mut state,
        &mut reload,
        &mut git_sync,
        mouse,
        &stop,
        show_title,
    );
    drop(guard);
    clipboard::release_system_clipboard();
    if result.is_ok() && config.exit_summary.unwrap_or(true) {
        let _ = writeln!(io::stdout(), "{}", launch::exit_summary(&state));
    }
    // Printed only once the alternate screen is gone, so it stays visible.
    // Ignored on failure: after SIGHUP there may be no terminal left.
    if let Some(report) = result? {
        let _ = writeln!(io::stderr(), "{report}");
    }
    Ok(())
}

/// `SIGTERM`, `SIGHUP` (a closed terminal or dropped SSH session) and a
/// `SIGINT` from `kill` set the returned flag, so `run` ends the way `q`
/// does and git sync is waited for and cleaned up. A second `SIGTERM` or
/// `SIGINT` exits at once, for when that wait is too long.
fn stop_on_signals() -> io::Result<Arc<AtomicBool>> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::flag;

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        flag::register_conditional_shutdown(signal, 1, Arc::clone(&stop))?;
    }
    for signal in [SIGTERM, SIGHUP, SIGINT] {
        flag::register(signal, Arc::clone(&stop))?;
    }
    Ok(stop)
}

/// Shown while `q` waits for a sync still running.
const WAITING_FOR_SYNC: &str = "Waiting for git sync… press q again to abandon";

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    reload: &mut ReloadTrigger,
    git_sync: &mut Option<git_sync::GitSync>,
    mouse: bool,
    stop: &AtomicBool,
    show_title: bool,
) -> anyhow::Result<Option<String>>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Rebuilding the frame costs layout work over the whole document, so it
    // only happens when something on screen may have changed.
    let mut dirty = true;
    let mut label_due: Option<SystemTime> = None;
    let (opener_failures, opener_failure_rx) = std::sync::mpsc::channel();
    // Set once `q` found a sync still running; the outcome it then waits for
    // is the one reported after the TUI closes.
    let mut quitting = false;
    let mut last_outcome = None;
    // Written only when it changes, not on every frame.
    let mut shown_title = String::new();
    loop {
        // Nothing is drawn from here on: after SIGHUP the terminal is gone.
        if stop.load(Ordering::Relaxed) {
            return Ok(git_sync.as_mut().and_then(|sync| {
                sync.flush();
                let last = wait_for_git_sync(sync, QUIT_WAIT).or(last_outcome);
                finish_git_sync(sync, last)
            }));
        }
        dirty |= reload.check(state, Instant::now());
        while let Ok(message) = opener_failure_rx.try_recv() {
            state.set_error(message);
            dirty = true;
        }
        // Drain a finished background sync before requesting the next
        // one, so a request queued during the finished run's `pending` slot
        // (see `GitSync::poll`) is already spawned by the time this
        // iteration's effects request another.
        if let Some(sync) = git_sync.as_mut() {
            if let Some(outcome) = sync.poll() {
                state.apply_git_sync_outcome(&outcome);
                if quitting {
                    last_outcome = Some(outcome);
                }
                dirty = true;
            }
            // Not gated on the outcome above: a retry can come due on a
            // frame where nothing just finished polling, and this is a
            // no-op unless one is actually pending and the backoff interval
            // has elapsed — see `retry_push_if_due`'s doc comment.
            sync.tick(Instant::now());
        }
        if let Some(sync) = git_sync.as_ref()
            && (state.git_sync.busy, state.git_sync.waiting) != (sync.is_busy(), sync.is_waiting())
        {
            state.git_sync.busy = sync.is_busy();
            state.git_sync.waiting = sync.is_waiting();
            dirty = true;
        }
        if quitting && !state.git_sync.busy {
            break;
        }
        let now = SystemTime::now();
        dirty |= state.expire_status(now);
        dirty |= label_due.is_some_and(|due| now >= due);
        if dirty {
            terminal.draw(|frame| ui::render(frame, state))?;
            if show_title {
                let title = launch::terminal_title(state);
                if title != shown_title {
                    let _ = execute!(io::stdout(), crossterm::terminal::SetTitle(&title));
                    shown_title = title;
                }
            }
            label_due = ui::next_label_change(state, SystemTime::now());
            dirty = false;
            // Debug builds only: lets `tests/pty.rs` force a main-thread
            // panic to check the panic hook restores the terminal.
            #[cfg(debug_assertions)]
            if std::env::var_os("MARKCHECK_TEST_PANIC").is_some() {
                panic!("forced test panic");
            }
        }

        if event::poll(Duration::from_millis(100))? {
            let event = event::read()?;
            dirty |= ui::event_needs_redraw(&event);
            match event {
                // The checklist is done with: a key either abandons the sync or
                // is answered with the prompt again, so no edit queues another.
                Event::Key(key) if quitting && key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('q') {
                        let sync = git_sync.as_mut().expect("quitting waits only on a sync");
                        let last = abandon_git_sync(sync).or(last_outcome);
                        return Ok(finish_git_sync(sync, last));
                    }
                    state.set_notice(WAITING_FOR_SYNC.to_string());
                }
                _ if quitting => {}
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    state.handle_key_with_mods(key.code, key.modifiers);
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => state.handle_scroll_up(),
                    MouseEventKind::ScrollDown => state.handle_scroll_down(),
                    MouseEventKind::Down(MouseButton::Left) => {
                        state.handle_left_click(mouse.column, mouse.row)
                    }
                    _ => {}
                },
                Event::Paste(text) => state.handle_paste(&text),
                _ => {}
            }
        }

        let drained = effect::drain(
            state,
            &mut TerminalHost {
                terminal,
                git_sync,
                mouse,
                opener_failures: &opener_failures,
            },
        )?;
        dirty |= drained.ran_any;
        if drained.quit {
            // A change still in its quiet period goes out now, not never.
            if let Some(sync) = git_sync.as_mut() {
                sync.flush();
            }
            if git_sync.as_ref().is_some_and(git_sync::GitSync::is_busy) {
                quitting = true;
                state.set_notice(WAITING_FOR_SYNC.to_string());
                dirty = true;
            } else {
                break;
            }
        }
    }

    Ok(git_sync
        .as_mut()
        .and_then(|sync| finish_git_sync(sync, last_outcome)))
}

/// Carries out `AppState`'s effects against the real terminal, processes
/// and git-sync worker.
struct TerminalHost<'a, B: Backend> {
    terminal: &'a mut Terminal<B>,
    git_sync: &'a mut Option<git_sync::GitSync>,
    mouse: bool,
    opener_failures: &'a std::sync::mpsc::Sender<String>,
}

impl<B: Backend> effect::Host for TerminalHost<'_, B>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    fn open_editor(
        &mut self,
        state: &mut AppState,
        line: Option<model::LineNumber>,
    ) -> anyhow::Result<()> {
        open_in_editor(self.terminal, state, line, self.mouse)
    }

    fn suspend(&mut self, state: &mut AppState) -> anyhow::Result<()> {
        suspend_to_shell(self.terminal, state, self.mouse)
    }

    fn open_link(&mut self, state: &mut AppState, url: &str) {
        open_link(url, state, self.opener_failures);
    }

    fn git_sync(&mut self, sync: model::PendingSync) {
        if let Some(worker) = self.git_sync.as_mut() {
            worker.request(sync);
        }
    }
}

/// How long a signal lets a running sync finish before it is abandoned.
const QUIT_WAIT: Duration = Duration::from_secs(5);
/// How long an abandoned sync's worker gets to unwind through its cleanup
/// after `SIGTERM`, and then after `SIGKILL`.
const ABANDON_GRACE: Duration = Duration::from_secs(2);

/// Polls `sync` until its in-flight (and any coalesced-behind-it) request
/// settles, or `timeout` elapses — whichever comes first. Returns the last
/// outcome seen.
fn wait_for_git_sync(
    sync: &mut git_sync::GitSync,
    timeout: Duration,
) -> Option<git_sync::SyncOutcome> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while sync.is_busy() && Instant::now() < deadline {
        if let Some(outcome) = sync.poll() {
            last = Some(outcome);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    last
}

/// Stops a running sync's git processes, then waits for the worker to remove
/// its temporary index and scratch files. Returns the last outcome seen.
fn abandon_git_sync(sync: &mut git_sync::GitSync) -> Option<git_sync::SyncOutcome> {
    sync.abandon(false);
    let mut last = wait_for_git_sync(sync, ABANDON_GRACE);
    if sync.is_busy() {
        sync.abandon(true);
        last = wait_for_git_sync(sync, ABANDON_GRACE).or(last);
    }
    last
}

/// Abandons whatever is still running, then gives the line to print after
/// the TUI closes when the last sync didn't land cleanly.
fn finish_git_sync(
    sync: &mut git_sync::GitSync,
    last: Option<git_sync::SyncOutcome>,
) -> Option<String> {
    let last = if sync.is_busy() {
        abandon_git_sync(sync).or(last)
    } else {
        last
    };
    git_sync::exit_report(last.as_ref(), sync.is_busy())
}

/// Opens `url` without suspending the TUI, trying each resolved opener in
/// turn until one spawns. No shell and discarded output; the child is reaped
/// on a detached thread, which sends a quick failed exit to `failures`. With
/// no browser to send to at all (a headless session), copies the link instead.
fn open_link(url: &str, state: &mut AppState, failures: &std::sync::mpsc::Sender<String>) {
    use std::process::{Command, Stdio};

    let browser = std::env::var("BROWSER").ok();
    let display = std::env::var("DISPLAY").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    if launch::no_browser_available(
        browser.as_deref(),
        display.as_deref(),
        wayland_display.as_deref(),
    ) {
        state.copy_link_instead(url);
        return;
    }
    let (candidates, warning) = launch::resolve_opener(browser.as_deref(), url);
    let mut last_error = None;
    for (program, args) in &candidates {
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                state.report_link_sent(program, url);
                if let Some(warning) = warning {
                    state.set_error(warning);
                }
                let program = program.clone();
                let failures = failures.clone();
                let started = Instant::now();
                std::thread::spawn(move || {
                    let outcome = child.wait();
                    if let Some(message) =
                        launch::opener_failure_message(&program, &outcome, started.elapsed())
                    {
                        let _ = failures.send(message);
                    }
                });
                return;
            }
            Err(err) => last_error = Some(format!("{program}: {err}")),
        }
    }
    if let Some(err) = last_error {
        state.set_error(format!("Couldn't open link ({err})"));
    }
}

/// Suspends the TUI, launches the editor on the current file, then
/// restores the terminal and reloads. Always restores, even if the
/// editor fails to spawn.
fn open_in_editor<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    line: Option<model::LineNumber>,
    mouse: bool,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use std::process::Command;

    disable_raw_mode()?;
    if mouse {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;

    let (program, args, warning) = launch::resolve_editor(
        std::env::var("VISUAL").ok().as_deref(),
        std::env::var("EDITOR").ok().as_deref(),
    );
    let mut command = Command::new(&program);
    command.args(&args);
    command.args(launch::editor_file_args(
        &program,
        &state.document.file_path,
        line,
    ));
    let started = std::time::Instant::now();
    let status = command.status();
    let elapsed = started.elapsed();

    // Restore the terminal regardless of how the editor fared.
    enable_raw_mode().context("cannot set up the terminal (markcheck is interactive)")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot set up the terminal (markcheck is interactive)")?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Force a full repaint by resizing: `Terminal::clear` queries the cursor,
    // which hangs on terminals that don't answer.
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))?;

    state.finish_editor(&program, warning, status, elapsed);
    Ok(())
}

/// `Ctrl-Z`: hands the terminal back to the shell and stops the process
/// until `fg` resumes it, then restores the TUI and picks up any edit made
/// meanwhile. Raw mode turns off `ISIG`, so the terminal itself never sends
/// `SIGTSTP` for the keypress.
fn suspend_to_shell<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    mouse: bool,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    disable_raw_mode()?;
    if mouse {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;

    let outcome = stop_self();

    enable_raw_mode().context("cannot set up the terminal (markcheck is interactive)")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot set up the terminal (markcheck is interactive)")?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Same full repaint as after the editor: see `open_in_editor`.
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))?;

    state.finish_suspend(outcome);
    Ok(())
}

/// Sends `SIGTSTP` to this process through `kill(1)`, as `git_sync` does for
/// its timeouts, rather than taking on a libc dependency. The process stops
/// while waiting for `kill`, so this returns only once it's been resumed.
fn stop_self() -> Result<(), String> {
    use std::process::{Command, Stdio};

    let status = Command::new("kill")
        .args(["-TSTP", "--"])
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("couldn't run kill ({err})"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill exited with {status}"))
    }
}
