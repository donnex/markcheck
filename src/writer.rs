use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::model::{Document, ItemKind, TaskState};

/// A per-call random `u64`, used to widen a temp file's name beyond just its
/// PID so two processes racing to create a same-named temp file (or a
/// crashed run whose PID gets reused) is vanishingly unlikely rather than
/// merely unlikely. `RandomState`'s keys are freshly drawn from the OS RNG
/// on each `new()` (the same mechanism `HashMap`'s default hasher uses for
/// DoS resistance), so hashing nothing and reading `finish()` is a
/// dependency-free way to get process-random entropy without pulling in a
/// dedicated `rand` crate for one temp-file suffix. `pub(crate)`: also used
/// by `scaffold::create_new_checklist`'s equivalent temp file.
pub(crate) fn random_suffix() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

/// Replaces the task checkbox on the line with `target`, leaving everything
/// else untouched. Recognizes `[ ]`, `[x]`, `[X]` (pulldown parses uppercase
/// `[X]` as done, so we must be able to rewrite it), and `[/]`; rewriting
/// normalizes the marker to `target`'s casing.
///
/// The checkbox is the *leftmost* marker on the line — it sits right after the
/// list bullet, before any body text. We must replace it by position, not the
/// first marker *type* in some fixed order: a body containing a literal marker
/// token (e.g. ``press the `[ ]` button``) would otherwise get its body token
/// rewritten while the real checkbox is left untouched, silently desyncing the
/// on-disk state from the app and corrupting the body.
fn set_task_marker(line: &mut String, target: &str) {
    let leftmost = ["[ ]", "[x]", "[X]", "[/]"]
        .into_iter()
        .filter_map(|marker| line.find(marker).map(|pos| (pos, marker)))
        .min_by_key(|&(pos, _)| pos);
    if let Some((pos, marker)) = leftmost {
        line.replace_range(pos..pos + marker.len(), target);
    }
}

/// Joins `lines` back into a single string using `document`'s original line
/// ending (`\r\n` if it was CRLF-authored — `raw_lines`/`str::lines()` strip
/// the `\r`, so it must be rejoined explicitly or every line ending would
/// silently flip to LF on the first toggle) and only reattaching a final
/// trailing newline when the source actually had one (`str::lines()` drops
/// that distinction too — rejoining unconditionally would silently add a
/// newline to a file that never had one).
fn join_lines(lines: &[String], document: &Document) -> String {
    let newline = if document.uses_crlf { "\r\n" } else { "\n" };
    let mut contents = lines.join(newline);
    if document.trailing_newline {
        contents.push_str(newline);
    }
    contents
}

/// The document's current content exactly as it stands in memory — no
/// checkbox mutation — reconstructed with its original line endings and
/// trailing-newline state. Used to capture the expected git-sync snapshot
/// for a change that already landed on disk outside `write_back` (e.g. an
/// external editor edit picked up by a reload).
pub fn document_contents(document: &Document) -> String {
    join_lines(&document.raw_lines, document)
}

/// Writes to a temp file in the same directory, fsyncs it, and renames it
/// over the target so a crash or full disk mid-write can't leave the
/// checklist truncated or corrupted; the original content stays intact
/// until the rename (atomic on the same filesystem) succeeds. The temp
/// file's data is fsynced before the rename (`write_temp`), and on unix its
/// parent directory is fsynced (best-effort) after the rename, so the
/// rename itself survives a crash — a bare temp-then-rename without fsync
/// can still lose the write on a crash/power-loss at the wrong moment on
/// many filesystems (ext4 `data=ordered` included). The original file's
/// permissions are applied to the temp file before the rename so they
/// survive the swap. Hard links and ownership are not preserved — the
/// rename gives the path a new inode, the same trade-off atomic-save
/// editors make. Returns the exact content that was written, so callers
/// (git-sync) can capture it as the expected snapshot for a later commit.
pub fn write_back(document: &Document) -> io::Result<String> {
    let mut lines = document.raw_lines.clone();
    for list in &document.lists {
        for item in &list.items {
            if let ItemKind::Checkbox(state) = item.kind {
                let target = match state {
                    TaskState::NotStarted => "[ ]",
                    TaskState::Started => "[/]",
                    TaskState::Done => "[x]",
                };
                set_task_marker(&mut lines[item.line_number - 1], target);
            }
        }
    }
    let contents = join_lines(&lines, document);

    // Read the source permissions before touching anything. Any error —
    // including NotFound — aborts before writing, so a deleted file is never
    // silently recreated. A 0600 file can also never be widened.
    let source_perms = {
        let meta = fs::metadata(&document.file_path)?;
        Some(meta.permissions())
    };

    let file_name = document
        .file_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no file name"))?
        .to_string_lossy();
    let temp_path_base = document.file_path.with_file_name(format!(
        ".{file_name}.markcheck-tmp-{}-{:x}",
        std::process::id(),
        random_suffix()
    ));

    // `write_temp` returns the *actual* path it wrote to, which can differ
    // from `temp_path_base` if that name was already taken (see its own
    // doc comment) — cleans up after itself on any failure past file
    // creation, so there's no separate `fs::remove_file` needed here on
    // that path, only for the rename step below.
    let temp_path = write_temp(&temp_path_base, &contents, source_perms)?;
    if let Err(err) = fs::rename(&temp_path, &document.file_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    #[cfg(unix)]
    sync_parent_dir(&document.file_path);
    Ok(contents)
}

/// Best-effort fsync of `path`'s parent directory so the rename's directory
/// entry itself survives a crash, not just the file's data (which
/// `write_temp` already fsyncs before the rename). Errors are ignored: by
/// this point the rename has already succeeded and the content is safely on
/// disk under normal operation, and some filesystems don't support or need
/// directory fsync at all — this is a best-effort strengthening of the
/// crash-safety guarantee, not a correctness requirement. `pub(crate)`: also
/// used by `scaffold::create_new_checklist`, whose hard-link-into-place has
/// the identical need to make a fresh directory entry durable.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Creates the temp file at `base_temp_path` (or, if that name is already
/// taken, at a freshly-suffixed retry path — see below) with mode 0600
/// (never more permissive than the most restrictive plausible source, even
/// for an instant), writes the contents, applies the exact source
/// permissions through the file handle — fchmod semantics, immune to the
/// process umask — then fsyncs the data to disk before returning the
/// *actual* path written to, so the caller's rename always targets a
/// flushed temp file at the right name regardless of which path was used.
///
/// The PID+random-suffix name makes a genuine collision with another live
/// process astronomically unlikely; a stale temp left behind by a crashed
/// prior run reusing the same PID is the only realistic cause. Retried
/// once at a freshly-suffixed path — never by deleting the existing file
/// and reusing its name: a *genuine* collision with another live,
/// legitimate writer using this exact name would otherwise have its temp
/// file deleted out from under it. The crashed-run scenario is inherently
/// racy to simulate deterministically, so this path is exercised by
/// inspection rather than a dedicated regression test.
#[cfg(unix)]
fn write_temp(
    base_temp_path: &Path,
    contents: &str,
    perms: Option<fs::Permissions>,
) -> io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let open = |path: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    let (path, mut file) = match open(base_temp_path) {
        Ok(file) => (base_temp_path.to_path_buf(), file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let mut retry_path = base_temp_path.as_os_str().to_os_string();
            retry_path.push(format!("-{:x}", random_suffix()));
            let retry_path = PathBuf::from(retry_path);
            let file = open(&retry_path)?;
            (retry_path, file)
        }
        Err(err) => return Err(err),
    };

    let write_result = (|| {
        file.write_all(contents.as_bytes())?;
        if let Some(perms) = perms {
            file.set_permissions(perms)?;
        }
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

#[cfg(not(unix))]
fn write_temp(
    temp_path: &Path,
    contents: &str,
    perms: Option<fs::Permissions>,
) -> io::Result<PathBuf> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp_path)?;
    file.write_all(contents.as_bytes())?;
    if let Some(perms) = perms {
        file.set_permissions(perms)?;
    }
    file.sync_all()?;
    Ok(temp_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_document;
    use std::path::PathBuf;

    const EXAMPLE: &str = "\
## Prepare workspace

- **Steps for the first workspace**
- [ ] `refresh-cache`
- [ ] `refresh-cache`
- [ ] `restart-service`

## Second workspace

- [ ] `refresh-cache`
";

    fn write_temp_file(contents: &str) -> PathBuf {
        let path = crate::test_support::unique_temp_path("writer", "", Some("md"));
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn toggling_completed_item_updates_only_that_line() {
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();

        // Complete the third checkbox item ("restart-service"), leave others untouched.
        let item = document.lists[0]
            .items
            .iter_mut()
            .find(|i| i.display_text == "restart-service")
            .unwrap();
        item.kind = ItemKind::Checkbox(TaskState::Done);

        write_back(&document).unwrap();

        let reparsed = parse_document(path.clone()).unwrap();
        let restart_item = reparsed.lists[0]
            .items
            .iter()
            .find(|i| i.display_text == "restart-service")
            .unwrap();
        assert_eq!(restart_item.kind, ItemKind::Checkbox(TaskState::Done));

        // All other checkbox items remain unaffected.
        let other_completed = reparsed.lists[0]
            .items
            .iter()
            .filter(|i| i.display_text != "restart-service")
            .filter(|i| matches!(i.kind, ItemKind::Checkbox(TaskState::Done)))
            .count();
        assert_eq!(other_completed, 0);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn uppercase_x_marker_is_rewritten_on_toggle() {
        // pulldown parses `[X]` as done; the writer must be able to rewrite it,
        // else toggling it off would silently not persist.
        let mut line = "- [X] done".to_string();
        set_task_marker(&mut line, "[ ]");
        assert_eq!(line, "- [ ] done");

        // End-to-end: a `[X]` task toggled off round-trips to `[ ]`.
        let path = write_temp_file("## S\n\n- [X] done\n");
        let mut document = parse_document(path.clone()).unwrap();
        assert_eq!(
            document.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::Done),
            "[X] parses as done"
        );
        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();
        let reparsed = parse_document(path.clone()).unwrap();
        assert_eq!(
            reparsed.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::NotStarted),
            "toggling [X] off now persists"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn set_task_marker_targets_the_checkbox_not_a_body_token() {
        // Only the leading checkbox (the leftmost marker) is rewritten; a
        // literal marker token in the body must be left exactly as-is.
        let mut line = "- [x] press the `[ ]` button".to_string();
        set_task_marker(&mut line, "[/]");
        assert_eq!(line, "- [/] press the `[ ]` button");

        // The checkbox is still the leftmost marker even when it's `[ ]` and a
        // `[x]` appears in the body.
        let mut line = "- [ ] type `[x]` to confirm".to_string();
        set_task_marker(&mut line, "[x]");
        assert_eq!(line, "- [x] type `[x]` to confirm");

        // No marker at all → line untouched.
        let mut line = "- a plain bullet".to_string();
        set_task_marker(&mut line, "[x]");
        assert_eq!(line, "- a plain bullet");
    }

    #[test]
    fn write_back_leaves_inline_body_marker_intact() {
        // End-to-end: toggling a done task whose body carries a literal
        // `[ ]` token flips the checkbox and leaves the body token untouched.
        let path = write_temp_file("## S\n\n- [x] press the `[ ]` button\n");
        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Started);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("- [/] press the `[ ]` button"),
            "checkbox toggled, body marker preserved; got: {written:?}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn write_back_leaves_no_temp_file_behind() {
        let path = write_temp_file(EXAMPLE);
        let document = parse_document(path.clone()).unwrap();

        write_back(&document).unwrap();

        let dir = path.parent().unwrap();
        let file_name = path.file_name().unwrap().to_string_lossy();
        let leftover = fs::read_dir(dir).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            name.to_string_lossy()
                .contains(&format!("{file_name}.markcheck-tmp-"))
        });
        assert!(!leftover, "temp file was not cleaned up after rename");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_preserves_untouched_content() {
        let path = write_temp_file(EXAMPLE);
        let document = parse_document(path.clone()).unwrap();

        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();

        assert_eq!(written.trim_end(), EXAMPLE.trim_end());

        fs::remove_file(&path).ok();
    }

    #[test]
    fn write_back_on_an_empty_document_leaves_the_file_empty() {
        // Not reachable through normal app use (there's nothing to toggle
        // with zero items), but write_back itself makes no assumption that
        // the document is non-empty — an empty source (no lists, no
        // raw_lines, no trailing newline) must round-trip to exactly the
        // same empty file, not gain a stray newline from an unconditional
        // join-and-push.
        let path = write_temp_file("");
        let document = parse_document(path.clone()).unwrap();
        assert!(document.lists.is_empty());
        assert!(document.raw_lines.is_empty());
        assert!(!document.trailing_newline);

        write_back(&document).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_does_not_add_a_missing_trailing_newline() {
        // A source file with no final newline must not gain one on toggle —
        // write_back's "only the checkbox changes" guarantee (documented in
        // README.md) otherwise silently breaks on the very first write.
        let no_trailing_newline = "## S\n\n- [ ] `task`";
        let path = write_temp_file(no_trailing_newline);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(!document.trailing_newline, "source has no final newline");

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, "## S\n\n- [x] `task`");
        assert!(!written.ends_with('\n'), "must not gain a trailing newline");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_preserves_an_existing_trailing_newline() {
        let with_trailing_newline = "## S\n\n- [ ] `task`\n";
        let path = write_temp_file(with_trailing_newline);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(document.trailing_newline, "source has a final newline");

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, "## S\n\n- [x] `task`\n");

        fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trip_preserves_crlf_line_endings() {
        // A file authored with CRLF line endings must keep them after a
        // toggle — write_back must not silently flip the whole file to LF
        // just because one marker byte changed.
        let crlf_example = EXAMPLE.replace('\n', "\r\n");
        let path = write_temp_file(&crlf_example);
        let mut document = parse_document(path.clone()).unwrap();
        assert!(document.uses_crlf, "CRLF source must be detected");

        let item = document
            .lists
            .iter_mut()
            .flat_map(|list| list.items.iter_mut())
            .find(|i| i.display_text == "refresh-cache")
            .unwrap();
        item.kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(
            written.matches('\n').count(),
            written.matches("\r\n").count(),
            "every line ending must be \\r\\n, not bare \\n; got: {written:?}"
        );
        assert!(
            written.contains("- [x] `refresh-cache`\r\n"),
            "toggled line keeps its CRLF ending; got: {written:?}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn toggling_second_duplicate_item_updates_correct_line() {
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();

        // There are two "refresh-cache" items in list 0; complete only the second.
        let target_line = document.lists[0]
            .items
            .iter()
            .filter(|i| i.display_text == "refresh-cache")
            .nth(1)
            .unwrap()
            .line_number;

        for item in document.lists[0].items.iter_mut() {
            if item.line_number == target_line {
                item.kind = ItemKind::Checkbox(TaskState::Done);
            }
        }

        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let written_lines: Vec<&str> = written.lines().collect();

        assert!(written_lines[target_line - 1].contains("[x]"));

        let first_duplicate_line = document.lists[0]
            .items
            .iter()
            .find(|i| i.display_text == "refresh-cache")
            .unwrap()
            .line_number;
        assert!(written_lines[first_duplicate_line - 1].contains("[ ]"));

        fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_back_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        for mode in [0o600u32, 0o640] {
            let path = write_temp_file(EXAMPLE);
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();

            let mut document = parse_document(path.clone()).unwrap();
            document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
            write_back(&document).unwrap();

            let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(actual, mode, "permissions changed for mode {mode:o}");

            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn write_back_errors_on_deleted_source_file() {
        // write_back must refuse to recreate a deleted file rather than
        // silently overwriting the deletion. The app layer detects
        // deletion and blocks the call before we get here; this test guards
        // the writer itself.
        let path = write_temp_file(EXAMPLE);
        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);

        fs::remove_file(&path).unwrap();
        let result = write_back(&document);

        assert!(result.is_err(), "write_back must fail on a deleted file");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(!path.exists(), "file must not be recreated");
    }

    /// Mirrors main.rs's startup canonicalization: with the resolved path,
    /// toggling through a symlink must update the target and leave the
    /// link itself intact.
    #[cfg(unix)]
    #[test]
    fn write_back_through_canonicalized_symlink_keeps_link_intact() {
        use std::os::unix::fs::PermissionsExt;

        let target = write_temp_file(EXAMPLE);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        let link_dir = std::env::temp_dir().join(format!(
            "markcheck-writer-test-linkdir-{}",
            std::process::id()
        ));
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("linked.md");
        fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let canonical = fs::canonicalize(&link).unwrap();
        let mut document = parse_document(canonical).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink must survive write-back"
        );
        assert!(fs::read_to_string(&target).unwrap().contains("[x]"));
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, "target permissions must survive");

        fs::remove_file(&link).ok();
        fs::remove_dir(&link_dir).ok();
        fs::remove_file(&target).ok();
    }

    #[test]
    fn untoggling_reverts_checkbox() {
        let path = write_temp_file("- [x] `already done`\n");
        let mut document = parse_document(path.clone()).unwrap();

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("[ ]"));
        assert!(!written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn writes_started_marker() {
        let path = write_temp_file("- [ ] `task`\n");
        let mut document = parse_document(path.clone()).unwrap();

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Started);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("[/]"),
            "started marker written: {written:?}"
        );
        assert!(!written.contains("[ ]") && !written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn started_marker_transitions_to_done_and_not_started() {
        let path = write_temp_file("- [/] `task`\n");
        let mut document = parse_document(path.clone()).unwrap();
        assert_eq!(
            document.lists[0].items[0].kind,
            ItemKind::Checkbox(TaskState::Started)
        );

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("[x]"));

        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::NotStarted);
        write_back(&document).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("[ ]"));
        assert!(!written.contains("[/]") && !written.contains("[x]"));

        fs::remove_file(&path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_temp_retries_at_a_new_name_instead_of_deleting_a_collision() {
        // External review: the old behavior removed whatever file was
        // already at the generated name before retrying at that *same*
        // name -- conceptually wrong regardless of how unlikely a genuine
        // collision with another live process is, since that file could
        // belong to one. Deterministically forceable now that write_temp
        // takes an explicit base path: pre-create a file there and confirm
        // write_temp writes somewhere else instead, leaving it untouched.
        let base = crate::test_support::unique_temp_path("writer", "collision", None);
        fs::write(&base, "a live process's own temp file, not markcheck's\n").unwrap();

        let written_path = write_temp(&base, "checklist content\n", None).unwrap();

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
            "checklist content\n"
        );

        fs::remove_file(&base).ok();
        fs::remove_file(&written_path).ok();
    }
}
