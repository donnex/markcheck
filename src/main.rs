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
use std::time::{Duration, SystemTime};

use anyhow::Context;
use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
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
    /// Use plain Unicode symbols instead of Nerd Font glyphs (config: nerd_font = false)
    #[arg(long)]
    no_nerd_font: bool,
    /// Disable mouse support (terminal text selection works without Shift) (config: mouse = false)
    #[arg(long)]
    no_mouse: bool,
    /// Also copy to the X11 PRIMARY selection (middle-click paste) (config: primary = true)
    #[arg(long)]
    primary: bool,
    /// Auto-copy an item's code to the clipboard when navigating to it (config: auto_copy = true)
    #[arg(long)]
    auto_copy: bool,
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // A CLI flag always wins when passed; otherwise the config file's
    // value applies, falling back to the built-in default. A
    // missing config file is not an error; a malformed one is, since the
    // user asked for these defaults and a silent fallback would hide a
    // typo rather than surface it.
    let config = match config::config_path(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    ) {
        Some(path) => config::load_config(&path)?,
        None => config::Config::default(),
    };

    // Resolve symlinks and relative paths up front: write-back renames over
    // the path (which would replace a symlink with a regular file), and the
    // watcher watches the path's parent directory (which is the wrong
    // directory for a cross-directory symlink). Canonicalizing once fixes
    // both and gives dangling links a clear startup error.
    //
    // `--new` already writes to a canonicalized path (`scaffold` resolves
    // the parent directory itself, since the file doesn't exist yet for
    // `canonicalize` to resolve), so only the plain positional-file path
    // needs canonicalizing here.
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

    let mut state = AppState::new(document);
    if created_new {
        state.set_status("Created new checklist".to_string());
    }
    let nerd_font = if cli.no_nerd_font {
        false
    } else {
        config.nerd_font.unwrap_or(true)
    };
    if !nerd_font {
        state.icons = IconSet::unicode();
    }
    state.clipboard_primary = cli.primary || config.primary.unwrap_or(false);
    state.auto_copy = cli.auto_copy || config.auto_copy.unwrap_or(false);
    // Pick the richest color representation the terminal supports.
    state.palette = model::Palette::detect();
    // Reload-from-disk is a convenience feature, not core functionality:
    // if the watch can't be set up (e.g. inotify unavailable), continue
    // without it rather than failing to start.
    let watcher = FileWatcher::new(&state.document.file_path).ok();

    // Git-sync: `--git-sync` always requests it; otherwise the file's
    // canonical path must fall under one of the configured prefixes. Either
    // way it only actually activates once `GitSync::detect` confirms the
    // file's directory is inside a git work tree — like the watcher, this is
    // a convenience feature that fails open rather than erroring out.
    let git_sync_requested = cli.git_sync
        || config
            .git_sync_paths
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|prefix| state.document.file_path.starts_with(prefix));
    let mut git_sync = git_sync_requested
        .then(|| git_sync::GitSync::detect(&state.document.file_path))
        .flatten();
    // Drives the persistent status-bar icon — set once here and never
    // touched again; `GitSyncState::last_at`/`pending` handle the per-sync
    // timing, this is just "is the feature on for this session at all".
    state.git_sync.active = git_sync.is_some();

    let mouse = if cli.no_mouse {
        false
    } else {
        config.mouse.unwrap_or(true)
    };
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    let _guard = TerminalGuard { mouse };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    run(
        &mut terminal,
        &mut state,
        watcher.as_ref(),
        &mut git_sync,
        mouse,
    )
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
        if let Some(sync) = git_sync.as_mut()
            && let Some(outcome) = sync.poll()
        {
            match outcome {
                git_sync::SyncOutcome::Synced => state.record_git_sync(),
                git_sync::SyncOutcome::Skipped => {}
                git_sync::SyncOutcome::SkippedUntracked => state.set_error(
                    "Git sync skipped: file is not tracked in git (run `git add` on it)"
                        .to_string(),
                ),
                git_sync::SyncOutcome::Failed(msg) => {
                    state.set_error(format!("Git sync failed: {msg}"))
                }
            }
        }
        if let Some(change_desc) = state.take_git_sync_request()
            && let Some(sync) = git_sync.as_mut()
        {
            sync.request(change_desc);
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
            return Ok(());
        }
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

/// Shell-word-splits an env-var-sourced command (`$EDITOR`/`$VISUAL`/
/// `$BROWSER`), returning `(program, args, warning)`. `command` is
/// `None`/blank, or splits to nothing meaningful (e.g. a validly-quoted-empty
/// value like `EDITOR="''"`, which `shell_words::split` returns as one empty
/// token rather than an empty list) → falls back to `fallback` silently, the
/// same as an unset variable. Unbalanced quotes can't be parsed at all, so
/// they also fall back to `fallback`, but with a warning naming `var_name` —
/// the caller surfaces it rather than failing outright.
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

/// Resolves the URL opener: `$BROWSER` (shell-word-split like the editor, so
/// `BROWSER="firefox --new-tab"` works and a quoted argument containing spaces
/// stays one token), else the platform default. The third element is
/// `Some(warning)` when `$BROWSER` was set but couldn't be parsed (unbalanced
/// quotes) — the caller surfaces it and still falls back to the default
/// rather than failing outright.
fn resolve_opener(browser: Option<&str>) -> (String, Vec<String>, Option<String>) {
    resolve_command(browser, default_opener(), "$BROWSER")
}

/// Opens `url` with the resolved opener without suspending the TUI. The
/// URL is passed as a plain argument (no shell), so it can't inject commands.
/// Output is discarded so a chatty opener can't corrupt the screen, and the
/// child is reaped in a detached thread so we neither block on the browser nor
/// leave a zombie. A spawn failure is surfaced in the status bar.
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

/// Resolves the editor command: `$VISUAL`, then `$EDITOR`, then `vi`. The
/// value is shell-word-split so `EDITOR="code --wait"` works (first token is
/// the program, the rest are args) and a quoted argument containing spaces
/// stays one token (`EDITOR="my-wrapper --arg='value with spaces'"`).
/// The third element is `Some(warning)` when a value was set but couldn't be
/// parsed (unbalanced quotes) — the caller surfaces it and still falls back
/// to `vi` rather than failing outright.
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

/// Extra arguments that place the editor's cursor on a 1-based `line`, spliced
/// in *before* the file path. The `+N file` convention is understood by
/// the common terminal editors below (matched on the program's basename, so
/// `/usr/bin/vim` and `EDITOR="vim -p"` both qualify). Editors that don't use
/// `+N` — VS Code (`code`), Sublime (`subl`), Helix (`hx`), and anything
/// unrecognised — get no line arg and open at the top of the file, unchanged.
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
    execute!(io::stdout(), LeaveAlternateScreen)?;

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
    execute!(io::stdout(), EnterAlternateScreen)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Force a full repaint: the editor clobbered the screen ratatui thinks
    // it drew. `resize` to the current size clears the screen and resets
    // the back buffer without querying the cursor — unlike `Terminal::clear`,
    // whose DSR cursor query hangs/errors on terminals that don't answer it
    // (bare PTYs, some multiplexers).
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

    // Reload now rather than waiting for the watcher; the mtime guard makes
    // the watcher's later events a no-op. A change here came from us
    // spawning the editor, so — unlike a generic watcher-detected external
    // edit from elsewhere — queue a git-sync request for it too, the same
    // as a toggle would, instead of leaving it to piggyback on (or be
    // missed by) some later markcheck-driven write.
    if state.reload_if_changed() {
        state.request_external_edit_sync(&program);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{editor_line_args, resolve_editor, resolve_opener};

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
}
