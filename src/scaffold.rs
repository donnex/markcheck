//! Creates a starter checklist file for `--new`.

use crate::writer::{write_new_file, write_new_temp};
use std::fs;
use std::io;
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
        "{}{}-{:x}",
        crate::writer::new_temp_prefix(&file_name.to_string_lossy()),
        std::process::id(),
        crate::writer::random_suffix()
    ));
    // A new file gets the umask default rather than explicit permissions.
    // `write_new_temp` cleans up after its own failures.
    let temp_path = write_new_temp(&temp_path_base, 0o666, &contents, |_| Ok(()))?;
    let link_result = link(&temp_path, &full_path);
    let _ = fs::remove_file(&temp_path);
    match link_result {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(err),
        // FAT32/exFAT, many SMB mounts and some FUSE filesystems have no hard
        // links, and report it with a range of error codes.
        Err(_) => write_new_file(&full_path, 0o666, &contents, |_| Ok(()))?,
    }

    crate::writer::sync_parent_dir(&full_path);

    Ok(full_path)
}

#[cfg(test)]
#[path = "scaffold_tests.rs"]
mod tests;
