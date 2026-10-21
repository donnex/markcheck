use super::*;
use crate::app::test_support::*;

#[test]
fn startup_selects_started_task_as_first_undone() {
    // A started task still counts as work remaining, so startup lands
    // on it rather than skipping past.
    let mut document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![
            checkbox(1, "task 1", true),
            checkbox(2, "task 2", false),
            checkbox(3, "task 3", false),
        ],
    }]);
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Started);
    let state = AppState::new(document);
    assert_eq!(state.current_item_index, 1, "lands on the started item");
}

#[test]
fn navigate_backward_crosses_into_previous_list_first_undone() {
    // List 0 has items 0,1 done and item 2 not-done; from the start of
    // list 1, `h` lands on list 0's first not-done item.
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_list_index = 1;
    state.current_item_index = 0; // first item of list 1
    state.navigate_backward();
    assert_eq!(state.current_list_index, 0, "moved to previous list");
    assert_eq!(state.current_item_index, 2, "first not-done of that list");
}

#[test]
fn startup_selects_first_undone_item() {
    let document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![
            checkbox(1, "task 1", true),
            checkbox(2, "task 2", true),
            checkbox(3, "task 3", false),
        ],
    }]);
    let state = AppState::new(document);
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 2);
}

#[test]
fn jump_to_list_lands_on_first_undone() {
    let document = document_with_lists(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, "task 1", false)],
        },
        List {
            title: "B".to_string(),
            banner: None,
            items: vec![checkbox(2, "task 2", true), checkbox(3, "task 3", false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('2'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1, "skips the done item");
}

#[test]
fn undo_reports_error_when_write_fails_mid_session() {
    // Unlike the other three, this needs a real write to succeed first (to
    // have something to undo) before the file disappears out from under it.
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    assert!(!state.status_is_error, "the initial toggle succeeds");
    let path = state.document.file_path.clone();
    fs::remove_file(&path).unwrap();

    state.undo();
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Undo failed")),
        "got {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    // The failed undo must not leave the document half-applied: item 1 (the
    // one the initial toggle touched, not necessarily the cursor's current
    // position — the toggle auto-advanced) must still read Done.
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
        "document rolled back to its pre-undo state"
    );
    // ...and the popped undo entry must go back, so retrying (e.g. once the
    // file reappears) is still possible instead of the history being lost.
    assert_eq!(
        state.undo_stack.len(),
        1,
        "popped entry restored for a retry"
    );
}

#[test]
fn redo_reports_error_when_write_fails_mid_session() {
    // Mirror of `undo_reports_error_when_write_fails_mid_session`: an undo
    // must succeed first (to have something to redo) before the file
    // disappears out from under a failing redo.
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    assert!(!state.status_is_error, "the initial toggle succeeds");
    state.undo();
    assert!(!state.status_is_error, "the undo succeeds");

    let path = state.document.file_path.clone();
    fs::remove_file(&path).unwrap();

    state.redo();
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Redo failed")),
        "got {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    // The failed redo must not leave the document half-applied: item 1
    // must still read its post-undo (NotStarted) state.
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "document rolled back to its pre-redo state"
    );
    // ...and the popped redo entry must go back, so retrying is possible.
    assert_eq!(
        state.redo_stack.len(),
        1,
        "popped entry restored for a retry"
    );
}

#[test]
fn u_and_ctrl_r_write_the_undone_and_redone_marker() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    focus_text(&mut state, "Charlie");
    state.handle_key(KeyCode::Char(' '));
    let done = markdown_with(&[("- [/] Charlie", "- [x] Charlie")]);
    assert_eq!(file_text(&path), done);

    state.handle_key(KeyCode::Char('u'));
    assert_eq!(file_text(&path), WRITE_PATH_MARKDOWN, "undo restores [/]");
    state.handle_key_with_mods(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(file_text(&path), done);
}

#[test]
fn undo_reverts_a_toggle_and_focuses_the_task() {
    let mut state = AppState::new(two_list_document());
    let original = file_text(&state.document.file_path);
    state.toggle_current(); // item 0 -> Done, auto-advances to item 1
    assert_eq!(state.current_item_index, 1);
    assert_eq!(
        file_text(&state.document.file_path),
        original.replace("- [ ] task 1", "- [x] task 1")
    );

    state.undo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "the toggle is reverted",
    );
    assert_eq!(
        state.current_item_index, 0,
        "cursor focuses the changed task"
    );
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Undo: marked not done")
    );
    assert!(!state.status_is_error);
    assert_eq!(
        file_text(&state.document.file_path),
        original,
        "the undo is written back"
    );
}

#[test]
fn redo_reapplies_an_undone_toggle() {
    let mut state = AppState::new(two_list_document());
    let original = file_text(&state.document.file_path);
    state.toggle_current();
    state.undo();
    state.redo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
        "redo restores the toggle",
    );
    assert_eq!(state.status_message.as_deref(), Some("Redo: marked done"));
    assert_eq!(
        file_text(&state.document.file_path),
        original.replace("- [ ] task 1", "- [x] task 1"),
        "the redo is written back"
    );
}

#[test]
fn undo_reverts_a_started_toggle() {
    let mut state = AppState::new(two_list_document());
    let original = file_text(&state.document.file_path);
    state.start_current(); // item 0 -> Started
    state.undo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
    );
    assert_eq!(file_text(&state.document.file_path), original);
}

#[test]
fn undo_reverts_a_reset_and_reports_the_count() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current(); // item 0 done (advances)
    state.current_item_index = 1;
    state.toggle_current(); // item 1 done
    let done_two = file_text(&state.document.file_path);
    assert!(done_two.starts_with("- [x] task 1\n- [x] task 2\n- [ ] task 3\n"));
    state.reset_all(); // both back to not-done
    assert!(!file_text(&state.document.file_path).contains("[x]"));
    assert!(
        state
            .current_list()
            .items
            .iter()
            .all(|i| i.kind == ItemKind::Checkbox(TaskState::NotStarted))
    );

    state.undo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
    );
    assert_eq!(
        state.current_list().items[1].kind,
        ItemKind::Checkbox(TaskState::Done),
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Undo: restored 2 tasks")
    );
    assert_eq!(file_text(&state.document.file_path), done_two);
}

#[test]
fn undo_reverts_an_overview_click_toggle() {
    let mut state = AppState::new(two_list_document());
    let original = file_text(&state.document.file_path);
    state.toggle_item(1, 0); // toggle a task in the *other* list, cursor stays
    assert_eq!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
    );
    state.undo();
    assert_eq!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
    );
    assert_eq!(file_text(&state.document.file_path), original);
}

#[test]
fn undo_on_empty_history_reports_nothing_to_undo() {
    let mut state = AppState::new(two_list_document());
    state.undo();
    assert_eq!(state.status_message.as_deref(), Some("Nothing to undo"));
    assert!(state.status_is_error);
}

#[test]
fn redo_on_empty_history_reports_nothing_to_redo() {
    let mut state = AppState::new(two_list_document());
    state.redo();
    assert_eq!(state.status_message.as_deref(), Some("Nothing to redo"));
    assert!(state.status_is_error);
}

#[test]
fn a_fresh_action_clears_the_redo_stack() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current(); // undo point 1
    state.undo(); // pushes a redo entry
    assert_eq!(state.redo_stack.len(), 1);

    state.toggle_current(); // a fresh change invalidates redo
    assert!(state.redo_stack.is_empty());
    state.redo();
    assert_eq!(state.status_message.as_deref(), Some("Nothing to redo"));
}

#[test]
fn undo_history_is_capped() {
    let mut state = AppState::new(two_list_document());
    for _ in 0..(UNDO_HISTORY_CAP + 10) {
        state.start_current(); // toggles item 0 started on/off, each an entry
    }
    assert_eq!(state.undo_stack.len(), UNDO_HISTORY_CAP);
}

#[test]
fn undo_is_blocked_when_file_deleted() {
    let mut state = AppState::new(two_list_document());
    state.file_gone = Some(FileGone::Deleted);
    state.undo();
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — changes cannot be saved until it exists again"),
    );
}

#[test]
fn undo_and_redo_are_wired_to_u_and_ctrl_r() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('u')); // nothing yet
    assert_eq!(state.status_message.as_deref(), Some("Nothing to undo"));

    state.toggle_current();
    state.handle_key(KeyCode::Char('u'));
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
    );
    state.handle_key_with_mods(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
    );
}

#[test]
fn ctrl_u_does_not_trigger_undo() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    state.current_item_index = 0;
    // Ctrl-U is card scroll, not undo: the toggle must survive.
    state.handle_key_with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
        "Ctrl-U must not undo",
    );
}

#[test]
fn external_reload_clears_undo_history() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current(); // build some history
    assert!(!state.undo_stack.is_empty());

    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.reload_if_changed();
    assert!(
        state.undo_stack.is_empty(),
        "external edit clears undo history"
    );
    assert!(state.redo_stack.is_empty());

    fs::remove_file(&path).ok();
}

const STALE_HISTORY_EXTERNAL_EDIT: &str = "## L\n\n- [x] new\n- [x] a\n- [x] b\n";

#[test]
fn undo_refused_by_an_external_edit_does_not_replay_a_stale_snapshot() {
    let path = write_real_file("## L\n\n- [ ] a\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    touch_with_new_mtime(&path, STALE_HISTORY_EXTERNAL_EDIT);

    state.undo();
    assert!(state.status_is_error);
    assert!(state.undo_stack.is_empty(), "history cleared by the reload");
    assert!(state.redo_stack.is_empty());

    state.undo();
    assert_eq!(state.status_message.as_deref(), Some("Nothing to undo"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        STALE_HISTORY_EXTERNAL_EDIT
    );

    fs::remove_file(&path).ok();
}

#[test]
fn redo_refused_by_an_external_edit_does_not_replay_a_stale_snapshot() {
    let path = write_real_file("## L\n\n- [ ] a\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    state.undo();
    assert_eq!(state.redo_stack.len(), 1);
    touch_with_new_mtime(&path, STALE_HISTORY_EXTERNAL_EDIT);

    state.redo();
    assert!(state.status_is_error);
    assert!(state.undo_stack.is_empty());
    assert!(state.redo_stack.is_empty(), "history cleared by the reload");

    state.redo();
    assert_eq!(state.status_message.as_deref(), Some("Nothing to redo"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        STALE_HISTORY_EXTERNAL_EDIT
    );

    fs::remove_file(&path).ok();
}

#[test]
fn a_retry_failing_for_the_same_reason_leaves_the_status_bar_alone() {
    let mut state = AppState::new(two_list_document());
    state.apply_git_sync_outcome(&SyncOutcome::CommittedNotPushed {
        message: "offline".to_string(),
        commit: "abc".to_string(),
    });
    state.set_status("Match 2/5".to_string());

    state.apply_git_sync_outcome(&SyncOutcome::RetryFailed("offline".to_string()));
    state.apply_git_sync_outcome(&SyncOutcome::CommittedNotPushed {
        message: "offline".to_string(),
        commit: "abc".to_string(),
    });
    assert_eq!(state.status_message.as_deref(), Some("Match 2/5"));

    state.apply_git_sync_outcome(&SyncOutcome::RetryFailed("auth expired".to_string()));
    assert!(
        state
            .status_message
            .as_deref()
            .unwrap()
            .contains("auth expired"),
        "a new reason is reported"
    );
}

#[test]
fn undo_redo_skip_display_only_items_in_the_snapshot() {
    // state_snapshot/apply_snapshot must ignore DisplayOnly items (nothing
    // to undo/redo on a note card) even when one sits alongside checkboxes.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![display_only(1, "heading 1"), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);
    state.current_item_index = 1;
    state.toggle_current(); // item 1 (the checkbox) -> Done
    state.undo();
    assert_eq!(
        state.document.lists[0].items[1].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "undo restores the checkbox"
    );
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::DisplayOnly,
        "the note card is never touched by undo"
    );
}

#[test]
fn redo_is_blocked_when_file_deleted() {
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_current();
    state.undo();
    state.file_gone = Some(FileGone::Deleted);
    state.redo();
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "blocked redo must not change state"
    );
}

#[test]
fn undo_after_start_then_toggle_reports_marked_started() {
    // finish_history_apply's exact-one-item-changed branch reports the
    // item's *new* state; landing back on Started (rather than Done or
    // NotStarted) exercises the "marked started" arm specifically.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let mut state = AppState::new(document);
    state.start_current(); // NotStarted -> Started
    state.toggle_current(); // Started -> Done
    state.undo(); // back to Started
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::Started)
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Undo: marked started")
    );
}

#[test]
fn undo_and_redo_of_a_skip_name_the_state() {
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);
    state.skip_current();
    state.undo();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Undo: marked not done")
    );
    state.redo();
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::Skipped)
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Redo: marked skipped")
    );
}

#[test]
fn undo_and_redo_commit_messages_name_the_task_and_its_new_state() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    state.undo();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Undo: Uncheck \"task 1\"".to_string())
    );
    state.redo();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Redo: Check \"task 1\"".to_string())
    );
    state.skip_current();
    state.undo();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Undo: Check \"task 1\"".to_string())
    );
}

#[test]
fn undo_of_several_tasks_commits_a_count_with_their_names() {
    let mut state = AppState::new(two_list_document());
    state.toggle_item(0, 0);
    state.toggle_item(0, 1);
    state.reset_all();
    state.undo();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Undo: 2 tasks (\"task 1\", \"task 2\")".to_string())
    );
}

#[test]
fn undo_of_many_tasks_names_the_first_few_and_elides_the_rest() {
    let mut state = AppState::new(two_list_document());
    for (li, ii) in [(0, 0), (0, 1), (0, 2), (1, 0)] {
        state.toggle_item(li, ii);
    }
    state.reset_all();
    state.undo();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Undo: 4 tasks (\"task 1\", \"task 2\", \"task 3\", \u{2026})".to_string())
    );
}

#[test]
fn history_change_desc_with_nothing_changed_says_so() {
    let state = AppState::new(two_list_document());
    assert_eq!(state.history_change_desc(&[]), "no change");
    assert_eq!(
        state.history_change_desc(&[999]),
        "no change",
        "a line no longer in the document is not named"
    );
}
