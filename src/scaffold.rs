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
/// location. `create_new` makes the existence check and the write a single
/// atomic filesystem operation, so a file that appears between a separate
/// check and the write can't be silently clobbered.
///
/// Unlike `writer::write_back`, this skips the temp-file-then-rename dance:
/// there's no prior content at risk, since the file doesn't exist yet.
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
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&full_path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(full_path)
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
