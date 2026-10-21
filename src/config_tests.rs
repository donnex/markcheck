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
fn config_path_ignores_relative_xdg_config_home() {
    let path = config_path(Some(".config"), Some("/home/user")).unwrap();
    assert_eq!(
        path,
        PathBuf::from("/home/user/.config/markcheck/config.toml")
    );
}

#[test]
fn config_path_none_when_home_is_relative_and_xdg_unset() {
    assert_eq!(config_path(None, Some("home/user")), None);
}

#[test]
fn config_path_none_when_both_are_relative() {
    assert_eq!(config_path(Some("xdg"), Some(".")), None);
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
             color = \"256\"\ngit_sync_paths = [\"/home/user/checklists\"]\ngit_sync_delay = 30\nclipboard = \"osc52\"\nterminal_title = false\nexit_summary = false\n",
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
            color: Some(ColorChoice::Palette256),
            git_sync_paths: Some(vec![PathBuf::from("/home/user/checklists")]),
            git_sync_delay: Some(30),
            clipboard: Some(ClipboardChoice::Osc52),
            terminal_title: Some(false),
            exit_summary: Some(false),
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
fn load_config_reads_every_color_value_and_rejects_others() {
    for (value, expected) in [
        ("auto", ColorChoice::Auto),
        ("truecolor", ColorChoice::TrueColor),
        ("256", ColorChoice::Palette256),
        ("16", ColorChoice::Basic),
    ] {
        let path = unique_temp_path();
        fs::write(&path, format!("color = \"{value}\"\n")).unwrap();
        assert_eq!(load_config(&path).unwrap().config.color, Some(expected));
        fs::remove_file(&path).ok();
    }

    let path = unique_temp_path();
    fs::write(&path, "color = \"rainbow\"\n").unwrap();
    let error = format!("{:#}", load_config(&path).unwrap_err());
    assert!(
        error.contains(r#"one of "auto", "truecolor", "256" or "16""#),
        "{error}"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn load_config_reads_unquoted_color_numbers_and_rejects_others() {
    for (value, expected) in [("256", ColorChoice::Palette256), ("16", ColorChoice::Basic)] {
        let path = unique_temp_path();
        fs::write(&path, format!("color = {value}\n")).unwrap();
        assert_eq!(load_config(&path).unwrap().config.color, Some(expected));
        fs::remove_file(&path).ok();
    }

    for value in ["8", "true", "[256]"] {
        let path = unique_temp_path();
        fs::write(&path, format!("color = {value}\n")).unwrap();
        let error = format!("{:#}", load_config(&path).unwrap_err());
        assert!(error.contains("invalid config file"), "{value}: {error}");
        assert!(
            error.contains(r#"one of "auto", "truecolor", "256" or "16""#),
            "{value}: {error}"
        );
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

#[test]
fn unreadable_config_path_is_an_error_naming_the_file() {
    let path = unique_temp_path();
    fs::create_dir_all(&path).unwrap();
    let error = format!("{:#}", load_config(&path).unwrap_err());
    fs::remove_dir_all(&path).unwrap();
    assert!(error.contains("cannot read config file"), "{error}");
}

#[test]
fn load_config_rejects_a_git_sync_delay_over_a_day() {
    let path = unique_temp_path();
    fs::write(&path, format!("git_sync_delay = {MAX_GIT_SYNC_DELAY}\n")).unwrap();
    assert_eq!(
        load_config(&path).unwrap().config.git_sync_delay,
        Some(MAX_GIT_SYNC_DELAY)
    );

    fs::write(&path, "git_sync_delay = 9223372036854775807\n").unwrap();
    let error = format!("{:#}", load_config(&path).unwrap_err());
    assert!(error.contains("invalid config file"), "{error}");
    assert!(
        error.contains("git_sync_delay must be at most 86400 seconds"),
        "{error}"
    );

    fs::remove_file(&path).ok();
}
