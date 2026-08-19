//! Startup-path integration tests: a nonexistent file path and a
//! valid file with zero checklist items both exit *before* any terminal
//! setup happens (`main.rs`), so — unlike `tests/pty.rs` — these need only a
//! plain subprocess, no pseudo-terminal.

use std::process::Command;

mod common;

fn unique_path(hint: &str) -> std::path::PathBuf {
    common::unique_temp_path("cli", hint, Some("md"))
}

fn unique_dir(hint: &str) -> std::path::PathBuf {
    common::unique_temp_path("cli", hint, None)
}

#[test]
fn nonexistent_file_reports_a_clear_error_and_exits_nonzero() {
    let path = unique_path("missing");
    // Deliberately not creating the file.
    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg(&path)
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero on an unresolvable path"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot resolve path"),
        "clear error context shown: {stderr:?}"
    );
}

#[test]
fn empty_checklist_file_exits_cleanly_with_a_message() {
    let path = unique_path("empty");
    // A heading with prose but no `- [ ]`/`- [x]` items: the parser drops
    // sections with no checkboxes, leaving zero lists.
    std::fs::write(
        &path,
        "# Runbook\n\n## Notes\n\nJust some prose, no tasks.\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg(&path)
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "an empty checklist is not an error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No checklist items found in file."),
        "clear message shown: {stderr:?}"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn version_flag_prints_cargo_version_and_git_sha_then_exits_cleanly() {
    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success(), "--version exits zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_prefix = format!("markcheck {}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.starts_with(&expected_prefix),
        "starts with name and cargo version: {stdout:?}"
    );
    assert!(
        stdout.trim_end().ends_with(')') && stdout.contains('('),
        "SHA is shown parenthesized after the version: {stdout:?}"
    );
}

#[test]
fn new_and_file_together_report_a_clear_conflict_and_exit_nonzero() {
    let path = unique_path("new-conflict");
    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg("--new")
        .arg(&path)
        .arg(&path)
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero when FILE and --new are both given"
    );
    assert!(
        !path.exists(),
        "the conflicting invocation must not create anything"
    );
}

#[test]
fn neither_new_nor_file_reports_a_clear_error_and_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero when neither FILE nor --new is given"
    );
}

#[test]
fn new_with_non_md_extension_reports_a_clear_error_and_exits_nonzero() {
    let path = unique_path("new-badext").with_extension("txt");
    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg("--new")
        .arg(&path)
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero for a non-.md --new path"
    );
    assert!(!path.exists(), "the rejected path must not be created");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must end in .md"),
        "clear error context shown: {stderr:?}"
    );
}

#[test]
fn new_with_existing_path_reports_a_clear_error_and_exits_nonzero() {
    let path = unique_path("new-exists");
    std::fs::write(&path, "already here").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg("--new")
        .arg(&path)
        .env("XDG_CONFIG_HOME", std::env::temp_dir())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero when the --new path already exists"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot create"),
        "clear error context shown: {stderr:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "already here",
        "the existing file must not be overwritten"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn malformed_config_file_reports_a_clear_error_and_exits_nonzero() {
    // A config file that exists but fails to parse is a hard error — the
    // user asked for these defaults, so silently falling back would hide
    // a typo rather than surface it.
    let md_path = unique_path("cfg-malformed");
    std::fs::write(&md_path, "## L\n\n- [ ] a\n").unwrap();

    let xdg_config_home = unique_dir("cfg-malformed-xdg");
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        "this is not valid toml =====",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_markcheck"))
        .arg(&md_path)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero on a malformed config file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid config file"),
        "clear error context shown: {stderr:?}"
    );

    std::fs::remove_file(&md_path).ok();
    std::fs::remove_dir_all(&xdg_config_home).ok();
}
