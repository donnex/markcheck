//! Startup-path integration tests: a nonexistent file path and a
//! valid file with zero checklist items both exit *before* any terminal
//! setup happens (`main.rs`), so — unlike `tests/pty.rs` — these need only a
//! plain subprocess, no pseudo-terminal.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn unique_path(hint: &str) -> std::path::PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "markcheck-cli-{hint}-{}-{n}.md",
        std::process::id()
    ))
}

fn unique_dir(hint: &str) -> std::path::PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("markcheck-cli-{hint}-{}-{n}", std::process::id()))
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
