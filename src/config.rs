use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::model::{ClipboardChoice, ColorChoice, IconChoice};

/// Defaults for the CLI flags, from a TOML file. Keys use the positive
/// sense (`mouse`). Unknown keys are ignored here and reported by
/// `load_config`, so a typo surfaces without a newer release's settings
/// stopping an older one from starting.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Config {
    /// Icon set, like `--icons`: `auto`, `unicode`, `nerd` or `ascii`.
    pub icons: Option<IconChoice>,
    /// Deprecated alias for `icons = "nerd"` (on) or `icons = "auto"` (off);
    /// `icons` wins when both are set.
    pub nerd_font: Option<bool>,
    pub mouse: Option<bool>,
    pub primary: Option<bool>,
    pub auto_copy: Option<bool>,
    /// Palette override, like `--color`: `auto`, `truecolor`, `256` or `16`.
    pub color: Option<ColorChoice>,
    /// Path prefixes under which git-sync activates, like `--git-sync`.
    pub git_sync_paths: Option<Vec<PathBuf>>,
    /// Seconds git sync waits after the last change before committing, like
    /// `--git-sync-delay`.
    pub git_sync_delay: Option<u64>,
    /// Where copies go, like `--clipboard`: `auto`, `system` or `osc52`.
    pub clipboard: Option<ClipboardChoice>,
    /// Show the checklist and its progress in the terminal title (default on).
    pub terminal_title: Option<bool>,
    /// Print where the run stopped after quitting (default on).
    pub exit_summary: Option<bool>,
}

/// The longest `git_sync_delay` accepted, in seconds: a day. Anything
/// longer is a typo or a stand-in for "only on quit", which the quit's own
/// sync already covers.
pub const MAX_GIT_SYNC_DELAY: u64 = 86_400;

/// Every key `Config` reads; anything else in the file is reported as
/// unknown.
const KNOWN_KEYS: &[&str] = &[
    "icons",
    "nerd_font",
    "mouse",
    "primary",
    "auto_copy",
    "color",
    "git_sync_paths",
    "git_sync_delay",
    "clipboard",
    "terminal_title",
    "exit_summary",
];

/// A parsed config plus the top-level keys it ignored, sorted.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub config: Config,
    pub unknown_keys: Vec<String>,
}

/// Where the config comes from for this run: `--no-config` skips it,
/// `--config PATH` names one (which must then exist), otherwise the
/// default location from `config_path`, where a missing file is fine.
pub fn load_for_run(
    explicit: Option<&Path>,
    no_config: bool,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> anyhow::Result<Loaded> {
    if no_config {
        return Ok(Loaded::default());
    }
    if let Some(path) = explicit {
        if let Err(err) = fs::metadata(path)
            && err.kind() == io::ErrorKind::NotFound
        {
            anyhow::bail!("config file not found: {}", path.display());
        }
        return load_config(path);
    }
    match config_path(xdg_config_home, home) {
        Some(path) => load_config(&path),
        None => Ok(Loaded::default()),
    }
}

/// The sticky startup warning for keys the config file has but this
/// release doesn't read.
pub fn unknown_keys_warning(keys: &[String]) -> Option<String> {
    let (first, rest) = keys.split_first()?;
    let listed = rest
        .iter()
        .fold(format!("`{first}`"), |acc, key| format!("{acc}, `{key}`"));
    let noun = if rest.is_empty() { "key" } else { "keys" };
    Some(format!(
        "Config: unknown {noun} {listed} ignored — a typo, or a setting from a newer markcheck?"
    ))
}

/// The startup note for a config that still sets the deprecated
/// `nerd_font` key: the `icons` value that replaces it, or that it is
/// ignored when `icons` is set too.
pub fn deprecated_keys_note(config: &Config) -> Option<String> {
    let nerd_font = config.nerd_font?;
    if config.icons.is_some() {
        return Some(
            "Config: `nerd_font` is deprecated and ignored, since `icons` is set — remove it"
                .to_string(),
        );
    }
    let replacement = if nerd_font { "nerd" } else { "auto" };
    Some(format!(
        "Config: `nerd_font` is deprecated — use `icons = \"{replacement}\"` instead"
    ))
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`. A value that is
/// empty or not an absolute path is ignored, as the XDG Base Directory
/// specification requires, so the config never depends on the current
/// directory. Takes the values as arguments so tests don't touch the
/// environment.
pub fn config_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let absolute = |v: &&str| Path::new(v).is_absolute();
    let base = xdg_config_home
        .filter(absolute)
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(absolute)
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("markcheck").join("config.toml"))
}

/// Loads and parses the config file at `path`. A missing file is not an
/// error — it just means no overrides — but a file that exists and fails
/// to parse, or gives a known key the wrong type, is: the user asked for
/// these defaults, so silently falling back would hide the mistake. An
/// unknown key is only reported, since it may belong to a newer release.
pub fn load_config(path: &Path) -> anyhow::Result<Loaded> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("cannot read config file: {}", path.display()));
        }
    };
    let invalid = || {
        format!(
            "invalid config file: {} (--no-config skips it for one run)",
            path.display()
        )
    };
    let table: toml::Table = toml::from_str(&contents).with_context(invalid)?;
    let config: Config = toml::from_str(&contents).with_context(invalid)?;
    if let Some(delay) = config.git_sync_delay
        && delay > MAX_GIT_SYNC_DELAY
    {
        return Err(anyhow::anyhow!(
            "git_sync_delay must be at most {MAX_GIT_SYNC_DELAY} seconds (a day), not {delay}"
        ))
        .with_context(invalid);
    }
    let mut unknown_keys: Vec<String> = table
        .keys()
        .filter(|key| !KNOWN_KEYS.contains(&key.as_str()))
        .cloned()
        .collect();
    unknown_keys.sort();
    Ok(Loaded {
        config,
        unknown_keys,
    })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
