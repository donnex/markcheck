//! Creates a starter checklist file for `--new`.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Builds a human title from a file stem: splits on `-`/`_`, title-cases
/// each word. Falls back to "Untitled" when the stem yields no words at
/// all (e.g. a stem made only of separators, or missing entirely).
pub fn derive_title(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let title = stem
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "Untitled".to_string()
    } else {
        title
    }
}

fn title_case_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Starter content for a freshly created checklist: a derived title and two
/// blank tasks ready to fill in. Blank checkbox items (`- [ ]` with no text)
/// are already a supported, tested parser case.
pub fn template(path: &Path) -> String {
    let title = derive_title(path);
    format!("# {title}\n\n- [ ]\n- [ ]\n")
}

/// Rejects anything but a `.md` (case-insensitive) path with a non-empty
/// file stem, before any filesystem access, so a naming mistake is reported
/// the same way regardless of where the file happens to live.
pub fn validate_new_path(path: &Path) -> io::Result<()> {
    let has_md_extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
    if !has_md_extension {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file name must end in .md",
        ));
    }
    let has_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| !s.is_empty());
    if !has_stem {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file name is empty",
        ));
    }
    Ok(())
}

/// Creates a starter checklist at `path` and returns its canonical path.
/// Written to a temp file and hard-linked into place, which fails rather
/// than overwrite an existing file and never leaves a partial one. On a
/// filesystem without hard links, the file is created directly instead,
/// still refusing to overwrite an existing one.
pub fn create_new_checklist(path: &Path) -> io::Result<PathBuf> {
    create_new_checklist_with(path, |from, to| fs::hard_link(from, to))
}

fn create_new_checklist_with(
    path: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    validate_new_path(path)?;

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let canonical_parent = fs::canonicalize(parent).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot resolve directory {}: {err}", parent.display()),
        )
    })?;
    let file_name = path
        .file_name()
        .expect("validate_new_path already confirmed a non-empty file stem");
    let full_path = canonical_parent.join(file_name);

    let contents = template(&full_path);
    let temp_path_base = canonical_parent.join(format!(
        ".{}.markcheck-new-{}-{:x}",
        file_name.to_string_lossy(),
        std::process::id(),
        crate::writer::random_suffix()
    ));
    // `write_temp` cleans up after its own failures.
    let temp_path = write_temp(&temp_path_base, &contents)?;
    let link_result = link(&temp_path, &full_path);
    let _ = fs::remove_file(&temp_path);
    match link_result {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(err),
        // FAT32/exFAT, many SMB mounts and some FUSE filesystems have no hard
        // links, and report it with a range of error codes.
        Err(_) => write_new_file(&full_path, &contents)?,
    }

    #[cfg(unix)]
    crate::writer::sync_parent_dir(&full_path);

    Ok(full_path)
}

/// Like `writer::write_temp`, without explicit permissions: a new file gets
/// the umask default. Retries once at a fresh name if taken, never deleting
/// the existing file.
fn write_temp(base_temp_path: &Path, contents: &str) -> io::Result<PathBuf> {
    match write_new_file(base_temp_path, contents) {
        Ok(()) => Ok(base_temp_path.to_path_buf()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let mut retry_path = base_temp_path.as_os_str().to_os_string();
            retry_path.push(format!("-{:x}", crate::writer::random_suffix()));
            let retry_path = PathBuf::from(retry_path);
            write_new_file(&retry_path, contents)?;
            Ok(retry_path)
        }
        Err(err) => Err(err),
    }
}

/// Creates `path` (failing if it exists), writes and fsyncs `contents`, and
/// removes the file again if the write fails.
fn write_new_file(path: &Path, contents: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let write_result = (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(path);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir() -> PathBuf {
        let dir = crate::test_support::unique_temp_path("scaffold", "", None);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_temp_retries_at_a_new_name_instead_of_deleting_a_collision() {
        // A file at the generated name must be left untouched.
        let dir = unique_temp_dir();
        let base = dir.join(".todo.md.markcheck-new-collision");
        fs::write(&base, "a live process's own temp file, not markcheck's\n").unwrap();

        let written_path = write_temp(&base, "# Todo\n\n- [ ]\n").unwrap();

        assert_ne!(
            written_path, base,
            "must not have written to (or reused) the colliding name"
        );
        assert_eq!(
            fs::read_to_string(&base).unwrap(),
            "a live process's own temp file, not markcheck's\n",
            "the pre-existing file at the colliding name must be completely untouched"
        );
        assert_eq!(
            fs::read_to_string(&written_path).unwrap(),
            "# Todo\n\n- [ ]\n"
        );

        fs::remove_dir_all(&dir).ok();
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn create_new_checklist_cleans_up_and_errors_when_the_temp_write_fails() {
        // A read-only directory makes the temp file's creation fail.
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir();
        let path = dir.join("todo.md");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

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
}
