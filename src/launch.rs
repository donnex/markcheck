//! Pure resolution for the things markcheck launches or acts on outside
//! itself: the editor and link-opener commands from the environment, the
//! editor's jump-to-line argument, the order `o` tries openers in, whether
//! git-sync applies to a file, and the startup warning about a file a save
//! would disturb. The spawning itself stays in `main.rs`, passed in as
//! closures.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// The platform default opener when `$BROWSER` is unset or unusable: `open` on
/// macOS, `xdg-open` elsewhere.
fn default_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Shell-word-splits one command into `(program, args)`; `Ok(None)` for a
/// blank value or one that quotes to an empty program name.
fn split_command(command: &str) -> Result<Option<(String, Vec<String>)>, shell_words::ParseError> {
    let command = command.trim();
    if command.is_empty() {
        return Ok(None);
    }
    let mut parts = shell_words::split(command)?;
    if parts.is_empty() || parts[0].is_empty() {
        return Ok(None);
    }
    let program = parts.remove(0);
    Ok(Some((program, parts)))
}

/// Shell-word-splits an env-var command into `(program, args, warning)`.
/// Blank or empty values fall back silently; unbalanced quotes fall back
/// with a warning naming `var_name`.
fn resolve_command(
    command: Option<&str>,
    fallback: &str,
    var_name: &str,
) -> (String, Vec<String>, Option<String>) {
    match split_command(command.unwrap_or_default()) {
        Ok(Some((program, args))) => (program, args, None),
        Ok(None) => (fallback.to_string(), Vec::new(), None),
        Err(err) => (
            fallback.to_string(),
            Vec::new(),
            Some(format!(
                "Couldn't parse {var_name} ({err}); falling back to {fallback}"
            )),
        ),
    }
}

/// Expands the `$BROWSER` placeholders in one argument: `%s` becomes `url`,
/// `%%` a literal `%`. Returns whether a `%s` was found.
fn expand_browser_placeholders(arg: &str, url: &str) -> (String, bool) {
    let mut out = String::with_capacity(arg.len());
    let mut found = false;
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('%', Some('s')) => {
                chars.next();
                out.push_str(url);
                found = true;
            }
            ('%', Some('%')) => {
                chars.next();
                out.push('%');
            }
            _ => out.push(c),
        }
    }
    (out, found)
}

/// The commands to try, in order, to open `url`. `$BROWSER` follows the
/// common convention: a colon-separated list of commands, each
/// shell-word-split, with `%s` standing for the URL (appended as the last
/// argument when absent). Unparseable entries are skipped with a warning;
/// with no usable entry, the platform default opener is used.
pub fn resolve_opener(
    browser: Option<&str>,
    url: &str,
) -> (Vec<(String, Vec<String>)>, Option<String>) {
    let mut candidates = Vec::new();
    let mut warning = None;
    for entry in browser.unwrap_or_default().split(':') {
        match split_command(entry) {
            Ok(Some((program, args))) => {
                let mut has_placeholder = false;
                let mut args: Vec<String> = args
                    .iter()
                    .map(|arg| {
                        let (arg, found) = expand_browser_placeholders(arg, url);
                        has_placeholder |= found;
                        arg
                    })
                    .collect();
                if !has_placeholder {
                    args.push(url.to_string());
                }
                candidates.push((program, args));
            }
            Ok(None) => {}
            Err(err) => {
                warning.get_or_insert_with(|| {
                    format!("Couldn't parse $BROWSER entry {:?} ({err})", entry.trim())
                });
            }
        }
    }
    if candidates.is_empty() {
        let fallback = default_opener();
        if let Some(w) = warning.as_mut() {
            w.push_str(&format!("; falling back to {fallback}"));
        }
        candidates.push((fallback.to_string(), vec![url.to_string()]));
    }
    (candidates, warning)
}

/// What `o` does with a link, from [`link_route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRoute {
    /// Hand it to the opener.
    Open,
    /// Copy it instead: there is nowhere to show it.
    CopyInstead,
    /// Hand it to the opener and copy it too: over SSH the opener may show
    /// it on the remote machine's screen, where a stale `$DISPLAY` or a Mac's
    /// console points, rather than the user's own.
    OpenAndCopy,
}

/// Where `o` sends a link. A usable `$BROWSER` entry always opens it. Without
/// one, outside macOS with no graphical session (`$DISPLAY` and
/// `$WAYLAND_DISPLAY` both unset or empty), the link is only copied; over
/// SSH (`remote`) it is otherwise opened and copied, so the copy reaches the
/// user's own terminal through OSC 52.
pub fn link_route(
    browser: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
    remote: bool,
) -> LinkRoute {
    let is_set = |value: Option<&str>| value.is_some_and(|v| !v.trim().is_empty());
    let browser_set = browser
        .unwrap_or_default()
        .split(':')
        .any(|entry| !entry.trim().is_empty());
    let graphical = cfg!(target_os = "macos") || is_set(display) || is_set(wayland_display);
    match (browser_set, graphical, remote) {
        (true, _, _) => LinkRoute::Open,
        (false, false, _) => LinkRoute::CopyInstead,
        (false, true, true) => LinkRoute::OpenAndCopy,
        (false, true, false) => LinkRoute::Open,
    }
}

/// A command's basename, for status messages: `/usr/bin/xdg-open` → `xdg-open`.
pub fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

/// Whether `program` (matched on its basename) is a browser that draws in
/// the terminal, such as `w3m` or `lynx`. Detached with no terminal it could
/// show nothing, so `o` hands it the terminal the way `e` does the editor.
pub fn is_terminal_browser(program: &str) -> bool {
    matches!(
        program_name(program),
        "w3m" | "lynx" | "links" | "links2" | "elinks" | "browsh" | "carbonyl"
    )
}

/// How long after starting an opener a failed exit is still reported. An
/// opener that hands off to a browser exits at once; one that *is* the
/// browser can run for hours, and its eventual exit says nothing about the
/// link.
pub const OPENER_FAILURE_WINDOW: Duration = Duration::from_secs(10);

/// The sticky message for an opener that failed to open the link, or `None`
/// when it succeeded or ran past `OPENER_FAILURE_WINDOW`.
pub fn opener_failure_message(
    program: &str,
    outcome: &std::io::Result<ExitStatus>,
    elapsed: Duration,
) -> Option<String> {
    if elapsed > OPENER_FAILURE_WINDOW {
        return None;
    }
    let name = program_name(program);
    let what = match outcome {
        Ok(status) if status.success() => return None,
        Ok(status) => match status.code() {
            Some(code) => format!("{name} exited with code {code}"),
            None => format!("{name} was stopped ({status})"),
        },
        Err(err) => format!("couldn't wait for {name}: {err}"),
    };
    Some(format!("Couldn't open link ({what})"))
}

/// `$VISUAL`, then `$EDITOR`, then `vi`, shell-word-split so
/// `EDITOR="code --wait"` works.
pub fn resolve_editor(
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

/// The editor arguments that open `path`, on `line` when the editor (matched
/// on its basename) has a known way to take one: a `+N` before the path
/// (vi-style), a `path:N` suffix (Helix, Sublime Text) or `--goto path:N`
/// (VS Code, VSCodium). Anything else, or no `line`, gets the bare path.
pub fn editor_file_args(program: &str, path: &Path, line: Option<usize>) -> Vec<OsString> {
    let base = Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    let with_line = |line: usize| {
        let mut arg = path.as_os_str().to_os_string();
        arg.push(format!(":{line}"));
        arg
    };
    match (base, line) {
        (
            "vi" | "vim" | "nvim" | "view" | "nano" | "pico" | "emacs" | "emacsclient" | "joe"
            | "gedit" | "kak" | "micro",
            Some(line),
        ) => vec![format!("+{line}").into(), path.into()],
        ("hx" | "helix" | "subl", Some(line)) => vec![with_line(line)],
        ("code" | "codium", Some(line)) => vec!["--goto".into(), with_line(line)],
        _ => vec![path.into()],
    }
}

/// A `git_sync_paths` entry with a leading `~` expanded against `home`, the
/// way a shell would have: the config file never passes through one, so
/// `~/checklists` would otherwise name a directory literally called `~`.
/// Left as written when `home` is unset or empty, or for `~user` forms.
pub fn expand_tilde(path: &Path, home: Option<&str>) -> PathBuf {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => Path::new(home).join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Whether a configured `git_sync_paths` entry covers the canonical
/// `file_path`. The prefix is canonicalized too, and compared by path
/// components, so `/foo/bar` doesn't match `/foo/barn`.
pub fn git_sync_path_matches(file_path: &Path, prefix: &Path) -> bool {
    std::fs::canonicalize(prefix).is_ok_and(|canonical| file_path.starts_with(canonical))
}

/// Whether git-sync was asked for, and which `git_sync_paths` entries could
/// not be used.
#[derive(Debug, PartialEq)]
pub struct GitSyncRequest {
    /// By `--git-sync`, or by a configured entry covering the file.
    pub requested: bool,
    /// Entries, as written in the config, that don't resolve to an existing
    /// path, so they can never match anything.
    pub unresolved: Vec<PathBuf>,
    /// Asked for with `--git-sync`, which accepts whatever repository holds
    /// the file.
    pub by_flag: bool,
    /// The configured entries covering the file.
    pub covering: Vec<CoveringEntry>,
}

/// A `git_sync_paths` entry that covers the file.
#[derive(Debug, PartialEq)]
pub struct CoveringEntry {
    /// As written in the config.
    pub entry: PathBuf,
    /// Canonical, without any `/**`.
    pub prefix: PathBuf,
    /// Written with a trailing `/**`, so repositories nested below the
    /// prefix are covered too.
    pub nested: bool,
}

/// An entry's path and whether it ends in `/**`.
fn split_nested(entry: &Path) -> (&Path, bool) {
    match (entry.file_name(), entry.parent()) {
        (Some(name), Some(parent)) if name == "**" => (parent, true),
        _ => (entry, false),
    }
}

/// Whether the repository at `repo_root` may be synced for `request`: a
/// plain entry covers only a repository rooted at the entry or above it,
/// since a repository nested inside it may have come from anywhere and git
/// would run its hooks and config (#124). `--git-sync` and a `/**` entry
/// accept any. `Err` holds why git sync stays off.
pub fn git_sync_scope(request: &GitSyncRequest, repo_root: &Path) -> Result<(), String> {
    let root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let allowed = request.by_flag
        || request
            .covering
            .iter()
            .any(|covering| covering.nested || covering.prefix.starts_with(&root));
    match request.covering.first() {
        Some(covering) if !allowed => Err(format!(
            "the checklist's repository ({}) is nested inside git_sync_paths entry {}; \
             end the entry with /** to cover nested repositories, or pass --git-sync",
            root.display(),
            covering.entry.display()
        )),
        _ => Ok(()),
    }
}

/// Resolves `--git-sync`, `--no-git-sync` and `git_sync_paths` for
/// `file_path`. `--git-sync` and a matching entry are an OR, not the
/// flag-over-config precedence of the boolean settings; `--no-git-sync`
/// beats both. Every entry is checked, even once one matches or the feature
/// is forced off, so a typo is reported either way.
pub fn git_sync_request(
    cli_flag: bool,
    cli_off: bool,
    paths: Option<&[PathBuf]>,
    file_path: &Path,
    home: Option<&str>,
) -> GitSyncRequest {
    let mut unresolved = Vec::new();
    let mut covering = Vec::new();
    for entry in paths.unwrap_or(&[]) {
        let (path, nested) = split_nested(entry);
        let prefix = expand_tilde(path, home);
        if !prefix.exists() {
            unresolved.push(entry.clone());
        } else if git_sync_path_matches(file_path, &prefix)
            && let Ok(prefix) = std::fs::canonicalize(&prefix)
        {
            covering.push(CoveringEntry {
                entry: entry.clone(),
                prefix,
                nested,
            });
        }
    }
    GitSyncRequest {
        requested: (cli_flag || !covering.is_empty()) && !cli_off,
        unresolved,
        by_flag: cli_flag,
        covering,
    }
}

/// The sticky startup message for a git-sync that was configured but won't
/// run: unusable `git_sync_paths` entries, and why `GitSync::detect` refused
/// when git-sync was asked for.
pub fn git_sync_warning(unresolved: &[PathBuf], detect_error: Option<&str>) -> Option<String> {
    let mut warnings = Vec::new();
    if !unresolved.is_empty() {
        let entries = unresolved
            .iter()
            .map(|entry| entry.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let verb = if unresolved.len() == 1 {
            "does not exist"
        } else {
            "do not exist"
        };
        warnings.push(format!("git_sync_paths: {entries} {verb} — ignored"));
    }
    if let Some(reason) = detect_error {
        warnings.push(format!("Git sync off: {reason}"));
    }
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

/// The startup warning shown when the file watch couldn't be set up, so live
/// reload falls back to the timed recheck alone.
pub fn watch_warning(error: &str) -> String {
    format!(
        "Live reload slowed: can't watch the file ({error}) — checking every {} s instead",
        crate::watcher::RECHECK_INTERVAL.as_secs()
    )
}

/// The startup note for prose markcheck read but can't show (#80): what
/// sits between tasks, after the last one, or under a heading with none, and
/// the bullets of a list with no checkbox.
pub fn unshown_warning(unshown: &crate::model::Unshown) -> Option<String> {
    if unshown.is_empty() {
        return None;
    }
    let counted = |count: usize, one: &str, many: &str| {
        (count > 0).then(|| format!("{count} {}", if count == 1 { one } else { many }))
    };
    let parts: Vec<String> = [
        counted(unshown.paragraphs, "paragraph", "paragraphs"),
        counted(unshown.quotes, "quote", "quotes"),
        counted(unshown.bullets, "bullet point", "bullet points"),
        counted(unshown.code_blocks, "code block", "code blocks"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let listed = match parts.as_slice() {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => unreachable!("not empty"),
    };
    Some(format!("{listed} outside task lists not shown"))
}

/// The document's title for the terminal title and the exit line: its
/// `# H1`, or one made from the file name.
fn document_name(state: &crate::model::AppState) -> String {
    state
        .document
        .title
        .clone()
        .unwrap_or_else(|| crate::scaffold::derive_title(&state.document.file_path))
}

/// The environment `o` decides where a link goes from.
pub struct LinkEnv<'a> {
    pub browser: Option<&'a str>,
    pub display: Option<&'a str>,
    pub wayland_display: Option<&'a str>,
    /// An SSH session (`cli::is_remote_session`).
    pub remote: bool,
}

/// Runs `program` with the whole terminal, as `e` runs the editor, and
/// takes it back once the program exits. The inner `Err` means it couldn't
/// be started; the outer one that the terminal couldn't be taken back.
pub type RunInTerminal<'a> =
    dyn FnMut(&str, &[String]) -> anyhow::Result<std::io::Result<ExitStatus>> + 'a;

/// Starts `program` detached from the terminal, without waiting for it.
pub type SpawnDetached<'a> = dyn FnMut(&str, &[String]) -> std::io::Result<()> + 'a;

/// Opens `url` along [`link_route`]: with no browser to send it to (a
/// headless session) it copies the link instead, and over SSH it copies it
/// as well. Opening tries each [`resolve_opener`] candidate in turn until one
/// starts: a terminal browser ([`is_terminal_browser`]) gets the terminal
/// through `run_here` until it exits, any other opener goes to `spawn`.
pub fn open_link(
    url: &str,
    env: &LinkEnv<'_>,
    state: &mut crate::model::AppState,
    run_here: &mut RunInTerminal<'_>,
    spawn: &mut SpawnDetached<'_>,
) -> anyhow::Result<()> {
    match link_route(env.browser, env.display, env.wayland_display, env.remote) {
        LinkRoute::CopyInstead => state.copy_link_instead(url),
        LinkRoute::Open => try_openers(url, env.browser, false, state, run_here, spawn)?,
        LinkRoute::OpenAndCopy => {
            try_openers(url, env.browser, true, state, run_here, spawn)?;
            state.copy_link_too(url);
        }
    }
    Ok(())
}

/// The opener loop behind [`open_link`]. A `$BROWSER` parse warning follows
/// the confirmation, so it is still on screen once the link has gone.
/// `on_this_host` words the confirmation for an SSH session.
fn try_openers(
    url: &str,
    browser: Option<&str>,
    on_this_host: bool,
    state: &mut crate::model::AppState,
    run_here: &mut RunInTerminal<'_>,
    spawn: &mut SpawnDetached<'_>,
) -> anyhow::Result<()> {
    let (candidates, warning) = resolve_opener(browser, url);
    let mut last_error = None;
    for (program, args) in &candidates {
        if is_terminal_browser(program) {
            match run_here(program, args)? {
                Err(err) => last_error = Some(format!("{program}: {err}")),
                outcome => {
                    state.finish_terminal_browser(program, url, &outcome);
                    if let Some(warning) = warning {
                        let shown = state.status_message.take().unwrap_or_default();
                        state.set_error(format!("{shown}; {warning}"));
                    }
                    return Ok(());
                }
            }
            continue;
        }
        match spawn(program, args) {
            Ok(()) => {
                state.report_link_sent(program, url, on_this_host);
                if let Some(warning) = warning {
                    state.set_error(warning);
                }
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

/// The terminal title last sent, so an unchanged one isn't rewritten every
/// frame.
#[derive(Default)]
pub struct ShownTitle(String);

impl ShownTitle {
    /// The title to send for `state`, or `None` when it is already showing.
    pub fn update(&mut self, state: &crate::model::AppState) -> Option<&str> {
        let title = terminal_title(state);
        if title == self.0 {
            return None;
        }
        self.0 = title;
        Some(&self.0)
    }

    /// Forgets the title sent, once another program may have replaced it.
    pub fn forget(&mut self) {
        self.0.clear();
    }
}

/// The terminal (and tmux pane) title while markcheck runs (#135):
/// `Deployment Runbook · 3/11 — markcheck`, so a tab says which checklist
/// it holds and how far along it is.
pub fn terminal_title(state: &crate::model::AppState) -> String {
    let progress = state.document.progress();
    crate::printable::printable_line(&format!(
        "{} · {}/{} — markcheck",
        document_name(state),
        progress.done,
        progress.total()
    ))
}

/// The line printed after a normal quit (#135), for handovers and notes:
/// where the run stopped, or that it finished.
pub fn exit_summary(state: &crate::model::AppState) -> String {
    let file = state
        .document
        .file_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let progress = state.document.progress();
    let (done, started, skipped, total) = (
        progress.done,
        progress.started,
        progress.skipped,
        progress.total(),
    );
    let mut extras = Vec::new();
    if started > 0 {
        extras.push(format!("{started} started"));
    }
    if skipped > 0 {
        extras.push(format!("{skipped} skipped"));
    }
    let extras = if extras.is_empty() {
        String::new()
    } else {
        format!(" ({})", extras.join(", "))
    };
    let name = document_name(state);
    if total > 0 && done == total {
        return crate::printable::printable_line(&format!(
            "{file} — {name}: all {total} done{extras}"
        ));
    }
    let stopped = state
        .current_item()
        .map(|item| format!(", stopped at \"{}\"", item.label()))
        .unwrap_or_default();
    crate::printable::printable_line(&format!(
        "{file} — {name}: {done}/{total} done{extras}{stopped}"
    ))
}

/// The startup warning that the checklist is already open in another
/// markcheck session (#101), whose changes will show up here as reloads.
pub fn already_open_warning(others: &[crate::writer::OtherSession]) -> Option<String> {
    let ago = |other: &crate::writer::OtherSession| match other.open_for {
        Some(open_for) => format!(", opened {}", time_ago(open_for)),
        None => String::new(),
    };
    match others {
        [] => None,
        [other] => Some(format!(
            "already open in another markcheck (pid {}{}): its changes show up here as reloads",
            other.pid,
            ago(other)
        )),
        many => Some(format!(
            "already open in {} other markcheck sessions (pids {}): their changes show up here as reloads",
            many.len(),
            many.iter()
                .map(|other| other.pid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// How long ago, in the largest unit that still reads naturally: minutes up
/// to two hours, hours up to two days, then days.
fn time_ago(elapsed: std::time::Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    match minutes {
        0..2 => "just now".to_string(),
        2..120 => format!("{minutes} min ago"),
        120..2880 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

/// The startup note for leftovers of an interrupted markcheck that were
/// removed (see `writer::sweep_interrupted_saves`), so a vanished file is
/// accounted for.
pub fn leftovers_warning(removed: usize) -> Option<String> {
    match removed {
        0 => None,
        1 => Some("removed 1 file left by an interrupted markcheck".to_string()),
        n => Some(format!(
            "removed {n} files left by an interrupted markcheck"
        )),
    }
}

/// Joins the startup messages into the one the status bar shows: sticky and
/// error-colored if any of them is a warning, otherwise the ephemeral
/// "Created new checklist" and `opened` (the `open_at` note) alone. Returns
/// the text and whether it's an error.
pub fn startup_message(
    created_new: bool,
    opened: Option<&str>,
    warnings: &[Option<String>],
) -> Option<(String, bool)> {
    let mut parts: Vec<&str> = Vec::new();
    if created_new {
        parts.push("Created new checklist");
    }
    parts.extend(opened);
    let warnings: Vec<&str> = warnings.iter().flatten().map(String::as_str).collect();
    let is_error = !warnings.is_empty();
    parts.extend(warnings);
    (!parts.is_empty()).then(|| (parts.join("; "), is_error))
}

/// The sticky warning shown at startup when no save will happen (the file
/// is write-protected, or `route` is refused), or a save would leave
/// hard-linked aliases behind (only a replacing save does) or take the file from `foreign_owner`, the
/// other user who owns it (see `writer::is_read_only`, `writer::save_route`,
/// `writer::hard_link_count` and `writer::foreign_owner`).
pub fn startup_warning(
    read_only: bool,
    route: crate::writer::SaveRoute,
    hard_links: Option<u64>,
    foreign_owner: Option<&str>,
) -> Option<String> {
    use crate::writer::SaveRoute;
    let mut warnings = Vec::new();
    if read_only {
        warnings.push(
            "this file is read-only — changes won't be saved (chmod u+w to allow saving)"
                .to_string(),
        );
    } else if route == SaveRoute::Refused {
        warnings.push(
            "you can't save this file (no write permission) — changes won't be saved".to_string(),
        );
    }
    let route = if read_only { SaveRoute::Refused } else { route };
    if let Some(links) = hard_links.filter(|links| *links > 1 && route == SaveRoute::Replace) {
        warnings.push(format!(
            "{links} hard links to this file — a save leaves the others behind"
        ));
    }
    match (foreign_owner, route) {
        (Some(owner), SaveRoute::Replace) => warnings.push(format!(
            "this file belongs to user {owner} — saving makes it yours"
        )),
        (Some(owner), SaveRoute::InPlace) => warnings.push(format!(
            "this file belongs to user {owner} — changes will be saved in place, so it stays theirs"
        )),
        _ => {}
    }
    (!warnings.is_empty()).then(|| format!("Warning: {}", warnings.join("; ")))
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
