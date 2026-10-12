mod app;
mod clipboard;
mod config;
mod git_sync;
mod model;
mod parser;
mod scaffold;
#[cfg(test)]
mod test_support;
mod ui;
mod watcher;
mod writer;

use std::io;
use std::path::PathBuf;
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

use model::{AppState, IconSet};
use watcher::FileWatcher;

/// `<cargo version> (<git short sha>)`, e.g. `1.0.0 (da90ddd)`. The SHA comes
/// from `build.rs`, which shells out to `git rev-parse --short HEAD` at
/// compile time and falls back to "unknown" outside a git checkout (e.g. a
/// source tarball).
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("MARKCHECK_GIT_SHA"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "markcheck",
    about = "Pilot-checklist TUI for Markdown runbooks",
    version = VERSION
)]
struct Cli {
    /// Path to the Markdown file (omit when using --new)
    #[arg(required_unless_present = "new")]
    file: Option<PathBuf>,
    /// Create a new starter checklist at PATH, then open it (PATH must not
    /// already exist and must end in .md)
    #[arg(long, value_name = "PATH", conflicts_with = "file")]
    new: Option<PathBuf>,
    /// Use Nerd Font glyphs, overriding `nerd_font = false` in the config
    #[arg(long, conflicts_with = "no_nerd_font")]
    nerd_font: bool,
    /// Use plain Unicode symbols instead of Nerd Font glyphs (config: nerd_font = false)
    #[arg(long)]
    no_nerd_font: bool,
    /// Enable mouse support, overriding `mouse = false` in the config
    #[arg(long, conflicts_with = "no_mouse")]
    mouse: bool,
    /// Disable mouse support (terminal text selection works without Shift) (config: mouse = false)
    #[arg(long)]
    no_mouse: bool,
    /// Also copy to the X11 PRIMARY selection (middle-click paste) (config: primary = true)
    #[arg(long)]
    primary: bool,
    /// Don't copy to the X11 PRIMARY selection, overriding `primary = true` in the config
    #[arg(long, conflicts_with = "primary")]
    no_primary: bool,
    /// Auto-copy an item's code to the clipboard when navigating to it (config: auto_copy = true)
    #[arg(long)]
    auto_copy: bool,
    /// Don't auto-copy on navigation, overriding `auto_copy = true` in the config
    #[arg(long, conflicts_with = "auto_copy")]
    no_auto_copy: bool,
    /// Commit and push this file's changes after every toggle, when it's inside a
    /// git repo (config: git_sync_paths matches this file's path)
    #[arg(long)]
    git_sync: bool,
}

struct TerminalGuard {
    mouse: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.mouse {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}

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
    let document = parser::parse_document(file_path)?;

    // The parser drops lists without checklist items, so no lists at all
    // means the file has nothing to work through.
    if document.lists.is_empty() {
        eprintln!("No checklist items found in file.");
        return Ok(());
    }

    // Warn *before* the first write, while quitting still costs nothing: a
    // toggle renames a new inode over this path and any hard-linked alias
    // silently stops tracking the checklist. See `writer::hard_link_count`
    // for why this warns rather than refuses.
    let hard_links = writer::hard_link_count(&document.file_path);

    let mut state = AppState::new(document);
    if let Some(links) = hard_links.filter(|links| *links > 1) {
        state.set_error(format!(
            "Warning: {links} hard links to this file — a save leaves the others behind"
        ));
    }
    if created_new {
        state.set_status("Created new checklist".to_string());
    }
    let settings = Settings::resolve(&cli, &config);
    if !settings.nerd_font {
        state.icons = IconSet::unicode();
    }
    state.clipboard_primary = settings.primary;
    state.auto_copy = settings.auto_copy;
    // Pick the richest color representation the terminal supports.
    state.palette = model::Palette::detect();
    // Reload-from-disk is a convenience feature, not core functionality:
    // if the watch can't be set up (e.g. inotify unavailable), continue
    // without it rather than failing to start.
    let watcher = FileWatcher::new(&state.document.file_path).ok();

    // Git-sync activates only if requested and `GitSync::detect` finds a work
    // tree.
    let git_sync_requested = cli.git_sync
        || config
            .git_sync_paths
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|prefix| git_sync_path_matches(&state.document.file_path, prefix));
    let mut git_sync = git_sync_requested
        .then(|| git_sync::GitSync::detect(&state.document.file_path))
        .flatten();
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
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    let _guard = TerminalGuard { mouse };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(
        &mut terminal,
        &mut state,
        watcher.as_ref(),
        &mut git_sync,
        mouse,
    );
    clipboard::release_system_clipboard();
    result
}

/// The four boolean settings, resolved in one place so the wiring can be
/// tested through real argv.
#[derive(Debug, PartialEq, Eq)]
struct Settings {
    nerd_font: bool,
    mouse: bool,
    primary: bool,
    auto_copy: bool,
}

impl Settings {
    fn resolve(cli: &Cli, config: &config::Config) -> Self {
        Settings {
            nerd_font: resolve_flag(cli.nerd_font, cli.no_nerd_font, config.nerd_font, true),
            mouse: resolve_flag(cli.mouse, cli.no_mouse, config.mouse, true),
            primary: resolve_flag(cli.primary, cli.no_primary, config.primary, false),
            auto_copy: resolve_flag(cli.auto_copy, cli.no_auto_copy, config.auto_copy, false),
        }
    }
}

/// Resolves one boolean setting: a passed flag wins, then the config, then
/// the default. Pass `false` for a flag a setting doesn't have. clap's
/// `conflicts_with` keeps both from being set.
fn resolve_flag(cli_on: bool, cli_off: bool, config: Option<bool>, default: bool) -> bool {
    if cli_on {
        return true;
    }
    if cli_off {
        return false;
    }
    config.unwrap_or(default)
}

/// Whether a configured `git_sync_paths` entry covers the canonical
/// `file_path`. The prefix is canonicalized too, and compared by path
/// components, so `/foo/bar` doesn't match `/foo/barn`.
fn git_sync_path_matches(file_path: &std::path::Path, prefix: &std::path::Path) -> bool {
    std::fs::canonicalize(prefix).is_ok_and(|canonical| file_path.starts_with(canonical))
}

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    watcher: Option<&FileWatcher>,
    git_sync: &mut Option<git_sync::GitSync>,
    mouse: bool,
) -> anyhow::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        if watcher.is_some_and(FileWatcher::poll_changed) {
            state.reload_if_changed();
        }
        // Drain a finished background sync before requesting the next
        // one, so a request queued during the finished run's `pending` slot
        // (see `GitSync::poll`) is already spawned by the time we check
        // `take_git_sync_request` below.
        if let Some(sync) = git_sync.as_mut() {
            if let Some(outcome) = sync.poll() {
                match outcome {
                    git_sync::SyncOutcome::Synced => state.record_git_sync(),
                    // Both are "nothing happened, and that's fine": an
                    // ordinary no-op sync, and a retry giving up on a commit
                    // something else has already superseded. Neither is worth
                    // interrupting the user for.
                    git_sync::SyncOutcome::Skipped | git_sync::SyncOutcome::RetryAbandoned => {}
                    git_sync::SyncOutcome::SkippedUntracked => state.set_error(
                        "Git sync skipped: file is not tracked in git (run `git add` on it)"
                            .to_string(),
                    ),
                    git_sync::SyncOutcome::CommittedNotPushed { message, .. } => state.set_error(
                        format!("Committed, not pushed: {message} (will keep retrying)"),
                    ),
                    git_sync::SyncOutcome::Failed(msg) => {
                        state.set_error(format!("Git sync failed: {msg}"))
                    }
                    // A retry of an already-made commit failed. The commit is
                    // still safely local and still armed, so say so rather
                    // than implying the change was lost.
                    git_sync::SyncOutcome::RetryFailed(msg) => {
                        state.set_error(format!("Push retry failed: {msg} (will keep retrying)"))
                    }
                }
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
        state.expire_status(SystemTime::now());
        terminal.draw(|frame| ui::render(frame, state))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
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
        }

        if let Some(url) = state.take_link_open_request() {
            open_link(&url, state);
        }

        if state.should_quit {
            break;
        }
    }

    // The last action may have queued a sync, which takes several git
    // subprocesses; give it a bounded window to land before exiting.
    if let Some(sync) = git_sync.as_mut() {
        if let Some(pending) = state.take_git_sync_request() {
            sync.request(pending);
        }
        wait_for_git_sync(sync);
    }

    Ok(())
}

/// Polls `sync` until its in-flight (and any coalesced-behind-it) request
/// settles, or `timeout` elapses — whichever comes first. Only meant to be
/// called once, right before quitting.
fn wait_for_git_sync(sync: &mut git_sync::GitSync) {
    const TIMEOUT: Duration = Duration::from_secs(5);
    let deadline = Instant::now() + TIMEOUT;
    while sync.is_busy() && Instant::now() < deadline {
        sync.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The platform default opener when `$BROWSER` is unset or unusable: `open` on
/// macOS, `xdg-open` elsewhere.
fn default_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Shell-word-splits an env-var command into `(program, args, warning)`.
/// Blank or empty values fall back silently; unbalanced quotes fall back
/// with a warning naming `var_name`.
fn resolve_command(
    command: Option<&str>,
    fallback: &str,
    var_name: &str,
) -> (String, Vec<String>, Option<String>) {
    let Some(command) = command.map(str::trim).filter(|v| !v.is_empty()) else {
        return (fallback.to_string(), Vec::new(), None);
    };
    match shell_words::split(command) {
        Ok(mut parts) if !parts.is_empty() && !parts[0].is_empty() => {
            let program = parts.remove(0);
            (program, parts, None)
        }
        Ok(_) => (fallback.to_string(), Vec::new(), None),
        Err(err) => (
            fallback.to_string(),
            Vec::new(),
            Some(format!(
                "Couldn't parse {var_name} ({err}); falling back to {fallback}"
            )),
        ),
    }
}

/// `$BROWSER` (shell-word-split), else the platform default.
fn resolve_opener(browser: Option<&str>) -> (String, Vec<String>, Option<String>) {
    resolve_command(browser, default_opener(), "$BROWSER")
}

/// Opens `url` without suspending the TUI. No shell, discarded output, and
/// the child is reaped on a detached thread.
fn open_link(url: &str, state: &mut AppState) {
    use std::process::{Command, Stdio};

    let (program, mut args, warning) = resolve_opener(std::env::var("BROWSER").ok().as_deref());
    if let Some(warning) = warning {
        state.set_error(warning);
    }
    args.push(url.to_string());
    match Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => state.set_error(format!("Couldn't open link ({program}): {err}")),
    }
}

/// `$VISUAL`, then `$EDITOR`, then `vi`, shell-word-split so
/// `EDITOR="code --wait"` works.
fn resolve_editor(
    visual: Option<&str>,
    editor: Option<&str>,
) -> (String, Vec<String>, Option<String>) {
    let chosen = [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty());
    resolve_command(chosen, "vi", "$EDITOR/$VISUAL")
}

/// `+N` for the terminal editors that understand it (matched on basename);
/// nothing for the rest.
fn editor_line_args(program: &str, line: usize) -> Vec<String> {
    let base = std::path::Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    match base {
        "vi" | "vim" | "nvim" | "view" | "nano" | "pico" | "emacs" | "emacsclient" | "joe"
        | "gedit" => vec![format!("+{line}")],
        _ => Vec::new(),
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

    let (program, args, warning) = resolve_editor(
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
        command.args(editor_line_args(&program, line));
    }
    let status = command.arg(&state.document.file_path).status();

    // Restore the terminal regardless of how the editor fared.
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Force a full repaint by resizing: `Terminal::clear` queries the cursor,
    // which hangs on terminals that don't answer.
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))?;

    match status {
        Err(err) => {
            state.set_error(format!("Failed to launch {program}: {err}"));
        }
        Ok(status) if !status.success() => {
            state.set_error(format!("{program} exited with {status}"));
        }
        Ok(_) => {}
    }

    // Reload now, and queue a git-sync request: this edit came from our own
    // editor launch.
    if state.reload_if_changed() {
        state.request_external_edit_sync(&program);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Settings, config, editor_line_args, git_sync_path_matches, resolve_editor,
        resolve_flag, resolve_opener,
    };
    use clap::Parser as _;

    #[test]
    fn resolve_flag_prefers_an_explicit_flag_then_config_then_the_default() {
        // The (true, true) row is ruled out by clap's `conflicts_with`.
        for default in [false, true] {
            // No flag passed: the config wins, else the default.
            assert_eq!(resolve_flag(false, false, None, default), default);
            assert!(resolve_flag(false, false, Some(true), default));
            assert!(!resolve_flag(false, false, Some(false), default));

            // A passed flag beats the config in both directions.
            for config in [None, Some(false), Some(true)] {
                assert!(
                    resolve_flag(true, false, config, default),
                    "the positive flag must win over {config:?}"
                );
                assert!(
                    !resolve_flag(false, true, config, default),
                    "the negative flag must win over {config:?}"
                );
            }
        }
    }

    /// Resolve the settings from real argv plus a config, so the test
    /// exercises clap's own parsing and the call sites together.
    fn settings_from(flags: &[&str], config: config::Config) -> Settings {
        let mut argv = vec!["markcheck", "checklist.md"];
        argv.extend_from_slice(flags);
        Settings::resolve(&Cli::parse_from(argv), &config)
    }

    fn config_all(value: bool) -> config::Config {
        config::Config {
            nerd_font: Some(value),
            mouse: Some(value),
            primary: Some(value),
            auto_copy: Some(value),
            git_sync_paths: None,
        }
    }

    #[test]
    fn settings_resolve_the_built_in_defaults_with_no_flags_and_no_config() {
        assert_eq!(
            settings_from(&[], config::Config::default()),
            Settings {
                nerd_font: true,
                mouse: true,
                primary: false,
                auto_copy: false,
            }
        );
    }

    #[test]
    fn settings_take_the_config_when_no_flag_is_passed() {
        assert_eq!(
            settings_from(&[], config_all(true)),
            Settings {
                nerd_font: true,
                mouse: true,
                primary: true,
                auto_copy: true,
            }
        );
        assert_eq!(
            settings_from(&[], config_all(false)),
            Settings {
                nerd_font: false,
                mouse: false,
                primary: false,
                auto_copy: false,
            }
        );
    }

    #[test]
    fn every_setting_can_be_forced_on_over_a_config_that_disables_it() {
        // `nerd_font` and `mouse` need a positive flag to override the config.
        let settings = settings_from(
            &["--nerd-font", "--mouse", "--primary", "--auto-copy"],
            config_all(false),
        );
        assert_eq!(
            settings,
            Settings {
                nerd_font: true,
                mouse: true,
                primary: true,
                auto_copy: true,
            },
            "a passed flag must beat the config in the positive direction"
        );
    }

    #[test]
    fn every_setting_can_be_forced_off_over_a_config_that_enables_it() {
        let settings = settings_from(
            &[
                "--no-nerd-font",
                "--no-mouse",
                "--no-primary",
                "--no-auto-copy",
            ],
            config_all(true),
        );
        assert_eq!(
            settings,
            Settings {
                nerd_font: false,
                mouse: false,
                primary: false,
                auto_copy: false,
            },
            "a passed flag must beat the config in the negative direction"
        );
    }

    #[test]
    fn each_setting_s_two_flags_conflict() {
        // clap enforcing this is what lets `resolve_flag` ignore the
        // both-set row entirely.
        for pair in [
            ["--nerd-font", "--no-nerd-font"],
            ["--mouse", "--no-mouse"],
            ["--primary", "--no-primary"],
            ["--auto-copy", "--no-auto-copy"],
        ] {
            let result = Cli::try_parse_from(["markcheck", "checklist.md", pair[0], pair[1]]);
            assert!(result.is_err(), "{pair:?} must be rejected together");
        }
    }

    #[test]
    fn opener_prefers_browser_then_platform_default() {
        let (prog, args, warning) = resolve_opener(Some("firefox --new-tab"));
        assert_eq!(prog, "firefox");
        assert_eq!(args, vec!["--new-tab".to_string()]);
        assert!(warning.is_none());
        // Empty/absent $BROWSER falls back to the platform opener.
        let (prog, args, warning) = resolve_opener(None);
        assert!(prog == "xdg-open" || prog == "open");
        assert!(args.is_empty());
        assert!(warning.is_none());
        assert_eq!(resolve_opener(Some("   ")).1, Vec::<String>::new());
    }

    #[test]
    fn opener_quoted_argument_with_spaces_stays_one_token() {
        // Shell-word splitting (not plain whitespace) so a quoted argument
        // containing spaces survives as a single token.
        let (prog, args, warning) = resolve_opener(Some(r#"my-wrapper --arg="value with spaces""#));
        assert_eq!(prog, "my-wrapper");
        assert_eq!(args, vec!["--arg=value with spaces".to_string()]);
        assert!(warning.is_none());
    }

    #[test]
    fn opener_validly_quoted_empty_value_falls_back_silently() {
        // A value that shell-quotes to nothing (e.g. `BROWSER="''"`) must
        // fall back the same as an unset/whitespace-only $BROWSER, not
        // attempt to launch a program with an empty name (`shell_words::split`
        // returns one empty token for this, not an empty list).
        let (prog, args, warning) = resolve_opener(Some("''"));
        assert!(prog == "xdg-open" || prog == "open");
        assert!(args.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn opener_malformed_quoting_falls_back_with_a_warning() {
        // Unbalanced quotes can't be parsed; fall back to the platform
        // default rather than misparsing, and surface why.
        let (prog, args, warning) = resolve_opener(Some("my-wrapper 'unterminated"));
        assert!(prog == "xdg-open" || prog == "open");
        assert!(args.is_empty());
        assert!(
            warning.is_some_and(|w| w.contains("$BROWSER")),
            "warning names the offending variable"
        );
    }

    #[test]
    fn visual_takes_priority_over_editor() {
        let (prog, args, warning) = resolve_editor(Some("hx"), Some("vim"));
        assert_eq!(prog, "hx");
        assert!(args.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn editor_used_when_visual_absent_or_empty() {
        assert_eq!(resolve_editor(None, Some("vim")).0, "vim");
        assert_eq!(resolve_editor(Some("  "), Some("vim")).0, "vim");
    }

    #[test]
    fn falls_back_to_vi() {
        assert_eq!(resolve_editor(None, None), ("vi".to_string(), vec![], None));
        assert_eq!(
            resolve_editor(Some(""), Some("")),
            ("vi".to_string(), vec![], None)
        );
    }

    #[test]
    fn editor_validly_quoted_empty_value_falls_back_to_vi_silently() {
        // Mirrors the opener case: `EDITOR="''"` splits to one empty token,
        // not an empty list, and must still fall back to vi rather than
        // attempting to launch a program with an empty name.
        let (prog, args, warning) = resolve_editor(None, Some("''"));
        assert_eq!(prog, "vi");
        assert!(args.is_empty());
        assert!(warning.is_none());
    }

    #[test]
    fn splits_program_and_args() {
        let (prog, args, warning) = resolve_editor(Some("code --wait -n"), None);
        assert_eq!(prog, "code");
        assert_eq!(args, vec!["--wait".to_string(), "-n".to_string()]);
        assert!(warning.is_none());
    }

    #[test]
    fn editor_quoted_argument_with_spaces_stays_one_token() {
        // Mirrors the opener case for $EDITOR/$VISUAL.
        let (prog, args, warning) =
            resolve_editor(None, Some(r#"my-wrapper --arg='value with spaces'"#));
        assert_eq!(prog, "my-wrapper");
        assert_eq!(args, vec!["--arg=value with spaces".to_string()]);
        assert!(warning.is_none());
    }

    #[test]
    fn editor_malformed_quoting_falls_back_to_vi_with_a_warning() {
        // Unbalanced quotes fall back to vi rather than misparsing, with an
        // explanatory warning rather than silence.
        let (prog, args, warning) = resolve_editor(None, Some("my-editor 'unterminated"));
        assert_eq!(prog, "vi");
        assert!(args.is_empty());
        assert!(
            warning.is_some_and(|w| w.contains("$EDITOR") || w.contains("$VISUAL")),
            "warning names the offending variable"
        );
    }

    #[test]
    fn line_arg_added_for_plusn_editors() {
        // The `+N file` convention: one `+N` arg, spliced before the path.
        for prog in [
            "vi",
            "vim",
            "nvim",
            "view",
            "nano",
            "pico",
            "emacs",
            "emacsclient",
            "joe",
            "gedit",
        ] {
            assert_eq!(editor_line_args(prog, 5), vec!["+5".to_string()], "{prog}");
        }
    }

    #[test]
    fn line_arg_matches_on_basename() {
        // A full path or a versioned wrapper still resolves to the base name.
        assert_eq!(
            editor_line_args("/usr/bin/vim", 12),
            vec!["+12".to_string()]
        );
        assert_eq!(
            editor_line_args("/opt/nvim/bin/nvim", 1),
            vec!["+1".to_string()]
        );
    }

    #[test]
    fn no_line_arg_for_non_plusn_editors() {
        // GUI editors that don't use `+N` and anything unrecognised open at the
        // top of the file (empty args, current behaviour).
        for prog in [
            "code",
            "subl",
            "hx",
            "kak",
            "micro",
            "notepad",
            "unknown-editor",
        ] {
            assert!(editor_line_args(prog, 5).is_empty(), "{prog}");
        }
    }

    #[test]
    fn git_sync_path_matches_requires_a_real_directory_boundary() {
        // `Path::starts_with` compares components, so a sibling sharing the prefix
        // must not match.
        let root = crate::test_support::unique_temp_path("main-gitsync", "", None);
        std::fs::create_dir_all(&root).unwrap();
        let checklists = root.join("checklists");
        let checklists_secret = root.join("checklists-secret");
        std::fs::create_dir_all(&checklists).unwrap();
        std::fs::create_dir_all(&checklists_secret).unwrap();
        let file = std::fs::canonicalize(&checklists).unwrap().join("todo.md");

        assert!(
            git_sync_path_matches(&file, &checklists),
            "a file under the configured directory must match"
        );
        assert!(
            !git_sync_path_matches(&file, &checklists_secret),
            "a same-prefix sibling directory must not match"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_sync_path_matches_rejects_a_nonexistent_prefix() {
        // Fails open (no match), the same as `starts_with` failing outright
        // would — not an error, and not a match by accident.
        let root = crate::test_support::unique_temp_path("main-gitsync-missing", "", None);
        std::fs::create_dir_all(&root).unwrap();
        let file = std::fs::canonicalize(&root).unwrap().join("todo.md");

        assert!(!git_sync_path_matches(&file, &root.join("does-not-exist")));

        std::fs::remove_dir_all(&root).ok();
    }
}
