//! The command line: clap's `Cli`, and the CLI-plus-config precedence that
//! resolves it into `Settings`.

use std::path::PathBuf;

use clap::Parser;

use crate::config;
use crate::model::ColorChoice;

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
    version = VERSION,
    after_help = "Defaults for the flags above can be set in the config file \
                  $XDG_CONFIG_HOME/markcheck/config.toml \
                  (or ~/.config/markcheck/config.toml when XDG_CONFIG_HOME is unset). \
                  --config PATH reads another file instead; --no-config skips it."
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
    /// Color palette: auto detects the terminal's support; truecolor, 256 or 16
    /// force one (config: color = "...")
    #[arg(long, value_name = "PALETTE", value_enum)]
    pub color: Option<ColorChoice>,
    /// Commit and push this file's changes after every toggle, when it's inside a
    /// git repo (config: git_sync_paths matches this file's path; --no-git-sync
    /// turns it off)
    #[arg(long, conflicts_with = "no_git_sync")]
    pub git_sync: bool,
    /// Never commit or push, even when git_sync_paths matches this file's path
    #[arg(long)]
    pub no_git_sync: bool,
    /// Read the config from PATH instead of the default location (PATH must exist)
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    pub config: Option<PathBuf>,
    /// Ignore the config file for this run: built-in defaults plus any flags passed
    #[arg(long)]
    pub no_config: bool,
}

/// The four boolean settings and the palette choice, resolved in one place
/// so the wiring can be tested through real argv.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub nerd_font: bool,
    pub mouse: bool,
    pub primary: bool,
    pub auto_copy: bool,
    pub color: ColorChoice,
}

impl Settings {
    pub fn resolve(cli: &Cli, config: &config::Config) -> Self {
        Settings {
            nerd_font: resolve_flag(cli.nerd_font, cli.no_nerd_font, config.nerd_font, true),
            mouse: resolve_flag(cli.mouse, cli.no_mouse, config.mouse, true),
            primary: resolve_flag(cli.primary, cli.no_primary, config.primary, false),
            auto_copy: resolve_flag(cli.auto_copy, cli.no_auto_copy, config.auto_copy, false),
            color: cli.color.or(config.color).unwrap_or_default(),
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
#[path = "cli_tests.rs"]
mod tests;
