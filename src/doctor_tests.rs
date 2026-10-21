use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser as _;

use super::*;
use crate::test_support::unique_temp_path;

fn cli(args: &[&str]) -> Cli {
    let mut argv = vec!["markcheck", "--doctor"];
    argv.extend_from_slice(args);
    Cli::try_parse_from(argv).unwrap()
}

fn temp_dir(hint: &str) -> PathBuf {
    let dir = unique_temp_path("doctor", hint, None);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

/// An environment whose config lookup finds an empty directory.
fn env() -> Env {
    Env {
        term: Some("xterm-256color".to_string()),
        xdg_config_home: Some(temp_dir("xdg").display().to_string()),
        ..Env::default()
    }
}

fn some(value: &str) -> Option<String> {
    Some(value.to_string())
}

fn named<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("no {name} check in {checks:?}"))
}

fn git(dir: &Path, args: &[&str]) {
    let status = crate::test_support::git()
        .current_dir(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A checklist committed in a repository of its own, with an identity set.
fn repo_with_checklist(hint: &str) -> PathBuf {
    let work = temp_dir(hint);
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    git(&work, &["config", "user.name", "test"]);
    fs::write(work.join("list.md"), "- [ ] one\n").unwrap();
    git(&work, &["add", "list.md"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    work.join("list.md")
}

#[test]
fn env_from_process_reads_the_real_environment() {
    assert_eq!(Env::from_process().home, std::env::var("HOME").ok());
}

#[test]
fn checks_cover_every_area_and_add_the_file_ones_only_with_a_file() {
    let names = |checks: Vec<Check>| checks.iter().map(|c| c.name).collect::<Vec<_>>();
    let base = [
        "config",
        "colors",
        "theme",
        "icons",
        "clipboard",
        "editor",
        "browser",
        "locks",
        "temp",
    ];
    assert_eq!(names(checks(&cli(&[]), &env())), base);

    let file = temp_dir("plain").join("list.md");
    fs::write(&file, "- [ ] one\n").unwrap();
    let with_file = names(checks(&cli(&[file.to_str().unwrap()]), &env()));
    assert_eq!(with_file[..base.len()], base);
    assert_eq!(with_file[base.len()..], ["file", "git sync"]);
}

#[test]
fn flags_and_config_values_are_named_as_the_source() {
    let dir = temp_dir("config-source");
    let config = dir.join("config.toml");
    fs::write(&config, "color = 16\ntheme = \"light\"\nicons = \"nerd\"\n").unwrap();
    let found = checks(
        &cli(&["--config", config.to_str().unwrap(), "--clipboard", "osc52"]),
        &env(),
    );
    assert_eq!(
        named(&found, "colors").detail,
        "16 colors (config: color, not detected)"
    );
    assert!(
        named(&found, "theme")
            .detail
            .starts_with("light (config: theme;")
    );
    assert_eq!(
        named(&found, "icons").detail,
        "nerd (config: icons); needs a Nerd Font in the terminal"
    );
    assert_eq!(
        named(&found, "clipboard").detail,
        "OSC 52 only (--clipboard)"
    );

    let found = checks(&cli(&["--nerd-font"]), &env());
    assert!(named(&found, "icons").detail.starts_with("nerd (--icons)"));
}

#[test]
fn no_config_skips_the_file() {
    let found = checks(&cli(&["--no-config"]), &env());
    assert_eq!(
        named(&found, "config"),
        &ok("config", "skipped (--no-config); built-in defaults")
    );
}

#[test]
fn a_missing_default_config_is_fine_and_names_where_it_was_looked_for() {
    let env = env();
    let (_, found) = config_checks(&cli(&[]), &env);
    let expected = Path::new(env.xdg_config_home.as_deref().unwrap())
        .join("markcheck")
        .join("config.toml");
    assert_eq!(
        found,
        [ok(
            "config",
            format!(
                "{} (XDG_CONFIG_HOME; not found; built-in defaults)",
                expected.display()
            )
        )]
    );
}

#[test]
fn a_relative_xdg_config_home_falls_back_to_home_with_a_warning() {
    let home = temp_dir("home");
    let env = Env {
        xdg_config_home: some("relative/config"),
        home: Some(home.display().to_string()),
        ..Env::default()
    };
    let (_, found) = config_checks(&cli(&[]), &env);
    assert_eq!(found[0].level, Level::Warning);
    assert!(
        found[0].detail.contains("(HOME; not found")
            && found[0]
                .detail
                .contains("XDG_CONFIG_HOME=relative/config ignored: not an absolute path"),
        "{found:?}"
    );
}

#[test]
fn no_absolute_location_is_a_warning() {
    let env = Env {
        home: some("relative-home"),
        ..Env::default()
    };
    let (_, found) = config_checks(&cli(&[]), &env);
    assert_eq!(
        found,
        [warning(
            "config",
            "no location: neither XDG_CONFIG_HOME nor HOME is an absolute path; built-in \
             defaults; HOME=relative-home ignored: not an absolute path"
        )]
    );
}

#[test]
fn unknown_and_deprecated_keys_are_warnings() {
    let config = temp_dir("config-keys").join("config.toml");
    fs::write(&config, "nerd_font = true\ncolour = \"auto\"\n").unwrap();
    let (loaded, found) = config_checks(&cli(&["--config", config.to_str().unwrap()]), &env());
    assert_eq!(loaded.config.nerd_font, Some(true));
    assert_eq!(found[0].level, Level::Warning);
    assert!(
        found[0]
            .detail
            .starts_with(&format!("{} (--config; read)", config.display()))
            && found[0].detail.contains("unknown key `colour`")
            && found[0].detail.contains("`nerd_font` is deprecated"),
        "{found:?}"
    );
}

#[test]
fn a_missing_or_malformed_config_is_reported_and_defaults_are_used() {
    let dir = temp_dir("config-bad");
    let missing = dir.join("missing.toml");
    let (_, found) = config_checks(&cli(&["--config", missing.to_str().unwrap()]), &env());
    assert_eq!(found[0].level, Level::Warning);
    assert!(
        found[0].detail.contains("config file not found"),
        "{found:?}"
    );

    let malformed = dir.join("bad.toml");
    fs::write(&malformed, "color = [\n").unwrap();
    let (loaded, found) = config_checks(&cli(&["--config", malformed.to_str().unwrap()]), &env());
    assert_eq!(loaded, config::Loaded::default());
    assert!(
        found[0].detail.contains("invalid config file")
            && found[0].detail.ends_with("built-in defaults"),
        "{found:?}"
    );
}

#[test]
fn colors_say_which_variables_decided_them() {
    let env = Env {
        colorterm: some("truecolor"),
        term: some("xterm"),
        ..Env::default()
    };
    assert_eq!(
        color_check(
            ColorChoice::Auto,
            String::new(),
            ColorDepth::TrueColor,
            &env
        ),
        ok("colors", "truecolor (COLORTERM=truecolor, TERM=xterm)")
    );
    assert_eq!(
        color_check(ColorChoice::Auto, String::new(), ColorDepth::Mono, &env),
        ok("colors", "none (NO_COLOR is set)")
    );
    assert_eq!(
        color_check(
            ColorChoice::Palette256,
            "--color".to_string(),
            ColorDepth::Palette256,
            &env
        ),
        ok("colors", "256 colors (--color, not detected)")
    );
}

#[test]
fn an_unset_or_dumb_term_is_a_warning() {
    for term in [None, some(""), some("dumb")] {
        let env = Env {
            term,
            ..Env::default()
        };
        let found = color_check(ColorChoice::Auto, String::new(), ColorDepth::Basic, &env);
        assert_eq!(found.level, Level::Warning, "{found:?}");
        assert!(found.detail.starts_with("16 colors (COLORTERM unset, TERM"));
    }
}

#[test]
fn the_theme_says_when_the_palette_ignores_it() {
    let shown = |depth| theme_check(Theme::Dark, "default".to_string(), depth).detail;
    assert_eq!(
        shown(ColorDepth::TrueColor),
        "dark (default; never detected)"
    );
    assert_eq!(
        shown(ColorDepth::Palette256),
        "dark (default; never detected)"
    );
    assert!(shown(ColorDepth::Basic).ends_with("follows the terminal's own colors"));
    assert!(shown(ColorDepth::Mono).ends_with("unused without color"));
}

#[test]
fn auto_icons_are_ascii_on_the_linux_console() {
    assert_eq!(
        icons_check(IconChoice::Auto, String::new(), Some("linux")).detail,
        "ascii (auto: TERM=linux, the Linux console, whose font lacks the symbols)"
    );
    assert_eq!(
        icons_check(IconChoice::Auto, String::new(), None).detail,
        "unicode (auto: TERM unset)"
    );
    assert_eq!(
        icons_check(IconChoice::Ascii, "--icons".to_string(), Some("linux")).detail,
        "ascii (--icons)"
    );
}

fn graphical() -> Env {
    Env {
        display: some(":0"),
        ..Env::default()
    }
}

fn over_ssh(env: Env) -> Env {
    Env {
        ssh_tty: some("/dev/pts/0"),
        ..env
    }
}

#[test]
fn the_clipboard_route_follows_the_session() {
    let source = || "default".to_string();
    assert_eq!(
        clipboard_check(ClipboardChoice::Auto, source(), &graphical()).detail,
        "system clipboard, OSC 52 if it fails (default; local session)"
    );
    assert!(
        clipboard_check(ClipboardChoice::Auto, source(), &over_ssh(graphical()))
            .detail
            .starts_with("system clipboard and OSC 52 (default; SSH session")
    );
    assert_eq!(
        clipboard_check(ClipboardChoice::System, source(), &graphical()),
        ok("clipboard", "system clipboard only (default)")
    );
    assert_eq!(
        clipboard_check(ClipboardChoice::Osc52, source(), &Env::default()),
        ok("clipboard", "OSC 52 only (default)")
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn without_a_display_the_system_clipboard_is_flagged() {
    let source = || "default".to_string();
    assert!(
        clipboard_check(ClipboardChoice::Auto, source(), &Env::default())
            .detail
            .starts_with("OSC 52, since the system clipboard needs DISPLAY")
    );
    let found = clipboard_check(ClipboardChoice::System, source(), &Env::default());
    assert_eq!(found.level, Level::Warning);
    assert!(
        found.detail.contains("copies will likely fail"),
        "{found:?}"
    );
}

#[test]
fn osc_52_names_the_multiplexer_it_is_wrapped_for() {
    let tmux = Env {
        tmux: some("/tmp/tmux-1/default,1,0"),
        ..graphical()
    };
    assert!(
        clipboard_check(ClipboardChoice::Osc52, String::new(), &tmux)
            .detail
            .ends_with("; OSC 52 also wrapped for tmux (TMUX is set)")
    );
    let screen = Env {
        term: some("screen-256color"),
        ..graphical()
    };
    assert!(
        clipboard_check(ClipboardChoice::Auto, String::new(), &screen)
            .detail
            .ends_with("; OSC 52 wrapped for GNU screen (TERM starts with screen)")
    );
    assert!(
        !clipboard_check(ClipboardChoice::System, String::new(), &tmux)
            .detail
            .contains("tmux"),
        "a route without OSC 52 has nothing to wrap"
    );
}

#[test]
fn the_editor_names_the_variable_it_came_from() {
    let editor = |visual: Option<String>, editor: Option<String>| {
        editor_check(&Env {
            visual,
            editor,
            ..Env::default()
        })
    };
    assert_eq!(
        editor(some("code --wait"), some("nano")),
        ok("editor", "code --wait (from VISUAL)")
    );
    assert_eq!(
        editor(some(" "), some("nano")),
        ok("editor", "nano (from EDITOR)")
    );
    assert_eq!(
        editor(None, None),
        ok("editor", "vi (VISUAL and EDITOR unset)")
    );
    let found = editor(some("'unbalanced"), None);
    assert_eq!(found.level, Level::Warning);
    assert!(found.detail.contains("falling back to vi"), "{found:?}");
}

#[test]
fn the_browser_lists_its_openers_and_text_browsers() {
    let browser = |browser: Option<String>, env: Env| browser_check(&Env { browser, ..env });
    assert_eq!(
        browser(None, graphical()),
        ok("browser", "xdg-open (BROWSER unset)")
    );
    assert_eq!(
        browser(some("firefox:w3m %s"), Env::default()),
        ok(
            "browser",
            "firefox, then w3m (from BROWSER); w3m is a text browser, so it gets the \
             terminal until it exits"
        )
    );
    assert_eq!(
        browser(None, over_ssh(graphical())),
        ok(
            "browser",
            "xdg-open, and the link is copied too (BROWSER unset; SSH session)"
        )
    );
    let found = browser(some("'broken"), graphical());
    assert_eq!(found.level, Level::Warning);
    assert!(
        found.detail.contains("Couldn't parse $BROWSER"),
        "{found:?}"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn without_a_display_or_browser_links_are_copied() {
    assert_eq!(
        browser_check(&Env::default()),
        ok(
            "browser",
            "links are copied, not opened (BROWSER, DISPLAY and WAYLAND_DISPLAY unset)"
        )
    );
}

#[test]
fn the_lock_directory_says_why_it_was_chosen() {
    let dir = PathBuf::from("/run/user/1000");
    assert_eq!(
        lock_check(Some(dir.clone()), Some("/run/user/1000")),
        ok("locks", "/run/user/1000 (XDG_RUNTIME_DIR)")
    );
    let fallback = PathBuf::from("/tmp/markcheck-1000");
    assert!(
        lock_check(Some(fallback.clone()), Some("/elsewhere"))
            .detail
            .contains("XDG_RUNTIME_DIR=/elsewhere is not one of this user's private directories")
    );
    for unset in [None, Some("")] {
        assert_eq!(
            lock_check(Some(fallback.clone()), unset).detail,
            "/tmp/markcheck-1000 (a private directory in the temp dir; XDG_RUNTIME_DIR unset)"
        );
    }
    assert_eq!(lock_check(None, None).level, Level::Warning);
}

#[test]
fn the_temp_directory_must_exist() {
    let dir = temp_dir("temp");
    assert_eq!(
        temp_check(&dir, Some(dir.to_str().unwrap())),
        ok("temp", format!("{} (from TMPDIR)", dir.display()))
    );
    let missing = dir.join("missing");
    let found = temp_check(&missing, None);
    assert_eq!(found.level, Level::Warning);
    assert!(found.detail.contains("(TMPDIR unset) is not a directory"));
}

#[test]
fn a_missing_file_is_a_warning() {
    let dir = temp_dir("missing");
    let md = dir.join("gone.md");
    let found = file_checks(&cli(&[]), &config::Config::default(), &md, &env());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].level, Level::Warning);
    assert!(found[0].detail.starts_with("no such file"), "{found:?}");

    let plain = dir.join("gone");
    let found = file_checks(&cli(&[]), &config::Config::default(), &plain, &env());
    assert!(
        found[0].detail.starts_with("cannot resolve path"),
        "{found:?}"
    );
}

#[test]
fn git_sync_off_says_why() {
    let file = temp_dir("off").join("list.md");
    fs::write(&file, "- [ ] one\n").unwrap();
    let anchored = PathBuf::from(format!("{}#section", file.display()));

    let found = file_checks(&cli(&[]), &config::Config::default(), &anchored, &env());
    assert_eq!(found[0], ok("file", file.display().to_string()));
    assert_eq!(
        found[1],
        ok(
            "git sync",
            "off (no --git-sync, and no git_sync_paths entry covers this file)"
        )
    );

    let config = config::Config {
        git_sync_paths: Some(vec![PathBuf::from("relative/dir")]),
        ..config::Config::default()
    };
    let found = file_checks(&cli(&["--no-git-sync"]), &config, &file, &env());
    assert_eq!(found[1].level, Level::Warning);
    assert!(
        found[1]
            .detail
            .starts_with("off (--no-git-sync); git_sync_paths: relative/dir"),
        "{found:?}"
    );
}

#[test]
fn git_sync_asked_for_outside_a_repository_is_a_warning() {
    let file = temp_dir("not-repo").join("list.md");
    fs::write(&file, "- [ ] one\n").unwrap();
    let found = file_checks(
        &cli(&["--git-sync"]),
        &config::Config::default(),
        &file,
        &env(),
    );
    assert_eq!(
        found[1],
        warning(
            "git sync",
            "off: the file is not in a git repository (asked for by --git-sync)"
        )
    );
}

#[test]
fn git_sync_on_reports_what_the_repository_says() {
    let file = repo_with_checklist("repo");
    let found = file_checks(
        &cli(&["--git-sync"]),
        &config::Config::default(),
        &file,
        &env(),
    );
    let root = file.parent().unwrap();
    assert_eq!(
        named(&found, "git sync"),
        &ok(
            "git sync",
            format!("on (--git-sync); repository {}", root.display())
        )
    );
    assert!(named(&found, "git").detail.starts_with("git "));
    assert_eq!(named(&found, "identity").level, Level::Ok);
    assert_eq!(named(&found, "tracked").level, Level::Ok);
    assert_eq!(named(&found, "push").level, Level::Warning);
    assert_eq!(named(&found, "remote").level, Level::Ok);

    let config = config::Config {
        git_sync_paths: Some(vec![root.to_path_buf()]),
        ..config::Config::default()
    };
    let found = file_checks(&cli(&[]), &config, &file, &env());
    assert_eq!(
        named(&found, "git sync").detail,
        format!(
            "on (git_sync_paths entry {}); repository {}",
            root.display(),
            root.display()
        )
    );
}

fn found_with(change: impl FnOnce(&mut Doctor)) -> Vec<Check> {
    let mut found = Doctor {
        version: "2.43.0".to_string(),
        identity_warning: None,
        tracked: Ok(true),
        push_target: Ok("origin/main".to_string()),
        unpushed: Some(0),
        remote: RemoteCheck::UpToDate,
    };
    change(&mut found);
    git_checks(&found)
}

#[test]
fn git_checks_render_every_finding() {
    let healthy = found_with(|_| {});
    assert_eq!(
        healthy,
        [
            ok("git", "git 2.43.0"),
            ok("identity", "user.name and user.email are set"),
            ok("tracked", "the checklist is tracked in git"),
            ok("push", "origin/main; nothing waiting to be pushed"),
            ok("remote", "no changes on the remote that aren't pulled"),
        ]
    );
    let push = |unpushed| {
        named(&found_with(|f| f.unpushed = unpushed), "push")
            .detail
            .clone()
    };
    assert_eq!(push(Some(1)), "origin/main; 1 commit not pushed yet");
    assert_eq!(push(Some(3)), "origin/main; 3 commits not pushed yet");
    assert_eq!(push(None), "origin/main; unpushed commits unknown");

    let warned = |change: fn(&mut Doctor), name: &str| {
        let found = found_with(change);
        let check = named(&found, name).clone();
        assert_eq!(check.level, Level::Warning, "{check:?}");
        check.detail
    };
    assert!(warned(|f| f.identity_warning = some("no identity"), "identity") == "no identity");
    assert!(warned(|f| f.tracked = Ok(false), "tracked").contains("git add"));
    assert_eq!(
        warned(|f| f.tracked = Err("odd".to_string()), "tracked"),
        "odd"
    );
    assert_eq!(
        warned(|f| f.push_target = Err("no upstream".to_string()), "push"),
        "commits stay local: no upstream"
    );
    assert!(warned(|f| f.remote = RemoteCheck::Unreachable, "remote").contains("couldn't reach"));
    assert!(warned(|f| f.remote = RemoteCheck::Absent, "remote").contains("no longer exists"));
    assert_eq!(
        warned(
            |f| f.remote = RemoteCheck::Ahead("pull first".to_string()),
            "remote"
        ),
        "pull first"
    );
    assert_eq!(
        named(
            &found_with(|f| f.remote = RemoteCheck::NotChecked),
            "remote"
        )
        .level,
        Level::Ok
    );
}

#[test]
fn color_only_on_a_terminal_without_no_color() {
    assert!(use_color(None, true));
    assert!(use_color(Some(""), true));
    assert!(!use_color(Some("1"), true));
    assert!(!use_color(None, false));
}

#[test]
fn the_report_aligns_names_and_counts_warnings() {
    let all_ok = [ok("git", "git 2.43.0"), ok("clipboard", "OSC 52 only")];
    assert_eq!(
        render(&all_ok, false),
        "ok    git        git 2.43.0\nok    clipboard  OSC 52 only\nAll 2 checks ok\n"
    );
    assert_eq!(exit_status(&all_ok), 0);

    let one = [ok("git", "fine"), warning("push", "line\u{1b}[31m")];
    let text = render(&one, false);
    assert!(text.contains("warn  push  line"), "{text:?}");
    assert!(
        !text.contains('\u{1b}'),
        "details are made printable: {text:?}"
    );
    assert!(text.ends_with("1 of 2 checks warns\n"), "{text:?}");
    assert_eq!(exit_status(&one), WARNING_EXIT_STATUS);

    let two = [warning("a", "x"), warning("b", "y")];
    assert!(render(&two, false).ends_with("2 of 2 checks warn\n"));
    let colored = render(&two, true);
    assert!(colored.contains("\x1b[33mwarn\x1b[0m"), "{colored:?}");
    assert!(render(&all_ok, true).contains("\x1b[32mok  \x1b[0m"));
}
