use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

/// Defaults for the CLI flags, from a TOML file. Keys use the positive
/// sense (`nerd_font`). Unknown keys are ignored here and reported by
/// `load_config`, so a typo surfaces without a newer release's settings
/// stopping an older one from starting.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub struct Config {
    pub nerd_font: Option<bool>,
    pub mouse: Option<bool>,
    pub primary: Option<bool>,
    pub auto_copy: Option<bool>,
    /// Path prefixes under which git-sync activates, like `--git-sync`.
    pub git_sync_paths: Option<Vec<PathBuf>>,
}

/// Every key `Config` reads; anything else in the file is reported as
/// unknown.
const KNOWN_KEYS: &[&str] = &[
    "nerd_font",
    "mouse",
    "primary",
    "auto_copy",
    "git_sync_paths",
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

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`. Takes the values
/// as arguments so tests don't touch the environment.
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
mod tests {
    use super::*;

    fn unique_temp_path() -> PathBuf {
        crate::test_support::unique_temp_path("config", "", Some("toml"))
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
        assert_eq!(load_config(&path).unwrap(), Loaded::default());
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

        let loaded = load_config(&path).unwrap();
        assert!(
            loaded.unknown_keys.is_empty(),
            "every Config field must be in KNOWN_KEYS: {:?}",
            loaded.unknown_keys
        );
        assert_eq!(
            loaded.config,
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

        let config = load_config(&path).unwrap().config;
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
    fn load_config_reports_unknown_keys_and_keeps_the_known_ones() {
        // A typo'd key (`nerdfont`) or one from a newer release must not stop
        // startup, but must be reported rather than silently dropped.
        let path = unique_temp_path();
        fs::write(&path, "nerdfont = true\nmouse = false\nnew_option = 3\n").unwrap();

        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.unknown_keys, vec!["nerdfont", "new_option"]);
        assert_eq!(
            loaded.config,
            Config {
                mouse: Some(false),
                ..Config::default()
            }
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn load_config_wrong_type_for_each_known_key_errors() {
        // Proves every KNOWN_KEYS entry is a real field: an ignored key
        // would accept any value.
        for key in KNOWN_KEYS {
            let path = unique_temp_path();
            fs::write(&path, format!("{key} = {{ not = \"valid\" }}\n")).unwrap();

            let error = format!("{:#}", load_config(&path).unwrap_err());
            assert!(error.contains("invalid config file"), "{key}: {error}");
            assert!(error.contains("--no-config"), "{key}: {error}");

            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn unknown_keys_warning_names_every_key() {
        assert_eq!(unknown_keys_warning(&[]), None);
        assert_eq!(
            unknown_keys_warning(&["new_option".to_string()]).as_deref(),
            Some(
                "Config: unknown key `new_option` ignored — a typo, or a setting from a \
                 newer markcheck?"
            )
        );
        assert_eq!(
            unknown_keys_warning(&["a".to_string(), "b".to_string()]).as_deref(),
            Some(
                "Config: unknown keys `a`, `b` ignored — a typo, or a setting from a newer markcheck?"
            )
        );
    }

    fn xdg_with_config(contents: &str) -> PathBuf {
        let xdg = crate::test_support::unique_temp_path("config-xdg", "", None);
        fs::create_dir_all(xdg.join("markcheck")).unwrap();
        fs::write(xdg.join("markcheck").join("config.toml"), contents).unwrap();
        xdg
    }

    #[test]
    fn load_for_run_reads_the_default_location() {
        let xdg = xdg_with_config("mouse = false\n");
        let loaded = load_for_run(None, false, xdg.to_str(), None).unwrap();
        assert_eq!(loaded.config.mouse, Some(false));
        fs::remove_dir_all(&xdg).ok();
    }

    #[test]
    fn load_for_run_with_no_location_uses_defaults() {
        assert_eq!(
            load_for_run(None, false, None, None).unwrap(),
            Loaded::default()
        );
    }

    #[test]
    fn load_for_run_no_config_skips_even_a_broken_file() {
        let xdg = xdg_with_config("this is not valid toml =====");
        assert!(load_for_run(None, false, xdg.to_str(), None).is_err());
        assert_eq!(
            load_for_run(None, true, xdg.to_str(), None).unwrap(),
            Loaded::default()
        );
        fs::remove_dir_all(&xdg).ok();
    }

    #[test]
    fn load_for_run_explicit_path_replaces_the_default_location() {
        let xdg = xdg_with_config("this is not valid toml =====");
        let path = unique_temp_path();
        fs::write(&path, "auto_copy = true\n").unwrap();

        let loaded = load_for_run(Some(&path), false, xdg.to_str(), None).unwrap();
        assert_eq!(loaded.config.auto_copy, Some(true));

        fs::remove_file(&path).ok();
        fs::remove_dir_all(&xdg).ok();
    }

    #[test]
    fn load_for_run_explicit_path_must_exist() {
        let path = unique_temp_path();
        let error = load_for_run(Some(&path), false, None, None).unwrap_err();
        assert!(
            error.to_string().starts_with("config file not found: "),
            "{error}"
        );
    }
}
