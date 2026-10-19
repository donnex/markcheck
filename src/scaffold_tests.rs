use super::*;

fn unique_temp_dir() -> PathBuf {
    let dir = crate::test_support::unique_temp_path("scaffold", "", None);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn derive_title_splits_and_title_cases_hyphens_and_underscores() {
    assert_eq!(
        derive_title(Path::new("meeting-notes_draft.md")),
        "Meeting Notes Draft"
    );
}

#[test]
fn derive_title_falls_back_to_untitled_for_only_separators() {
    assert_eq!(derive_title(Path::new("---.md")), "Untitled");
}

#[test]
fn derive_title_handles_single_word() {
    assert_eq!(derive_title(Path::new("todo.md")), "Todo");
}

#[test]
fn template_contains_derived_title_and_blank_items() {
    let content = template(Path::new("release-checklist.md"));
    assert!(content.starts_with("# Release Checklist\n"));
    assert_eq!(content.matches("- [ ]").count(), 2);
}

#[test]
fn validate_rejects_non_md_extension() {
    let err = validate_new_path(Path::new("notes.txt")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn validate_accepts_uppercase_extension() {
    assert!(validate_new_path(Path::new("notes.MD")).is_ok());
}

#[test]
fn validate_rejects_empty_stem() {
    let err = validate_new_path(Path::new(".md")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn create_new_checklist_writes_parseable_template() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");

    let created = create_new_checklist(&path).unwrap();
    let contents = fs::read_to_string(&created).unwrap();
    assert!(contents.starts_with("# Todo\n"));

    let document = crate::parser::parse_document(created).unwrap();
    assert!(!document.lists.is_empty());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_leaves_no_temp_file_behind() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");

    create_new_checklist(&path).unwrap();

    let leftover = fs::read_dir(&dir).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("markcheck-new-")
    });
    assert!(!leftover, "temp file was not cleaned up after hard-link");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_leaves_no_temp_file_behind_when_the_target_exists() {
    // The failure path (hard_link refuses an existing target) must clean
    // up the temp file too, not just the success path.
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");
    fs::write(&path, "already here").unwrap();

    create_new_checklist(&path).unwrap_err();

    let leftover = fs::read_dir(&dir).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("markcheck-new-")
    });
    assert!(
        !leftover,
        "temp file was not cleaned up after a refused link"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_uses_default_umask_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_dir();
    let created = create_new_checklist(&dir.join("todo.md")).unwrap();
    let plain = dir.join("plain-reference-file");
    fs::File::options()
        .write(true)
        .create_new(true)
        .open(&plain)
        .unwrap();

    let created_mode = fs::metadata(&created).unwrap().permissions().mode() & 0o777;
    let plain_mode = fs::metadata(&plain).unwrap().permissions().mode() & 0o777;
    assert_eq!(created_mode, plain_mode);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_cleans_up_and_errors_when_the_temp_write_fails() {
    // A read-only directory makes the temp file's creation fail.
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_dir();
    let path = dir.join("todo.md");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    if fs::write(dir.join("probe"), "").is_ok() {
        eprintln!("skipped: a read-only directory is still writable here (running as root)");
        return;
    }

    let result = create_new_checklist(&path);

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(!path.exists());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_refuses_to_overwrite_existing_file() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");
    fs::write(&path, "already here").unwrap();

    let err = create_new_checklist(&path).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&path).unwrap(), "already here");

    fs::remove_dir_all(&dir).ok();
}

fn no_hard_links(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Operation not supported",
    ))
}

#[test]
fn create_new_checklist_falls_back_to_a_direct_write_without_hard_links() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");

    let created = create_new_checklist_with(&path, no_hard_links).unwrap();

    assert_eq!(fs::read_to_string(&created).unwrap(), template(&created));
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("todo.md")]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_fallback_still_refuses_to_overwrite() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.md");
    fs::write(&path, "already here").unwrap();

    let err = create_new_checklist_with(&path, no_hard_links).unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(&path).unwrap(), "already here");
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_rejects_bad_extension_before_touching_disk() {
    let dir = unique_temp_dir();
    let path = dir.join("todo.txt");

    let err = create_new_checklist(&path).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(!path.exists());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn create_new_checklist_errors_on_missing_parent_directory() {
    let dir = unique_temp_dir();
    let path = dir.join("does-not-exist").join("todo.md");

    let err = create_new_checklist(&path).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);

    fs::remove_dir_all(&dir).ok();
}
