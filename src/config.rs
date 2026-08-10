use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// Default values for the CLI flags, loaded from a TOML file so
/// users don't have to pass the same flags on every invocation. Keys use
/// the flag's positive sense (`nerd_font`, not `no_nerd_font`) even though
/// two of the CLI flags are negations — merging a config value with its
/// flag happens in `main.rs`, not here. Unknown keys are a hard error
/// (`deny_unknown_fields`) rather than silently ignored, so a typo in the
/// file surfaces instead of silently not doing what the user asked.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub nerd_font: Option<bool>,
    pub mouse: Option<bool>,
    pub primary: Option<bool>,
    pub auto_copy: Option<bool>,
    /// Path prefixes that auto-activate git-sync: a file whose
    /// canonical path starts with one of these commits and pushes after
    /// every write-back, the same as passing `--git-sync` for that one
    /// invocation. The odd one out among these fields — a list rather than
    /// a single CLI-flag default, since `--git-sync` is a blanket override
    /// rather than a per-key default (main.rs ORs the two together).
    pub git_sync_paths: Option<Vec<PathBuf>>,
}

/// Resolves the config file path from `XDG_CONFIG_HOME` (falling back to
/// `$HOME/.config`), pure and testable — `main.rs` passes the real
/// environment values in rather than this function reading them, so tests
/// don't need to mutate global process state. `None` only when neither
/// variable is set, in which case the caller proceeds with built-in
/// defaults.
pub fn config_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("markcheck").join("config.toml"))
}

/// Loads and parses the config file at `path`. A missing file is not an
/// error — it just means no overrides — but a file that exists and fails
/// to parse is: the user asked for these defaults, so silently falling
/// back would hide a typo rather than surface it.
pub fn load_config(path: &Path) -> anyhow::Result<Config> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("cannot read config file: {}", path.display()));
        }
    };
    toml::from_str(&contents).with_context(|| format!("invalid config file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_temp_path() -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "markcheck-config-test-{}-{unique}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn config_path_prefers_xdg_config_home() {
        let path = config_path(Some("/xdg"), Some("/home/user")).unwrap();
        assert_eq!(path, PathBuf::from("/xdg/markcheck/config.toml"));
    }

    #[test]
    fn config_path_falls_back_to_home_dot_config() {
        let path = config_path(None, Some("/home/user")).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/user/.config/markcheck/config.toml")
        );
    }

    #[test]
    fn config_path_ignores_empty_xdg_config_home() {
        let path = config_path(Some(""), Some("/home/user")).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/user/.config/markcheck/config.toml")
        );
    }

    #[test]
    fn config_path_none_when_neither_set() {
        assert_eq!(config_path(None, None), None);
    }

    #[test]
    fn config_path_none_when_home_is_empty_and_xdg_unset() {
        // An empty $HOME (a broken/minimal environment, or a service
        // manager that clears it) must be treated the same as an absent
        // one, not produce a relative "./.config/..." path.
        assert_eq!(config_path(None, Some("")), None);
    }

    #[test]
    fn load_config_missing_file_returns_defaults() {
        let path = unique_temp_path();
        assert_eq!(load_config(&path).unwrap(), Config::default());
    }

    #[test]
    fn load_config_parses_all_fields() {
        let path = unique_temp_path();
        fs::write(
            &path,
            "nerd_font = false\nmouse = false\nprimary = true\nauto_copy = true\n\
             git_sync_paths = [\"/home/user/checklists\"]\n",
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config,
            Config {
                nerd_font: Some(false),
                mouse: Some(false),
                primary: Some(true),
                auto_copy: Some(true),
                git_sync_paths: Some(vec![PathBuf::from("/home/user/checklists")]),
            }
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_config_partial_file_leaves_other_fields_none() {
        let path = unique_temp_path();
        fs::write(&path, "auto_copy = true\n").unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config,
            Config {
                auto_copy: Some(true),
                ..Config::default()
            }
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_config_malformed_toml_errors() {
        let path = unique_temp_path();
        fs::write(&path, "this is not valid toml =====").unwrap();

        assert!(load_config(&path).is_err());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_config_unknown_key_errors() {
        // A typo'd key (e.g. `nerdfont` for `nerd_font`) must surface as an
        // error rather than silently doing nothing.
        let path = unique_temp_path();
        fs::write(&path, "nerdfont = true\n").unwrap();

        assert!(load_config(&path).is_err());

        fs::remove_file(&path).ok();
    }
}
