//! Startup-path integration tests: an unresolvable or unreadable path, a
//! valid file with zero checklist items, and a run without a terminal all
//! exit *before* any terminal setup happens (`main.rs`), so — unlike
//! `tests/pty.rs` — these need only a plain subprocess, no pseudo-terminal.

use std::process::Command;

mod common;

fn unique_path(hint: &str) -> std::path::PathBuf {
    common::unique_temp_path("cli", hint, Some("md"))
}

/// The binary under test, isolated per [`common::isolated_env`].
fn markcheck() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_markcheck"));
    for var in common::ambient_git_vars() {
        cmd.env_remove(var);
    }
    cmd.envs(common::isolated_env("cli"));
    cmd
}

fn unique_dir(hint: &str) -> std::path::PathBuf {
    common::unique_temp_path("cli", hint, None)
}

#[test]
fn nonexistent_file_reports_a_clear_error_and_exits_nonzero() {
    let path = unique_path("missing");
    // Deliberately not creating the file.
    let output = markcheck().arg(&path).output().unwrap();

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

    let output = markcheck().arg(&path).output().unwrap();

    assert!(
        output.status.success(),
        "an empty checklist is not an error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No checklist items found in"),
        "clear message shown: {stderr:?}"
    );
    assert!(
        stderr.contains(&*path.file_name().unwrap().to_string_lossy()),
        "names the file: {stderr:?}"
    );
    assert!(
        stderr.contains("- [ ] step"),
        "says what syntax is expected: {stderr:?}"
    );
}

/// Runs markcheck on `path` (with stdout piped, so not a terminal) and
/// returns its stderr, asserting a non-zero exit that names the file.
fn startup_failure_stderr(path: &std::path::Path) -> String {
    let output = markcheck().arg(path).output().unwrap();
    assert!(!output.status.success(), "should exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains(&*path.file_name().unwrap().to_string_lossy()),
        "names the file: {stderr:?}"
    );
    stderr
}

#[test]
fn non_utf8_file_reports_the_file_and_the_encoding_fix() {
    let path = unique_path("utf16");
    std::fs::write(&path, b"\xff\xfe- \x00[\x00 \x00]\x00").unwrap();

    let stderr = startup_failure_stderr(&path);
    assert!(stderr.contains("cannot read"), "{stderr:?}");
    assert!(stderr.contains("not valid UTF-8"), "{stderr:?}");
    assert!(stderr.contains("iconv"), "names the fix: {stderr:?}");
}

#[test]
fn directory_path_reports_the_path_and_the_cause() {
    let dir = unique_dir("is-a-dir");
    std::fs::create_dir_all(&dir).unwrap();

    let stderr = startup_failure_stderr(&dir);
    assert!(stderr.contains("cannot read"), "{stderr:?}");
    assert!(
        stderr.to_lowercase().contains("directory"),
        "readable cause: {stderr:?}"
    );

    std::fs::remove_dir(&dir).ok();
}

#[test]
fn non_tty_stdout_reports_that_a_terminal_is_needed() {
    let path = unique_path("no-tty");
    std::fs::write(&path, "# Runbook\n\n- [ ] step\n").unwrap();

    let stderr = startup_failure_stderr(&path);
    assert!(stderr.contains("needs a terminal"), "{stderr:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "# Runbook\n\n- [ ] step\n",
        "file left untouched"
    );
}

#[test]
fn help_names_the_config_file() {
    let output = markcheck().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("markcheck/config.toml"),
        "config path shown: {stdout:?}"
    );
}

#[test]
fn version_flag_prints_cargo_version_and_git_sha_then_exits_cleanly() {
    let output = markcheck().arg("--version").output().unwrap();

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
    let output = markcheck()
        .arg("--new")
        .arg(&path)
        .arg(&path)
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
    let output = markcheck().output().unwrap();

    assert!(
        !output.status.success(),
        "should exit non-zero when neither FILE nor --new is given"
    );
}

#[test]
fn new_with_non_md_extension_reports_a_clear_error_and_exits_nonzero() {
    let path = unique_path("new-badext").with_extension("txt");
    let output = markcheck().arg("--new").arg(&path).output().unwrap();

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

    let output = markcheck().arg("--new").arg(&path).output().unwrap();

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

    let output = markcheck()
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
}

/// Runs markcheck on a one-task checklist with `config` as its default
/// config file plus `flags`, and returns its stderr. Without a terminal it
/// always fails, but only after the config has been loaded.
fn stderr_with_config(hint: &str, config: &str, flags: &[&str]) -> String {
    let md_path = unique_path(hint);
    std::fs::write(&md_path, "## L\n\n- [ ] a\n").unwrap();
    let xdg_config_home = unique_dir(&format!("{hint}-xdg"));
    std::fs::create_dir_all(xdg_config_home.join("markcheck")).unwrap();
    std::fs::write(
        xdg_config_home.join("markcheck").join("config.toml"),
        config,
    )
    .unwrap();

    let output = markcheck()
        .args(flags)
        .arg(&md_path)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .output()
        .unwrap();

    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unknown_config_key_does_not_stop_startup() {
    // A key from a newer release must not lock an older one out: startup
    // gets past the config to the terminal check.
    let stderr = stderr_with_config("cfg-unknown", "new_option = true\nmouse = false\n", &[]);
    assert!(!stderr.contains("invalid config file"), "{stderr:?}");
    assert!(stderr.contains("needs a terminal"), "{stderr:?}");
}

#[test]
fn an_out_of_range_git_sync_delay_is_a_clear_error() {
    let stderr = stderr_with_config("cfg-delay", "git_sync_delay = 9223372036854775807\n", &[]);
    assert!(
        stderr.contains("git_sync_delay must be at most 86400 seconds"),
        "{stderr:?}"
    );
    assert!(!stderr.contains("crashed"), "{stderr:?}");

    let stderr = stderr_with_config("cfg-delay-flag", "", &["--git-sync-delay=99999999"]);
    assert!(stderr.contains("--git-sync-delay"), "{stderr:?}");
    assert!(stderr.contains("0..=86400"), "{stderr:?}");
}

#[test]
fn no_config_bypasses_a_malformed_config_file() {
    let broken = "this is not valid toml =====";
    let stderr = stderr_with_config("cfg-bypass", broken, &[]);
    assert!(
        stderr.contains("--no-config"),
        "names the way out: {stderr:?}"
    );

    let stderr = stderr_with_config("cfg-bypass", broken, &["--no-config"]);
    assert!(!stderr.contains("invalid config file"), "{stderr:?}");
    assert!(stderr.contains("needs a terminal"), "{stderr:?}");
}

#[test]
fn config_flag_reads_the_named_file_instead() {
    let other = unique_dir("cfg-explicit").with_extension("toml");
    std::fs::write(&other, "this is not valid toml =====").unwrap();
    let flag = format!("--config={}", other.display());

    let stderr = stderr_with_config("cfg-explicit", "", &[&flag]);
    assert!(stderr.contains("invalid config file"), "{stderr:?}");
    assert!(
        stderr.contains(&*other.file_name().unwrap().to_string_lossy()),
        "names the file: {stderr:?}"
    );

    std::fs::remove_file(&other).ok();
    let stderr = stderr_with_config("cfg-explicit", "", &[&flag]);
    assert!(stderr.contains("config file not found"), "{stderr:?}");
}
