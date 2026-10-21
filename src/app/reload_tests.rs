use super::*;
use crate::app::test_support::*;

#[test]
fn e_requests_editor_and_is_consumed_once() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('e'));
    assert!(state.take_editor_request());
    assert!(!state.take_editor_request(), "request is consumed");
}

#[test]
fn e_asks_for_the_editor_at_the_current_task_line() {
    let mut state = AppState::new(two_list_document());
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('e'));
    assert_eq!(state.effects, vec![Effect::OpenEditor { line: Some(5) }]);
}

#[test]
fn e_requests_editor_from_completion_screen() {
    let mut state = AppState::new(two_list_document());
    state.view = View::AllComplete;
    state.handle_key(KeyCode::Char('e'));
    assert!(state.take_editor_request());
}

#[test]
fn ctrl_z_requests_suspend_and_is_consumed_once() {
    let mut state = AppState::new(two_list_document());
    state.handle_key_with_mods(KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(state.take_suspend_request());
    assert!(!state.take_suspend_request(), "request is consumed");
}

#[test]
fn plain_z_does_not_request_suspend() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('z'));
    assert!(!state.take_suspend_request());
}

#[test]
fn ctrl_z_suspends_from_search_input_without_typing_a_z() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task");
    state.handle_key_with_mods(KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(state.take_suspend_request());
    assert_eq!(state.search.query, "task");
    assert_eq!(state.overlay, Some(Overlay::Search));
}

#[test]
fn ctrl_z_cancels_a_pending_gg() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('g'));
    state.handle_key_with_mods(KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(!state.pending_g());
}

#[test]
fn finish_suspend_failure_is_a_sticky_error() {
    let mut state = AppState::new(two_list_document());
    state.finish_suspend(Err("Operation not permitted (os error 1)".to_string()));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Couldn't suspend: Operation not permitted (os error 1)")
    );
    assert!(state.status_is_error);
}

#[test]
fn finish_suspend_with_no_change_confirms_the_resume() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_suspend(Ok(()));
    assert_eq!(state.status_message.as_deref(), Some("Resumed"));
    assert!(!state.status_is_error);
    fs::remove_file(&path).ok();
}

#[test]
fn waiting_for_sync_before_suspend_shows_a_notice_that_resuming_replaces() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.wait_for_sync_before_suspend();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Waiting for git sync before suspending…")
    );
    assert!(!state.status_is_error);
    state.finish_waiting_to_suspend();
    assert_eq!(state.status_message, None);
    state.finish_suspend(Ok(()));
    assert_eq!(state.status_message.as_deref(), Some("Resumed"));
    fs::remove_file(&path).ok();
}

#[test]
fn finishing_the_wait_keeps_what_the_sync_reported() {
    let mut state = AppState::new(two_list_document());
    state.wait_for_sync_before_suspend();
    state.set_error("Git sync failed: example".to_string());
    state.finish_waiting_to_suspend();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Git sync failed: example")
    );
    assert!(state.status_is_error);
}

#[test]
fn finish_suspend_reloads_an_edit_made_while_suspended() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_suspend(Ok(()));
    assert_eq!(state.current_list().items.len(), 2);
    assert_eq!(state.status_message.as_deref(), Some("Reloaded: 1 added"));
    fs::remove_file(&path).ok();
}

fn exit_status(raw: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(raw)
}

const EDITOR_WAITED: Duration = Duration::from_secs(30);

#[test]
fn finish_editor_launch_failure_is_a_sticky_error_naming_the_editor() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor(
        "no-such-editor",
        None,
        Err(io::Error::other("not found")),
        Duration::ZERO,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Failed to launch no-such-editor: not found")
    );
    assert!(state.status_is_error);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_missing_editor_says_how_to_pick_one() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor(
        "vi",
        None,
        Err(io::Error::from(io::ErrorKind::NotFound)),
        Duration::ZERO,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi not found — set $EDITOR (or $VISUAL) to your editor")
    );
    state.finish_editor(
        "/opt/editors/my-editor",
        None,
        Err(io::Error::from(io::ErrorKind::NotFound)),
        Duration::ZERO,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("my-editor not found — set $EDITOR (or $VISUAL) to your editor")
    );
    assert!(state.status_is_error);
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_non_zero_exit_is_a_sticky_error() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor("vi", None, Ok(exit_status(1 << 8)), Duration::ZERO);
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi exited with exit status: 1")
    );
    assert!(state.status_is_error);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_with_no_change_is_silent_and_queues_no_sync() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor("vi", None, Ok(exit_status(0)), EDITOR_WAITED);
    assert_eq!(state.status_message, None);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_reloads_an_edit_and_queues_a_sync_naming_the_editor() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    let edited = "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n";
    touch_with_new_mtime(&path, edited);
    state.finish_editor("vi", None, Ok(exit_status(0)), EDITOR_WAITED);
    assert_eq!(state.current_list().items.len(), 2);
    let pending = state.take_git_sync_request().expect("a sync is queued");
    assert_eq!(pending.description, "Edited in vi");
    assert_eq!(pending.content, edited);
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_non_zero_exit_after_an_edit_reloads_but_keeps_the_error_and_skips_sync() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.git_sync.active = true;
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_editor("vi", None, Ok(exit_status(1 << 8)), Duration::ZERO);
    assert_eq!(state.current_list().items.len(), 2);
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi exited with exit status: 1; Reloaded: 1 added — not synced")
    );
    assert!(state.status_is_error);
    assert_eq!(state.status_expiry, None);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_without_git_sync_does_not_mention_syncing() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_editor("vi", None, Ok(exit_status(1 << 8)), Duration::ZERO);
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi exited with exit status: 1; Reloaded: 1 added")
    );
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_keeps_the_editor_warning_past_a_reload_and_still_syncs() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_editor(
        "vi",
        Some("Bad $EDITOR/$VISUAL — using vi".to_string()),
        Ok(exit_status(0)),
        Duration::ZERO,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Bad $EDITOR/$VISUAL — using vi; Reloaded: 1 added")
    );
    assert!(state.status_is_error);
    assert!(state.take_git_sync_request().is_some());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_warning_alone_is_a_sticky_error() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor(
        "vi",
        Some("a warning".to_string()),
        Ok(exit_status(0)),
        EDITOR_WAITED,
    );
    assert_eq!(state.status_message.as_deref(), Some("a warning"));
    assert!(state.status_is_error);
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_returning_at_once_without_a_change_hints_at_a_wait_flag() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor(
        "/usr/bin/code",
        None,
        Ok(exit_status(0)),
        Duration::from_millis(200),
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some(
            "code returned immediately — if it opened a window, make it wait: \
             EDITOR=\"code --wait\" (subl -w, gedit --wait)"
        )
    );
    assert!(state.status_is_error);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[test]
fn finish_editor_wait_hint_needs_a_quick_successful_exit_and_an_unchanged_file() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor("code", None, Ok(exit_status(0)), Duration::from_secs(1));
    assert_eq!(
        state.status_message, None,
        "a full second is a real session"
    );

    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `two`\n");
    state.finish_editor("vi", None, Ok(exit_status(0)), Duration::ZERO);
    assert!(
        !state
            .status_message
            .as_deref()
            .unwrap_or_default()
            .contains("returned immediately"),
        "a quick edit still changed the file"
    );
    assert!(state.take_git_sync_request().is_some());

    state.finish_editor("vi", None, Ok(exit_status(1 << 8)), Duration::ZERO);
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi exited with exit status: 1")
    );
    fs::remove_file(&path).ok();
}

#[test]
fn toggle_refuses_and_reloads_when_disk_content_changed_since_load() {
    // Another writer changed the file; the write must be refused.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
    }]);
    let path = document.file_path.clone();
    let mut state = AppState::new(document);

    let external_content = "## Other\n\n- [x] external change\n";
    fs::write(&path, external_content).unwrap();

    state.toggle_current();

    assert!(state.status_is_error, "got {:?}", state.status_message);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("changed on disk")),
        "got {:?}",
        state.status_message
    );
    // The external writer's content must survive byte-for-byte — our toggle
    // must never have been allowed to overwrite it.
    assert_eq!(fs::read_to_string(&path).unwrap(), external_content);
    // The conflict forces a reload, so the external writer's content is
    // picked up immediately rather than only on the next watcher tick.
    assert_eq!(state.document.lists[0].title, "Other");
}

#[test]
fn reload_picks_up_external_change() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.reload_if_changed();

    assert_eq!(state.current_list().items.len(), 2);
    assert_eq!(state.status_message.as_deref(), Some("Reloaded: 1 added"));

    fs::remove_file(&path).ok();
}

#[test]
fn reload_if_changed_returns_true_only_when_new_content_actually_loaded() {
    // Callers that triggered the on-disk change themselves (the editor)
    // need to tell a real reload apart from a no-op, to know whether
    // there's now something worth a git-sync request.
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    assert!(!state.reload_if_changed(), "nothing changed yet");

    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    assert!(state.reload_if_changed(), "a real external edit landed");

    assert!(
        !state.reload_if_changed(),
        "already reloaded, mtime/size unchanged since"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_if_changed_returns_false_when_reload_is_skipped_or_fails() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    touch_with_new_mtime(&path, "Just a paragraph, no headings or lists.\n");
    assert!(
        !state.reload_if_changed(),
        "a skipped reload (no checklist items) is not a real reload"
    );
    assert_eq!(
        state.current_list().items.len(),
        1,
        "last good document kept"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn failed_reload_does_not_advance_the_write_conflict_fingerprint() {
    // A failed parse must not advance the fingerprint.
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    let good_hash = state.file_content_hash;

    // `parse_document` only errors on a read failure (parsing itself is
    // lenient markdown, never "malformed"), so an invalid-UTF-8 byte
    // sequence stands in for the transient broken intermediate save an
    // editor might momentarily leave on disk mid-write.
    fs::write(&path, [0xFF, 0xFE, 0xFD]).unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let new_mtime = metadata.modified().unwrap() + std::time::Duration::from_secs(1);
    fs::File::open(&path)
        .unwrap()
        .set_modified(new_mtime)
        .unwrap();

    assert!(
        !state.reload_if_changed(),
        "a failed parse is not a real reload"
    );
    assert_eq!(
        state.current_list().items.len(),
        1,
        "last good document kept in memory"
    );
    assert_eq!(
        state.file_content_hash, good_hash,
        "fingerprint must stay pinned to the last good content, not the broken save"
    );
    assert!(
        state.disk_content_diverged(),
        "the broken content on disk must still read as diverged from what markcheck knows"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn request_external_edit_sync_queues_a_git_sync_request() {
    let mut state = AppState::new(two_list_document());
    assert!(state.take_git_sync_request().is_none());
    state.request_external_edit_sync("vim");
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Edited in vim".to_string())
    );
}

#[test]
fn request_external_edit_sync_names_only_the_editor_file_not_its_directory() {
    let mut state = AppState::new(two_list_document());
    state.request_external_edit_sync("/home/example-user/bin/my-editor");
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Edited in my-editor".to_string())
    );
}

#[test]
fn reload_catches_same_mtime_change_via_size_cross_check() {
    // A same-instant external edit can leave the mtime identical on a
    // coarse-mtime filesystem; forcing that here shows the size cross-check
    // still catches the change even when mtime alone would have missed it.
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();

    fs::write(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n").unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    state.reload_if_changed();

    assert_eq!(
        state.current_list().items.len(),
        2,
        "size differed even though mtime was pinned to the same value"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_is_noop_when_file_unchanged() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.reload_if_changed();
    assert_eq!(state.status_message, None);

    fs::remove_file(&path).ok();
}

#[test]
fn own_write_does_not_trigger_spurious_reload() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.toggle_current();
    state.status_message = None;

    state.reload_if_changed();
    assert_eq!(
        state.status_message, None,
        "toggling our own file should not look like an external change"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_keeps_search_open_and_typed_keys_stay_query_text() {
    let path = write_real_file("## A\n\n- [ ] `alpha`\n\n## B\n\n- [ ] `bravo`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.jump_to_list(1);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "alp");

    let changed = "## A\n\n- [ ] `alpha`\n- [ ] `also`\n";
    touch_with_new_mtime(&path, changed);
    assert!(state.reload_if_changed());

    assert_eq!(state.overlay, Some(Overlay::Search));
    assert_eq!(state.current_item().unwrap().display_text, "alpha");
    type_str(&mut state, "hs");
    assert_eq!(state.search.query, "alphs");
    assert!(!state.take_editor_request());
    assert_eq!(fs::read_to_string(&path).unwrap(), changed);

    // The pre-search list is gone, so Esc lands on the remapped fallback
    // rather than an index past the end of the new document.
    state.handle_key(KeyCode::Esc);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(state.current_item().unwrap().display_text, "alpha");

    fs::remove_file(&path).ok();
}

#[test]
fn reload_keeps_the_picker_open_and_clamps_its_selection() {
    let path = write_real_file("## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n- [ ] `charlie`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('T'));
    state.handle_key(KeyCode::Down);
    state.handle_key(KeyCode::Down);
    assert_eq!(state.picker.selection, 2);

    touch_with_new_mtime(&path, "## L\n\n- [ ] `alpha`\n");
    assert!(state.reload_if_changed());

    assert_eq!(state.overlay, Some(Overlay::ListPicker));
    assert_eq!(state.picker.selection, 0);
    type_str(&mut state, "es");
    assert_eq!(state.picker.query, "es");
    assert!(!state.take_editor_request());
    assert_eq!(state.overlay, Some(Overlay::ListPicker));

    fs::remove_file(&path).ok();
}

#[test]
fn reload_keeps_help_open_and_closing_it_returns_to_the_checklist() {
    let path = write_real_file("## L\n\n- [x] `alpha`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('?'));

    touch_with_new_mtime(&path, "## L\n\n- [ ] `alpha`\n");
    assert!(state.reload_if_changed());

    assert_eq!(state.overlay, Some(Overlay::Help));
    state.handle_key(KeyCode::Char('x'));
    assert_eq!(shown(&state), (View::Checklist, None));

    fs::remove_file(&path).ok();
}

#[test]
fn reload_preserves_cursor_by_title_and_line_number() {
    let path = write_real_file(
        "## List A\n\n- [ ] `alpha`\n- [ ] `bravo`\n\n## List B\n\n- [ ] `charlie`\n",
    );
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.jump_to_list(1);

    // Rewrite with an extra list inserted before List B, shifting
    // its line numbers. The cursor should still land on "charlie".
    touch_with_new_mtime(
        &path,
        "## List A\n\n- [ ] `alpha`\n- [ ] `bravo`\n\n## List New\n\n- [ ] `extra`\n\n## List B\n\n- [ ] `charlie`\n",
    );
    state.reload_if_changed();

    assert_eq!(state.current_list().title, "List B");
    assert_eq!(state.current_item().unwrap().display_text, "charlie");

    fs::remove_file(&path).ok();
}

#[test]
fn reload_disambiguates_duplicate_list_titles_by_occurrence() {
    // Two lists share a title; the cursor stays on the second one.
    let path = write_real_file(
        "## Servers\n\n- [ ] `first-list-task`\n\n## Servers\n\n- [ ] `second-list-task`\n",
    );
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.jump_to_list(1);
    assert_eq!(
        state.current_item().unwrap().display_text,
        "second-list-task"
    );

    // Rewrite with a line inserted before both lists, shifting line
    // numbers, but keeping both same-titled lists in the same relative
    // order.
    touch_with_new_mtime(
        &path,
        "## Other\n\n- [ ] `unrelated`\n\n## Servers\n\n- [ ] `first-list-task`\n\n## Servers\n\n- [ ] `second-list-task`\n",
    );
    state.reload_if_changed();

    assert_eq!(state.current_list().title, "Servers");
    assert_eq!(
        state.current_item().unwrap().display_text,
        "second-list-task",
        "must stay on the second \"Servers\" list, not snap back to the first"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_keeps_the_cursor_on_its_task_when_a_line_is_inserted_above() {
    let path = write_real_file("## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n- [ ] `charlie`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 2;

    touch_with_new_mtime(
        &path,
        "## L\n\n- [ ] `alpha`\n- [ ] `inserted`\n- [ ] `bravo`\n- [ ] `charlie`\n",
    );
    assert!(state.reload_if_changed());

    assert_eq!(state.current_item().unwrap().display_text, "charlie");
    assert_eq!(state.status_message.as_deref(), Some("Reloaded: 1 added"));
}

#[test]
fn reload_picks_the_nearest_of_several_identical_tasks() {
    let path = write_real_file("## L\n\n- [ ] `same`\n- [ ] `other`\n- [ ] `same`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 2;

    touch_with_new_mtime(
        &path,
        "## L\n\n- [ ] `same`\n- [ ] `new`\n- [ ] `other`\n- [ ] `same`\n",
    );
    assert!(state.reload_if_changed());

    assert_eq!(state.current_item_index, 3);
    assert_eq!(state.current_item().unwrap().display_text, "same");
}

#[test]
fn reload_stays_on_a_task_edited_in_place() {
    let path = write_real_file("## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 1;

    touch_with_new_mtime(&path, "## L\n\n- [ ] `alpha`\n- [ ] `bravo two`\n");
    assert!(state.reload_if_changed());

    assert_eq!(state.current_item().unwrap().display_text, "bravo two");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: 1 added · 1 removed")
    );
}

#[test]
fn reload_moves_to_the_nearest_task_and_says_so_when_the_task_is_gone() {
    let path =
        write_real_file("## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n- [ ] `charlie`\n- [ ] `delta`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 2;

    touch_with_new_mtime(
        &path,
        "## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n\n- [ ] `delta`\n",
    );
    assert!(state.reload_if_changed());

    assert_eq!(state.current_item().unwrap().display_text, "bravo");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: 1 removed — task not found, cursor moved")
    );
}

#[test]
fn reload_says_so_when_the_cursor_list_is_gone() {
    let path = write_real_file("## A\n\n- [ ] `alpha`\n\n## B\n\n- [ ] `bravo`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.jump_to_list(1);

    touch_with_new_mtime(&path, "## A\n\n- [ ] `alpha`\n- [ ] `also`\n");
    assert!(state.reload_if_changed());

    assert_eq!(state.current_item().unwrap().display_text, "alpha");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: 1 added · 1 removed — task not found, cursor moved")
    );
}

#[test]
fn reload_never_auto_copies() {
    let path = write_real_file("## L\n\n- [ ] `alpha`\n- [ ] `bravo`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.auto_copy = true;
    crate::clipboard::fake::with(|c| c.system_attempts.clear());

    touch_with_new_mtime(&path, "## L\n\n- [ ] `alpha`\n- [x] `bravo`\n");
    assert!(state.reload_if_changed());

    crate::clipboard::fake::with(|c| {
        assert!(c.system_attempts.is_empty());
        assert!(c.osc52.is_empty());
    });
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: 1 done (\"bravo\")")
    );
}

#[test]
fn reload_skips_when_new_content_has_no_lists() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    touch_with_new_mtime(&path, "Just a paragraph, no headings or lists.\n");
    state.reload_if_changed();

    assert_eq!(state.current_list().title, "List 1");
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Reload skipped: file has no checklist items")),
        "{:?}",
        state.status_message
    );
    assert!(
        state.status_is_error,
        "sticky, not ephemeral: the condition persists until the file has tasks again, \
         and an expiring message is how the overwrite below went unnoticed"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn skipped_reload_does_not_advance_the_write_conflict_fingerprint() {
    // A file with no checklist items isn't adopted, so the fingerprint must
    // not advance either.
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    let good_hash = state.file_content_hash;

    touch_with_new_mtime(
        &path,
        "Rewritten from scratch. Still drafting, no tasks yet.\n",
    );

    assert!(
        !state.reload_if_changed(),
        "a document with no lists is not a real reload"
    );
    assert_eq!(
        state.current_list().items.len(),
        1,
        "last good document kept in memory"
    );
    assert_eq!(
        state.file_content_hash, good_hash,
        "fingerprint must stay pinned to the content the in-memory document came from"
    );
    assert!(
        state.disk_content_diverged(),
        "the rewritten file must still read as diverged from what markcheck knows"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_write_after_a_skipped_reload_is_refused_and_leaves_the_file_alone() {
    // The behavioural half of the test above, and what a user would actually
    // report: rewrite an open checklist into something with no tasks, then
    // toggle. The toggle must be refused, and the rewrite must survive
    // byte-for-byte rather than being replaced by markcheck's stale copy.
    let path = write_real_file("## Tasks\n\n- [ ] alpha\n- [ ] beta\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    let rewritten = "# Runbook\n\nRewritten from scratch. No tasks yet -- still drafting.\n";
    touch_with_new_mtime(&path, rewritten);
    state.reload_if_changed();

    state.toggle_current();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        rewritten,
        "the user's rewrite must survive the toggle untouched"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Reload skipped: file has no checklist items")),
        "the refusal must keep saying why, not claim a reload: {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "the in-memory toggle must be rolled back"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_detects_deleted_file_and_blocks_writes() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.reload_if_changed();

    assert_eq!(
        state.file_gone,
        Some(FileGone::Deleted),
        "flag must be set after deletion"
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — changes cannot be saved until it exists again"),
    );
    assert_eq!(state.status_expiry, None, "deletion message is sticky");

    // Toggling must be blocked.
    state.toggle_current();
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — changes cannot be saved until it exists again"),
    );
}

/// Replaces the checklist at `path` with a symlink to a private file, the
/// way someone with write access to the directory could after startup.
fn swap_for_symlink_to_private_file(path: &Path) -> PathBuf {
    let private = unique_temp_path();
    fs::write(&private, "## P\n\n- [ ] secret: hunter2\n").unwrap();
    fs::remove_file(path).unwrap();
    std::os::unix::fs::symlink(&private, path).unwrap();
    private
}

fn swap_for_fifo(path: &Path) {
    fs::remove_file(path).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

fn assert_refused_as_not_regular(state: &mut AppState, path: &Path) {
    assert_eq!(state.file_gone, Some(FileGone::NotRegular));
    assert_eq!(
        state.status_message.as_deref(),
        Some(FileGone::NotRegular.message())
    );
    assert_eq!(state.status_expiry, None, "the refusal is sticky");
    assert!(
        !state.document.raw_lines.concat().contains("secret"),
        "nothing behind the swapped path may be loaded"
    );

    state.status_message = None;
    state.toggle_current();
    assert_eq!(
        state.status_message.as_deref(),
        Some(FileGone::NotRegular.message()),
        "a toggle says why it was refused"
    );
    assert!(state.git_sync_request().is_none(), "nothing may be synced");
    assert!(
        !fs::symlink_metadata(path).unwrap().is_file(),
        "the swapped-in entry must not be written over"
    );
}

#[test]
fn a_checklist_swapped_for_a_symlink_is_neither_read_nor_written() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    let private = swap_for_symlink_to_private_file(&path);
    assert!(!state.reload_if_changed());

    assert_refused_as_not_regular(&mut state, &path);
    assert_eq!(
        fs::read_to_string(&private).unwrap(),
        "## P\n\n- [ ] secret: hunter2\n",
        "the link's target is untouched"
    );
}

#[test]
fn a_checklist_swapped_for_a_fifo_does_not_block_the_reload() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    swap_for_fifo(&path);
    assert!(!state.reload_if_changed());

    assert_refused_as_not_regular(&mut state, &path);
}

#[test]
fn a_swap_between_reloads_is_caught_by_the_write_itself() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    let private = swap_for_symlink_to_private_file(&path);
    state.toggle_current();

    assert_eq!(state.file_gone, Some(FileGone::NotRegular));
    assert_eq!(
        state.status_message.as_deref(),
        Some(FileGone::NotRegular.message())
    );
    assert!(state.git_sync_request().is_none());
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert!(!fs::read_to_string(&private).unwrap().contains("[x]"));
}

#[test]
fn a_regular_file_put_back_after_a_swap_restores_normal_operation() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    swap_for_fifo(&path);
    state.reload_if_changed();
    assert_eq!(state.file_gone, Some(FileGone::NotRegular));

    fs::remove_file(&path).unwrap();
    touch_with_new_mtime(&path, "## S\n\n- [ ] `task`\n");
    assert!(state.reload_if_changed());
    assert_eq!(state.file_gone, None);
    assert_eq!(
        state.status_message.as_deref(),
        Some("File restored — reloaded")
    );

    state.toggle_current();
    assert!(fs::read_to_string(&path).unwrap().contains("- [x] `task`"));
    assert!(state.git_sync_request().is_some());
}

#[test]
fn reload_detects_deleted_file_only_once() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.reload_if_changed();
    // Clear message to check it isn't re-emitted on the next tick.
    state.status_message = None;
    state.reload_if_changed();

    assert_eq!(state.status_message, None, "second tick must not re-emit");
}

#[test]
fn reload_clears_deleted_flag_when_file_restored() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.reload_if_changed();
    assert_eq!(state.file_gone, Some(FileGone::Deleted));

    // Restore the file.
    touch_with_new_mtime(&path, "## S\n\n- [ ] `task`\n");
    state.reload_if_changed();

    assert_eq!(
        state.file_gone, None,
        "flag must clear when file is restored"
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("File restored — reloaded"),
    );

    fs::remove_file(&path).ok();
}

#[test]
fn toggle_item_is_blocked_when_file_deleted() {
    let mut state = AppState::new(two_list_document());
    state.file_gone = Some(FileGone::Deleted);
    state.toggle_item(0, 0);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "blocked toggle must not change state"
    );
}

#[test]
fn reload_reports_a_parse_error_and_keeps_the_last_good_document() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    // Invalid UTF-8 makes fs::read_to_string (and so parse_document) fail.
    fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let new_mtime = metadata.modified().unwrap() + std::time::Duration::from_secs(1);
    fs::File::open(&path)
        .unwrap()
        .set_modified(new_mtime)
        .unwrap();

    state.reload_if_changed();

    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Reload failed:")),
        "got {:?}",
        state.status_message
    );
    assert_eq!(
        state.current_list().items.len(),
        1,
        "last good document kept"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn an_external_reload_drops_a_queued_git_sync_request() {
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n- [ ] beta\n").unwrap();
    let document = crate::parser::parse_document(path.clone()).unwrap();
    let mut state = AppState::new(document);

    state.toggle_current();
    assert!(
        state.git_sync_request().is_some(),
        "test setup: the toggle must have queued a sync request"
    );

    touch_with_new_mtime(&path, "## L\n\n- [ ] alpha\n- [x] beta\n");
    assert!(
        state.reload_if_changed(),
        "the external edit must be adopted"
    );

    assert_eq!(
        state.effects,
        vec![Effect::CancelGitSync],
        "the request here is dropped and the one waiting in the worker is cancelled"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_reload_after_the_request_reached_the_worker_cancels_it_there() {
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n- [ ] beta\n").unwrap();
    let mut state = AppState::new(crate::parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    let mut host = crate::test_support::RecordingHost::default();
    crate::effect::drain(&mut state, &mut host).unwrap();
    assert_eq!(
        host.syncs.len(),
        1,
        "test setup: the toggle reached the worker"
    );

    touch_with_new_mtime(&path, "## L\n\n- [ ] alpha\n- [x] beta\n");
    assert!(state.reload_if_changed());
    host.sync_waiting = true;
    crate::effect::drain(&mut state, &mut host).unwrap();

    assert_eq!(host.cancelled_syncs, 1);
    assert_eq!(host.syncs.len(), 1, "the reload requests nothing new");
    let message = state.status_message.as_deref().unwrap_or_default();
    assert!(
        message.starts_with("Reloaded: ") && message.ends_with(" — pending git sync dropped"),
        "got {message:?}"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_reload_with_nothing_waiting_in_the_worker_says_nothing_about_git_sync() {
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n").unwrap();
    let mut state = AppState::new(crate::parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## L\n\n- [x] alpha\n");
    assert!(state.reload_if_changed());
    let mut host = crate::test_support::RecordingHost::default();
    crate::effect::drain(&mut state, &mut host).unwrap();

    assert_eq!(host.cancelled_syncs, 1);
    let message = state.status_message.as_deref().unwrap_or_default();
    assert!(
        message.starts_with("Reloaded: ") && !message.contains("git sync"),
        "got {message:?}"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_reload_of_unchanged_content_keeps_the_queued_git_sync_request() {
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n").unwrap();
    let mut state = AppState::new(crate::parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    let written = fs::read_to_string(&path).unwrap();

    touch_with_new_mtime(&path, &written);
    assert!(state.reload_if_changed(), "test setup: the stat changed");

    assert!(
        matches!(state.effects.as_slice(), [Effect::GitSync(_)]),
        "the request still matches the file, so it must still be synced: {:?}",
        state.effects
    );

    fs::remove_file(&path).ok();
}

#[test]
fn an_editor_edit_folds_a_waiting_toggle_into_its_own_request() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    let mut host = crate::test_support::RecordingHost::default();
    crate::effect::drain(&mut state, &mut host).unwrap();

    touch_with_new_mtime(&path, "## List 1\n\n- [x] `task one`\n- [ ] `task two`\n");
    state.finish_editor("vi", None, Ok(exit_status(0)), EDITOR_WAITED);

    assert!(
        matches!(state.effects.as_slice(), [Effect::GitSync(sync)] if sync.description == "Edited in vi"),
        "the edit's request replaces the cancel: {:?}",
        state.effects
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_dropped_git_sync_is_reported_even_without_a_reload_message() {
    let mut state = AppState::new(two_list_document());
    state.report_dropped_git_sync();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Pending git sync dropped: the file changed on disk")
    );
    assert!(!state.status_is_error);
}

#[test]
fn the_fingerprint_describes_the_loaded_document_not_a_later_read() {
    // The fingerprint comes from the parsed document, not a second read.
    let path = unique_temp_path();
    let loaded = "## L\n\n- [ ] alpha\n- [ ] beta\n";
    fs::write(&path, loaded).unwrap();
    let document = crate::parser::parse_document(path.clone()).unwrap();

    // Somebody rewrites the file before the state is built.
    let newer = "## L\n\n- [x] alpha\n- [ ] beta\n- [ ] gamma\n";
    fs::write(&path, newer).unwrap();

    let mut state = AppState::new(document);

    assert_eq!(
        state.file_content_hash,
        Some(hash_bytes(
            writer::document_contents(&state.document).as_bytes()
        )),
        "the fingerprint must describe the document in memory"
    );
    assert_ne!(
        state.file_content_hash,
        Some(hash_bytes(newer.as_bytes())),
        "and must not have been taken from the newer content on disk"
    );

    // The consequence that matters: a write now correctly refuses and
    // reloads, instead of overwriting content it never saw.
    state.toggle_current();
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("File changed on disk")),
        "the divergence must be detected: {:?}",
        state.status_message
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        newer,
        "the newer content must survive untouched"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn an_external_reload_disarms_a_pending_input_chord() {
    // A reload cancels a pending `o` prompt.
    let path = unique_temp_path();
    fs::write(
        &path,
        "## L\n\n- [ ] see [a](https://example.com/a) and [b](https://example.com/b)\n- [ ] beta\n",
    )
    .unwrap();
    let document = crate::parser::parse_document(path.clone()).unwrap();
    let mut state = AppState::new(document);

    // `o` on a multi-link card arms the digit prompt.
    state.handle_key(KeyCode::Char('o'));
    assert!(
        state.pending_open_link(),
        "test setup: the digit prompt must be armed"
    );

    // The file changes underneath and the reload is adopted.
    touch_with_new_mtime(
        &path,
        "## L\n\n- [ ] plain first task\n- [ ] see [c](https://example.com/c)\n",
    );
    assert!(state.reload_if_changed(), "the reload must be adopted");

    assert!(
        !state.pending_open_link(),
        "a reload must disarm the chord rather than leave it pointed at new content"
    );
    assert_eq!(
        state.pending_chord,
        PendingChord::None,
        "nothing else is armed"
    );

    // The consequence: the digit is no longer read as a link choice.
    state.handle_key(KeyCode::Char('2'));
    assert!(
        state.take_link_open_request().is_none(),
        "the digit must not open a link on a card the user never chose from"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_change_between_parse_and_construction_is_not_recorded_as_already_seen() {
    // A stat taken after the parse may describe a newer revision; it must not
    // make that change look already seen.
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n").unwrap();
    let document = crate::parser::parse_document(path.clone()).unwrap();

    // The file grows before the state records what it saw.
    let newer = "## L\n\n- [ ] alpha\n- [ ] beta\n- [ ] gamma\n";
    fs::write(&path, newer).unwrap();

    let mut state = AppState::new(document);

    assert!(
        state.reload_if_changed(),
        "a change that landed before construction must still be picked up"
    );
    assert_eq!(
        state.current_list().items.len(),
        3,
        "and the newer content must actually be loaded"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn ctrl_e_on_a_completion_screen_does_not_open_the_editor() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    state.handle_key_with_mods(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert!(!state.editor_requested());
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.view = View::AllComplete;
    state.handle_key_with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn every_screen_help_lists_ctrl_c_ctrl_l_and_ctrl_z() {
    let words = help_words(&[""]);
    assert!(
        ["Ctrl-C", "Ctrl-L", "Ctrl-Z"]
            .iter()
            .all(|key| words.contains(key))
    );
}

#[test]
fn a_reload_says_which_tasks_changed_and_how() {
    let path = write_real_file("## S\n\n- [ ] one\n- [ ] two\n- [x] three\n- [ ] four\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(
        &path,
        "## S\n\n- [x] one\n- [x] two\n- [ ] three\n- [/] four\n- [ ] five\n",
    );
    assert!(state.reload_if_changed());
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: 2 done, 1 started, 1 reopened (\"one\", \"two\", …) · 1 added")
    );
}

#[test]
fn a_reload_that_changes_no_task_keeps_the_plain_message() {
    let path = write_real_file("## S\n\n- [ ] one\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## S\n\nAn added note above.\n\n- [ ] one\n");
    assert!(state.reload_if_changed());
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: file changed on disk")
    );
}

#[test]
fn an_outside_reload_drops_the_toggles_base() {
    let original = "## List 1\n\n- [ ] `task one`\n";
    let path = write_real_file(original);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    state.take_git_sync_request();

    let outside = "## List 1\n\n- [x] `task one`\n- [ ] `draft`\n";
    touch_with_new_mtime(&path, outside);
    assert!(state.reload_if_changed());
    assert_eq!(state.git_sync.unsynced_base, None);

    state.toggle_current();
    let pending = state.take_git_sync_request().expect("a sync is queued");
    assert_eq!(
        pending.base,
        SyncBase::Own {
            epoch: 2,
            content: Some(outside.to_string()),
        }
    );
    fs::remove_file(&path).ok();
}

#[test]
fn a_reload_that_changes_nothing_keeps_the_toggles_base() {
    let original = "## List 1\n\n- [ ] `task one`\n";
    let path = write_real_file(original);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    state.take_git_sync_request();

    let current = fs::read_to_string(&path).unwrap();
    touch_with_new_mtime(&path, &current);
    assert!(state.reload_if_changed());
    assert_eq!(state.git_sync.unsynced_base.as_deref(), Some(original));
    fs::remove_file(&path).ok();
}

#[test]
fn a_saved_edit_becomes_the_base_of_the_toggles_after_it() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    let edited = "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n";
    touch_with_new_mtime(&path, edited);
    state.finish_editor("vi", None, Ok(exit_status(0)), EDITOR_WAITED);
    let pending = state.take_git_sync_request().expect("a sync is queued");
    assert_eq!(pending.base, SyncBase::Edited { epoch: 1 });

    state.toggle_current();
    let pending = state.take_git_sync_request().expect("a sync is queued");
    assert_eq!(
        pending.base,
        SyncBase::Own {
            epoch: 1,
            content: Some(edited.to_string()),
        }
    );
    fs::remove_file(&path).ok();
}

#[test]
fn force_reload_of_an_unreadable_file_says_so_and_adopts_nothing() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    fs::remove_file(&path).unwrap();

    assert!(!state.force_reload());
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reload failed: the file can't be read")
    );
    assert!(state.status_is_error);
    assert_eq!(state.current_list().title, "List 1");
}

#[test]
fn refuse_suspend_is_a_sticky_error_naming_the_way_out() {
    let mut state = AppState::new(two_list_document());
    state.refuse_suspend();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Can't suspend: markcheck wasn't started from a shell with job control (q quits)")
    );
    assert!(state.status_is_error);
}
