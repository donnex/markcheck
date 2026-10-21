use super::LinkRoute;
use super::{
    CoveringEntry, GitSyncRequest, OPENER_FAILURE_WINDOW, already_open_warning, editor_file_args,
    exit_summary, expand_tilde, git_sync_path_matches, git_sync_request, git_sync_scope,
    git_sync_warning, leftovers_warning, link_route, opener_failure_message, resolve_editor,
    resolve_opener, startup_message, startup_warning, terminal_title, unshown_warning,
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
fn link_is_copied_instead_only_without_browser_and_without_a_graphical_session() {
    let headless = if cfg!(target_os = "macos") {
        LinkRoute::Open
    } else {
        LinkRoute::CopyInstead
    };
    assert_eq!(link_route(None, None, None, false), headless);
    assert_eq!(
        link_route(Some(" : "), Some(""), Some(" "), false),
        headless
    );
    assert_eq!(link_route(Some("w3m"), None, None, false), LinkRoute::Open);
    assert_eq!(link_route(None, Some(":0"), None, false), LinkRoute::Open);
    assert_eq!(
        link_route(None, None, Some("wayland-0"), false),
        LinkRoute::Open
    );
}

#[test]
fn over_ssh_a_display_opens_the_link_and_copies_it_too() {
    assert_eq!(
        link_route(None, Some(":0"), None, true),
        LinkRoute::OpenAndCopy
    );
    assert_eq!(
        link_route(None, None, Some("wayland-0"), true),
        LinkRoute::OpenAndCopy
    );
    assert_eq!(
        link_route(Some("w3m"), Some(":0"), None, true),
        LinkRoute::Open
    );
    let headless = if cfg!(target_os = "macos") {
        LinkRoute::OpenAndCopy
    } else {
        LinkRoute::CopyInstead
    };
    assert_eq!(link_route(None, None, None, true), headless);
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

    assert!(git_sync_request(true, false, None, &file, None).requested);
    assert!(git_sync_request(false, false, Some(&covering), &file, None).requested);
    assert!(!git_sync_request(false, false, Some(&other), &file, None).requested);
    assert!(!git_sync_request(false, false, None, &file, None).requested);

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
        git_sync_request(false, false, Some(&paths), &file, None),
        GitSyncRequest {
            requested: true,
            unresolved: vec![typo.clone()],
            by_flag: false,
            covering: vec![CoveringEntry {
                entry: root.clone(),
                prefix: std::fs::canonicalize(&root).unwrap(),
                nested: false,
            }],
        }
    );
    assert_eq!(
        git_sync_request(true, false, Some(&paths[..1]), &file, None).unresolved,
        vec![typo]
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn no_git_sync_turns_off_a_matching_entry_but_still_reports_typos() {
    let root = crate::test_support::unique_temp_path("launch-gitsync-off", "", None);
    std::fs::create_dir_all(&root).unwrap();
    let file = std::fs::canonicalize(&root).unwrap().join("todo.md");
    let typo = root.join("checklsts");
    let paths = vec![typo.clone(), root.clone()];

    assert_eq!(
        git_sync_request(false, true, Some(&paths), &file, None),
        GitSyncRequest {
            requested: false,
            unresolved: vec![typo],
            by_flag: false,
            covering: vec![CoveringEntry {
                entry: root.clone(),
                prefix: std::fs::canonicalize(&root).unwrap(),
                nested: false,
            }],
        }
    );
    assert!(!git_sync_request(false, true, None, &file, None).requested);

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
        git_sync_request(false, false, Some(&paths), &file, home.to_str()),
        GitSyncRequest {
            requested: true,
            unresolved: Vec::new(),
            by_flag: false,
            covering: vec![CoveringEntry {
                entry: PathBuf::from("~/checklists"),
                prefix: std::fs::canonicalize(home.join("checklists")).unwrap(),
                nested: false,
            }],
        }
    );
    let without_home = git_sync_request(false, false, Some(&paths), &file, None);
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
    assert_eq!(startup_message(false, None, &[None, None]), None);
    assert_eq!(
        startup_message(true, None, &[None, None]),
        Some(("Created new checklist".to_string(), false))
    );
    assert_eq!(
        startup_message(false, Some("Opened at #b"), &[None]),
        Some(("Opened at #b".to_string(), false))
    );
    assert_eq!(
        startup_message(
            false,
            None,
            &[Some("one".to_string()), Some("two".to_string())]
        ),
        Some(("one; two".to_string(), true))
    );
    assert_eq!(
        startup_message(true, None, &[None, Some("Git sync off: x".to_string())]),
        Some(("Created new checklist; Git sync off: x".to_string(), true))
    );
    assert_eq!(
        startup_message(
            false,
            Some("Opened at line 4"),
            &[Some("Warning: x".to_string())]
        ),
        Some(("Opened at line 4; Warning: x".to_string(), true))
    );
}

#[test]
fn startup_warning_names_each_hazard() {
    use crate::writer::SaveRoute::{InPlace, Replace};
    assert_eq!(startup_warning(false, Replace, None, None), None);
    assert_eq!(startup_warning(false, Replace, Some(1), None), None);
    assert_eq!(startup_warning(false, InPlace, None, None), None);
    assert_eq!(
        startup_warning(true, Replace, None, None).as_deref(),
        Some(
            "Warning: this file is read-only — changes won't be saved (chmod u+w to allow \
                 saving)"
        )
    );
    assert_eq!(
        startup_warning(false, Replace, Some(3), None).as_deref(),
        Some("Warning: 3 hard links to this file — a save leaves the others behind")
    );
    assert_eq!(
        startup_warning(false, Replace, None, Some("1001")).as_deref(),
        Some("Warning: this file belongs to user 1001 — saving makes it yours")
    );
    let both = startup_warning(false, Replace, Some(2), Some("1001")).unwrap();
    assert!(both.contains("2 hard links") && both.contains("; this file belongs to user 1001"));
}

#[test]
fn startup_warning_says_when_another_users_file_cant_be_taken_over() {
    use crate::writer::SaveRoute::{InPlace, Refused, Replace};
    assert_eq!(
        startup_warning(false, Refused, Some(2), Some("root")).as_deref(),
        Some("Warning: you can't save this file (no write permission) — changes won't be saved")
    );
    assert_eq!(
        startup_warning(false, InPlace, Some(2), Some("example-user")).as_deref(),
        Some(
            "Warning: this file belongs to user example-user — changes will be saved in place, \
             so it stays theirs"
        )
    );
    let read_only = startup_warning(true, Replace, Some(2), Some("example-user")).unwrap();
    assert!(read_only.contains("read-only"), "{read_only}");
    assert!(!read_only.contains("belongs to"), "{read_only}");
    assert!(!read_only.contains("hard links"), "{read_only}");
}

#[test]
fn leftovers_warning_counts_what_was_removed() {
    assert_eq!(leftovers_warning(0), None);
    assert_eq!(
        leftovers_warning(1).as_deref(),
        Some("removed 1 file left by an interrupted markcheck")
    );
    assert_eq!(
        leftovers_warning(3).as_deref(),
        Some("removed 3 files left by an interrupted markcheck")
    );
}

fn covering_request(entry: &str, prefix: &Path, nested: bool) -> GitSyncRequest {
    GitSyncRequest {
        requested: true,
        unresolved: Vec::new(),
        by_flag: false,
        covering: vec![CoveringEntry {
            entry: PathBuf::from(entry),
            prefix: prefix.to_path_buf(),
            nested,
        }],
    }
}

#[test]
fn git_sync_scope_keeps_a_plain_entry_to_its_own_repository() {
    let root = crate::test_support::unique_temp_path("launch-gitsync-scope", "", None);
    let nested = root.join("notes").join("imported");
    std::fs::create_dir_all(&nested).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let notes = root.join("notes");
    let nested = notes.join("imported");

    let plain = covering_request("~/notes", &notes, false);
    assert_eq!(
        git_sync_scope(&plain, &notes),
        Ok(()),
        "the entry's own repository"
    );
    assert_eq!(
        git_sync_scope(&plain, &root),
        Ok(()),
        "a repository above it"
    );
    let refused = git_sync_scope(&plain, &nested).unwrap_err();
    assert!(
        refused.contains("nested inside git_sync_paths entry ~/notes")
            && refused.contains("/**")
            && refused.contains("--git-sync"),
        "{refused}"
    );

    let recursive = covering_request("~/notes/**", &notes, true);
    assert_eq!(git_sync_scope(&recursive, &nested), Ok(()));

    let mut by_flag = covering_request("~/notes", &notes, false);
    by_flag.by_flag = true;
    assert_eq!(git_sync_scope(&by_flag, &nested), Ok(()));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_trailing_double_star_marks_an_entry_as_covering_nested_repositories() {
    let root = crate::test_support::unique_temp_path("launch-gitsync-star", "", None);
    std::fs::create_dir_all(root.join("deep")).unwrap();
    let file = std::fs::canonicalize(&root)
        .unwrap()
        .join("deep")
        .join("todo.md");
    let paths = vec![root.join("**")];

    let request = git_sync_request(false, false, Some(&paths), &file, None);
    assert!(request.requested);
    assert!(request.unresolved.is_empty());
    assert_eq!(
        request.covering,
        vec![CoveringEntry {
            entry: root.join("**"),
            prefix: std::fs::canonicalize(&root).unwrap(),
            nested: true,
        }]
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn unshown_warning_lists_what_is_not_shown() {
    use crate::model::Unshown;
    assert_eq!(unshown_warning(&Unshown::default()), None);
    assert_eq!(
        unshown_warning(&Unshown {
            paragraphs: 1,
            ..Unshown::default()
        })
        .as_deref(),
        Some("1 paragraph outside task lists not shown")
    );
    assert_eq!(
        unshown_warning(&Unshown {
            paragraphs: 2,
            quotes: 1,
            bullets: 0,
            code_blocks: 3,
        })
        .as_deref(),
        Some("2 paragraphs, 1 quote and 3 code blocks outside task lists not shown")
    );
    assert_eq!(
        unshown_warning(&Unshown {
            bullets: 1,
            ..Unshown::default()
        })
        .as_deref(),
        Some("1 bullet point outside task lists not shown")
    );
    assert_eq!(
        unshown_warning(&Unshown {
            paragraphs: 1,
            bullets: 2,
            ..Unshown::default()
        })
        .as_deref(),
        Some("1 paragraph and 2 bullet points outside task lists not shown")
    );
}

#[test]
fn already_open_warning_names_the_other_sessions() {
    use crate::writer::OtherSession;
    use std::time::Duration;
    assert_eq!(already_open_warning(&[]), None);
    assert_eq!(
        already_open_warning(&[OtherSession {
            pid: 4242,
            open_for: Some(Duration::from_secs(12 * 60 + 5)),
        }])
        .as_deref(),
        Some(
            "already open in another markcheck (pid 4242, opened 12 min ago): its changes show up here as reloads"
        )
    );
    assert_eq!(
        already_open_warning(&[OtherSession {
            pid: 7,
            open_for: Some(Duration::from_secs(3)),
        }])
        .as_deref(),
        Some(
            "already open in another markcheck (pid 7, opened just now): its changes show up here as reloads"
        )
    );
    let two = [
        OtherSession {
            pid: 1,
            open_for: None,
        },
        OtherSession {
            pid: 2,
            open_for: None,
        },
    ];
    assert!(
        already_open_warning(&two)
            .unwrap()
            .starts_with("already open in 2 other markcheck sessions (pids 1, 2)")
    );
}

fn state_from(markdown: &str, name: &str) -> crate::model::AppState {
    let dir = crate::test_support::unique_temp_path("launch", "summary", None);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, markdown).unwrap();
    crate::model::AppState::new(crate::parser::parse_document(path).unwrap())
}

#[test]
fn the_terminal_title_names_the_checklist_and_its_progress() {
    let state = state_from("# Deploy\n\n## A\n\n- [x] one\n- [ ] two\n", "deploy.md");
    assert_eq!(terminal_title(&state), "Deploy · 1/2 — markcheck");
    let untitled = state_from("- [ ] one\n", "night-shift.md");
    assert_eq!(terminal_title(&untitled), "Night Shift · 0/1 — markcheck");
}

#[test]
fn the_terminal_title_and_exit_line_drop_terminal_controls() {
    let state = state_from(
        "# Run\x07\x1b]52;c;ZWNobw==\x07book\n\n- [ ] st\x1b[2Jep\n",
        "deploy.md",
    );
    assert_eq!(
        terminal_title(&state),
        "Run]52;c;ZWNobw==book · 0/1 — markcheck"
    );
    assert_eq!(
        exit_summary(&state),
        "deploy.md — Run]52;c;ZWNobw==book: 0/1 done, stopped at \"st[2Jep\""
    );
}

#[test]
fn the_exit_summary_says_where_the_run_stopped_or_that_it_finished() {
    let state = state_from(
        "# Deploy\n\n## A\n\n- [x] one\n- [/] two\n- [-] three\n- [ ] four\n",
        "deploy.md",
    );
    assert_eq!(
        exit_summary(&state),
        "deploy.md — Deploy: 1/3 done (1 started, 1 skipped), stopped at \"two\""
    );
    let finished = state_from("# Deploy\n\n- [x] one\n- [x] two\n", "deploy.md");
    assert_eq!(exit_summary(&finished), "deploy.md — Deploy: all 2 done");
}
