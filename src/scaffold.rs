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

/// Creates a new starter checklist at `path` and returns its canonicalized
/// location. The existence check and the write are kept atomic — a file
/// that appears between a separate check and the write can't be silently
/// clobbered — while also never leaving a partially-written file behind if
/// the write itself fails partway: the template is written to a temp file
/// in the same directory (fsynced) first, then hard-linked into place.
/// Unlike `rename`, `hard_link` fails with `AlreadyExists` instead of
/// silently replacing an existing target, so it preserves the same
/// never-overwrite guarantee `create_new` gave directly; the temp name is
/// removed either way (on success it and `full_path` are the same inode, so
/// nothing is lost by removing it).
pub fn create_new_checklist(path: &Path) -> io::Result<PathBuf> {
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
    // `write_temp` returns the *actual* path it wrote to, which can differ
    // from `temp_path_base` if that name was already taken (see its own doc
    // comment) — and cleans up after itself on any failure past file
    // creation, so there's no separate `fs::remove_file` needed here on that
    // path, only for the hard-link step below. Mirrors `writer::write_back`.
    let temp_path = write_temp(&temp_path_base, &contents)?;
    let link_result = fs::hard_link(&temp_path, &full_path);
    let _ = fs::remove_file(&temp_path);
    link_result?;

    #[cfg(unix)]
    crate::writer::sync_parent_dir(&full_path);

    Ok(full_path)
}

/// Creates the temp file at `base_temp_path` (or, if that name is already
/// taken, at a freshly-suffixed retry path — see below), writes `contents`,
/// then fsyncs it before returning the *actual* path written to, so the
/// caller's hard-link always targets a flushed temp file at the right name
/// regardless of which path was used. No explicit permissions are set —
/// unlike `writer::write_temp`, there's no source file's permissions to
/// protect or restore, so this keeps the same default (umask-masked)
/// permissions `create_new` on the final path gave directly.
///
/// The PID+random-suffix name makes a genuine collision with another live
/// process astronomically unlikely; a stale temp left behind by a crashed
/// prior run reusing the same PID is the only realistic cause. Retried once
/// at a freshly-suffixed path — never by deleting the existing file and
/// reusing its name: a *genuine* collision with another live, legitimate
/// writer using this exact name would otherwise have its temp file deleted
/// out from under it. This mirrors `writer::write_temp` exactly, which is
/// what an earlier version of this function only *claimed* to do while in
/// fact deleting the collision — the precise behavior `writer::write_temp`
/// had already been changed away from.
fn write_temp(base_temp_path: &Path, contents: &str) -> io::Result<PathBuf> {
    let open = |path: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let (path, mut file) = match open(base_temp_path) {
        Ok(file) => (base_temp_path.to_path_buf(), file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let mut retry_path = base_temp_path.as_os_str().to_os_string();
            retry_path.push(format!("-{:x}", crate::writer::random_suffix()));
            let retry_path = PathBuf::from(retry_path);
            let file = open(&retry_path)?;
            (retry_path, file)
        }
        Err(err) => return Err(err),
    };

    let write_result = (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    match write_result {
        Ok(()) => Ok(path),
        Err(err) => {
            let _ = fs::remove_file(&path);
            Err(err)
        }
    }
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
        // Deep review: this function used to delete whatever file was
        // already at the generated name and reuse that name -- the exact
        // behavior `writer::write_temp` had already been changed away from,
        // while a comment here claimed to mirror it. A genuine collision
        // with another live process would have had its temp file deleted
        // out from under it. Mirrors
        // `writer::tests::write_temp_retries_at_a_new_name_instead_of_deleting_a_collision`.
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
        // No explicit mode is set on the temp file (unlike writer::write_temp,
        // which protects an existing source file's permissions) — the final
        // file should get the same default, umask-masked permissions any
        // plain `create_new` file in this environment would, regardless of
        // what the umask actually is here.
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
        // A read-only directory lets canonicalize succeed (needs only
        // read+execute) while the temp file's own create_new fails with
        // PermissionDenied — exercising create_new_checklist's cleanup
        // branch for a write_temp failure, distinct from the AlreadyExists
        // path covered by create_new_checklist_refuses_to_overwrite_existing_file.
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
