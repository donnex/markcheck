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
    /// Use Nerd Font glyphs instead of plain Unicode symbols (config: nerd_font = true)
    #[arg(long, conflicts_with = "no_nerd_font")]
    pub nerd_font: bool,
    /// Use plain Unicode symbols, overriding `nerd_font = true` in the config (the default)
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
    /// Wait until SECONDS pass with no further change before git sync commits,
    /// so a burst of toggles becomes one commit; 0 commits every change at once
    /// (default: 10; config: git_sync_delay)
    #[arg(long, value_name = "SECONDS")]
    pub git_sync_delay: Option<u64>,
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
            // Plain Unicode renders on every terminal; Nerd Font glyphs only
            // with a patched font, so they are the opt-in (#83).
            nerd_font: resolve_flag(cli.nerd_font, cli.no_nerd_font, config.nerd_font, false),
            mouse: resolve_flag(cli.mouse, cli.no_mouse, config.mouse, true),
            primary: resolve_flag(cli.primary, cli.no_primary, config.primary, false),
            auto_copy: resolve_flag(cli.auto_copy, cli.no_auto_copy, config.auto_copy, false),
            color: cli.color.or(config.color).unwrap_or_default(),
        }
    }
}

/// How long git sync waits for the changes to stop before committing: the
/// flag, then the config, then [`DEFAULT_GIT_SYNC_DELAY`].
pub fn git_sync_delay(cli: &Cli, config: &config::Config) -> std::time::Duration {
    std::time::Duration::from_secs(
        cli.git_sync_delay
            .or(config.git_sync_delay)
            .unwrap_or(DEFAULT_GIT_SYNC_DELAY),
    )
}

/// Seconds: long enough to fold a burst of ticks (or a tick and its undo)
/// into one commit, short enough that a pause publishes the work.
pub const DEFAULT_GIT_SYNC_DELAY: u64 = 10;

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
