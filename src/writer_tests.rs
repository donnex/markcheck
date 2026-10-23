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

// --- Advisory write lock ---

/// Writes a lock file by hand claiming to be held by `pid`.
fn plant_lock(target: &Path, pid: u32) -> PathBuf {
    let path = lock_path(target).unwrap();
    fs::write(&path, format!("markcheck {pid} deadbeef")).unwrap();
    path
}

/// A PID that has certainly exited: spawned, waited for, and reaped.
fn a_dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn age_past_the_sweep(path: &Path) {
    let old = std::time::SystemTime::now() - LEFTOVER_MIN_AGE - std::time::Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

/// The premise behind the startup hard-link warning, pinned so it cannot
/// quietly stop being true: an ordinary checklist has one link, a
/// hard-linked one reports more, and the atomic write really does break
/// the alias rather than updating both names.
#[test]
fn a_hard_linked_checklist_is_detected_and_the_write_breaks_the_alias() {
    let target = write_temp_file(EXAMPLE);
    assert_eq!(
        hard_link_count(&target),
        Some(1),
        "an ordinary checklist has exactly one name"
    );

    let alias = target.with_extension("alias.md");
    fs::hard_link(&target, &alias).unwrap();
    assert_eq!(
        hard_link_count(&target),
        Some(2),
        "the second name must be visible before anything is written"
    );

    // The divergence itself: write through one name, and the other is
    // left holding the old content. This is what the warning is for.
    let mut document = parse_document(target.clone()).unwrap();
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();
    assert!(
        fs::read_to_string(&target).unwrap().contains("[x]"),
        "the write must land on the path that was written"
    );
    assert_eq!(
        fs::read_to_string(&alias).unwrap(),
        EXAMPLE,
        "the alias keeps the old content — it is now a separate file"
    );
    assert_eq!(
        hard_link_count(&target),
        Some(1),
        "and the link count drops, because the rename made a new inode"
    );

    fs::remove_file(&alias).ok();
    fs::remove_file(&target).ok();
}

/// A group other than `exclude` that the test user belongs to, if any.
fn a_supplementary_group(exclude: u32) -> Option<u32> {
    let output = Command::new("id").arg("-G").output().ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .split_whitespace()
        .filter_map(|gid| gid.parse().ok())
        .find(|&gid| gid != exclude)
}

/// A shared group checklist keeps its group across a save, so the rest
/// of the group can still write it, instead of passing to the saver's
/// primary group.
#[test]
fn a_save_keeps_the_checklist_group() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let target = write_temp_file(EXAMPLE);
    let default_gid = fs::metadata(&target).unwrap().gid();
    let Some(shared_gid) = a_supplementary_group(default_gid) else {
        eprintln!("skipped: the test user has no second group");
        fs::remove_file(&target).ok();
        return;
    };
    std::os::unix::fs::chown(&target, None, Some(shared_gid)).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o664)).unwrap();

    let mut document = parse_document(target.clone()).unwrap();
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();

    let meta = fs::metadata(&target).unwrap();
    assert!(fs::read_to_string(&target).unwrap().contains("[x]"));
    assert_eq!(meta.gid(), shared_gid, "the save must keep the group");
    assert_eq!(meta.mode() & 0o7777, 0o664, "and the mode with it");

    fs::remove_file(&target).ok();
}

/// Only a file someone else owns, opened by a non-root user, is flagged:
/// that is the one case where a save hands the file to the saver.
#[test]
fn only_a_file_another_user_owns_is_flagged() {
    use std::os::unix::fs::MetadataExt;
    let target = write_temp_file(EXAMPLE);
    let owner = fs::metadata(&target).unwrap().uid();

    assert_eq!(
        current_uid(),
        owner,
        "a new file belongs to the current user"
    );
    assert_eq!(foreign_owner(&target), None, "your own file is not flagged");
    assert_eq!(
        foreign_owner_for(&target, owner.wrapping_add(1)),
        Some(owner),
        "another user's file is flagged with its owner"
    );
    if owner != 0 {
        assert_eq!(
            foreign_owner_for(&target, 0),
            None,
            "root keeps the owner, so nothing to flag"
        );
    }
    assert_eq!(
        foreign_owner_for(&target.with_extension("missing"), owner.wrapping_add(1)),
        None,
        "an unreadable file is not flagged"
    );

    fs::remove_file(&target).ok();
}

/// Every spelling of one file must map to the same lock.
#[test]
fn every_spelling_of_one_checklist_maps_to_the_same_lock() {
    let target = write_temp_file(EXAMPLE);
    let dir = target.parent().unwrap();
    let name = target.file_name().unwrap();
    let expected = lock_path(&target).unwrap();

    // A `.` component.
    assert_eq!(
        lock_path(&dir.join(".").join(name)).unwrap(),
        expected,
        "a `.` component must not create a second lock"
    );

    // A `..` component that walks out of the directory and back in.
    let round_trip = dir.join("sub").join("..").join(name);
    fs::create_dir_all(dir.join("sub")).unwrap();
    assert_eq!(
        lock_path(&round_trip).unwrap(),
        expected,
        "a `..` component must not create a second lock"
    );

    let link = dir.join(format!(
        "link-to-{}",
        crate::test_support::unique_temp_path("l", "", None)
            .file_name()
            .unwrap()
            .to_string_lossy()
    ));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        lock_path(&link).unwrap(),
        expected,
        "a symlink to the checklist must share its lock"
    );

    // Sanity: the guard only means something if distinct files still get
    // distinct locks. Without this the test passes for a `lock_path`
    // that returns one constant.
    let other = write_temp_file(EXAMPLE);
    assert_ne!(
        lock_path(&other).unwrap(),
        expected,
        "two genuinely different checklists must not share a lock"
    );

    fs::remove_file(&link).ok();
    fs::remove_dir(dir.join("sub")).ok();
    fs::remove_file(&target).ok();
    fs::remove_file(&other).ok();
}

/// A second writer reaching the file by another spelling is kept out.
#[test]
fn a_writer_arriving_through_a_symlink_is_locked_out_by_the_first() {
    let target = write_temp_file(EXAMPLE);
    let dir = target.parent().unwrap();
    let link = dir.join(format!(
        "aliased-{}.md",
        std::process::id() as u64 + random_suffix()
    ));
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let held = WriteLock::acquire(&target);
    assert!(matches!(held, LockOutcome::Acquired(_)));
    assert!(
        matches!(WriteLock::acquire(&link), LockOutcome::Busy(_)),
        "the same file through a symlink must be refused, not handed a second lock"
    );

    drop(held);
    assert!(
        matches!(WriteLock::acquire(&link), LockOutcome::Acquired(_)),
        "and it must become available once the first writer is done"
    );

    fs::remove_file(&link).ok();
    fs::remove_file(&target).ok();
}

/// The lock sits in a shared temp directory at a path any account on the
/// machine can derive, so it should not be readable by all of them.
#[test]
fn the_lock_file_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let target = write_temp_file(EXAMPLE);
    let held = WriteLock::acquire(&target);
    let LockOutcome::Acquired(lock) = &held else {
        panic!("test setup: the lock must be acquired, got something else");
    };

    let mode = fs::metadata(&lock.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "lock file mode should be 0600, was {mode:o}");

    drop(held);
    fs::remove_file(&target).ok();
}

#[test]
fn a_second_acquire_is_busy_until_the_first_guard_drops() {
    let path = write_temp_file(EXAMPLE);

    let first = WriteLock::acquire(&path);
    assert!(matches!(first, LockOutcome::Acquired(_)));
    assert!(
        matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
        "a second instance must not get in while the first holds it"
    );

    drop(first);
    assert!(
        matches!(WriteLock::acquire(&path), LockOutcome::Acquired(_)),
        "releasing the guard must free the lock"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_live_owner_is_never_displaced_however_long_it_holds_the_lock() {
    // Our own PID is guaranteed alive: an old lock held by a live process must
    // not be recovered.
    let path = write_temp_file(EXAMPLE);
    let lock_file = plant_lock(&path, std::process::id());

    assert!(
        matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
        "a lock whose owner is alive must be honoured regardless of its age"
    );
    assert!(lock_file.exists(), "and must not have been taken over");

    let _ = fs::remove_file(&lock_file);
    fs::remove_file(&path).ok();
}

/// Whether a `ps` can be run at all; slim container images ship none.
fn ps_is_installed() -> bool {
    Command::new("ps")
        .args(["-p", &std::process::id().to_string()])
        .output()
        .is_ok()
}

#[test]
fn a_lock_whose_owner_has_died_is_recovered() {
    // The other half: a crash while holding the lock must not wedge every
    // future write. Recovery is immediate once the owner is *shown* to be
    // gone, rather than waiting out a timeout.
    #[cfg(target_os = "linux")]
    let proc_answers = proc_reports_gone("1").is_some();
    #[cfg(not(target_os = "linux"))]
    let proc_answers = false;
    if !proc_answers && !ps_is_installed() {
        eprintln!("skipped: no /proc and no ps to prove the owner gone");
        return;
    }
    let path = write_temp_file(EXAMPLE);
    plant_lock(&path, a_dead_pid());

    assert!(
        matches!(WriteLock::acquire(&path), LockOutcome::Acquired(_)),
        "a lock left behind by a dead process must be recoverable"
    );

    fs::remove_file(&path).ok();
}

/// A stand-in `ps` running `script` under `sh`; the real arguments
/// (`-p <pid> -o pid=`) arrive as `$1`...
fn stub_ps(script: &str) -> Command {
    let mut ps = Command::new("sh");
    ps.args(["-c", script, "ps"]);
    ps
}

#[test]
fn a_ps_that_rejects_its_arguments_does_not_prove_the_owner_gone() {
    // BusyBox `ps` has no `-p`: it prints usage and exits 1 for every
    // PID, which must not read as "no such process".
    let ps = stub_ps("echo 'ps: unrecognized option: p' >&2; exit 1");
    assert!(!ps_reports_gone(ps, "4242"));
}

#[test]
fn ps_proves_the_owner_gone_only_with_an_empty_listing_and_no_stderr() {
    assert!(ps_reports_gone(stub_ps("exit 1"), "4242"));
    assert!(ps_reports_gone(stub_ps("exit 0"), "4242"));
    assert!(!ps_reports_gone(stub_ps("echo \" $2\""), "4242"));
    assert!(!ps_reports_gone(
        stub_ps("echo warning >&2; exit 0"),
        "4242"
    ));
    assert!(!ps_reports_gone(stub_ps("exit 2"), "4242"));
}

#[test]
fn a_ps_that_cannot_be_run_does_not_prove_the_owner_gone() {
    let ps = Command::new("markcheck-test-no-such-ps-binary");
    assert!(!ps_reports_gone(ps, "4242"));
}

#[test]
fn the_real_ps_tells_a_live_owner_from_a_dead_one() {
    if !ps_is_installed() {
        eprintln!("skipped: ps is not installed");
        return;
    }
    let alive = std::process::id().to_string();
    assert!(!ps_reports_gone(Command::new("ps"), &alive));
    assert!(ps_reports_gone(
        Command::new("ps"),
        &a_dead_pid().to_string()
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn proc_tells_a_live_owner_from_a_dead_one() {
    let alive = std::process::id().to_string();
    assert_eq!(proc_reports_gone(&alive), Some(false));
    assert_eq!(proc_reports_gone(&a_dead_pid().to_string()), Some(true));
}

#[test]
fn an_unreadable_owner_is_treated_as_alive() {
    // Fails closed: anything short of proof that the owner is gone
    // honours the lock. Silently permitting a second writer on an
    // unparseable lock is the failure this whole change is about.
    let path = write_temp_file(EXAMPLE);
    let lock_file = lock_path(&path).unwrap();
    fs::write(&lock_file, "not a token at all").unwrap();

    assert!(
        matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)),
        "an unparseable lock must be honoured, not stolen"
    );

    let _ = fs::remove_file(&lock_file);
    fs::remove_file(&path).ok();
}

#[test]
fn the_lock_file_is_removed_when_the_guard_drops() {
    let path = write_temp_file(EXAMPLE);
    let lock_file = lock_path(&path).unwrap();

    {
        let _held = WriteLock::acquire(&path);
        assert!(lock_file.exists(), "the lock file exists while held");
    }
    assert!(!lock_file.exists(), "and is cleaned up on drop");

    fs::remove_file(&path).ok();
}

#[test]
fn dropping_a_guard_never_removes_a_lock_someone_else_now_holds() {
    // Recovery removes then re-creates, which is not atomic, so a guard
    // must never delete a file it no longer owns.
    let path = write_temp_file(EXAMPLE);
    let lock_file = lock_path(&path).unwrap();

    let mine = WriteLock::acquire(&path);
    assert!(matches!(mine, LockOutcome::Acquired(_)));

    // Another instance takes over, as the recovery path would.
    fs::remove_file(&lock_file).unwrap();
    fs::write(&lock_file, "markcheck 999999 cafe").unwrap();

    drop(mine);

    assert!(
        lock_file.exists(),
        "dropping a guard must not delete a lock another instance now holds"
    );
    assert_eq!(
        fs::read_to_string(&lock_file).unwrap(),
        "markcheck 999999 cafe",
        "and certainly must not replace its contents"
    );

    let _ = fs::remove_file(&lock_file);
    fs::remove_file(&path).ok();
}

#[test]
fn two_checklists_do_not_share_a_lock() {
    let a = write_temp_file(EXAMPLE);
    let b = write_temp_file(EXAMPLE);

    let held_a = WriteLock::acquire(&a);
    assert!(matches!(held_a, LockOutcome::Acquired(_)));
    assert!(
        matches!(WriteLock::acquire(&b), LockOutcome::Acquired(_)),
        "a different checklist must not be blocked by this one"
    );

    fs::remove_file(&a).ok();
    fs::remove_file(&b).ok();
}

#[test]
fn a_write_protected_checklist_is_refused_and_left_unchanged() {
    use std::os::unix::fs::PermissionsExt;
    let path = write_temp_file(EXAMPLE);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    assert!(is_read_only(&path));
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);

    let err = write_back(&document).unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(fs::read_to_string(&path).unwrap(), EXAMPLE);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!is_read_only(&path));
    assert!(!is_read_only(&path.with_extension("missing")));
    fs::remove_file(&path).ok();
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
fn toggling_one_task_leaves_an_untouched_uppercase_x_byte_identical() {
    let mut line = "- [X] done".to_string();
    set_task_marker(&mut line, "[x]");
    assert_eq!(line, "- [X] done");

    let path = write_temp_file("## S\n\n- [X] done upper\n- [ ] other\n");
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "## S\n\n- [X] done upper\n- [x] other\n"
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
fn toggling_below_front_matter_keeps_the_block_byte_for_byte() {
    let source = "---\ntitle: Weekly ops\ntags: [ops]\n---\n\n- [ ] first task\n";
    let path = write_temp_file(source);
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert_eq!(written, source.replace("- [ ]", "- [x]"));

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
    // An empty source must round-trip without gaining a newline.
    let path = write_temp_file("");
    let document = parse_document(path.clone()).unwrap();
    assert!(document.lists.is_empty());
    assert!(document.raw_lines.is_empty());

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
    assert!(
        !document.raw_lines.last().unwrap().ends_with('\n'),
        "source has no final newline"
    );

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
    assert!(
        document.raw_lines.last().unwrap().ends_with('\n'),
        "source has a final newline"
    );

    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();

    let written = fs::read_to_string(&path).unwrap();
    assert_eq!(written, "## S\n\n- [x] `task`\n");

    fs::remove_file(&path).ok();
}

#[test]
fn a_toggle_changes_one_marker_and_no_other_byte_for_any_line_ending() {
    // Each line keeps its own terminator, so only the checkbox changes for
    // CR-only and mixed endings too.
    for (name, source, expected) in [
        (
            "LF",
            "## S\n\n- [ ] alpha\n- [ ] beta\n",
            "## S\n\n- [x] alpha\n- [ ] beta\n",
        ),
        (
            "CRLF",
            "## S\r\n\r\n- [ ] alpha\r\n- [ ] beta\r\n",
            "## S\r\n\r\n- [x] alpha\r\n- [ ] beta\r\n",
        ),
        (
            "CR-only",
            "## S\r\r- [ ] alpha\r- [ ] beta\r",
            "## S\r\r- [x] alpha\r- [ ] beta\r",
        ),
        (
            "mixed",
            "## S\r\n\r\n- [ ] alpha\n- [ ] beta\r\n",
            "## S\r\n\r\n- [x] alpha\n- [ ] beta\r\n",
        ),
        (
            "no final newline",
            "## S\n\n- [ ] alpha\n- [ ] beta",
            "## S\n\n- [x] alpha\n- [ ] beta",
        ),
    ] {
        let path = write_temp_file(source);
        let mut document = parse_document(path.clone()).unwrap();
        let alpha = document
            .lists
            .iter_mut()
            .flat_map(|list| list.items.iter_mut())
            .find(|i| i.display_text == "alpha")
            .unwrap_or_else(|| panic!("{name}: alpha must parse as an item"));
        alpha.kind = ItemKind::Checkbox(TaskState::Done);

        write_back(&document).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            expected,
            "{name}: exactly one marker may change, every other byte \
                 (terminators included) must be identical"
        );
        fs::remove_file(&path).ok();
    }
}

#[test]
fn round_trip_preserves_crlf_line_endings() {
    // A file authored with CRLF line endings must keep them after a
    // toggle — write_back must not silently flip the whole file to LF
    // just because one marker byte changed.
    let crlf_example = EXAMPLE.replace('\n', "\r\n");
    let path = write_temp_file(&crlf_example);
    let mut document = parse_document(path.clone()).unwrap();
    assert!(
        document.raw_lines.iter().all(|l| l.ends_with("\r\n")),
        "CRLF source must keep every CRLF ending"
    );

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

#[test]
fn write_back_preserves_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    for mode in [0o600u32, 0o640] {
        let path = write_temp_file(EXAMPLE);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();

        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(actual, mode, "permissions changed for mode {mode:o}");

        fs::remove_file(&path).ok();
    }
}

#[test]
fn write_back_clears_setuid_setgid_and_sticky_bits() {
    use std::os::unix::fs::PermissionsExt;

    for (mode, expected) in [(0o6755u32, 0o755u32), (0o4700, 0o700), (0o1644, 0o644)] {
        let path = write_temp_file(EXAMPLE);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let before = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(before, mode, "test setup must apply mode {mode:o}");

        let mut document = parse_document(path.clone()).unwrap();
        document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
        write_back(&document).unwrap();

        let actual = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            actual, expected,
            "special bits must be dropped from mode {mode:o}"
        );

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
#[test]
fn write_back_through_canonicalized_symlink_keeps_link_intact() {
    use std::os::unix::fs::PermissionsExt;

    let target = write_temp_file(EXAMPLE);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

    let link_dir = crate::test_support::unique_temp_path("writer", "linkdir", None);
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

#[test]
fn write_temp_retries_at_a_new_name_instead_of_deleting_a_collision() {
    // A pre-existing file at the temp name must be left untouched.
    let base = crate::test_support::unique_temp_path("writer", "collision", None);
    fs::write(&base, "a live process's own temp file, not markcheck's\n").unwrap();

    let source = fs::metadata(&base).unwrap();
    let written_path = write_temp(&base, "checklist content\n", &source).unwrap();

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

fn temp_path() -> PathBuf {
    crate::test_support::unique_temp_path("writer", "", Some("md"))
}

#[test]
fn read_checklist_reads_a_regular_file() {
    let path = temp_path();
    fs::write(&path, "- [ ] a\n").unwrap();
    assert_eq!(read_checklist(&path).unwrap(), b"- [ ] a\n");
}

#[test]
fn read_checklist_refuses_a_symlink_a_fifo_and_a_directory() {
    let target = temp_path();
    fs::write(&target, "- [ ] private\n").unwrap();
    let link = temp_path();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let fifo = temp_path();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let dir = temp_path();
    fs::create_dir(&dir).unwrap();

    let device = PathBuf::from("/dev/null");
    for (path, what) in [
        (link, "a symlink"),
        (fifo, "a named pipe"),
        (dir, "a directory"),
        (device, "a device or socket"),
    ] {
        let err = within_seconds(10, {
            let path = path.clone();
            move || read_checklist(&path)
        })
        .unwrap_err();
        assert!(is_not_regular(&err), "{}: {err}", path.display());
        assert_eq!(err.to_string(), format!("not a regular file: it is {what}"));
    }
    assert!(!is_not_regular(&read_checklist(&temp_path()).unwrap_err()));
}

#[test]
fn write_back_refuses_a_symlink_and_leaves_its_target_alone() {
    let path = temp_path();
    fs::write(&path, "- [ ] a\n").unwrap();
    let mut doc = parse_document(path.clone()).unwrap();
    let target = temp_path();
    fs::write(&target, "- [ ] private\n").unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    doc.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
    let err = write_back(&doc).unwrap_err();

    assert!(is_not_regular(&err), "{err}");
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), "- [ ] private\n");
}

#[test]
fn remove_leftovers_removes_only_a_dead_markcheck_s_files() {
    let dir = crate::test_support::unique_temp_path("writer", "leftovers", None);
    fs::create_dir(&dir).unwrap();
    let dead = a_dead_pid();
    let live = std::process::id();
    let names = [
        format!("markcheck-index-{dead}-1f"),
        format!("markcheck-index-{dead}-2a.lock"),
        format!("markcheck-index-{dead}-3b-4c"),
        format!("markcheck-index-{live}-5d"),
        "markcheck-index-1".to_string(),
        format!("markcheck-index-{dead}-zz"),
        format!("markcheck-index-{dead}-"),
        format!("other-{dead}-1f"),
    ];
    for name in &names {
        fs::write(dir.join(name), "").unwrap();
        age_past_the_sweep(&dir.join(name));
    }
    fs::create_dir(dir.join(format!("markcheck-index-{dead}-6e"))).unwrap();

    assert_eq!(remove_leftovers(&dir, "markcheck-index-"), 3);

    let mut left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    let mut expected: Vec<String> = names[3..].to_vec();
    expected.push(format!("markcheck-index-{dead}-6e"));
    expected.sort();
    assert_eq!(
        left, expected,
        "a live PID, a malformed name and a directory stay"
    );
    assert_eq!(
        remove_leftovers(&dir.join("missing"), "markcheck-index-"),
        0
    );
}

#[test]
fn remove_leftovers_keeps_a_young_file_whose_pid_looks_gone() {
    let dir = crate::test_support::unique_temp_path("writer", "leftovers-young", None);
    fs::create_dir(&dir).unwrap();
    let dead = a_dead_pid();
    let young = dir.join(format!("markcheck-index-{dead}-1f"));
    fs::write(&young, "").unwrap();

    assert_eq!(
        remove_leftovers(&dir, "markcheck-index-"),
        0,
        "another host's or namespace's live sync can look like a dead PID"
    );
    assert!(young.exists());

    let past_a_commit_timeout =
        std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 60);
    fs::File::options()
        .write(true)
        .open(&young)
        .unwrap()
        .set_modified(past_a_commit_timeout)
        .unwrap();
    assert_eq!(
        remove_leftovers(&dir, "markcheck-index-"),
        0,
        "a sync stuck in a commit's 300-second timeout still holds its files"
    );
    assert!(young.exists());

    age_past_the_sweep(&young);
    assert_eq!(remove_leftovers(&dir, "markcheck-index-"), 1);
    assert!(!young.exists());
}

#[test]
fn remove_leftovers_keeps_another_user_s_file() {
    let dir = crate::test_support::unique_temp_path("writer", "leftovers-owner", None);
    fs::create_dir(&dir).unwrap();
    let dead = a_dead_pid();
    let file = dir.join(format!("markcheck-index-{dead}-1f"));
    fs::write(&file, "").unwrap();
    let other_uid = current_uid().wrapping_add(1);

    assert_eq!(
        remove_leftovers_owned_by(
            &dir,
            "markcheck-index-",
            other_uid,
            std::time::Duration::ZERO
        ),
        0
    );
    assert!(file.exists());
    assert_eq!(
        remove_leftovers_owned_by(
            &dir,
            "markcheck-index-",
            current_uid(),
            std::time::Duration::ZERO
        ),
        1
    );
}

#[test]
fn sweep_interrupted_saves_removes_a_dead_save_s_temp_files_beside_the_checklist() {
    let dir = crate::test_support::unique_temp_path("writer", "sweep", None);
    fs::create_dir(&dir).unwrap();
    let checklist = dir.join("runbook.md");
    fs::write(&checklist, "- [ ] a\n").unwrap();
    let dead = a_dead_pid();
    for name in [
        format!(".runbook.md.markcheck-tmp-{dead}-1f"),
        format!(".runbook.md.markcheck-tmp-{dead}-1f-2e"),
        format!(".runbook.md.markcheck-new-{dead}-3d"),
        format!(".other.md.markcheck-tmp-{dead}-4c"),
    ] {
        fs::write(dir.join(&name), "- [x] a\n").unwrap();
        age_past_the_sweep(&dir.join(name));
    }

    assert_eq!(sweep_interrupted_saves(&checklist), 3);
    assert!(
        dir.join(format!(".other.md.markcheck-tmp-{dead}-4c"))
            .exists()
    );
    assert!(checklist.exists());
    assert_eq!(sweep_interrupted_saves(Path::new("/")), 0);
}

#[test]
fn a_lock_planted_in_the_shared_temp_dir_no_longer_blocks_saving() {
    // Where every user's locks used to live, with a PID that never dies.
    let path = write_temp_file(EXAMPLE);
    let name = lock_path(&path).unwrap().file_name().unwrap().to_owned();
    let shared = std::env::temp_dir().join(name);
    fs::write(&shared, "markcheck 1 deadbeef").unwrap();

    assert!(matches!(
        WriteLock::acquire(&path),
        LockOutcome::Acquired(_)
    ));

    let _ = fs::remove_file(&shared);
    fs::remove_file(&path).ok();
}

#[test]
fn lock_dir_in_uses_only_a_directory_private_to_the_user() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let base = crate::test_support::unique_temp_path("writer", "lockdir", None);
    fs::create_dir(&base).unwrap();
    let uid = fs::metadata(&base).unwrap().uid();
    let runtime = base.join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let temp = base.join("tmp");
    fs::create_dir(&temp).unwrap();

    assert_eq!(
        lock_dir_in(Some(runtime.as_os_str()), &temp, uid),
        Some(runtime.clone())
    );

    let fallback = temp.join(format!("markcheck-{uid}"));
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        lock_dir_in(Some(runtime.as_os_str()), &temp, uid),
        Some(fallback.clone()),
        "a runtime dir others can read is passed over"
    );
    assert_eq!(
        fs::metadata(&fallback).unwrap().mode() & 0o777,
        0o700,
        "the fallback is created private"
    );
    assert_eq!(
        lock_dir_in(Some(std::ffi::OsStr::new("relative")), &temp, uid),
        Some(fallback.clone())
    );
    assert_eq!(lock_dir_in(None, &temp, uid + 1), None, "someone else's");

    fs::set_permissions(&fallback, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(lock_dir_in(None, &temp, uid), None, "opened to others");

    fs::remove_dir(&fallback).unwrap();
    std::os::unix::fs::symlink(&runtime, &fallback).unwrap();
    assert_eq!(
        lock_dir_in(None, &temp, uid),
        None,
        "a symlink squatting the name"
    );
}

#[test]
fn a_lock_that_is_a_symlink_to_a_fifo_neither_blocks_nor_is_honoured() {
    let path = write_temp_file(EXAMPLE);
    let lock_file = lock_path(&path).unwrap();
    let fifo = temp_path();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink(&fifo, &lock_file).unwrap();

    let outcome = within_seconds(10, {
        let path = path.clone();
        move || matches!(WriteLock::acquire(&path), LockOutcome::Unavailable)
    });
    assert!(outcome);

    let _ = fs::remove_file(&lock_file);
    fs::remove_file(&path).ok();
}

#[test]
fn read_lock_token_reads_at_most_a_small_prefix() {
    let path = temp_path();
    fs::write(&path, "x".repeat(10_000)).unwrap();
    assert_eq!(read_lock_token(&path).unwrap().len(), 256);
}

/// A directory the test can make read-only, or `None` when the user can
/// write to it anyway (root), where the case can't be set up.
fn read_only_dir_with(name: &str, contents: &str) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_support::unique_temp_path("writer", "ro-dir", None);
    fs::create_dir(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    if fs::write(dir.join("probe"), "").is_ok() {
        eprintln!("skipped: the directory stays writable (running as root?)");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        return None;
    }
    Some((dir, path))
}

#[test]
fn a_checklist_in_a_read_only_folder_is_saved_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let Some((dir, path)) = read_only_dir_with("list.md", "- [ ] a\n") else {
        return;
    };
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);

    let saved = write_back(&document).unwrap();

    assert!(saved.in_place);
    assert_eq!(saved.contents, "- [x] a\n");
    assert_eq!(fs::read_to_string(&path).unwrap(), "- [x] a\n");
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        1,
        "no temp file was left"
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}

/// A file over `data` whose writes fail with a full disk once `fail_at`
/// bytes are written, recording each length cut and fsync.
struct FlakyFile {
    data: std::cell::RefCell<Vec<u8>>,
    pos: usize,
    fail_at: usize,
    calls: std::cell::RefCell<Vec<String>>,
}

impl FlakyFile {
    fn new(data: &str, fail_at: usize) -> Self {
        FlakyFile {
            data: std::cell::RefCell::new(data.as_bytes().to_vec()),
            pos: 0,
            fail_at,
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn text(&self) -> String {
        String::from_utf8(self.data.borrow().clone()).unwrap()
    }
}

impl io::Write for FlakyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.fail_at {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        let n = buf.len().min(self.fail_at - self.pos);
        let end = self.pos + n;
        let mut data = self.data.borrow_mut();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[self.pos..end].copy_from_slice(&buf[..n]);
        self.pos = end;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Rewritable for FlakyFile {
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.calls.borrow_mut().push(format!("set_len {len}"));
        self.data.borrow_mut().resize(len as usize, 0);
        Ok(())
    }

    fn sync_all(&self) -> io::Result<()> {
        self.calls.borrow_mut().push("sync".to_string());
        Ok(())
    }
}

#[test]
fn an_in_place_write_that_fails_part_way_leaves_a_whole_checklist() {
    let old = "- [ ] a\n- [ ] b\n";
    let new = "- [x] a\n- [x] b\n";
    let mut file = FlakyFile::new(old, 10);

    let err = rewrite(&mut file, new.as_bytes()).unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    assert_eq!(file.text(), "- [x] a\n- [ ] b\n", "never truncated");
    assert!(file.calls.borrow().is_empty(), "{:?}", file.calls.borrow());
}

#[test]
fn an_in_place_write_to_a_real_file_cuts_it_to_length() {
    let target = write_temp_file("- [ ] a\n- [ ] b\n");

    write_in_place(&target, "- [x] a\n").unwrap();

    assert_eq!(fs::read_to_string(&target).unwrap(), "- [x] a\n");
}

#[test]
fn an_in_place_write_cuts_a_longer_file_to_length_and_syncs() {
    let mut file = FlakyFile::new("- [ ] a\n- [ ] b\n", usize::MAX);

    rewrite(&mut file, b"- [x] a\n").unwrap();

    assert_eq!(file.text(), "- [x] a\n");
    assert_eq!(*file.calls.borrow(), ["set_len 8", "sync"]);
}

#[test]
fn a_very_long_checklist_name_can_still_be_saved() {
    let dir = crate::test_support::unique_temp_path("writer", "long-name", None);
    fs::create_dir(&dir).unwrap();
    let path = dir.join(format!("{}.md", "n".repeat(227)));
    fs::write(&path, "- [ ] a\n").unwrap();
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);

    let saved = write_back(&document).unwrap();

    assert!(!saved.in_place, "the temp-and-rename path still worked");
    assert_eq!(fs::read_to_string(&path).unwrap(), "- [x] a\n");
}

#[test]
fn temp_name_stem_cuts_a_long_name_at_a_character_boundary() {
    assert_eq!(temp_name_stem("short.md"), "short.md");
    let long = "é".repeat(100);
    let stem = temp_name_stem(&long);
    assert!(stem.len() <= 160 && stem.len() >= 159, "{}", stem.len());
    assert!(long.starts_with(stem));
}

#[test]
fn describe_write_error_names_the_common_causes_plainly() {
    use io::ErrorKind::*;
    for (kind, expected) in [
        (
            PermissionDenied,
            "no permission to write the file or its folder",
        ),
        (StorageFull, "the disk is full"),
        (QuotaExceeded, "your disk quota is used up"),
        (ReadOnlyFilesystem, "the disk is mounted read-only"),
        (NotFound, "the file or its folder no longer exists"),
        (InvalidFilename, "the file name is too long to save next to"),
    ] {
        assert_eq!(describe_write_error(&io::Error::from(kind)), expected);
    }
    assert_eq!(
        describe_write_error(&io::Error::other("something odd")),
        "something odd"
    );
}

/// Every checkbox's state, by line.
fn states_by_line(document: &crate::model::Document) -> Vec<(usize, TaskState)> {
    document
        .lists
        .iter()
        .flat_map(|list| &list.items)
        .filter_map(|item| match item.kind {
            ItemKind::Checkbox(state) => Some((item.line_number, state)),
            ItemKind::DisplayOnly => None,
        })
        .collect()
}

proptest::proptest! {
    // 256 cases (`PROPTEST_CASES` raises it for a longer run). Shrunk
    // failures become named regression tests rather than a
    // proptest-regressions file.
    #![proptest_config(proptest::prelude::ProptestConfig {
        failure_persistence: None,
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// A toggle rewrites one marker and no other byte, and a re-parse sees the
    /// new state there and the old one everywhere else (#79).
    #[test]
    fn one_toggle_changes_one_marker_and_nothing_else(
        source in crate::test_support::markdown::document(),
        pick in proptest::prelude::any::<proptest::sample::Index>(),
        state in proptest::sample::select(vec![
            TaskState::NotStarted,
            TaskState::Started,
            TaskState::Done,
            TaskState::Skipped,
        ]),
    ) {
        let path = write_temp_file(&source);
        let mut document = parse_document(path.clone()).unwrap();
        let tasks: Vec<(usize, usize)> = document
            .lists
            .iter()
            .enumerate()
            .flat_map(|(li, list)| {
                list.items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| matches!(item.kind, ItemKind::Checkbox(_)))
                    .map(move |(ii, _)| (li, ii))
            })
            .collect();
        if tasks.is_empty() {
            return Ok(());
        }
        let (li, ii) = tasks[pick.index(tasks.len())];
        let line = document.lists[li].items[ii].line_number;
        let expected: Vec<(usize, TaskState)> = states_by_line(&document)
            .into_iter()
            .map(|(at, old)| (at, if at == line { state } else { old }))
            .collect();

        document.lists[li].items[ii].kind = ItemKind::Checkbox(state);
        write_back(&document).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        let before = crate::parser::split_lines_keeping_endings(&source);
        let after = crate::parser::split_lines_keeping_endings(&written);
        proptest::prop_assert_eq!(before.len(), after.len());
        for (index, (old, new)) in before.iter().zip(&after).enumerate() {
            if index + 1 != line {
                proptest::prop_assert_eq!(old, new, "line {} changed", index + 1);
                continue;
            }
            proptest::prop_assert_eq!(old.len(), new.len());
            let changed: Vec<usize> = old
                .bytes()
                .zip(new.bytes())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(at, _)| at)
                .collect();
            if let (Some(first), Some(last)) = (changed.first(), changed.last()) {
                proptest::prop_assert!(last - first < 3, "more than a marker changed: {old:?} -> {new:?}");
            }
        }
        let reparsed = parse_document(path).unwrap();
        proptest::prop_assert_eq!(states_by_line(&reparsed), expected);
    }
}

#[test]
fn write_back_writes_and_replaces_a_skipped_marker() {
    let path = write_temp_file("- [ ] a\n- [-] b\n");
    let mut document = parse_document(path.clone()).unwrap();
    document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Skipped);
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Done);
    write_back(&document).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "- [-] a\n- [x] b\n");
}

#[test]
fn a_second_session_sees_the_live_one_and_clears_a_dead_ones_marker() {
    let target = write_temp_file(EXAMPLE);
    let dir = lock_dir().unwrap();
    let prefix = format!("markcheck-open-{}-", path_digest(&target));
    let live = std::os::unix::process::parent_id();
    let dead = a_dead_pid();
    fs::write(dir.join(format!("{prefix}{live}")), "").unwrap();
    fs::write(dir.join(format!("{prefix}{dead}")), "").unwrap();

    let (marker, others) = SessionMarker::register(&target);

    assert_eq!(others.len(), 1, "{others:?}");
    assert_eq!(others[0].pid, live);
    assert!(others[0].open_for.is_some());
    assert!(
        !dir.join(format!("{prefix}{dead}")).exists(),
        "the dead marker is gone"
    );
    let own = dir.join(format!("{prefix}{}", std::process::id()));
    assert!(own.exists(), "this session is recorded");
    drop(marker);
    assert!(!own.exists(), "and removed again on exit");
    let _ = fs::remove_file(dir.join(format!("{prefix}{live}")));
}

#[cfg(target_os = "linux")]
#[test]
fn a_marker_whose_pid_was_reused_by_another_process_is_swept() {
    let parent = std::os::unix::process::parent_id().to_string();
    let Some(identity) = process_identity(Path::new("/proc"), &parent) else {
        eprintln!("skipped: /proc offers no boot ID or start time");
        return;
    };
    let target = write_temp_file(EXAMPLE);
    let dir = lock_dir().unwrap();
    let marker = dir.join(format!("markcheck-open-{}-{parent}", path_digest(&target)));
    let (boot_id, _) = identity.split_once(' ').unwrap();
    fs::write(&marker, format!("{boot_id} 1")).unwrap();

    let (own, others) = SessionMarker::register(&target);

    assert!(others.is_empty(), "{others:?}");
    assert!(!marker.exists(), "the reused PID's marker is swept");
    drop(own);

    fs::write(&marker, &identity).unwrap();
    let (own, others) = SessionMarker::register(&target);
    assert_eq!(others.len(), 1, "the same process is still reported");
    drop(own);
    let _ = fs::remove_file(marker);
}

#[cfg(target_os = "linux")]
#[test]
fn this_sessions_marker_records_its_identity() {
    let target = write_temp_file(EXAMPLE);
    let (marker, _) = SessionMarker::register(&target);
    let path = marker.path.clone().unwrap();
    let pid = std::process::id().to_string();
    assert_eq!(
        Some(fs::read_to_string(&path).unwrap()),
        process_identity(Path::new("/proc"), &pid)
    );
}

/// A stand-in `/proc` with the given boot ID, uptime and `stat` lines.
fn fake_proc(boot_id: Option<&str>, uptime: Option<&str>, stats: &[(&str, &str)]) -> PathBuf {
    let proc = crate::test_support::unique_temp_path("writer", "proc", None);
    fs::create_dir_all(proc.join("sys/kernel/random")).unwrap();
    if let Some(boot_id) = boot_id {
        fs::write(proc.join("sys/kernel/random/boot_id"), boot_id).unwrap();
    }
    if let Some(uptime) = uptime {
        fs::write(proc.join("uptime"), uptime).unwrap();
    }
    for (pid, stat) in stats {
        fs::create_dir_all(proc.join(pid)).unwrap();
        fs::write(proc.join(pid).join("stat"), stat).unwrap();
    }
    proc
}

const A_STAT: &str =
    "812 (odd) name)) S 1 812 812 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10\n";

#[test]
fn process_identity_reads_the_boot_id_and_start_time() {
    let proc = fake_proc(
        Some("abc-123\n"),
        None,
        &[("812", A_STAT), ("9", "9 (short) S 1")],
    );
    assert_eq!(
        process_identity(&proc, "812").as_deref(),
        Some("abc-123 98765")
    );
    assert_eq!(process_identity(&proc, "9"), None, "a stat cut short");
    assert_eq!(process_identity(&proc, "77"), None, "no such process");
    let no_boot_id = fake_proc(None, None, &[("812", A_STAT)]);
    assert_eq!(process_identity(&no_boot_id, "812"), None);
    let _ = fs::remove_dir_all(proc);
    let _ = fs::remove_dir_all(no_boot_id);
}

#[test]
fn a_marker_is_stale_when_its_identity_differs() {
    let proc = fake_proc(Some("abc-123"), Some("100.5 200.0\n"), &[("812", A_STAT)]);
    let now = Some(std::time::SystemTime::now());
    assert!(!marker_is_stale(&proc, "812", "abc-123 98765\n", now));
    assert!(marker_is_stale(&proc, "812", "abc-123 11111", now));
    assert!(marker_is_stale(&proc, "812", "other-boot 98765", now));
    let _ = fs::remove_dir_all(proc);
}

#[test]
fn without_an_identity_a_marker_is_stale_only_when_older_than_the_boot() {
    use std::time::{Duration, SystemTime};
    let proc = fake_proc(None, Some("3600.25 7200.00\n"), &[]);
    let before_boot = SystemTime::now().checked_sub(Duration::from_secs(7200));
    let since_boot = SystemTime::now().checked_sub(Duration::from_secs(60));
    assert!(marker_is_stale(&proc, "812", "", before_boot));
    assert!(marker_is_stale(&proc, "812", "abc-123 98765", before_boot));
    assert!(!marker_is_stale(&proc, "812", "", since_boot));
    assert!(!marker_is_stale(&proc, "812", "", None));

    for uptime in [None, Some("garbage"), Some("-5 0"), Some("inf 0"), Some("")] {
        let proc = fake_proc(None, uptime, &[]);
        assert!(
            !marker_is_stale(&proc, "812", "", before_boot),
            "{uptime:?} proves nothing"
        );
        let _ = fs::remove_dir_all(proc);
    }
    let _ = fs::remove_dir_all(proc);
}

#[test]
fn a_file_owned_by_root_is_reported_as_someone_elses() {
    use std::os::unix::fs::MetadataExt;
    let target = Path::new("/etc/hostname");
    if !target.exists() || current_uid() == 0 {
        eprintln!("skipped: needs a root-owned file and a non-root user");
        return;
    }
    assert_eq!(fs::metadata(target).unwrap().uid(), 0);
    assert_eq!(foreign_owner(target), Some(0));
}

#[test]
fn a_lock_held_by_a_live_process_is_busy() {
    let path = write_temp_file(EXAMPLE);
    let lock = plant_lock(&path, std::os::unix::process::parent_id());
    assert!(matches!(WriteLock::acquire(&path), LockOutcome::Busy(_)));
    let _ = fs::remove_file(lock);
}

#[test]
fn temp_name_stem_backs_up_to_a_character_boundary() {
    // A one-byte lead puts every two-byte boundary at an odd offset, so the
    // 160-byte cut lands mid-character and has to move back.
    let long = format!("a{}", "é".repeat(100));
    let stem = within_seconds(10, {
        let long = long.clone();
        move || temp_name_stem(&long).len()
    });
    assert_eq!(stem, 159);
    let stem = &long[..stem];
    assert!(long.starts_with(stem));
}

/// Runs `f` on its own thread and fails the test when it hasn't returned
/// within `secs`, so a guard that regresses into blocking (a FIFO opened
/// without `O_NONBLOCK`, a loop that never ends) fails instead of hanging.
fn within_seconds<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(secs))
        .expect("did not return in time")
}

#[test]
fn a_marker_whose_pid_part_is_not_a_number_is_left_alone() {
    let target = write_temp_file(EXAMPLE);
    let dir = lock_dir().unwrap();
    let prefix = format!("markcheck-open-{}-", path_digest(&target));
    let odd = dir.join(format!("{prefix}abc"));
    fs::write(&odd, "").unwrap();

    let (marker, others) = SessionMarker::register(&target);

    assert!(others.is_empty(), "{others:?}");
    assert!(odd.exists(), "not ours to remove");
    drop(marker);
    let _ = fs::remove_file(odd);
}

#[test]
fn a_marker_left_under_this_pid_is_taken_over_and_a_directory_is_not() {
    let target = write_temp_file(EXAMPLE);
    let dir = lock_dir().unwrap();
    let own = dir.join(format!(
        "markcheck-open-{}-{}",
        path_digest(&target),
        std::process::id()
    ));
    fs::write(&own, "stale identity").unwrap();
    let (marker, _) = SessionMarker::register(&target);
    assert_eq!(marker.path.as_deref(), Some(own.as_path()));
    assert_ne!(fs::read_to_string(&own).unwrap(), "stale identity");
    drop(marker);
    assert!(!own.exists());

    fs::create_dir(&own).unwrap();
    let (marker, _) = SessionMarker::register(&target);
    assert_eq!(marker.path, None, "a directory isn't a marker");
    drop(marker);
    fs::remove_dir(&own).unwrap();
}

#[test]
fn save_route_follows_what_the_file_and_folder_allow() {
    use std::os::unix::fs::PermissionsExt;
    let own = write_temp_file(EXAMPLE);
    assert_eq!(save_route(&own), SaveRoute::Replace);
    fs::remove_file(&own).ok();

    let Some((dir, path)) = read_only_dir_with("list.md", "- [ ] a\n") else {
        return;
    };
    assert_eq!(save_route(&path), SaveRoute::InPlace);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(save_route(&path), SaveRoute::Refused);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_bare_file_name_is_judged_by_the_current_folder() {
    let missing = Path::new("markcheck-no-such-checklist.md");
    assert_eq!(save_route(missing), SaveRoute::Replace);
    assert!(!may_access(
        Path::new("nul\0byte"),
        rustix::fs::Access::WRITE_OK
    ));
}

#[test]
fn a_root_owned_file_in_a_root_owned_folder_cannot_be_saved() {
    let target = Path::new("/etc/hostname");
    if !target.exists() || current_uid() == 0 {
        eprintln!("skipped: needs a root-owned file and a non-root user");
        return;
    }
    assert_eq!(save_route(target), SaveRoute::Refused);
}

#[test]
fn save_route_for_prefers_a_rename_then_in_place() {
    assert_eq!(save_route_for(true, true, false), SaveRoute::Replace);
    assert_eq!(save_route_for(false, true, false), SaveRoute::Replace);
    assert_eq!(save_route_for(true, true, true), SaveRoute::InPlace);
    assert_eq!(save_route_for(true, false, false), SaveRoute::InPlace);
    assert_eq!(save_route_for(false, true, true), SaveRoute::Refused);
    assert_eq!(save_route_for(false, false, false), SaveRoute::Refused);
}

#[test]
fn a_sticky_folder_refuses_only_a_stranger_the_rename() {
    let sticky = 0o41777;
    assert!(sticky_refuses_rename(sticky, 0, 2000, 1000));
    assert!(!sticky_refuses_rename(0o40777, 0, 2000, 1000), "not sticky");
    assert!(!sticky_refuses_rename(sticky, 0, 1000, 1000), "own file");
    assert!(
        !sticky_refuses_rename(sticky, 1000, 2000, 1000),
        "own folder"
    );
    assert!(!sticky_refuses_rename(sticky, 3000, 2000, 0), "root");
}

#[test]
fn user_label_names_a_known_user_and_numbers_an_unknown_one() {
    assert_eq!(user_label(0), "root");
    assert_eq!(user_label(4_000_000_123), "4000000123");
}

#[test]
fn a_refused_rename_falls_back_to_writing_in_place() {
    let target = write_temp_file("- [ ] a\n");
    let temp = target.with_extension("tmp");
    fs::write(&temp, "- [x] a\n").unwrap();
    let refused = Err(io::Error::from(io::ErrorKind::PermissionDenied));

    assert!(settle_rename(refused, &temp, &target, "- [x] a\n").unwrap());

    assert_eq!(fs::read_to_string(&target).unwrap(), "- [x] a\n");
    assert!(!temp.exists(), "the temp file is removed");
    fs::remove_file(&target).ok();
}

#[test]
fn a_rename_failing_otherwise_is_reported_and_cleaned_up() {
    let target = write_temp_file("- [ ] a\n");
    let temp = target.with_extension("tmp");
    fs::write(&temp, "- [x] a\n").unwrap();
    let failed = Err(io::Error::from(io::ErrorKind::StorageFull));

    let err = settle_rename(failed, &temp, &target, "- [x] a\n").unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    assert_eq!(fs::read_to_string(&target).unwrap(), "- [ ] a\n");
    assert!(!temp.exists(), "the temp file is removed");
    assert!(!settle_rename(Ok(()), &temp, &target, "").unwrap());
    fs::remove_file(&target).ok();
}

#[test]
fn a_refused_rename_of_a_file_nobody_may_write_stays_a_failure() {
    let missing = write_temp_file("").with_extension("gone");
    let temp = missing.with_extension("tmp");
    let refused = Err(io::Error::from(io::ErrorKind::PermissionDenied));

    let err = settle_rename(refused, &temp, &missing, "- [x] a\n").unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}
