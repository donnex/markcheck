//! Pure resolution for the things markcheck launches or acts on outside
//! itself: the editor and link-opener commands from the environment, the
//! editor's jump-to-line argument, whether git-sync applies to a file, and the
//! startup warning about a file a save would disturb. The spawning itself
//! stays in `main.rs`.

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

/// Whether `o` has nowhere to send a link: no usable `$BROWSER` entry and,
/// outside macOS, no graphical session (`$DISPLAY` and `$WAYLAND_DISPLAY`
/// both unset or empty) — typically an SSH login on a server. The platform
/// opener can't show anything there, so the link is copied instead.
pub fn no_browser_available(
    browser: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
) -> bool {
    let is_set = |value: Option<&str>| value.is_some_and(|v| !v.trim().is_empty());
    let browser_set = browser
        .unwrap_or_default()
        .split(':')
        .any(|entry| !entry.trim().is_empty());
    !cfg!(target_os = "macos") && !browser_set && !is_set(display) && !is_set(wayland_display)
}

/// A command's basename, for status messages: `/usr/bin/xdg-open` → `xdg-open`.
pub fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
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
}

/// Resolves `--git-sync` and `git_sync_paths` for `file_path`. An OR, not
/// the flag-over-config precedence of the boolean settings. Every entry is
/// checked, even once one matches, so a typo is reported either way.
pub fn git_sync_request(
    cli_flag: bool,
    paths: Option<&[PathBuf]>,
    file_path: &Path,
    home: Option<&str>,
) -> GitSyncRequest {
    let mut requested = cli_flag;
    let mut unresolved = Vec::new();
    for entry in paths.unwrap_or(&[]) {
        let prefix = expand_tilde(entry, home);
        if !prefix.exists() {
            unresolved.push(entry.clone());
        } else if git_sync_path_matches(file_path, &prefix) {
            requested = true;
        }
    }
    GitSyncRequest {
        requested,
        unresolved,
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

/// Joins the startup messages into the one the status bar shows: sticky and
/// error-colored if any of them is a warning, otherwise the ephemeral
/// "Created new checklist" alone. Returns the text and whether it's an error.
pub fn startup_message(created_new: bool, warnings: &[Option<String>]) -> Option<(String, bool)> {
    let mut parts: Vec<&str> = Vec::new();
    if created_new {
        parts.push("Created new checklist");
    }
    let warnings: Vec<&str> = warnings.iter().flatten().map(String::as_str).collect();
    let is_error = !warnings.is_empty();
    parts.extend(warnings);
    (!parts.is_empty()).then(|| (parts.join("; "), is_error))
}

/// The sticky warning shown at startup when the file is write-protected, so
/// no save will happen, or a save would leave hard-linked aliases behind or
/// take the file from another user (see `writer::is_read_only`,
/// `writer::hard_link_count` and `writer::foreign_owner`).
pub fn startup_warning(
    read_only: bool,
    hard_links: Option<u64>,
    foreign_owner: Option<u32>,
) -> Option<String> {
    let mut warnings = Vec::new();
    if read_only {
        warnings.push(
            "this file is read-only — changes won't be saved (chmod u+w to allow saving)"
                .to_string(),
        );
    }
    if let Some(links) = hard_links.filter(|links| *links > 1) {
        warnings.push(format!(
            "{links} hard links to this file — a save leaves the others behind"
        ));
    }
    if let Some(owner) = foreign_owner {
        warnings.push(format!(
            "this file belongs to user {owner} — saving makes it yours"
        ));
    }
    (!warnings.is_empty()).then(|| format!("Warning: {}", warnings.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::{
        GitSyncRequest, OPENER_FAILURE_WINDOW, editor_file_args, expand_tilde,
        git_sync_path_matches, git_sync_request, git_sync_warning, no_browser_available,
        opener_failure_message, resolve_editor, resolve_opener, startup_message, startup_warning,
        watch_warning,
    };
    use std::path::{Path, PathBuf};

    const URL: &str = "https://example.com/x";

    fn is_default_opener(candidates: &[(String, Vec<String>)]) -> bool {
        candidates.len() == 1
            && (candidates[0].0 == "xdg-open" || candidates[0].0 == "open")
            && candidates[0].1 == vec![URL.to_string()]
    }

    #[test]
    fn no_browser_only_without_browser_and_without_a_graphical_session() {
        let headless = !cfg!(target_os = "macos");
        assert_eq!(no_browser_available(None, None, None), headless);
        assert_eq!(
            no_browser_available(Some(" : "), Some(""), Some(" ")),
            headless
        );
        assert!(!no_browser_available(Some("w3m"), None, None));
        assert!(!no_browser_available(None, Some(":0"), None));
        assert!(!no_browser_available(None, None, Some("wayland-0")));
    }

    #[test]
    fn opener_failure_reports_a_quick_non_zero_exit_only() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        use std::time::Duration;

        let quick = Duration::from_millis(50);
        let ok = Ok(ExitStatus::from_raw(0));
        assert_eq!(opener_failure_message("xdg-open", &ok, quick), None);

        let failed = Ok(ExitStatus::from_raw(3 << 8));
        assert_eq!(
            opener_failure_message("/usr/bin/xdg-open", &failed, quick).as_deref(),
            Some("Couldn't open link (xdg-open exited with code 3)")
        );
        let late = OPENER_FAILURE_WINDOW + Duration::from_secs(1);
        assert_eq!(opener_failure_message("xdg-open", &failed, late), None);

        let killed = Ok(ExitStatus::from_raw(9));
        let message = opener_failure_message("w3m", &killed, quick).unwrap();
        assert!(
            message.starts_with("Couldn't open link (w3m was stopped ("),
            "{message}"
        );

        let err = Err(std::io::Error::other("boom"));
        assert_eq!(
            opener_failure_message("w3m", &err, quick).as_deref(),
            Some("Couldn't open link (couldn't wait for w3m: boom)")
        );
    }

    #[test]
    fn opener_prefers_browser_then_platform_default() {
        let (candidates, warning) = resolve_opener(Some("firefox --new-tab"), URL);
        assert_eq!(
            candidates,
            vec![(
                "firefox".to_string(),
                vec!["--new-tab".to_string(), URL.to_string()]
            )]
        );
        assert!(warning.is_none());
        // Empty/absent $BROWSER falls back to the platform opener.
        let (candidates, warning) = resolve_opener(None, URL);
        assert!(is_default_opener(&candidates));
        assert!(warning.is_none());
        assert!(is_default_opener(&resolve_opener(Some("   "), URL).0));
    }

    #[test]
    fn opener_colon_list_yields_each_command_in_order() {
        let (candidates, warning) = resolve_opener(Some("firefox:w3m -o x"), URL);
        assert_eq!(
            candidates,
            vec![
                ("firefox".to_string(), vec![URL.to_string()]),
                (
                    "w3m".to_string(),
                    vec!["-o".to_string(), "x".to_string(), URL.to_string()]
                ),
            ]
        );
        assert!(warning.is_none());
        // Empty entries (doubled or trailing colons) are skipped.
        let (candidates, _) = resolve_opener(Some(":firefox::"), URL);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "firefox");
        // Only colons: nothing usable, so the platform default.
        assert!(is_default_opener(&resolve_opener(Some("::"), URL).0));
    }

    #[test]
    fn opener_substitutes_the_url_placeholder_instead_of_appending() {
        let (candidates, warning) = resolve_opener(Some("firefox --new-tab %s"), URL);
        assert_eq!(
            candidates,
            vec![(
                "firefox".to_string(),
                vec!["--new-tab".to_string(), URL.to_string()]
            )]
        );
        assert!(warning.is_none());
        // Embedded in a larger argument, and `%%` is a literal percent sign.
        let (candidates, _) = resolve_opener(Some("wrap --url=%s --pct=100%% --odd=%x %"), URL);
        assert_eq!(
            candidates[0].1,
            vec![
                format!("--url={URL}"),
                "--pct=100%".to_string(),
                "--odd=%x".to_string(),
                "%".to_string(),
            ]
        );
        // `%%s` is a literal `%s`, not a placeholder, so the URL is appended.
        let (candidates, _) = resolve_opener(Some("wrap %%s"), URL);
        assert_eq!(candidates[0].1, vec!["%s".to_string(), URL.to_string()]);
    }

    #[test]
    fn opener_quoted_argument_with_spaces_stays_one_token() {
        // Shell-word splitting (not plain whitespace) so a quoted argument
        // containing spaces survives as a single token.
        let (candidates, warning) =
            resolve_opener(Some(r#"my-wrapper --arg="value with spaces""#), URL);
        assert_eq!(candidates[0].0, "my-wrapper");
        assert_eq!(
            candidates[0].1,
            vec!["--arg=value with spaces".to_string(), URL.to_string()]
        );
        assert!(warning.is_none());
    }

    #[test]
    fn opener_validly_quoted_empty_value_falls_back_silently() {
        // A value that shell-quotes to nothing (e.g. `BROWSER="''"`) must
        // fall back the same as an unset/whitespace-only $BROWSER, not
        // attempt to launch a program with an empty name (`shell_words::split`
        // returns one empty token for this, not an empty list).
        let (candidates, warning) = resolve_opener(Some("''"), URL);
        assert!(is_default_opener(&candidates));
        assert!(warning.is_none());
    }

    #[test]
    fn opener_malformed_quoting_falls_back_with_a_warning() {
        // Unbalanced quotes can't be parsed; fall back to the platform
        // default rather than misparsing, and surface why.
        let (candidates, warning) = resolve_opener(Some("my-wrapper 'unterminated"), URL);
        assert!(is_default_opener(&candidates));
        assert!(
            warning.is_some_and(|w| w.contains("$BROWSER") && w.contains("falling back")),
            "warning names the offending variable"
        );
        // In a list, the bad entry is skipped and the good one still used.
        let (candidates, warning) = resolve_opener(Some("bad 'x:firefox"), URL);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "firefox");
        assert!(warning.is_some_and(|w| w.contains("bad 'x") && !w.contains("falling back")));
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

    fn args(program: &str, line: Option<usize>) -> Vec<String> {
        editor_file_args(program, Path::new("dir/run.md"), line)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn plus_n_editors_get_the_line_before_the_path() {
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
            "kak",
            "micro",
        ] {
            assert_eq!(args(prog, Some(5)), ["+5", "dir/run.md"], "{prog}");
        }
    }

    #[test]
    fn colon_suffix_editors_get_the_line_after_the_path() {
        for prog in ["hx", "helix", "subl"] {
            assert_eq!(args(prog, Some(42)), ["dir/run.md:42"], "{prog}");
        }
    }

    #[test]
    fn vscode_editors_get_a_goto_argument() {
        for prog in ["code", "codium"] {
            assert_eq!(args(prog, Some(7)), ["--goto", "dir/run.md:7"], "{prog}");
        }
    }

    #[test]
    fn line_arg_matches_on_basename() {
        // A full path or a versioned wrapper still resolves to the base name.
        assert_eq!(args("/usr/bin/vim", Some(12)), ["+12", "dir/run.md"]);
        assert_eq!(args("/opt/nvim/bin/nvim", Some(1)), ["+1", "dir/run.md"]);
        assert_eq!(args("/usr/local/bin/hx", Some(3)), ["dir/run.md:3"]);
    }

    #[test]
    fn unknown_editors_and_no_line_get_the_bare_path() {
        for prog in ["notepad", "unknown-editor"] {
            assert_eq!(args(prog, Some(5)), ["dir/run.md"], "{prog}");
        }
        for prog in ["vim", "hx", "code", "notepad"] {
            assert_eq!(args(prog, None), ["dir/run.md"], "{prog}");
        }
    }

    #[test]
    fn git_sync_path_matches_requires_a_real_directory_boundary() {
        // `Path::starts_with` compares components, so a sibling sharing the prefix
        // must not match.
        let root = crate::test_support::unique_temp_path("launch-gitsync", "", None);
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
        let root = crate::test_support::unique_temp_path("launch-gitsync-missing", "", None);
        std::fs::create_dir_all(&root).unwrap();
        let file = std::fs::canonicalize(&root).unwrap().join("todo.md");

        assert!(!git_sync_path_matches(&file, &root.join("does-not-exist")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_sync_is_requested_by_the_flag_or_a_covering_path() {
        let root = crate::test_support::unique_temp_path("launch-gitsync-req", "", None);
        std::fs::create_dir_all(&root).unwrap();
        let file = std::fs::canonicalize(&root).unwrap().join("todo.md");
        let covering = vec![root.clone()];
        let other = vec![root.join("missing")];

        assert!(git_sync_request(true, None, &file, None).requested);
        assert!(git_sync_request(false, Some(&covering), &file, None).requested);
        assert!(!git_sync_request(false, Some(&other), &file, None).requested);
        assert!(!git_sync_request(false, None, &file, None).requested);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_sync_request_reports_entries_that_do_not_resolve() {
        // Reported even when another entry, or the flag, turns git-sync on:
        // a typo is worth knowing about either way.
        let root = crate::test_support::unique_temp_path("launch-gitsync-unresolved", "", None);
        std::fs::create_dir_all(&root).unwrap();
        let file = std::fs::canonicalize(&root).unwrap().join("todo.md");
        let typo = root.join("checklsts");
        let paths = vec![typo.clone(), root.clone()];

        assert_eq!(
            git_sync_request(false, Some(&paths), &file, None),
            GitSyncRequest {
                requested: true,
                unresolved: vec![typo.clone()],
            }
        );
        assert_eq!(
            git_sync_request(true, Some(&paths[..1]), &file, None).unresolved,
            vec![typo]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn git_sync_request_expands_a_tilde_entry_against_home() {
        let home = crate::test_support::unique_temp_path("launch-gitsync-home", "", None);
        std::fs::create_dir_all(home.join("checklists")).unwrap();
        let file = std::fs::canonicalize(home.join("checklists"))
            .unwrap()
            .join("todo.md");
        let paths = vec![PathBuf::from("~/checklists")];

        assert_eq!(
            git_sync_request(false, Some(&paths), &file, home.to_str()),
            GitSyncRequest {
                requested: true,
                unresolved: Vec::new(),
            }
        );
        let without_home = git_sync_request(false, Some(&paths), &file, None);
        assert!(!without_home.requested);
        assert_eq!(without_home.unresolved, paths);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expand_tilde_only_rewrites_a_leading_tilde_component() {
        let home = Some("/home/example-user");
        assert_eq!(
            expand_tilde(Path::new("~/checklists"), home),
            PathBuf::from("/home/example-user/checklists")
        );
        assert_eq!(
            expand_tilde(Path::new("~"), home),
            PathBuf::from("/home/example-user")
        );
        for unchanged in ["/srv/checklists", "~other/checklists", "notes/~/x"] {
            assert_eq!(
                expand_tilde(Path::new(unchanged), home),
                Path::new(unchanged)
            );
        }
        assert_eq!(
            expand_tilde(Path::new("~/checklists"), Some("")),
            Path::new("~/checklists")
        );
        assert_eq!(
            expand_tilde(Path::new("~/checklists"), None),
            Path::new("~/checklists")
        );
    }

    #[test]
    fn git_sync_warning_names_unresolved_entries_and_the_detect_reason() {
        assert_eq!(git_sync_warning(&[], None), None);
        assert_eq!(
            git_sync_warning(&[PathBuf::from("~/checklsts")], None).as_deref(),
            Some("git_sync_paths: ~/checklsts does not exist — ignored")
        );
        assert_eq!(
            git_sync_warning(&[PathBuf::from("/a"), PathBuf::from("/b")], None).as_deref(),
            Some("git_sync_paths: /a, /b do not exist — ignored")
        );
        assert_eq!(
            git_sync_warning(&[], Some("git not found on PATH")).as_deref(),
            Some("Git sync off: git not found on PATH")
        );
        assert_eq!(
            git_sync_warning(
                &[PathBuf::from("/a")],
                Some("the file is not in a git repository")
            )
            .as_deref(),
            Some(
                "git_sync_paths: /a does not exist — ignored; \
                 Git sync off: the file is not in a git repository"
            )
        );
    }

    #[test]
    fn watch_warning_names_the_error_and_the_fallback_interval() {
        assert_eq!(
            watch_warning("too many open files"),
            "Live reload slowed: can't watch the file (too many open files) — \
             checking every 2 s instead"
        );
    }

    #[test]
    fn startup_message_is_sticky_only_when_something_needs_attention() {
        assert_eq!(startup_message(false, &[None, None]), None);
        assert_eq!(
            startup_message(true, &[None, None]),
            Some(("Created new checklist".to_string(), false))
        );
        assert_eq!(
            startup_message(false, &[Some("one".to_string()), Some("two".to_string())]),
            Some(("one; two".to_string(), true))
        );
        assert_eq!(
            startup_message(true, &[None, Some("Git sync off: x".to_string())]),
            Some(("Created new checklist; Git sync off: x".to_string(), true))
        );
    }

    #[test]
    fn startup_warning_names_each_hazard() {
        assert_eq!(startup_warning(false, None, None), None);
        assert_eq!(startup_warning(false, Some(1), None), None);
        assert_eq!(
            startup_warning(true, None, None).as_deref(),
            Some(
                "Warning: this file is read-only — changes won't be saved (chmod u+w to allow \
                 saving)"
            )
        );
        assert_eq!(
            startup_warning(false, Some(3), None).as_deref(),
            Some("Warning: 3 hard links to this file — a save leaves the others behind")
        );
        assert_eq!(
            startup_warning(false, None, Some(1001)).as_deref(),
            Some("Warning: this file belongs to user 1001 — saving makes it yours")
        );
        let both = startup_warning(false, Some(2), Some(1001)).unwrap();
        assert!(both.contains("2 hard links") && both.contains("; this file belongs to user 1001"));
    }
}
