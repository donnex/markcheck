use std::time::Duration;

use super::{Cli, ColorChoice, Settings, config, is_remote_session, resolve_flag};
use crate::model::{ClipboardChoice, IconChoice, Theme};
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
        icons: None,
        nerd_font: Some(value),
        mouse: Some(value),
        primary: Some(value),
        auto_copy: Some(value),
        color: None,
        theme: None,
        git_sync_paths: None,
        git_sync_delay: None,
        clipboard: None,
        terminal_title: Some(value),
        exit_summary: Some(value),
    }
}

/// The settings with every boolean set to `value` and the rest at their
/// defaults.
fn settings_all(value: bool) -> Settings {
    Settings {
        icons: IconChoice::from_nerd_font(value),
        mouse: value,
        primary: value,
        auto_copy: value,
        terminal_title: value,
        exit_summary: value,
        color: ColorChoice::Auto,
        theme: Theme::Dark,
        clipboard: ClipboardChoice::Auto,
        git_sync_delay: Duration::from_secs(10),
    }
}

#[test]
fn settings_resolve_the_built_in_defaults_with_no_flags_and_no_config() {
    assert_eq!(
        settings_from(&[], config::Config::default()),
        Settings {
            icons: IconChoice::Auto,
            mouse: true,
            primary: false,
            auto_copy: false,
            terminal_title: true,
            exit_summary: true,
            ..settings_all(false)
        }
    );
}

#[test]
fn settings_take_the_config_when_no_flag_is_passed() {
    assert_eq!(settings_from(&[], config_all(true)), settings_all(true));
    assert_eq!(settings_from(&[], config_all(false)), settings_all(false));
}

#[test]
fn every_setting_can_be_forced_on_over_a_config_that_disables_it() {
    // `nerd_font` and `mouse` need a positive flag to override the config.
    let settings = settings_from(
        &[
            "--nerd-font",
            "--mouse",
            "--primary",
            "--auto-copy",
            "--terminal-title",
            "--exit-summary",
        ],
        config_all(false),
    );
    assert_eq!(
        settings,
        settings_all(true),
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
            "--no-terminal-title",
            "--no-exit-summary",
        ],
        config_all(true),
    );
    assert_eq!(
        settings,
        settings_all(false),
        "a passed flag must beat the config in the negative direction"
    );
}

#[test]
fn the_icons_flag_and_key_pick_each_set() {
    for (name, choice) in [
        ("auto", IconChoice::Auto),
        ("unicode", IconChoice::Unicode),
        ("nerd", IconChoice::Nerd),
        ("ascii", IconChoice::Ascii),
    ] {
        let flag = format!("--icons={name}");
        assert_eq!(
            settings_from(&[&flag], config::Config::default()).icons,
            choice
        );
        let config = config::Config {
            icons: Some(choice),
            ..config::Config::default()
        };
        assert_eq!(settings_from(&[], config).icons, choice);
    }
    assert!(Cli::try_parse_from(["markcheck", "checklist.md", "--icons", "emoji"]).is_err());
}

#[test]
fn icons_wins_over_the_deprecated_nerd_font_alias_and_a_flag_over_the_config() {
    let config = |icons, nerd_font| config::Config {
        icons,
        nerd_font,
        ..config::Config::default()
    };
    let cases: [(&[&str], config::Config, IconChoice); 8] = [
        // `icons` beats `nerd_font` in the same layer, either way round.
        (
            &[],
            config(Some(IconChoice::Ascii), Some(true)),
            IconChoice::Ascii,
        ),
        (
            &[],
            config(Some(IconChoice::Nerd), Some(false)),
            IconChoice::Nerd,
        ),
        (
            &["--icons", "unicode", "--nerd-font"],
            config(None, None),
            IconChoice::Unicode,
        ),
        // The alias alone still works, in the config and as flags.
        (&[], config(None, Some(true)), IconChoice::Nerd),
        (&["--nerd-font"], config(None, None), IconChoice::Nerd),
        // A flag, even the deprecated one, beats the config's `icons`.
        (
            &["--nerd-font"],
            config(Some(IconChoice::Ascii), None),
            IconChoice::Nerd,
        ),
        (
            &["--no-nerd-font"],
            config(Some(IconChoice::Nerd), None),
            IconChoice::Auto,
        ),
        (
            &["--icons", "ascii"],
            config(Some(IconChoice::Nerd), Some(true)),
            IconChoice::Ascii,
        ),
    ];
    for (flags, config, expected) in cases {
        let shown = format!("{flags:?} over {config:?}");
        assert_eq!(settings_from(flags, config).icons, expected, "{shown}");
    }
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
        ["--terminal-title", "--no-terminal-title"],
        ["--exit-summary", "--no-exit-summary"],
        ["--git-sync", "--no-git-sync"],
        ["--config=other.toml", "--no-config"],
    ] {
        let result = Cli::try_parse_from(["markcheck", "checklist.md", pair[0], pair[1]]);
        assert!(result.is_err(), "{pair:?} must be rejected together");
    }
}

#[test]
fn git_sync_flags_parse_into_their_own_fields() {
    let parse = |flags: &[&str]| {
        let mut argv = vec!["markcheck", "checklist.md"];
        argv.extend_from_slice(flags);
        let cli = Cli::parse_from(argv);
        (cli.git_sync, cli.no_git_sync)
    };
    assert_eq!(parse(&[]), (false, false));
    assert_eq!(parse(&["--git-sync"]), (true, false));
    assert_eq!(parse(&["--no-git-sync"]), (false, true));
}

#[test]
fn config_flags_parse_into_their_own_fields() {
    let parse = |flags: &[&str]| {
        let mut argv = vec!["markcheck", "checklist.md"];
        argv.extend_from_slice(flags);
        let cli = Cli::parse_from(argv);
        (cli.config, cli.no_config)
    };
    assert_eq!(parse(&[]), (None, false));
    assert_eq!(
        parse(&["--config", "other.toml"]),
        (Some(std::path::PathBuf::from("other.toml")), false)
    );
    assert_eq!(parse(&["--no-config"]), (None, true));
}

#[test]
fn color_flag_beats_the_config_which_beats_auto() {
    let config = |color| config::Config {
        color,
        ..config::Config::default()
    };
    assert_eq!(settings_from(&[], config(None)).color, ColorChoice::Auto);
    assert_eq!(
        settings_from(&[], config(Some(ColorChoice::Basic))).color,
        ColorChoice::Basic
    );
    for (flag, expected) in [
        ("auto", ColorChoice::Auto),
        ("truecolor", ColorChoice::TrueColor),
        ("256", ColorChoice::Palette256),
        ("16", ColorChoice::Basic),
    ] {
        assert_eq!(
            settings_from(&["--color", flag], config(Some(ColorChoice::Basic))).color,
            expected,
            "--color {flag} must win over the config"
        );
    }
    assert!(Cli::try_parse_from(["markcheck", "checklist.md", "--color", "rainbow"]).is_err());
}

#[test]
fn theme_flag_beats_the_config_which_beats_dark() {
    let config = |theme| config::Config {
        theme,
        ..config::Config::default()
    };
    assert_eq!(settings_from(&[], config(None)).theme, Theme::Dark);
    assert_eq!(
        settings_from(&[], config(Some(Theme::Light))).theme,
        Theme::Light
    );
    assert_eq!(
        settings_from(&["--theme", "dark"], config(Some(Theme::Light))).theme,
        Theme::Dark
    );
    assert_eq!(
        settings_from(&["--theme", "light"], config(Some(Theme::Dark))).theme,
        Theme::Light
    );
    assert!(Cli::try_parse_from(["markcheck", "checklist.md", "--theme", "auto"]).is_err());
}

#[test]
fn git_sync_delay_takes_the_flag_then_the_config_then_ten_seconds() {
    assert_eq!(
        settings_from(&[], config_all(false)).git_sync_delay,
        Duration::from_secs(10)
    );
    let delay = |flags: &[&str]| {
        let config = config::Config {
            git_sync_delay: Some(30),
            ..config_all(false)
        };
        settings_from(flags, config).git_sync_delay
    };
    assert_eq!(delay(&[]), Duration::from_secs(30));
    assert_eq!(delay(&["--git-sync-delay", "0"]), Duration::ZERO);
    assert_eq!(
        delay(&["--git-sync-delay", "86400"]),
        Duration::from_secs(86_400)
    );
}

#[test]
fn git_sync_delay_over_a_day_is_rejected() {
    for value in ["86401", "18446744073709551615"] {
        let Err(error) =
            Cli::try_parse_from(["markcheck", "checklist.md", "--git-sync-delay", value])
        else {
            panic!("{value} must be rejected");
        };
        let error = error.to_string();
        assert!(error.contains("--git-sync-delay"), "{value}: {error}");
        assert!(error.contains("0..=86400"), "{value}: {error}");
    }
}

#[test]
fn clipboard_choice_takes_the_flag_then_the_config_then_auto() {
    let config = |clipboard| config::Config {
        clipboard,
        ..config::Config::default()
    };
    let choice = |flags: &[&str], clipboard| settings_from(flags, config(clipboard)).clipboard;
    assert_eq!(choice(&[], None), ClipboardChoice::Auto);
    assert_eq!(
        choice(&[], Some(ClipboardChoice::System)),
        ClipboardChoice::System
    );
    assert_eq!(
        choice(&["--clipboard", "osc52"], Some(ClipboardChoice::System)),
        ClipboardChoice::Osc52
    );
}

#[test]
fn an_ssh_session_is_recognised_from_either_variable() {
    assert!(!is_remote_session(None, None));
    assert!(!is_remote_session(Some(""), None));
    assert!(is_remote_session(
        Some("203.0.113.10 50000 203.0.113.20 22"),
        None
    ));
    assert!(is_remote_session(None, Some("/dev/pts/3")));
}

#[test]
fn a_plus_line_argument_is_taken_out_before_clap() {
    use std::ffi::OsString;
    let args: Vec<OsString> = ["markcheck", "+42", "runbook.md", "+x", "+"]
        .iter()
        .map(OsString::from)
        .collect();
    let (rest, line) = super::take_start_line(args);
    assert_eq!(line, Some(42));
    assert_eq!(rest, vec!["markcheck", "runbook.md", "+x", "+"]);
    let (_, none) = super::take_start_line(vec![OsString::from("+7")]);
    assert_eq!(none, None, "the program name is never a line");
}

#[test]
fn a_hash_suffix_is_an_anchor_only_when_the_file_before_it_exists() {
    let dir = crate::test_support::unique_temp_path("cli", "anchor", None);
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("runbook.md");
    std::fs::write(&file, "- [ ] x\n").unwrap();
    let hashed = dir.join("notes#1.md");
    std::fs::write(&hashed, "- [ ] y\n").unwrap();

    assert_eq!(
        super::split_anchor(&dir.join("runbook.md#rollback")),
        Some((file.clone(), "rollback".to_string()))
    );
    assert_eq!(
        super::split_anchor(&hashed),
        None,
        "a real file with # opens as itself"
    );
    assert_eq!(super::split_anchor(&dir.join("runbook.md#")), None);
    assert_eq!(super::split_anchor(&dir.join("missing.md#x")), None);
}

#[test]
fn the_man_page_covers_the_flags_the_config_file_and_points_to_the_readme() {
    let page = String::from_utf8(super::man_page().unwrap()).unwrap();
    assert!(page.contains(".TH markcheck 1"));
    assert!(
        page.contains(r"\-\-git\-sync\-delay"),
        "every flag is listed"
    );
    assert!(
        !page.contains("generate"),
        "the packaging flags stay hidden"
    );
    assert!(page.contains(".SH FILES") && page.contains("config.toml"));
    assert!(page.contains("github.com/donnex/markcheck#readme"));
}

#[test]
fn the_man_page_lists_the_environment_and_the_exit_status() {
    let page = String::from_utf8(super::man_page().unwrap()).unwrap();
    let environment = page.split(".SH ENVIRONMENT").nth(1).expect("section");
    for var in [
        "NO_COLOR",
        "COLORTERM",
        "VISUAL",
        "EDITOR",
        "BROWSER",
        "WAYLAND_DISPLAY",
        "SSH_CONNECTION",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ] {
        assert!(environment.contains(var), "{var}");
    }
    assert!(page.contains(".SH \"EXIT STATUS\""));
}

#[test]
fn the_help_names_the_start_targets_and_the_git_sync_delay() {
    use clap::CommandFactory;
    let help = super::Cli::command().render_help().to_string();
    assert!(help.contains("FILE#anchor") && help.contains("+N before FILE"));
    assert!(!help.contains("after every toggle"), "{help}");
    let git_sync = help.split("--git-sync ").nth(1).unwrap();
    assert!(git_sync.contains("--git-sync-delay"), "{git_sync}");
}

#[test]
fn completions_are_generated_for_each_shell() {
    for shell in [
        clap_complete::Shell::Bash,
        clap_complete::Shell::Zsh,
        clap_complete::Shell::Fish,
    ] {
        let script = String::from_utf8(super::completions(shell)).unwrap();
        assert!(script.contains("markcheck"), "{shell}");
        assert!(script.contains("git-sync-delay"), "{shell}");
    }
}

#[test]
fn the_packaging_flags_need_no_file_but_take_nothing_else() {
    assert!(Cli::try_parse_from(["markcheck", "--generate-man"]).is_ok());
    assert!(Cli::try_parse_from(["markcheck", "--generate-completions", "zsh"]).is_ok());
    assert!(Cli::try_parse_from(["markcheck", "--generate-man", "x.md"]).is_err());
}
