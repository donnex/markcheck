use super::{Cli, ColorChoice, Settings, config, git_sync_delay, resolve_flag};
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
