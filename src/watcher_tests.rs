use super::*;
use std::fs;
use std::time::{Duration, Instant};

fn unique_temp_path(name_hint: &str) -> std::path::PathBuf {
    crate::test_support::unique_temp_path("watcher", name_hint, Some("md"))
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn detects_write_to_watched_file() {
    let path = unique_temp_path("target");
    fs::write(&path, "initial").unwrap();
    let watcher = FileWatcher::new(&path).unwrap();

    fs::write(&path, "changed").unwrap();

    assert!(
        wait_until(|| watcher.poll_changed()),
        "expected a change event for the watched file"
    );

    fs::remove_file(&path).ok();
}

/// With the canonicalized path (as main.rs resolves at startup), the
/// watcher watches the symlink target's parent directory, so edits to
/// the target are detected even when the symlink lives in a different
/// directory.
#[test]
fn detects_target_changes_through_canonicalized_symlink() {
    let target = unique_temp_path("linktarget");
    fs::write(&target, "initial").unwrap();

    let link_dir = crate::test_support::unique_temp_path("watcher", "linkdir", None);
    fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("linked.md");
    fs::remove_file(&link).ok();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let canonical = fs::canonicalize(&link).unwrap();
    let watcher = FileWatcher::new(&canonical).unwrap();

    fs::write(&target, "changed").unwrap();

    assert!(
        wait_until(|| watcher.poll_changed()),
        "expected a change event for the symlink target"
    );

    fs::remove_file(&link).ok();
    fs::remove_dir(&link_dir).ok();
    fs::remove_file(&target).ok();
}

#[test]
fn detects_delete_then_recreate_of_the_watched_file() {
    // Some editors save by deleting the target and writing a fresh file
    // at the same path, rather than a rename. Since the watch is on the
    // *directory* and events are matched by filename (not inode), this
    // should be detected exactly like an atomic-rename save.
    let path = unique_temp_path("target");
    fs::write(&path, "initial").unwrap();
    let watcher = FileWatcher::new(&path).unwrap();

    fs::remove_file(&path).unwrap();
    fs::write(&path, "recreated").unwrap();

    assert!(
        wait_until(|| watcher.poll_changed()),
        "expected a change event after delete-then-recreate"
    );

    fs::remove_file(&path).ok();
}

fn state_for(path: &Path) -> AppState {
    AppState::new(crate::parser::parse_document(path.to_path_buf()).unwrap())
}

fn write_with_new_mtime(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mtime = fs::metadata(path).unwrap().modified().unwrap() + Duration::from_secs(1);
    fs::File::open(path).unwrap().set_modified(mtime).unwrap();
}

#[test]
fn a_failed_watch_is_reported_and_the_timer_still_picks_up_edits() {
    let dir = crate::test_support::unique_temp_path("watcher", "nodir", None);
    let missing = dir.join("list.md");
    let start = Instant::now();
    let (mut trigger, error) = ReloadTrigger::new(&missing, start);
    assert!(error.is_some(), "watching a missing directory must fail");
    assert!(trigger.watcher.is_none());

    fs::create_dir_all(&dir).unwrap();
    fs::write(&missing, "## List\n\n- [ ] one\n").unwrap();
    let mut state = state_for(&missing);
    write_with_new_mtime(&missing, "## List\n\n- [ ] one\n- [ ] two\n");

    assert!(!trigger.check(&mut state, start), "not due yet");
    assert_eq!(state.document.lists[0].items.len(), 1);

    assert!(trigger.check(&mut state, start + RECHECK_INTERVAL));
    assert_eq!(state.document.lists[0].items.len(), 2);
    assert!(
        trigger.watcher.is_some(),
        "a change the watch missed sets the watch up again"
    );
    assert!(
        !trigger.check(&mut state, start + RECHECK_INTERVAL * 2),
        "nothing changed since"
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_removed_and_recreated_directory_clears_the_deleted_state() {
    let dir = crate::test_support::unique_temp_path("watcher", "recreated", None);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("list.md");
    fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
    let mut state = state_for(&path);
    let start = Instant::now();
    let (mut trigger, error) = ReloadTrigger::new(&path, start);
    assert!(error.is_none());

    fs::remove_dir_all(&dir).unwrap();
    assert!(trigger.check(&mut state, start + RECHECK_INTERVAL));
    assert!(state.file_deleted);

    fs::create_dir_all(&dir).unwrap();
    fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
    assert!(trigger.check(&mut state, start + RECHECK_INTERVAL * 2));
    assert!(!state.file_deleted, "the restored file is picked up");
    assert_eq!(
        state.status_message.as_deref(),
        Some("File restored — reloaded")
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_watcher_event_triggers_a_check_before_the_timer_is_due() {
    let path = unique_temp_path("event");
    fs::write(&path, "## List\n\n- [ ] one\n").unwrap();
    let mut state = state_for(&path);
    let start = Instant::now();
    let (mut trigger, _) = ReloadTrigger::new(&path, start);

    write_with_new_mtime(&path, "## List\n\n- [ ] one\n- [ ] two\n");
    assert!(wait_until(|| trigger.check(&mut state, start)));
    assert_eq!(state.document.lists[0].items.len(), 2);

    fs::remove_file(&path).ok();
}

#[test]
fn errors_and_rescans_count_as_a_possible_change() {
    let (sender, receiver) = mpsc::channel();
    let watcher = FileWatcher {
        _watcher: notify::recommended_watcher(|_| {}).unwrap(),
        receiver,
        target_file_name: OsString::from("list.md"),
    };
    let unrelated = Event::new(notify::EventKind::Any).add_path(PathBuf::from("other.md"));

    sender.send(Ok(unrelated.clone())).unwrap();
    assert!(!watcher.poll_changed());
    sender
        .send(Ok(unrelated.set_flag(notify::event::Flag::Rescan)))
        .unwrap();
    assert!(watcher.poll_changed(), "a rescan means events were lost");
    sender
        .send(Err(notify::Error::generic("queue overflow")))
        .unwrap();
    assert!(watcher.poll_changed(), "an error may hide a change");
}

#[test]
fn ignores_writes_to_other_files_in_same_directory() {
    let path = unique_temp_path("target");
    let sibling = unique_temp_path("sibling");
    fs::write(&path, "initial").unwrap();
    let watcher = FileWatcher::new(&path).unwrap();

    fs::write(&sibling, "unrelated").unwrap();
    // Give the OS a moment to deliver (or not deliver) an event, then
    // confirm none of what arrived matches our target file.
    std::thread::sleep(Duration::from_millis(200));
    assert!(!watcher.poll_changed());

    fs::remove_file(&sibling).ok();
    fs::remove_file(&path).ok();
}
