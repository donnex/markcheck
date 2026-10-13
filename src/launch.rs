//! Pure resolution for the things markcheck launches or acts on outside
//! itself: the editor and link-opener commands from the environment, the
//! editor's jump-to-line argument, whether git-sync applies to a file, and the
//! startup warning about a file a save would disturb. The spawning itself
//! stays in `main.rs`.

use std::path::{Path, PathBuf};

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

/// `+N` for the terminal editors that understand it (matched on basename);
/// nothing for the rest.
pub fn editor_line_args(program: &str, line: usize) -> Vec<String> {
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

/// Whether a configured `git_sync_paths` entry covers the canonical
/// `file_path`. The prefix is canonicalized too, and compared by path
/// components, so `/foo/bar` doesn't match `/foo/barn`.
pub fn git_sync_path_matches(file_path: &Path, prefix: &Path) -> bool {
    std::fs::canonicalize(prefix).is_ok_and(|canonical| file_path.starts_with(canonical))
}

/// Whether git-sync was asked for: by `--git-sync`, or by a configured
/// `git_sync_paths` entry covering `file_path`. An OR, not the
/// flag-over-config precedence of the boolean settings.
pub fn git_sync_requested(cli_flag: bool, paths: Option<&[PathBuf]>, file_path: &Path) -> bool {
    cli_flag
        || paths
            .unwrap_or(&[])
            .iter()
            .any(|prefix| git_sync_path_matches(file_path, prefix))
}

/// The sticky warning shown at startup when a save would leave hard-linked
/// aliases behind or take the file from another user (see
/// `writer::hard_link_count` and `writer::foreign_owner`).
pub fn startup_warning(hard_links: Option<u64>, foreign_owner: Option<u32>) -> Option<String> {
    let mut warnings = Vec::new();
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
        editor_line_args, git_sync_path_matches, git_sync_requested, resolve_editor,
        resolve_opener, startup_warning,
    };

    const URL: &str = "https://example.com/x";

    fn is_default_opener(candidates: &[(String, Vec<String>)]) -> bool {
        candidates.len() == 1
            && (candidates[0].0 == "xdg-open" || candidates[0].0 == "open")
            && candidates[0].1 == vec![URL.to_string()]
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
        let covering = vec![root.join("missing"), root.clone()];
        let other = vec![root.join("missing")];

        assert!(git_sync_requested(true, None, &file));
        assert!(git_sync_requested(false, Some(&covering), &file));
        assert!(!git_sync_requested(false, Some(&other), &file));
        assert!(!git_sync_requested(false, None, &file));
    }

    #[test]
    fn startup_warning_names_each_hazard() {
        assert_eq!(startup_warning(None, None), None);
        assert_eq!(startup_warning(Some(1), None), None);
        assert_eq!(
            startup_warning(Some(3), None).as_deref(),
            Some("Warning: 3 hard links to this file — a save leaves the others behind")
        );
        assert_eq!(
            startup_warning(None, Some(1001)).as_deref(),
            Some("Warning: this file belongs to user 1001 — saving makes it yours")
        );
        let both = startup_warning(Some(2), Some(1001)).unwrap();
        assert!(both.contains("2 hard links") && both.contains("; this file belongs to user 1001"));
    }
}
