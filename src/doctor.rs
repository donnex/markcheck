//! `--doctor` (#195): what markcheck detects about its environment and why,
//! one check per line, from the same detection the app runs at startup.
//! Nothing here starts the TUI or changes a file.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::cli::{self, Cli, Settings};
use crate::config;
use crate::git_sync::{Doctor, RemoteCheck};
use crate::launch;
use crate::model::{
    self, ClipboardChoice, ColorChoice, ColorDepth, CopyRoute, IconChoice, Multiplexer, Theme,
};

/// The exit status when at least one check warns; `0` when none does.
pub const WARNING_EXIT_STATUS: i32 = 3;

/// The environment variables the checks read, so tests can supply them.
#[derive(Debug, Default, Clone)]
pub struct Env {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub no_color: Option<String>,
    pub tmux: Option<String>,
    pub ssh_connection: Option<String>,
    pub ssh_tty: Option<String>,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub visual: Option<String>,
    pub editor: Option<String>,
    pub browser: Option<String>,
    pub xdg_config_home: Option<String>,
    pub home: Option<String>,
    pub xdg_runtime_dir: Option<String>,
    pub tmpdir: Option<String>,
}

impl Env {
    /// The real process environment.
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Env {
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            no_color: var("NO_COLOR"),
            tmux: var("TMUX"),
            ssh_connection: var("SSH_CONNECTION"),
            ssh_tty: var("SSH_TTY"),
            display: var("DISPLAY"),
            wayland_display: var("WAYLAND_DISPLAY"),
            visual: var("VISUAL"),
            editor: var("EDITOR"),
            browser: var("BROWSER"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            home: var("HOME"),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
            tmpdir: var("TMPDIR"),
        }
    }

    fn remote(&self) -> bool {
        cli::is_remote_session(self.ssh_connection.as_deref(), self.ssh_tty.as_deref())
    }

    fn graphical(&self) -> bool {
        launch::graphical_session(self.display.as_deref(), self.wayland_display.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warning,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Ok,
        detail: detail.into(),
    }
}

fn warning(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        level: Level::Warning,
        detail: detail.into(),
    }
}

/// `ok` or `warning`, as `warn` says.
fn check(name: &'static str, warn: bool, detail: impl Into<String>) -> Check {
    if warn {
        warning(name, detail)
    } else {
        ok(name, detail)
    }
}

/// `NAME=value`, or `NAME unset`.
fn var(name: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{name}={value}"),
        None => format!("{name} unset"),
    }
}

/// Where a setting came from: its flag, its config key, or the default.
fn source(from_flag: bool, flag: &str, from_config: bool, key: &str) -> String {
    if from_flag {
        format!("--{flag}")
    } else if from_config {
        format!("config: {key}")
    } else {
        "default".to_string()
    }
}

/// A value's name as its flag takes it, e.g. `osc52`.
fn value_name(value: impl ValueEnum) -> String {
    value
        .to_possible_value()
        .map_or_else(String::new, |value| value.get_name().to_string())
}

/// Every check, in report order: the FILE ones only when one is given.
pub fn checks(cli: &Cli, env: &Env) -> Vec<Check> {
    let (loaded, mut checks) = config_checks(cli, env);
    let config = &loaded.config;
    let settings = Settings::resolve(cli, config);
    let depth = settings.color.forced_depth().unwrap_or_else(|| {
        model::depth_from_env(
            env.no_color.as_deref(),
            env.colorterm.as_deref(),
            env.term.as_deref(),
        )
    });
    let icons_from_flag = cli.icons.is_some() || cli.nerd_font || cli.no_nerd_font;
    let icons_from_config = config.icons.is_some() || config.nerd_font.is_some();
    checks.extend([
        color_check(
            settings.color,
            source(
                cli.color.is_some(),
                "color",
                config.color.is_some(),
                "color",
            ),
            depth,
            env,
        ),
        theme_check(
            settings.theme,
            source(
                cli.theme.is_some(),
                "theme",
                config.theme.is_some(),
                "theme",
            ),
            depth,
        ),
        icons_check(
            settings.icons,
            source(icons_from_flag, "icons", icons_from_config, "icons"),
            env.term.as_deref(),
        ),
        clipboard_check(
            settings.clipboard,
            source(
                cli.clipboard.is_some(),
                "clipboard",
                config.clipboard.is_some(),
                "clipboard",
            ),
            env,
        ),
        editor_check(env),
        browser_check(env),
        lock_check(
            crate::writer::lock_dir_for(env.xdg_runtime_dir.as_deref().map(OsStr::new)),
            env.xdg_runtime_dir.as_deref(),
        ),
        temp_check(&std::env::temp_dir(), env.tmpdir.as_deref()),
    ]);
    if let Some(file) = &cli.file {
        checks.extend(file_checks(cli, config, file, env));
    }
    checks
}

/// The config file used and what was wrong with it, plus the config itself
/// (built-in defaults when it couldn't be read).
fn config_checks(cli: &Cli, env: &Env) -> (config::Loaded, Vec<Check>) {
    if cli.no_config {
        return (
            config::Loaded::default(),
            vec![ok("config", "skipped (--no-config); built-in defaults")],
        );
    }
    let xdg = env.xdg_config_home.as_deref();
    let home = env.home.as_deref();
    let absolute = |value: Option<&str>| value.is_some_and(|v| Path::new(v).is_absolute());
    let mut ignored = Vec::new();
    let (path, why) = match &cli.config {
        Some(path) => (path.clone(), "--config"),
        None => {
            if xdg.is_some_and(|v| !v.is_empty()) && !absolute(xdg) {
                ignored.push(format!(
                    "{} ignored: not an absolute path",
                    var("XDG_CONFIG_HOME", xdg)
                ));
            }
            if !absolute(xdg) && home.is_some_and(|v| !v.is_empty()) && !absolute(home) {
                ignored.push(format!(
                    "{} ignored: not an absolute path",
                    var("HOME", home)
                ));
            }
            match config::config_path(xdg, home) {
                Some(path) if absolute(xdg) => (path, "XDG_CONFIG_HOME"),
                Some(path) => (path, "HOME"),
                None => {
                    ignored.insert(
                        0,
                        "no location: neither XDG_CONFIG_HOME nor HOME is an absolute path; \
                         built-in defaults"
                            .to_string(),
                    );
                    return (
                        config::Loaded::default(),
                        vec![warning("config", ignored.join("; "))],
                    );
                }
            }
        }
    };
    let loaded = match config::load_for_run(cli.config.as_deref(), false, xdg, home) {
        Ok(loaded) => loaded,
        Err(err) => {
            return (
                config::Loaded::default(),
                vec![warning("config", format!("{err:#}; built-in defaults"))],
            );
        }
    };
    let status = if path.is_file() {
        "read"
    } else {
        "not found; built-in defaults"
    };
    let mut detail = format!("{} ({why}; {status})", path.display());
    let problems: Vec<String> = ignored
        .into_iter()
        .chain(config::unknown_keys_warning(&loaded.unknown_keys))
        .chain(config::deprecated_keys_note(&loaded.config))
        .collect();
    for problem in &problems {
        detail.push_str("; ");
        detail.push_str(problem);
    }
    let checks = vec![check("config", !problems.is_empty(), detail)];
    (loaded, checks)
}

fn depth_name(depth: ColorDepth) -> &'static str {
    match depth {
        ColorDepth::TrueColor => "truecolor",
        ColorDepth::Palette256 => "256 colors",
        ColorDepth::Basic => "16 colors",
        ColorDepth::Mono => "none",
    }
}

fn color_check(choice: ColorChoice, source: String, depth: ColorDepth, env: &Env) -> Check {
    let name = depth_name(depth);
    if choice != ColorChoice::Auto {
        return ok("colors", format!("{name} ({source}, not detected)"));
    }
    if depth == ColorDepth::Mono {
        return ok("colors", format!("{name} (NO_COLOR is set)"));
    }
    let term = env.term.as_deref();
    let detail = format!(
        "{name} ({}, {})",
        var("COLORTERM", env.colorterm.as_deref()),
        var("TERM", term)
    );
    if term.is_none_or(|term| term.is_empty() || term == "dumb") {
        warning(
            "colors",
            format!("{detail}; TERM is unset or dumb, so the terminal may not draw the checklist"),
        )
    } else {
        ok("colors", detail)
    }
}

fn theme_check(theme: Theme, source: String, depth: ColorDepth) -> Check {
    let unused = match depth {
        ColorDepth::Basic => "; unused: the 16-color palette follows the terminal's own colors",
        ColorDepth::Mono => "; unused without color",
        ColorDepth::TrueColor | ColorDepth::Palette256 => "",
    };
    ok(
        "theme",
        format!("{} ({source}; never detected){unused}", value_name(theme)),
    )
}

fn icons_check(choice: IconChoice, source: String, term: Option<&str>) -> Check {
    let resolved = choice.resolve(term);
    let why = match choice {
        IconChoice::Auto if resolved == IconChoice::Ascii => {
            "auto: TERM=linux, the Linux console, whose font lacks the symbols".to_string()
        }
        IconChoice::Auto => format!("auto: {}", var("TERM", term)),
        _ => source,
    };
    let font = if resolved == IconChoice::Nerd {
        "; needs a Nerd Font in the terminal"
    } else {
        ""
    };
    ok("icons", format!("{} ({why}){font}", value_name(resolved)))
}

fn clipboard_check(choice: ClipboardChoice, source: String, env: &Env) -> Check {
    let route = choice.route(env.remote());
    let graphical = env.graphical();
    let mut result = match route {
        CopyRoute::SystemThenOsc52 if graphical => ok(
            "clipboard",
            format!("system clipboard, OSC 52 if it fails ({source}; local session)"),
        ),
        CopyRoute::SystemThenOsc52 => ok(
            "clipboard",
            format!(
                "OSC 52, since the system clipboard needs DISPLAY or WAYLAND_DISPLAY \
                 ({source}; neither is set)"
            ),
        ),
        CopyRoute::Both => ok(
            "clipboard",
            format!(
                "system clipboard and OSC 52 ({source}; SSH session, where the system \
                 clipboard may be the remote machine's)"
            ),
        ),
        CopyRoute::SystemOnly if graphical => {
            ok("clipboard", format!("system clipboard only ({source})"))
        }
        CopyRoute::SystemOnly => warning(
            "clipboard",
            format!(
                "system clipboard only ({source}), but neither DISPLAY nor WAYLAND_DISPLAY \
                 is set, so copies will likely fail; --clipboard auto falls back to OSC 52"
            ),
        ),
        CopyRoute::Osc52Only => ok("clipboard", format!("OSC 52 only ({source})")),
    };
    if route != CopyRoute::SystemOnly {
        match Multiplexer::detect(env.tmux.as_deref(), env.term.as_deref()) {
            Multiplexer::Tmux => result
                .detail
                .push_str("; OSC 52 also wrapped for tmux (TMUX is set)"),
            Multiplexer::Screen => result
                .detail
                .push_str("; OSC 52 wrapped for GNU screen (TERM starts with screen)"),
            Multiplexer::None => {}
        }
    }
    result
}

fn editor_check(env: &Env) -> Check {
    let (program, args, problem) =
        launch::resolve_editor(env.visual.as_deref(), env.editor.as_deref());
    if let Some(problem) = problem {
        return warning("editor", problem);
    }
    let set = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    let from = if set(&env.visual) {
        "from VISUAL"
    } else if set(&env.editor) {
        "from EDITOR"
    } else {
        "VISUAL and EDITOR unset"
    };
    let command = shell_words::join(std::iter::once(program).chain(args));
    ok("editor", format!("{command} ({from})"))
}

fn browser_check(env: &Env) -> Check {
    let browser = env.browser.as_deref();
    let (candidates, problem) = launch::resolve_opener(browser, "%s");
    let names = candidates
        .iter()
        .map(|(program, _)| launch::program_name(program))
        .collect::<Vec<_>>()
        .join(", then ");
    let text_browsers: Vec<String> = candidates
        .iter()
        .filter(|(program, _)| launch::is_terminal_browser(program))
        .map(|(program, _)| {
            format!(
                "{} is a text browser, so it gets the terminal until it exits",
                launch::program_name(program)
            )
        })
        .collect();
    let route = launch::link_route(
        browser,
        env.display.as_deref(),
        env.wayland_display.as_deref(),
        env.remote(),
    );
    let from = if launch::browser_set(browser) {
        "from BROWSER"
    } else {
        "BROWSER unset"
    };
    let mut detail = match route {
        launch::LinkRoute::Open => format!("{names} ({from})"),
        launch::LinkRoute::OpenAndCopy => {
            format!("{names}, and the link is copied too ({from}; SSH session)")
        }
        launch::LinkRoute::CopyInstead => {
            "links are copied, not opened (BROWSER, DISPLAY and WAYLAND_DISPLAY unset)".to_string()
        }
    };
    for note in text_browsers.iter().chain(&problem) {
        detail.push_str("; ");
        detail.push_str(note);
    }
    check("browser", problem.is_some(), detail)
}

fn lock_check(dir: Option<PathBuf>, runtime_dir: Option<&str>) -> Check {
    let Some(dir) = dir else {
        return warning(
            "locks",
            "no private directory for the write lock, so saves aren't locked against another \
             markcheck and other sessions on the same file go unnoticed",
        );
    };
    let why = match runtime_dir.filter(|value| !value.is_empty()) {
        Some(runtime_dir) if Path::new(runtime_dir) == dir => "XDG_RUNTIME_DIR".to_string(),
        Some(runtime_dir) => format!(
            "a private directory in the temp dir; XDG_RUNTIME_DIR={runtime_dir} is not one of \
             this user's private directories"
        ),
        None => "a private directory in the temp dir; XDG_RUNTIME_DIR unset".to_string(),
    };
    ok("locks", format!("{} ({why})", dir.display()))
}

fn temp_check(dir: &Path, tmpdir: Option<&str>) -> Check {
    let from = if tmpdir.is_some_and(|value| !value.is_empty()) {
        "from TMPDIR"
    } else {
        "TMPDIR unset"
    };
    if dir.is_dir() {
        ok("temp", format!("{} ({from})", dir.display()))
    } else {
        warning(
            "temp",
            format!(
                "{} ({from}) is not a directory, so git sync can't keep its scratch files",
                dir.display()
            ),
        )
    }
}

/// The checks for FILE: that it resolves, whether git sync would be on and
/// why, and with it on, what the repository says.
fn file_checks(cli: &Cli, config: &config::Config, file: &Path, env: &Env) -> Vec<Check> {
    let file = cli::split_anchor(file).map_or_else(|| file.to_path_buf(), |(path, _)| path);
    let path = match std::fs::canonicalize(&file) {
        Ok(path) => path,
        Err(err) => {
            let message = crate::scaffold::missing_file_message(&file, &err)
                .unwrap_or_else(|| format!("cannot resolve path: {}: {err}", file.display()));
            return vec![warning("file", message)];
        }
    };
    let mut checks = vec![ok("file", path.display().to_string())];
    let request = launch::git_sync_request(
        cli.git_sync,
        cli.no_git_sync,
        config.git_sync_paths.as_deref(),
        &path,
        env.home.as_deref(),
    );
    let config_problem = launch::git_sync_warning(&request, None);
    let with_problem = |detail: String| match &config_problem {
        Some(problem) => format!("{detail}; {problem}"),
        None => detail,
    };
    let why_on = match request.covering.first() {
        Some(covering) if !request.by_flag => {
            format!("git_sync_paths entry {}", covering.entry.display())
        }
        _ => "--git-sync".to_string(),
    };
    match launch::detect_git_sync(&request, &path) {
        None => {
            let why_off = if cli.no_git_sync {
                "--no-git-sync"
            } else {
                "no --git-sync, and no git_sync_paths entry covers this file"
            };
            checks.push(check(
                "git sync",
                config_problem.is_some(),
                with_problem(format!("off ({why_off})")),
            ));
        }
        Some(Err(reason)) => checks.push(warning(
            "git sync",
            with_problem(format!("off: {reason} (asked for by {why_on})")),
        )),
        Some(Ok(sync)) => {
            checks.push(check(
                "git sync",
                config_problem.is_some(),
                with_problem(format!(
                    "on ({why_on}); repository {}",
                    sync.repo_root().display()
                )),
            ));
            checks.extend(git_checks(&sync.doctor()));
        }
    }
    checks
}

/// The lines for what [`GitSync::doctor`](crate::git_sync::GitSync::doctor)
/// found.
fn git_checks(found: &Doctor) -> Vec<Check> {
    let identity = match &found.identity_warning {
        Some(problem) => warning("identity", problem.clone()),
        None => ok("identity", "user.name and user.email are set"),
    };
    let tracked = match &found.tracked {
        Ok(true) => ok("tracked", "the checklist is tracked in git"),
        Ok(false) => warning(
            "tracked",
            "the checklist is not tracked in git, so git sync skips it (run `git add` on it)",
        ),
        Err(problem) => warning("tracked", problem.clone()),
    };
    let push = match &found.push_target {
        Ok(target) => {
            let waiting = match found.unpushed {
                Some(0) => "nothing waiting to be pushed".to_string(),
                Some(1) => "1 commit not pushed yet".to_string(),
                Some(count) => format!("{count} commits not pushed yet"),
                None => "unpushed commits unknown".to_string(),
            };
            ok("push", format!("{target}; {waiting}"))
        }
        Err(problem) => warning("push", format!("commits stay local: {problem}")),
    };
    let remote = match &found.remote {
        RemoteCheck::NotChecked => ok("remote", "not checked: there is no remote to push to"),
        RemoteCheck::Unreachable => warning(
            "remote",
            "couldn't reach the remote to check for changes not pulled yet",
        ),
        RemoteCheck::Absent => warning(
            "remote",
            "the branch no longer exists on the remote, and git sync won't recreate it",
        ),
        RemoteCheck::UpToDate => ok("remote", "no changes on the remote that aren't pulled"),
        RemoteCheck::Ahead(message) => warning("remote", message.clone()),
    };
    vec![
        ok("git", format!("git {}", found.version)),
        identity,
        tracked,
        push,
        remote,
    ]
}

/// Whether to color the report: on a terminal, unless `NO_COLOR` is set.
pub fn use_color(no_color: Option<&str>, stdout_is_terminal: bool) -> bool {
    stdout_is_terminal && no_color.is_none_or(str::is_empty)
}

/// The report: one line per check, then a count.
pub fn render(checks: &[Check], color: bool) -> String {
    let paint = |text: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for c in checks {
        let tag = match c.level {
            Level::Ok => paint("ok  ", "32"),
            Level::Warning => paint("warn", "33"),
        };
        out.push_str(&format!(
            "{tag}  {:<width$}  {}\n",
            c.name,
            crate::printable::printable_line(&c.detail)
        ));
    }
    let warnings = checks.iter().filter(|c| c.level == Level::Warning).count();
    let summary = match warnings {
        0 => paint(&format!("All {} checks ok", checks.len()), "32"),
        1 => paint(&format!("1 of {} checks warns", checks.len()), "33"),
        n => paint(&format!("{n} of {} checks warn", checks.len()), "33"),
    };
    out.push_str(&summary);
    out.push('\n');
    out
}

/// `0` when no check warns, otherwise [`WARNING_EXIT_STATUS`].
pub fn exit_status(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.level == Level::Warning) {
        WARNING_EXIT_STATUS
    } else {
        0
    }
}

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod tests;
