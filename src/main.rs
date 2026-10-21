#[cfg(not(unix))]
compile_error!("markcheck supports Unix-like systems only");

mod app;
mod cli;
mod clipboard;
mod config;
mod effect;
mod git_sync;
#[cfg(test)]
mod git_test_support;
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    let _ = leave_tui(mouse);
}

/// Hands the terminal back in its normal state. Every step is tried even if
/// an earlier one fails; the first failure is returned.
fn leave_tui(mouse: bool) -> io::Result<()> {
    let raw = disable_raw_mode();
    let mouse = if mouse {
        execute!(io::stdout(), DisableMouseCapture)
    } else {
        Ok(())
    };
    let screen = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    raw.and(mouse).and(screen)
}

/// Puts the terminal into the TUI's modes: raw input, the alternate screen,
/// bracketed paste and, with `mouse`, mouse capture. `leave_tui` undoes it.
fn take_terminal(mouse: bool) -> anyhow::Result<()> {
    enable_raw_mode().context("cannot set up the terminal (markcheck is interactive)")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)
        .context("cannot set up the terminal (markcheck is interactive)")?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    Ok(())
}

/// The terminal title markcheck shows while it holds the terminal. The
/// title it replaced is saved on xterm's title stack (which tmux and most
/// terminals keep) and handed back whenever another program gets the
/// terminal.
struct TerminalTitle {
    enabled: bool,
    /// What was last sent, so an unchanged title isn't rewritten every frame.
    shown: String,
}

impl TerminalTitle {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            shown: String::new(),
        }
    }

    fn show(&mut self, state: &AppState) {
        if !self.enabled {
            return;
        }
        let title = launch::terminal_title(state);
        if title != self.shown {
            let _ = execute!(io::stdout(), crossterm::terminal::SetTitle(&title));
            self.shown = title;
        }
    }

    /// Restores the title markcheck replaced, before another program takes
    /// the terminal.
    fn release(&mut self) {
        if self.enabled {
            let _ = write!(io::stdout(), "\x1b[23;2t");
            let _ = io::stdout().flush();
            self.shown.clear();
        }
    }

    /// Saves the title the other program left, so the next `show` writes
    /// markcheck's own over it and `release` can restore it.
    fn reclaim(&self) {
        if self.enabled {
            let _ = write!(io::stdout(), "\x1b[22;2t");
        }
    }
}

/// Takes the terminal back after `leave_tui` and repaints the whole frame.
fn enter_tui<B: Backend>(terminal: &mut Terminal<B>, mouse: bool) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    take_terminal(mouse)?;
    repaint(terminal)
}

/// Forgets what is on screen so the next draw sends every cell. Resizing does
/// it: `Terminal::clear` queries the cursor, which hangs on terminals that
/// don't answer.
fn repaint<B: Backend>(terminal: &mut Terminal<B>) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))?;
    Ok(())
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
        let shown_new = printable::printable_line(&new_path.display().to_string());
        // Checked before creating the file, or the retry from a terminal
        // would find it already there and refuse it. A bad name or an
        // existing path is reported first, as it would be from a terminal.
        scaffold::validate_new_path(new_path)
            .with_context(|| format!("cannot create {shown_new}"))?;
        if !io::stdout().is_terminal() && new_path.symlink_metadata().is_err() {
            anyhow::bail!(
                "cannot create {shown_new}: markcheck is interactive and needs a terminal (stdout is not a TTY)"
            );
        }
        let created = scaffold::create_new_checklist(new_path).map_err(|err| {
            match scaffold::existing_file_message(new_path, &err) {
                Some(message) => anyhow::anyhow!(message),
                None => anyhow::Error::new(err).context(format!("cannot create {shown_new}")),
            }
        })?;
        (created, true)
    } else {
        // clap's `required_unless_present_any` guarantees this is `Some`
        // whenever `cli.new` is `None` (the generate flags returned above).
        let mut file = cli
            .file
            .clone()
            .expect("clap requires FILE when --new is absent");
        if let Some((path, anchor)) = cli::split_anchor(&file) {
            file = path;
            start_at = Some(model::StartAt::Anchor(anchor));
        }
        let resolved = std::fs::canonicalize(&file).map_err(|err| {
            match scaffold::missing_file_message(&file, &err) {
                Some(message) => anyhow::anyhow!(message),
                None => anyhow::Error::new(err).context(format!(
                    "cannot resolve path: {}",
                    printable::printable_line(&file.display().to_string())
                )),
            }
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
    // A write-protected file is refused on save (`writer::write_back`), and
    // a file or folder this user can't write may refuse it too
    // (`writer::save_route`), so say so before the first keypress rather
    // than at it.
    // Held until exit, so another session opening this checklist can tell.
    let (_session_marker, other_sessions) = writer::SessionMarker::register(&document.file_path);
    let read_only = writer::is_read_only(&document.file_path);
    let hard_links = writer::hard_link_count(&document.file_path);
    let save_route = writer::save_route(&document.file_path);
    let foreign_owner = writer::foreign_owner(&document.file_path).map(writer::user_label);

    let mut state = AppState::new(document);
    let settings = Settings::resolve(&cli, &config);
    if settings.nerd_font {
        state.icons = IconSet::nerd();
    }
    state.clipboard_primary = settings.primary;
    state.copy_route = settings.clipboard.route(cli::is_remote_session(
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
    // `detect` runs only `git version` and `rev-parse`, which run no hooks or
    // helpers, so the scope check comes before any command a nested
    // repository could hijack.
    let detected = request.requested.then(|| {
        git_sync::GitSync::detect(&state.document.file_path)
            .and_then(|sync| launch::git_sync_scope(&request, sync.repo_root()).map(|()| sync))
    });
    let detect_error = detected.as_ref().and_then(|d| d.as_ref().err()).cloned();
    let mut git_sync = detected.and_then(Result::ok);
    if let Some(sync) = git_sync.as_mut() {
        sync.set_delay(settings.git_sync_delay);
    }
    // What no signal handler can catch (SIGKILL, the OOM killer, a power
    // loss) still leaves these behind.
    let leftovers = writer::sweep_interrupted_saves(&state.document.file_path)
        + git_sync
            .as_ref()
            .map_or(0, git_sync::GitSync::sweep_interrupted_syncs);
    let warnings = [
        config::unknown_keys_warning(&unknown_keys),
        launch::startup_warning(read_only, save_route, hard_links, foreign_owner.as_deref()),
        launch::git_sync_warning(&request.unresolved, detect_error.as_deref()),
        git_sync
            .as_ref()
            .and_then(git_sync::GitSync::identity_warning),
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
    match launch::startup_message(created_new, opened_note.as_deref(), &warnings) {
        Some((message, true)) => state.set_error(message),
        Some((message, false)) => state.set_status(message),
        None => {}
    }
    // Drives the persistent `⇅ git` section in the title bar
    // (`render_title_bar` in `ui/mod.rs`) — set once here and never touched
    // again; `GitSyncState::last_at`/`busy`/`waiting` handle the per-sync timing,
    // this is just "is the feature on for this session at all".
    state.git_sync.active = git_sync.is_some();
    // A previous session may have quit before pushing its commit. This only
    // pushes; it never commits.
    if let Some(sync) = git_sync.as_mut() {
        sync.request_catch_up_push();
    }

    let mut signals = StopSignals::install().context("cannot install signal handlers")?;
    let mouse = settings.mouse;
    install_panic_hook();
    take_terminal(mouse)?;
    let show_title = settings.terminal_title;
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
        &mut signals,
        show_title,
    );
    drop(guard);
    clipboard::release_system_clipboard();
    if result.is_ok() && settings.exit_summary {
        let _ = writeln!(io::stdout(), "{}", launch::exit_summary(&state));
    }
    // A terminal error ends the loop without its own git-sync teardown, so it
    // gets the signal path's here.
    let report = match &result {
        Ok(report) => report.clone(),
        Err(_) => git_sync.as_mut().and_then(|sync| {
            sync.flush();
            if sync.is_busy() {
                let _ = writeln!(io::stderr(), "markcheck: waiting for git sync…");
            }
            settle_git_sync(sync, &signals)
        }),
    };
    // Printed only once the alternate screen is gone, so it stays visible.
    // Ignored on failure: after SIGHUP there may be no terminal left.
    if let Some(report) = report {
        let _ = writeln!(io::stderr(), "{report}");
    }
    if result.is_ok() && signals.hurried() {
        anyhow::bail!("stopped by a second signal");
    }
    result.map(|_| ())
}

/// `SIGTERM`, `SIGHUP` (a closed terminal or dropped SSH session) and a
/// `SIGINT` from `kill` set `stop`, so `run` ends the way `q` does and git
/// sync is waited for and cleaned up. A second `SIGTERM` or `SIGINT` cuts
/// that wait short: the sync is killed at once and the terminal still
/// restored. Only a third exits on the spot.
struct StopSignals {
    stop: Arc<AtomicBool>,
    repeats: Arc<AtomicUsize>,
    interrupt: [signal_hook::SigId; 2],
}

impl StopSignals {
    fn install() -> io::Result<Self> {
        use signal_hook::consts::{SIGHUP, SIGTERM};
        use signal_hook::flag;

        let stop = Arc::new(AtomicBool::new(false));
        let repeats = Arc::new(AtomicUsize::new(0));
        Self::register_repeat(SIGTERM, &repeats)?;
        for signal in [SIGTERM, SIGHUP] {
            flag::register(signal, Arc::clone(&stop))?;
        }
        let interrupt = Self::register_interrupt(&stop, &repeats)?;
        Ok(Self {
            stop,
            repeats,
            interrupt,
        })
    }

    fn register_interrupt(
        stop: &Arc<AtomicBool>,
        repeats: &Arc<AtomicUsize>,
    ) -> io::Result<[signal_hook::SigId; 2]> {
        use signal_hook::consts::SIGINT;

        Ok([
            Self::register_repeat(SIGINT, repeats)?,
            signal_hook::flag::register(SIGINT, Arc::clone(stop))?,
        ])
    }

    /// Counts `signal`, and exits on the third `SIGTERM`/`SIGINT` in all.
    fn register_repeat(
        signal: libc::c_int,
        repeats: &Arc<AtomicUsize>,
    ) -> io::Result<signal_hook::SigId> {
        let repeats = Arc::clone(repeats);
        let action = move || {
            if repeats.fetch_add(1, Ordering::SeqCst) >= 2 {
                signal_hook::low_level::exit(1);
            }
        };
        // SAFETY: the action only touches an atomic and calls `_exit`, both
        // async-signal-safe.
        unsafe { signal_hook::low_level::register(signal, action) }
    }

    /// A second `SIGTERM`/`SIGINT` arrived: stop waiting for git sync.
    fn hurried(&self) -> bool {
        self.repeats.load(Ordering::SeqCst) >= 2
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Runs `child` with `SIGINT` caught and dropped. Without raw mode the
    /// terminal sends its interrupt key (`Ctrl-C`, or `C-g` under Emacs) to
    /// the whole foreground process group, markcheck included, though it is
    /// meant for the editor. Caught rather than ignored, so the child still
    /// starts with the default action.
    fn ignoring_interrupts<T>(&mut self, child: impl FnOnce() -> T) -> io::Result<T> {
        use signal_hook::consts::SIGINT;
        use signal_hook::low_level::unregister;

        let dropped = signal_hook::flag::register(SIGINT, Arc::new(AtomicBool::new(false)))?;
        for id in self.interrupt {
            unregister(id);
        }
        let result = child();
        let restored = Self::register_interrupt(&self.stop, &self.repeats);
        if let Ok(interrupt) = restored {
            self.interrupt = interrupt;
            unregister(dropped);
        }
        restored.map(|_| result)
    }
}

/// Shown while `q` waits for a sync still running.
const WAITING_FOR_SYNC: &str = "Waiting for git sync… press q again to abandon";

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    reload: &mut ReloadTrigger,
    git_sync: &mut Option<git_sync::GitSync>,
    mouse: bool,
    signals: &mut StopSignals,
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
    // Set once `q` found a sync still running, which it then waits for.
    let mut quitting = false;
    let mut title = TerminalTitle::new(show_title);
    loop {
        // Nothing is drawn from here on: after SIGHUP the terminal is gone.
        if signals.stopped() {
            return Ok(git_sync.as_mut().and_then(|sync| {
                sync.flush();
                settle_git_sync(sync, signals)
            }));
        }
        dirty |= reload.check(state, Instant::now());
        while let Ok(message) = opener_failure_rx.try_recv() {
            state.set_error(message);
            dirty = true;
        }
        // Drain a finished background sync before requesting the next
        // one, so a request queued in the finished run's `GitSync::queued` slot
        // (see `GitSync::poll`) is already spawned by the time this
        // iteration's effects request another.
        if let Some(sync) = git_sync.as_mut() {
            if let Some(outcome) = sync.poll() {
                state.apply_git_sync_outcome(&outcome);
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
            // Debug builds only: lets `tests/pty.rs` make a draw fail the way
            // one to a terminal that has just hung up does.
            #[cfg(debug_assertions)]
            if std::env::var_os("MARKCHECK_TEST_FAIL_DRAW")
                .is_some_and(|trigger| std::path::Path::new(&trigger).exists())
            {
                anyhow::bail!("forced test draw failure");
            }
            terminal.draw(|frame| ui::render(frame, state))?;
            title.show(state);
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
                        return Ok(finish_git_sync(sync, signals));
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
                signals,
                title: &mut title,
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
        .and_then(|sync| finish_git_sync(sync, signals)))
}

/// Carries out `AppState`'s effects against the real terminal, processes
/// and git-sync worker.
struct TerminalHost<'a, B: Backend> {
    terminal: &'a mut Terminal<B>,
    git_sync: &'a mut Option<git_sync::GitSync>,
    mouse: bool,
    signals: &'a mut StopSignals,
    title: &'a mut TerminalTitle,
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
        open_in_editor(
            self.terminal,
            state,
            line,
            self.mouse,
            self.signals,
            self.title,
        )
    }

    fn suspend(&mut self, state: &mut AppState) -> anyhow::Result<()> {
        let still_running = match self.git_sync.as_mut() {
            Some(sync) => settle_git_sync_before_suspend(self.terminal, state, sync, self.signals)?,
            None => false,
        };
        if self.signals.stopped() {
            return Ok(());
        }
        suspend_to_shell(self.terminal, state, self.mouse, self.title, still_running)
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        repaint(self.terminal)
    }

    fn open_link(&mut self, state: &mut AppState, url: &str) -> anyhow::Result<()> {
        let TerminalHost {
            terminal,
            mouse,
            signals,
            title,
            opener_failures,
            ..
        } = self;
        open_link(url, state, opener_failures, &mut |program, args| {
            run_in_terminal(terminal, *mouse, signals, title, program, args)
        })
    }

    fn git_sync(&mut self, sync: model::PendingSync) {
        if let Some(worker) = self.git_sync.as_mut() {
            worker.request(sync);
        }
    }

    fn cancel_git_sync(&mut self) -> bool {
        self.git_sync
            .as_mut()
            .is_some_and(git_sync::GitSync::cancel_queued)
    }
}

/// How long a signal lets a running sync finish before it is abandoned, and
/// `Ctrl-Z` waits for one before stopping anyway.
const QUIT_WAIT: Duration = Duration::from_secs(5);
/// How long an abandoned sync's worker gets to unwind through its cleanup
/// after `SIGTERM`, and then after `SIGKILL`.
const ABANDON_GRACE: Duration = Duration::from_secs(2);

/// Stops a running sync's git processes, then waits for the worker to remove
/// its temporary index and scratch files. A hurried quit skips straight to
/// `SIGKILL`.
fn abandon_git_sync(sync: &mut git_sync::GitSync, signals: &StopSignals) {
    if !signals.hurried() {
        sync.abandon(false);
        sync.wait_for_outcomes_unless(ABANDON_GRACE, || signals.hurried());
    }
    if sync.is_busy() {
        sync.abandon(true);
        sync.wait_for_outcomes(ABANDON_GRACE);
    }
}

/// Gives a running sync up to [`QUIT_WAIT`] to land, unless a second signal
/// cuts the wait short, then finishes it as [`finish_git_sync`] does.
fn settle_git_sync(sync: &mut git_sync::GitSync, signals: &StopSignals) -> Option<String> {
    sync.wait_for_outcomes_unless(QUIT_WAIT, || signals.hurried());
    finish_git_sync(sync, signals)
}

/// Abandons whatever is still running, then gives the line to print after
/// the TUI closes for a sync problem still standing, however long before the
/// quit it was reported.
fn finish_git_sync(sync: &mut git_sync::GitSync, signals: &StopSignals) -> Option<String> {
    if sync.is_busy() {
        abandon_git_sync(sync, signals);
    }
    git_sync::exit_report(sync.unresolved(), sync.is_busy())
}

/// Runs `program` with the whole terminal, as `e` runs the editor, and
/// takes it back once the program exits.
type RunInTerminal<'a> =
    dyn FnMut(&str, &[String]) -> anyhow::Result<io::Result<std::process::ExitStatus>> + 'a;

/// Opens `url`, trying each resolved opener in turn until one starts. A
/// terminal browser (`launch::is_terminal_browser`) gets the terminal through
/// `run_here` until it exits; any other opener runs without suspending the
/// TUI, with no shell and discarded output, reaped on a detached thread that
/// sends a quick failed exit to `failures`. With no browser to send to at all
/// (a headless session), copies the link instead; over SSH, copies it as well
/// (see `launch::link_route`).
fn open_link(
    url: &str,
    state: &mut AppState,
    failures: &std::sync::mpsc::Sender<String>,
    run_here: &mut RunInTerminal<'_>,
) -> anyhow::Result<()> {
    let browser = std::env::var("BROWSER").ok();
    let display = std::env::var("DISPLAY").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let remote = cli::is_remote_session(
        std::env::var("SSH_CONNECTION").ok().as_deref(),
        std::env::var("SSH_TTY").ok().as_deref(),
    );
    match launch::link_route(
        browser.as_deref(),
        display.as_deref(),
        wayland_display.as_deref(),
        remote,
    ) {
        launch::LinkRoute::CopyInstead => state.copy_link_instead(url),
        launch::LinkRoute::Open => {
            spawn_opener(url, browser.as_deref(), false, state, failures, run_here)?;
        }
        launch::LinkRoute::OpenAndCopy => {
            spawn_opener(url, browser.as_deref(), true, state, failures, run_here)?;
            state.copy_link_too(url);
        }
    }
    Ok(())
}

/// Tries each resolved opener for `url` in turn until one starts.
/// `on_this_host` words the confirmation for an SSH session.
fn spawn_opener(
    url: &str,
    browser: Option<&str>,
    on_this_host: bool,
    state: &mut AppState,
    failures: &std::sync::mpsc::Sender<String>,
    run_here: &mut RunInTerminal<'_>,
) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};

    let (candidates, warning) = launch::resolve_opener(browser, url);
    let mut last_error = None;
    for (program, args) in &candidates {
        if launch::is_terminal_browser(program) {
            match run_here(program, args)? {
                Err(err) => last_error = Some(format!("{program}: {err}")),
                outcome => {
                    state.finish_terminal_browser(program, url, &outcome);
                    if let Some(warning) = warning {
                        let shown = match state.status_message.take() {
                            Some(shown) => format!("{shown}; {warning}"),
                            None => warning,
                        };
                        state.set_error(shown);
                    }
                    return Ok(());
                }
            }
            continue;
        }
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                state.report_link_sent(program, url, on_this_host);
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
                return Ok(());
            }
            Err(err) => last_error = Some(format!("{program}: {err}")),
        }
    }
    if let Some(err) = last_error {
        state.set_error(format!("Couldn't open link ({err})"));
    }
    Ok(())
}

/// Hands the terminal to `program` (a terminal browser) until it exits, then
/// restores the TUI. Its outcome is `Err` when it couldn't be started.
fn run_in_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    mouse: bool,
    signals: &mut StopSignals,
    title: &mut TerminalTitle,
    program: &str,
    args: &[String],
) -> anyhow::Result<io::Result<std::process::ExitStatus>>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    leave_tui(mouse)?;
    title.release();
    let status =
        signals.ignoring_interrupts(|| std::process::Command::new(program).args(args).status());
    title.reclaim();
    enter_tui(terminal, mouse)?;
    status.context("cannot restore the SIGINT handler")
}

/// Suspends the TUI, launches the editor on the current file, then
/// restores the terminal and reloads. Always restores, even if the
/// editor fails to spawn.
fn open_in_editor<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    line: Option<model::LineNumber>,
    mouse: bool,
    signals: &mut StopSignals,
    title: &mut TerminalTitle,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use std::process::Command;

    leave_tui(mouse)?;
    title.release();

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
    let status = signals.ignoring_interrupts(|| command.status());
    let elapsed = started.elapsed();

    // Restore the terminal regardless of how the editor fared.
    title.reclaim();
    enter_tui(terminal, mouse)?;
    let status = status.context("cannot restore the SIGINT handler")?;

    state.finish_editor(&program, warning, status, elapsed);
    Ok(())
}

/// Printed on the shell when `Ctrl-Z` stops markcheck with a sync running.
const SUSPENDED_MID_SYNC: &str = "markcheck: git sync paused while suspended; fg to finish it";

/// A stopped process commits and pushes nothing, so before `Ctrl-Z` stops it
/// a change still in its quiet period starts now and a running sync gets up
/// to [`QUIT_WAIT`] to land, or until a signal asks markcheck to quit.
/// Returns whether one is still running.
fn settle_git_sync_before_suspend<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    sync: &mut git_sync::GitSync,
    signals: &StopSignals,
) -> anyhow::Result<bool>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    sync.flush();
    if sync.is_busy() {
        state.wait_for_sync_before_suspend();
        state.git_sync.busy = true;
        state.git_sync.waiting = false;
        terminal.draw(|frame| ui::render(frame, state))?;
        for outcome in sync.wait_for_outcomes_unless(QUIT_WAIT, || signals.stopped()) {
            state.apply_git_sync_outcome(&outcome);
        }
        state.finish_waiting_to_suspend();
    }
    Ok(sync.is_busy())
}

/// `Ctrl-Z`: hands the terminal back to the shell, saying so when a git sync
/// is left `sync_running`, and stops the process until `fg` resumes it, then
/// restores the TUI and picks up any edit made meanwhile. Raw mode turns off `ISIG`, so the terminal itself never sends
/// `SIGTSTP` for the keypress.
fn suspend_to_shell<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    mouse: bool,
    title: &mut TerminalTitle,
    sync_running: bool,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    leave_tui(mouse)?;
    title.release();
    if sync_running {
        let _ = writeln!(io::stderr(), "{SUSPENDED_MID_SYNC}");
    }
    let outcome = stop_self();
    title.reclaim();
    enter_tui(terminal, mouse)?;

    state.finish_suspend(outcome);
    Ok(())
}

/// Sends `SIGTSTP` to this process. A signal a process sends itself is
/// delivered before `kill` returns, so this returns only once it's been
/// resumed.
fn stop_self() -> Result<(), String> {
    // SAFETY: `getpid` and `kill` take plain integers and touch no memory.
    if unsafe { libc::kill(libc::getpid(), libc::SIGTSTP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}
