mod app;
mod cli;
mod clipboard;
mod config;
mod git_sync;
mod launch;
mod model;
mod parser;
mod scaffold;
#[cfg(test)]
mod test_support;
mod ui;
mod watcher;
mod writer;

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind, MouseButton, MouseEventKind,
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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(self.mouse);
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
    let cli = Cli::parse();
    // A passed flag wins, then the config, then the default. A malformed
    // config is an error rather than a silent fallback.
    let config = match config::config_path(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    ) {
        Some(path) => config::load_config(&path)?,
        None => config::Config::default(),
    };

    // Canonicalize up front: write-back would replace a symlink with a regular
    // file, and the watcher would watch the wrong directory. `--new` already
    // returns a canonical path.
    let (file_path, created_new) = if let Some(new_path) = &cli.new {
        let created = scaffold::create_new_checklist(new_path)
            .with_context(|| format!("cannot create {}", new_path.display()))?;
        (created, true)
    } else {
        // clap's `required_unless_present = "new"` guarantees this is `Some`
        // whenever `cli.new` is `None`.
        let file = cli
            .file
            .as_ref()
            .expect("clap requires FILE when --new is absent");
        let resolved = std::fs::canonicalize(file)
            .with_context(|| format!("cannot resolve path: {}", file.display()))?;
        (resolved, false)
    };
    let shown_path = file_path.display().to_string();
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
    let read_only = writer::is_read_only(&document.file_path);
    let hard_links = writer::hard_link_count(&document.file_path);
    let foreign_owner = writer::foreign_owner(&document.file_path);

    let mut state = AppState::new(document);
    let settings = Settings::resolve(&cli, &config);
    if !settings.nerd_font {
        state.icons = IconSet::unicode();
    }
    state.clipboard_primary = settings.primary;
    state.auto_copy = settings.auto_copy;
    // Pick the richest color representation the terminal supports.
    state.palette = model::Palette::detect();
    // A failed watch (e.g. inotify limits reached) only costs reload latency:
    // the trigger's timed recheck still runs, and the user is told.
    let (mut reload, watch_error) = ReloadTrigger::new(&state.document.file_path, Instant::now());

    // Git-sync activates only if requested and `GitSync::detect` finds a work
    // tree. Either half failing is reported rather than silently leaving it
    // off: the user asked for it.
    let request = launch::git_sync_request(
        cli.git_sync,
        config.git_sync_paths.as_deref(),
        &state.document.file_path,
        std::env::var("HOME").ok().as_deref(),
    );
    let detected = request
        .requested
        .then(|| git_sync::GitSync::detect(&state.document.file_path));
    let detect_error = detected.as_ref().and_then(|d| d.as_ref().err()).cloned();
    let mut git_sync = detected.and_then(Result::ok);
    let warnings = [
        launch::startup_warning(read_only, hard_links, foreign_owner),
        launch::git_sync_warning(&request.unresolved, detect_error.as_deref()),
        watch_error.map(|error| launch::watch_warning(&error.to_string())),
    ];
    match launch::startup_message(created_new, &warnings) {
        Some((message, true)) => state.set_error(message),
        Some((message, false)) => state.set_status(message),
        None => {}
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

    let mouse = settings.mouse;
    install_panic_hook();
    enable_raw_mode().context("cannot set up the terminal (markcheck is interactive)")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot set up the terminal (markcheck is interactive)")?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    let guard = TerminalGuard { mouse };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut state, &mut reload, &mut git_sync, mouse);
    drop(guard);
    clipboard::release_system_clipboard();
    // Printed only once the alternate screen is gone, so it stays visible.
    if let Some(report) = result? {
        eprintln!("{report}");
    }
    Ok(())
}

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    reload: &mut ReloadTrigger,
    git_sync: &mut Option<git_sync::GitSync>,
    mouse: bool,
) -> anyhow::Result<Option<String>>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Rebuilding the frame costs layout work over the whole document, so it
    // only happens when something on screen may have changed.
    let mut dirty = true;
    let mut label_due: Option<SystemTime> = None;
    let (opener_failures, opener_failure_rx) = std::sync::mpsc::channel();
    loop {
        dirty |= reload.check(state, Instant::now());
        while let Ok(message) = opener_failure_rx.try_recv() {
            state.set_error(message);
            dirty = true;
        }
        // Drain a finished background sync before requesting the next
        // one, so a request queued during the finished run's `pending` slot
        // (see `GitSync::poll`) is already spawned by the time we check
        // `take_git_sync_request` below.
        if let Some(sync) = git_sync.as_mut() {
            if let Some(outcome) = sync.poll() {
                if outcome == git_sync::SyncOutcome::Synced {
                    state.record_git_sync();
                }
                if let Some(message) = git_sync::status_message(&outcome) {
                    state.set_error(message);
                }
                dirty = true;
            }
            // Not gated on the outcome above: a retry can come due on a
            // frame where nothing just finished polling, and this is a
            // no-op unless one is actually pending and the backoff interval
            // has elapsed — see `retry_push_if_due`'s doc comment.
            sync.retry_push_if_due(Instant::now());
        }
        if let Some(pending) = state.take_git_sync_request()
            && let Some(sync) = git_sync.as_mut()
        {
            sync.request(pending);
        }
        let now = SystemTime::now();
        dirty |= state.expire_status(now);
        dirty |= label_due.is_some_and(|due| now >= due);
        if dirty {
            terminal.draw(|frame| ui::render(frame, state))?;
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

        if state.take_editor_request() {
            open_in_editor(terminal, state, mouse)?;
            dirty = true;
        }

        if state.take_suspend_request() {
            suspend_to_shell(terminal, state, mouse)?;
            dirty = true;
        }

        if let Some(url) = state.take_link_open_request() {
            open_link(&url, state, &opener_failures);
            dirty = true;
        }

        if state.should_quit {
            break;
        }
    }

    // The last action may have queued a sync, which takes several git
    // subprocesses; give it a bounded window to land before exiting.
    let Some(sync) = git_sync.as_mut() else {
        return Ok(None);
    };
    if let Some(pending) = state.take_git_sync_request() {
        sync.request(pending);
    }
    Ok(wait_for_git_sync(sync))
}

/// Polls `sync` until its in-flight (and any coalesced-behind-it) request
/// settles, or `timeout` elapses — whichever comes first. Only meant to be
/// called once, right before quitting. Returns the line to print after the
/// TUI closes when that last sync didn't land cleanly.
fn wait_for_git_sync(sync: &mut git_sync::GitSync) -> Option<String> {
    const TIMEOUT: Duration = Duration::from_secs(5);
    let deadline = Instant::now() + TIMEOUT;
    let mut last = None;
    while sync.is_busy() && Instant::now() < deadline {
        if let Some(outcome) = sync.poll() {
            last = Some(outcome);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
    if let Some(warning) = warning {
        state.set_error(warning);
    }
    let mut command = Command::new(&program);
    command.args(&args);
    // Jump to the selected task's source line for editors that support it;
    // the arg goes before the path per the `+N file` convention.
    if let Some(line) = state.current_item().map(|item| item.line_number) {
        command.args(launch::editor_line_args(&program, line));
    }
    let status = command.arg(&state.document.file_path).status();

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

    state.finish_editor(&program, status);
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
