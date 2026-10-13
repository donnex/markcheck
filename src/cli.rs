//! The command line: clap's `Cli`, and the CLI-plus-config precedence that
//! resolves it into `Settings`.

use std::path::PathBuf;

use clap::Parser;

use crate::config;

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
pub struct Cli {
    /// Path to the Markdown file (omit when using --new)
    #[arg(required_unless_present = "new")]
    pub file: Option<PathBuf>,
    /// Create a new starter checklist at PATH, then open it (PATH must not
    /// already exist and must end in .md)
    #[arg(long, value_name = "PATH", conflicts_with = "file")]
    pub new: Option<PathBuf>,
    /// Use Nerd Font glyphs, overriding `nerd_font = false` in the config
    #[arg(long, conflicts_with = "no_nerd_font")]
    pub nerd_font: bool,
    /// Use plain Unicode symbols instead of Nerd Font glyphs (config: nerd_font = false)
    #[arg(long)]
    pub no_nerd_font: bool,
    /// Enable mouse support, overriding `mouse = false` in the config
    #[arg(long, conflicts_with = "no_mouse")]
    pub mouse: bool,
    /// Disable mouse support (terminal text selection works without Shift) (config: mouse = false)
    #[arg(long)]
    pub no_mouse: bool,
    /// Also copy to the X11 PRIMARY selection (middle-click paste) (config: primary = true)
    #[arg(long)]
    pub primary: bool,
    /// Don't copy to the X11 PRIMARY selection, overriding `primary = true` in the config
    #[arg(long, conflicts_with = "primary")]
    pub no_primary: bool,
    /// Auto-copy an item's code to the clipboard when navigating to it (config: auto_copy = true)
    #[arg(long)]
    pub auto_copy: bool,
    /// Don't auto-copy on navigation, overriding `auto_copy = true` in the config
    #[arg(long, conflicts_with = "auto_copy")]
    pub no_auto_copy: bool,
    /// Commit and push this file's changes after every toggle, when it's inside a
    /// git repo (config: git_sync_paths matches this file's path)
    #[arg(long)]
    pub git_sync: bool,
}

/// The four boolean settings, resolved in one place so the wiring can be
/// tested through real argv.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub nerd_font: bool,
    pub mouse: bool,
    pub primary: bool,
    pub auto_copy: bool,
}

impl Settings {
    pub fn resolve(cli: &Cli, config: &config::Config) -> Self {
        Settings {
            nerd_font: resolve_flag(cli.nerd_font, cli.no_nerd_font, config.nerd_font, true),
            mouse: resolve_flag(cli.mouse, cli.no_mouse, config.mouse, true),
            primary: resolve_flag(cli.primary, cli.no_primary, config.primary, false),
            auto_copy: resolve_flag(cli.auto_copy, cli.no_auto_copy, config.auto_copy, false),
        }
    }
}

/// Resolves one boolean setting: a passed flag wins, then the config, then
/// the default. Pass `false` for a flag a setting doesn't have. clap's
/// `conflicts_with` keeps both from being set.
fn resolve_flag(cli_on: bool, cli_off: bool, config: Option<bool>, default: bool) -> bool {
    if cli_on {
        return true;
    }
    if cli_off {
        return false;
    }
    config.unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::{Cli, Settings, config, resolve_flag};
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
            git_sync_paths: None,
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
            }
        );
        assert_eq!(
            settings_from(&[], config_all(false)),
            Settings {
                nerd_font: false,
                mouse: false,
                primary: false,
                auto_copy: false,
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
        ] {
            let result = Cli::try_parse_from(["markcheck", "checklist.md", pair[0], pair[1]]);
            assert!(result.is_err(), "{pair:?} must be rejected together");
        }
    }
}
