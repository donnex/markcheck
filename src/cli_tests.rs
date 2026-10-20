use super::{
    Cli, ColorChoice, Settings, clipboard_choice, config, git_sync_delay, is_remote_session,
    resolve_flag,
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
        color: None,
        git_sync_paths: None,
        git_sync_delay: None,
        clipboard: None,
        terminal_title: None,
        exit_summary: None,
    }
}

#[test]
fn settings_resolve_the_built_in_defaults_with_no_flags_and_no_config() {
    assert_eq!(
        settings_from(&[], config::Config::default()),
        Settings {
            nerd_font: false,
            mouse: true,
            primary: false,
            auto_copy: false,
            color: ColorChoice::Auto,
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
            color: ColorChoice::Auto,
        }
    );
    assert_eq!(
        settings_from(&[], config_all(false)),
        Settings {
            nerd_font: false,
            mouse: false,
            primary: false,
            auto_copy: false,
            color: ColorChoice::Auto,
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
            color: ColorChoice::Auto,
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
            color: ColorChoice::Auto,
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
fn git_sync_delay_takes_the_flag_then_the_config_then_ten_seconds() {
    use std::time::Duration;
    let parse = |flags: &[&str]| {
        let mut argv = vec!["markcheck", "checklist.md"];
        argv.extend_from_slice(flags);
        Cli::parse_from(argv)
    };
    let mut config = config_all(false);
    assert_eq!(
        git_sync_delay(&parse(&[]), &config),
        Duration::from_secs(10)
    );
    config.git_sync_delay = Some(30);
    assert_eq!(
        git_sync_delay(&parse(&[]), &config),
        Duration::from_secs(30)
    );
    assert_eq!(
        git_sync_delay(&parse(&["--git-sync-delay", "0"]), &config),
        Duration::ZERO
    );
}

#[test]
fn clipboard_choice_takes_the_flag_then_the_config_then_auto() {
    use crate::model::ClipboardChoice;
    let parse = |flags: &[&str]| {
        let mut argv = vec!["markcheck", "checklist.md"];
        argv.extend_from_slice(flags);
        Cli::parse_from(argv)
    };
    let mut config = config_all(false);
    assert_eq!(
        clipboard_choice(&parse(&[]), &config),
        ClipboardChoice::Auto
    );
    config.clipboard = Some(ClipboardChoice::System);
    assert_eq!(
        clipboard_choice(&parse(&[]), &config),
        ClipboardChoice::System
    );
    assert_eq!(
        clipboard_choice(&parse(&["--clipboard", "osc52"]), &config),
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
