//! The command line: clap's `Cli`, and the CLI-plus-config precedence that
//! resolves it into `Settings`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::Parser;

use crate::config;
use crate::model::{ClipboardChoice, ColorChoice, LineNumber};

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
    #[arg(
        required_unless_present_any = ["new", "generate_man", "generate_completions"],
        value_hint = clap::ValueHint::FilePath
    )]
    pub file: Option<PathBuf>,
    /// Print the man page (roff) and exit; for packaging
    #[arg(long, hide = true, exclusive = true)]
    pub generate_man: bool,
    /// Print shell completions for SHELL and exit; for packaging
    #[arg(long, value_name = "SHELL", hide = true, exclusive = true)]
    pub generate_completions: Option<clap_complete::Shell>,
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
    /// Where y copies to: auto uses the system clipboard and falls back to
    /// OSC 52 (both over SSH); system or osc52 use only that one
    /// (config: clipboard = "...")
    #[arg(long, value_name = "WHERE", value_enum)]
    pub clipboard: Option<ClipboardChoice>,
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
    /// (default: 10, at most 86400, a day; config: git_sync_delay)
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(..=config::MAX_GIT_SYNC_DELAY)
    )]
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

/// Takes a vim-style `+N` (open at line N) out of the arguments, so clap
/// sees only the rest. Only an argument that is `+` and digits counts.
pub fn take_start_line(args: Vec<OsString>) -> (Vec<OsString>, Option<LineNumber>) {
    let mut line = None;
    let rest = args
        .into_iter()
        .enumerate()
        .filter(|(index, arg)| {
            let number = arg
                .to_str()
                .and_then(|arg| arg.strip_prefix('+'))
                .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|digits| digits.parse().ok());
            match number {
                Some(number) if *index > 0 => {
                    line = Some(number);
                    false
                }
                _ => true,
            }
        })
        .map(|(_, arg)| arg)
        .collect();
    (rest, line)
}

/// `file.md#anchor` as the file and the anchor, when the path as given
/// doesn't exist but the part before its last `#` does, so a real file name
/// with a `#` in it still opens as itself.
pub fn split_anchor(path: &Path) -> Option<(PathBuf, String)> {
    if path.exists() {
        return None;
    }
    let text = path.to_str()?;
    let (file, anchor) = text.rsplit_once('#')?;
    let file = PathBuf::from(file);
    (!anchor.is_empty() && file.exists()).then(|| (file, anchor.to_string()))
}

/// The man page (#90): the flags from the clap definition, the config file's
/// location, and a pointer to the README for keybindings and the Markdown
/// handling, rather than another copy of them to keep in step.
pub fn man_page() -> std::io::Result<Vec<u8>> {
    use clap::CommandFactory;
    let mut out = Vec::new();
    clap_mangen::Man::new(Cli::command()).render(&mut out)?;
    out.extend_from_slice(
        br#".SH FILES
.TP
\fI$XDG_CONFIG_HOME/markcheck/config.toml\fR
Defaults for the flags, one key per flag (for example \fBnerd_font = true\fR).
Without \fBXDG_CONFIG_HOME\fR, \fI~/.config/markcheck/config.toml\fR.
\fB\-\-config\fR reads another file; \fB\-\-no\-config\fR skips it.
.SH SEE ALSO
The keybindings (also shown by \fB?\fR in the app) and how each piece of
Markdown is handled are described in the README:
https://github.com/donnex/markcheck#readme
"#,
    );
    Ok(out)
}

/// Shell completions for `shell` (#90).
pub fn completions(shell: clap_complete::Shell) -> Vec<u8> {
    use clap::CommandFactory;
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "markcheck", &mut out);
    out
}

/// Where copies go: the flag, then the config, then `auto`.
pub fn clipboard_choice(cli: &Cli, config: &config::Config) -> ClipboardChoice {
    cli.clipboard.or(config.clipboard).unwrap_or_default()
}

/// Whether this runs inside an SSH session, from the variables sshd sets.
pub fn is_remote_session(ssh_connection: Option<&str>, ssh_tty: Option<&str>) -> bool {
    [ssh_connection, ssh_tty]
        .iter()
        .any(|var| var.is_some_and(|value| !value.is_empty()))
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
