use super::*;
use crate::app::test_support::*;

#[test]
fn a_write_is_refused_while_another_instance_holds_the_write_lock() {
    // The lock spans the hash check and the write.
    use crate::writer::{LockOutcome, WriteLock};

    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let path = document.file_path.clone();
    let mut state = AppState::new(document);

    // Stand in for the other instance being mid-write.
    let held = WriteLock::acquire(&path);
    assert!(
        matches!(held, LockOutcome::Acquired(_)),
        "test setup: the lock must be free to begin with"
    );

    state.toggle_current();

    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Another markcheck")),
        "the refusal must be reported, not silent: {:?}",
        state.status_message
    );
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "the in-memory toggle must be rolled back, not left diverged from disk"
    );
    assert!(
        state.take_git_sync_request().is_none(),
        "a refused write must not queue a git-sync request"
    );

    // Once the other instance is done, the same toggle goes through.
    drop(held);
    state.toggle_current();
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::Done),
        "the write must succeed once the lock is free"
    );
    assert!(
        state.take_git_sync_request().is_some(),
        "and the successful write queues its sync request as usual"
    );

    fs::remove_file(&path).ok();
}

#[test]
fn start_current_marks_started_without_advancing() {
    let mut state = AppState::new(two_list_document());
    state.start_current();
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::Started)
    );
    assert!(file_text(&state.document.file_path).starts_with("- [/] task 1\n- [ ] task 2\n"));
    assert_eq!(state.current_item_index, 0, "does not auto-advance");
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(state.status_message.as_deref(), Some("Marked started"));
}

#[test]
fn start_current_toggles_started_off() {
    let mut state = AppState::new(two_list_document());
    let original = file_text(&state.document.file_path);
    state.start_current();
    state.start_current();
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
    assert_eq!(file_text(&state.document.file_path), original);
    assert_eq!(state.status_message.as_deref(), Some("Marked not started"));
}

#[test]
fn start_current_on_done_task_reopens_as_started() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current(); // Done (then it auto-advances)
    state.current_item_index = 0;
    state.start_current();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Started)
    );
    assert_eq!(state.status_message.as_deref(), Some("Marked started"));
}

#[test]
fn toggle_completes_a_started_task() {
    let mut state = AppState::new(two_list_document());
    state.start_current();
    state.toggle_current();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    );
}

#[test]
fn started_task_does_not_complete_the_list() {
    // List 1's other items are done; marking the last one *started*
    // must not trigger a ListComplete transition (started ≠ done).
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.start_current();
    assert_eq!(shown(&state), (View::Checklist, None));
    let progress = state.document.progress();
    assert_eq!((progress.done, progress.total()), (2, 5), "started ≠ done");
}

#[test]
fn completing_a_task_skips_done_tasks_after_it() {
    let path = write_real_file("## L\n\n- [ ] a\n- [ ] b\n- [x] c\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 1;
    state.toggle_current();
    assert_eq!(
        state.current_item_index, 0,
        "wraps to the open task, not onto done c"
    );
    assert_eq!(state.status_message.as_deref(), Some("Marked done"));
    state.toggle_current();
    let saved = fs::read_to_string(&path).unwrap();
    assert_eq!(
        saved, "## L\n\n- [x] a\n- [x] b\n- [x] c\n",
        "second Space checks a"
    );
    assert_eq!(shown(&state), (View::AllComplete, None));
    fs::remove_file(&path).ok();
}

#[test]
fn completing_the_last_task_returns_to_a_skipped_open_one() {
    let path = write_real_file("## L\n\n- [ ] a\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 1;
    state.toggle_current();
    assert_eq!(
        state.current_item_index, 0,
        "does not stay on the just-done b"
    );
    state.toggle_current();
    let saved = fs::read_to_string(&path).unwrap();
    assert_eq!(saved, "## L\n\n- [x] a\n- [x] b\n", "second Space checks a");
    fs::remove_file(&path).ok();
}

#[test]
fn unchecking_a_task_with_space_says_so() {
    let path = write_real_file("## L\n\n- [x] a\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.current_item_index = 0;
    state.toggle_current();
    assert_eq!(state.status_message.as_deref(), Some("Marked not done"));
    assert!(!state.status_is_error);
    fs::remove_file(&path).ok();
}

#[test]
fn backspace_at_the_open_prompt_deletes_the_last_digit() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Backspace);
    assert!(state.pending_open_link(), "still armed");
    assert_eq!(state.pending_number(), 0);
    state.handle_key(KeyCode::Char('7'));
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/7")
    );
}

#[test]
fn toggle_current_on_display_only_advances_without_editing() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![display_only(1, "heading 1"), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);
    // Space/Enter on an info card pages past it instead of doing nothing;
    // the item itself is untouched.
    state.current_item_index = 0;
    state.toggle_current();
    assert_eq!(state.current_item_index, 1);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::DisplayOnly,
        "the info item is not edited"
    );
}

#[test]
fn toggle_current_advances_when_more_items_remain() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    assert_eq!(state.current_item_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn startup_skips_fully_done_first_list() {
    let document = document_with_lists(vec![
        List {
            title: "Done".to_string(),
            banner: None,
            items: vec![checkbox(1, "task 1", true)],
        },
        List {
            title: "Todo".to_string(),
            banner: None,
            items: vec![checkbox(2, "task 2", false)],
        },
    ]);
    let state = AppState::new(document);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn complete_list_advance_skips_to_next_incomplete() {
    let document = document_with_lists(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, "task 1", false)],
        },
        List {
            title: "B (all done)".to_string(),
            banner: None,
            items: vec![checkbox(2, "task 2", true)],
        },
        List {
            title: "C".to_string(),
            banner: None,
            items: vec![checkbox(3, "task 3", false)],
        },
    ]);
    let mut state = AppState::new(document);
    // Complete list A's only item -> ListComplete.
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    // Advancing should skip fully-done B and land on C's undone item.
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_list_index, 2);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn a_later_write_replaces_a_sync_not_yet_handed_over() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    state.toggle_current();
    let syncs: Vec<_> = state
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::GitSync(_)))
        .collect();
    assert_eq!(syncs.len(), 1, "only the latest content needs committing");
    let host = state.settle();
    assert_eq!(host.syncs.len(), 1);
    assert_eq!(
        host.syncs[0].content,
        fs::read_to_string(&state.document.file_path).unwrap()
    );
}

#[test]
fn click_inside_card_copies_current_command() {
    use ratatui::layout::Rect;
    // A click inside the card behaves like `y`. No-code item →
    // the same "nothing to copy" hint.
    let mut state = AppState::new(two_list_document());
    state.layout.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(15, 8);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to copy: item has no code")
    );

    // Ambiguous item inside the card → `y`'s digit prompt.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.layout.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(12, 6);
    assert!(state.pending_copy_code(), "armed like `y`");
}

#[test]
fn click_on_code_region_copies_that_specific_row() {
    use ratatui::layout::Rect;
    // An ambiguous card (2 candidates) normally refuses to copy,
    // but clicking a specific code region copies that row directly —
    // overriding the ambiguous hint a non-code click in the card produces.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.layout.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.layout.code_regions = vec![(Rect::new(11, 7, 38, 1), "b".into())];

    // On the region: the specific path, never the ambiguous hint.
    state.handle_left_click(15, 7);
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to clipboard (OSC 52)")
    );
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("b".to_string(), false)]);
    });

    // Elsewhere in the card (no region): `y`'s digit prompt.
    state.handle_left_click(15, 6);
    assert!(state.pending_copy_code());
}

#[test]
fn click_outside_card_or_off_checklist_is_ignored() {
    use ratatui::layout::Rect;
    let mut state = AppState::new(two_list_document());
    state.layout.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(2, 2); // outside the card rect
    assert_eq!(
        state.status_message, None,
        "click outside the card is a no-op"
    );

    state.view = View::AllComplete;
    state.handle_left_click(15, 8); // inside the rect, but wrong screen
    assert_eq!(
        state.status_message, None,
        "click ignored off the checklist"
    );
}

#[test]
fn click_on_overview_item_row_focuses_that_task() {
    use ratatui::layout::Rect;
    // A click on a recorded overview item row moves the cursor to that
    // exact task and returns to the checklist screen.
    let mut state = AppState::new(two_list_document());
    state.layout.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 1))];

    state.handle_left_click(70, 5);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn click_on_overview_list_row_jumps_to_that_list() {
    use ratatui::layout::Rect;
    // A click on a list-title row jumps to that list, landing on its
    // first not-done item (like the number keys / jump_to_list).
    let mut state = AppState::new(two_list_document());
    state.layout.overview_rows = vec![(Rect::new(60, 3, 30, 1), OverviewTarget::List(1))];

    state.handle_left_click(65, 3);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn click_on_overview_works_from_completion_screen() {
    use ratatui::layout::Rect;
    // The overview is a plain nav aid on the completion screens too, so
    // a click there jumps to the task and drops back to the checklist.
    let mut state = AppState::new(two_list_document());
    state.view = View::AllComplete;
    state.layout.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 0))];

    state.handle_left_click(70, 5);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn click_on_overview_ignored_while_a_modal_is_open() {
    use ratatui::layout::Rect;
    // Overview rows are still drawn beside the Help/Search/picker
    // overlays, but a click there must not jump the cursor mid-modal.
    let mut state = AppState::new(two_list_document());
    let before = (state.current_list_index, state.current_item_index);
    state.overlay = Some(Overlay::Help);
    state.layout.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 1))];

    state.handle_left_click(70, 5);
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        before,
        "modal click leaves the cursor put"
    );
    assert_eq!(state.overlay, Some(Overlay::Help), "still on the modal");
}

#[test]
fn click_on_overview_marker_toggles_item_without_moving_cursor() {
    use ratatui::layout::Rect;
    // A click on the marker zone toggles that task done/not-done in
    // place — the cursor (and card view) don't move.
    let mut state = AppState::new(two_list_document());
    let before = (state.current_list_index, state.current_item_index);
    state.layout.overview_rows = vec![(Rect::new(60, 5, 6, 1), OverviewTarget::Toggle(1, 0))];

    let original = file_text(&state.document.file_path);
    state.handle_left_click(61, 5);
    assert!(matches!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    ));
    assert_eq!(
        file_text(&state.document.file_path),
        original.replace("- [ ] task 4", "- [x] task 4")
    );
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        before,
        "cursor stays put on a marker toggle"
    );
    assert_eq!(state.status_message.as_deref(), Some("Marked done"));

    // Toggling again reverts it.
    state.handle_left_click(61, 5);
    assert!(matches!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    ));
    assert_eq!(state.status_message.as_deref(), Some("Marked not done"));
    assert_eq!(file_text(&state.document.file_path), original);
}

#[test]
fn toggle_item_is_a_no_op_on_a_display_only_item() {
    // Display-only items have no toggle state; toggle_item ignores them.
    let mut document = two_list_document();
    document.lists[0].items[1].kind = ItemKind::DisplayOnly;
    let mut state = AppState::new(document);
    state.toggle_item(0, 1);
    assert!(matches!(
        state.document.lists[0].items[1].kind,
        ItemKind::DisplayOnly
    ));
}

#[test]
fn marker_toggle_demotes_a_stale_completion_screen_but_never_promotes() {
    // Un-doing a task from a completion screen drops back to the
    // checklist; completing the last task from the checklist does NOT pop a
    // completion screen (that would steal focus from the cursor).
    let document = document_with_lists(vec![List {
        title: "Only".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", true), checkbox(2, "task 2", true)],
    }]);
    let mut state = AppState::new(document);
    // All tasks done: a keyboard toggle would have shown AllComplete.
    state.view = View::AllComplete;

    // Un-done the first task via a marker click → demote to the checklist.
    state.toggle_item(0, 0);
    assert_eq!(shown(&state), (View::Checklist, None));

    // Re-complete it from the checklist: no promotion to AllComplete.
    state.toggle_item(0, 0);
    assert_eq!(
        shown(&state),
        (View::Checklist, None),
        "a marker toggle never promotes to a completion screen"
    );
}

#[test]
fn toggle_current_reports_error_and_does_not_advance_when_write_fails() {
    // A write_back failure must surface as an error, not a silent
    // "Marked done" success — and must not advance the cursor/screen either,
    // since the toggle wasn't actually saved.
    let document = document_with_missing_file(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_current();
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Write failed")),
        "got {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    assert_eq!(state.current_item_index, 0, "did not advance");
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "in-memory state rolled back since the toggle was never saved"
    );
}

#[test]
fn failed_toggle_does_not_leave_a_phantom_done_item_that_falsely_completes_the_list() {
    // A failed write must roll back, or a later toggle could count the
    // phantom item and show a false completion screen.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            checkbox(1, "task 1", true),
            checkbox(2, "task 2", false),
            checkbox(3, "task 3", false),
        ],
    }]);
    let path = document.file_path.clone();
    let original = file_text(&path);
    let mut state = AppState::new(document);
    state.current_item_index = 1; // item 2, the one whose write will fail

    fs::remove_file(&path).unwrap();
    state.toggle_current();
    assert!(state.status_is_error, "the write failure must be reported");
    assert_eq!(
        state.document.lists[0].items[1].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "rolled back — must not silently become a phantom done item"
    );
    assert_eq!(
        shown(&state),
        (View::Checklist, None),
        "no premature completion screen"
    );

    // The file reappears. Clear the sticky error first, as a real keypress
    // does.
    fs::write(&path, &original).unwrap();
    state.clear_status();
    state.current_item_index = 2; // item 3, genuinely not done
    state.toggle_current();
    assert!(!state.status_is_error, "the retry succeeds");
    assert_eq!(
        file_text(&path),
        "- [x] task 1\n- [ ] task 2\n- [x] task 3\n",
        "only item 3 is written done; the rolled-back item 2 stays open"
    );
    assert_eq!(
        shown(&state),
        (View::Checklist, None),
        "item 2 is still genuinely not-done, so the list must not appear complete"
    );
}

#[test]
fn toggle_is_refused_on_a_write_protected_checklist() {
    use std::os::unix::fs::PermissionsExt;
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let path = document.file_path.clone();
    let before = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let mut state = AppState::new(document);

    state.toggle_current();

    assert!(state.status_is_error, "got {:?}", state.status_message);
    assert_eq!(state.status_message.as_deref(), Some(READ_ONLY_MESSAGE));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
    assert!(state.git_sync_request().is_none());
}

#[test]
fn toggle_succeeds_normally_when_disk_content_is_unchanged() {
    // Sanity check alongside the conflict test above: when nothing external
    // touched the file, the new hash check must not itself become a false
    // positive that blocks every ordinary write.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let mut state = AppState::new(document);

    state.toggle_current();

    assert!(!state.status_is_error, "got {:?}", state.status_message);
}

#[test]
fn write_back_can_still_overwrite_a_change_that_lands_after_the_diverged_check() {
    // `commit_write`'s write lock closes the gap between the divergence check
    // and the rename only against other markcheck instances; a writer that
    // doesn't take the lock (an editor, a script) can still land in it. That
    // race, inserted by hand.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let path = document.file_path.clone();
    let state = AppState::new(document);

    assert!(
        !state.disk_content_diverged(),
        "nothing external has happened yet"
    );

    // Stands in for another writer landing in the gap between the check
    // above and the write below.
    let external_content = "## Other\n\n- [x] external change\n";
    fs::write(&path, external_content).unwrap();

    crate::writer::write_back(&state.document).unwrap();

    assert_ne!(
        fs::read_to_string(&path).unwrap(),
        external_content,
        "a writer that bypasses the lock is silently overwritten here -- \
         this is the remaining gap, not a regression"
    );
}

#[test]
fn commit_write_updates_content_hash_so_a_second_toggle_does_not_false_positive() {
    // The conflict check must compare against the hash of what *we* last
    // wrote, not a stale hash from load time — otherwise our own successful
    // write would immediately look like an external change on the very next
    // action.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);

    state.toggle_current();
    assert!(
        !state.status_is_error,
        "first toggle: got {:?}",
        state.status_message
    );

    state.toggle_current();
    assert!(
        !state.status_is_error,
        "second toggle must not see our own prior write as a conflict: got {:?}",
        state.status_message
    );
}

#[test]
fn start_current_reports_error_when_write_fails() {
    let document = document_with_missing_file(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let mut state = AppState::new(document);
    state.start_current();
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Write failed")),
        "got {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "in-memory state rolled back since the write was never saved"
    );
}

#[test]
fn space_writes_the_done_marker_on_the_task_line_only() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    focus_text(&mut state, "first task");
    state.handle_key(KeyCode::Char(' '));
    assert_eq!(
        file_text(&path),
        markdown_with(&[("- [ ] **Alpha**", "- [x] **Alpha**")])
    );

    focus_text(&mut state, "first task");
    state.handle_key(KeyCode::Char(' '));
    assert_eq!(
        file_text(&path),
        WRITE_PATH_MARKDOWN,
        "a second toggle reverts"
    );
}

#[test]
fn space_on_an_uppercase_x_task_writes_it_open() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    focus_text(&mut state, "Bravo");
    state.handle_key(KeyCode::Char(' '));
    assert_eq!(
        file_text(&path),
        markdown_with(&[("- [X] Bravo", "- [ ] Bravo")])
    );
}

#[test]
fn s_writes_the_started_marker_and_s_again_clears_it() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    focus_text(&mut state, "Delta");
    state.handle_key(KeyCode::Char('s'));
    assert_eq!(
        file_text(&path),
        markdown_with(&[("- [ ] Delta", "- [/] Delta")])
    );
    state.handle_key(KeyCode::Char('s'));
    assert_eq!(file_text(&path), WRITE_PATH_MARKDOWN);
}

#[test]
fn an_overview_marker_click_writes_that_task_only() {
    use ratatui::layout::Rect;
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    state.layout.overview_rows = vec![(Rect::new(60, 5, 6, 1), OverviewTarget::Toggle(1, 0))];
    state.handle_left_click(61, 5);
    assert_eq!(
        file_text(&path),
        markdown_with(&[("- [ ] Delta", "- [x] Delta")])
    );
}

#[test]
fn the_git_sync_request_carries_the_written_file() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    focus_text(&mut state, "Delta");
    state.handle_key(KeyCode::Char(' '));
    let pending = state.take_git_sync_request().unwrap();
    assert_eq!(pending.content, file_text(&path));
    assert_eq!(
        pending.previous_content.as_deref(),
        Some(WRITE_PATH_MARKDOWN)
    );
}

#[test]
fn record_git_sync_sets_the_synced_timestamp() {
    let mut state = AppState::new(two_list_document());
    assert!(state.git_sync.last_at.is_none());
    state.record_git_sync();
    assert!(state.git_sync.last_at.is_some());
}

#[test]
fn a_failed_push_keeps_the_unsynced_state_across_keys_until_a_sync_lands() {
    let mut state = AppState::new(two_list_document());
    state.git_sync.active = true;
    state.record_git_sync();

    state.apply_git_sync_outcome(&SyncOutcome::CommittedNotPushed {
        message: "offline".to_string(),
        commit: "abc".to_string(),
    });
    assert_eq!(state.git_sync.problem, Some("not pushed"));
    assert!(
        state.git_sync.last_at.is_none(),
        "an older success no longer vouches for the file"
    );
    assert!(state.status_is_error);
    assert!(state.status_message.as_deref().unwrap().contains("offline"));

    state.handle_key(KeyCode::Char('j'));
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(
        state.git_sync.problem,
        Some("not pushed"),
        "keys clear the status bar, not the indicator"
    );

    state.apply_git_sync_outcome(&SyncOutcome::Skipped);
    state.apply_git_sync_outcome(&SyncOutcome::RetryAbandoned);
    assert_eq!(state.git_sync.problem, Some("not pushed"));

    state.apply_git_sync_outcome(&SyncOutcome::Synced);
    assert_eq!(state.git_sync.problem, None);
    assert!(state.git_sync.last_at.is_some());
    assert!(!state.status_is_error);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Git sync caught up: committed and pushed")
    );
}

#[test]
fn a_synced_outcome_without_a_problem_shows_no_status() {
    let mut state = AppState::new(two_list_document());
    state.apply_git_sync_outcome(&SyncOutcome::Synced);
    assert!(state.git_sync.last_at.is_some());
    assert!(state.status_message.is_none());
}

#[test]
fn content_sync_failures_and_untracked_files_are_always_reported() {
    let mut state = AppState::new(two_list_document());
    for _ in 0..2 {
        state.status_message = None;
        state.apply_git_sync_outcome(&SyncOutcome::Failed("merge in progress".to_string()));
        assert_eq!(state.git_sync.problem, Some("not synced"));
        assert!(state.status_message.is_some(), "reported every time");
    }
    state.status_message = None;
    state.apply_git_sync_outcome(&SyncOutcome::SkippedUntracked);
    assert_eq!(state.git_sync.problem, Some("not tracked"));
    assert!(
        state
            .status_message
            .as_deref()
            .unwrap()
            .contains("not tracked")
    );
}

#[test]
fn toggle_item_out_of_range_is_ignored() {
    let mut state = AppState::new(two_list_document());
    state.toggle_item(9, 9); // must not panic
    state.toggle_item(0, 99);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
    );
}

#[test]
fn toggle_item_reports_error_when_write_fails() {
    let document = document_with_missing_file(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_item(0, 0);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Write failed")),
        "got {:?}",
        state.status_message
    );
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "in-memory state rolled back since the toggle was never saved"
    );
}

#[test]
fn toggle_item_click_on_list_complete_screen_stays_when_list_still_done() {
    // ListComplete is per the *current* list. Toggling a task in a
    // *different* list (an overview click can target any visible row,
    // regardless of the current screen) must not demote it, since the
    // current list is still fully done either way.
    let document = document_with_lists(vec![
        List {
            title: "Done list".to_string(),
            banner: None,
            items: vec![checkbox(1, "task 1", true)],
        },
        List {
            title: "Other list".to_string(),
            banner: None,
            items: vec![checkbox(2, "task 2", false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.current_list_index = 0;
    state.view = View::ListComplete;
    state.toggle_item(1, 0); // toggles the *other* list's task
    assert_eq!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    );
    assert_eq!(
        shown(&state),
        (View::ListComplete, None),
        "the current list is still fully done, so ListComplete must not be demoted"
    );
}

#[test]
fn start_current_is_a_no_op_when_no_current_item() {
    let document = document_with_lists(vec![List {
        title: "Empty".to_string(),
        banner: None,
        items: vec![],
    }]);
    let mut state = AppState::new(document);
    state.start_current(); // current_item() is None: must not panic
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn start_current_reports_instead_of_going_silent_on_a_display_only_card() {
    // `s` on an info card explains the no-op.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![display_only(1, "heading 1"), checkbox(2, "task 2", false)],
    }]);
    let mut state = AppState::new(document);
    // AppState::new starts on the first *not-done* item, which skips the
    // DisplayOnly card — land on it explicitly.
    state.current_item_index = 0;
    state.start_current(); // current item is DisplayOnly
    assert_eq!(
        state.document.lists[0].items[1].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "the checkbox item must be untouched"
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to start: this card is a note, not a task")
    );
    assert!(state.status_is_error, "sticky, error-colored");
}

#[test]
fn toggle_current_reports_on_a_display_only_card_with_nowhere_to_advance() {
    // The info-card toggle pages forward instead of doing nothing -- but on
    // the very last card there is nowhere to page to, which was the one
    // remaining silent no-op in this group.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, "task 1", false), display_only(2, "heading 2")],
    }]);
    let mut state = AppState::new(document);
    state.current_item_index = 1; // the trailing info card
    state.toggle_current();
    assert_eq!(state.current_item_index, 1, "nowhere to advance to");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to toggle: this is a note, and it's the last card")
    );
    assert!(state.status_is_error, "sticky, error-colored");
}

#[test]
fn git_sync_descriptions_name_an_ordered_step_by_its_number() {
    let path = write_real_file("## S\n\n1. [ ] alpha\n2. [ ] beta\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Check \"1. alpha\"".to_string())
    );
    state.toggle_item(0, 1);
    assert_eq!(
        state.take_git_sync_request().map(|p| p.description),
        Some("Check \"2. beta\"".to_string())
    );
    fs::remove_file(&path).ok();
}

#[test]
fn a_startup_catch_up_push_is_announced_with_its_count() {
    let mut state = AppState::new(two_list_document());
    state.apply_git_sync_outcome(&SyncOutcome::CaughtUp { pushed: 2 });
    assert_eq!(
        state.status_message.as_deref(),
        Some("Pushed 2 earlier markcheck commits")
    );
    assert!(!state.status_is_error);
    assert!(state.git_sync.last_at.is_some());

    state.apply_git_sync_outcome(&SyncOutcome::CaughtUp { pushed: 1 });
    assert_eq!(
        state.status_message.as_deref(),
        Some("Pushed 1 earlier markcheck commit")
    );
}

#[test]
fn a_catch_up_that_pushed_nothing_says_why_and_stays() {
    let mut state = AppState::new(two_list_document());
    state.apply_git_sync_outcome(&SyncOutcome::NotCaughtUp(
        "commit abc1234 \"WIP\" is not pushed".to_string(),
    ));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Git sync: commit abc1234 \"WIP\" is not pushed")
    );
    assert_eq!(state.status_expiry, None, "sticky until read");
    assert_eq!(state.git_sync.problem, Some("not pushed"));
}

#[test]
fn a_remote_with_unpulled_changes_is_a_sticky_warning_that_leaves_the_sync_state_alone() {
    let mut state = AppState::new(two_list_document());
    state.apply_git_sync_outcome(&SyncOutcome::RemoteAhead(
        "refs/heads/main on origin has changes this checklist hasn't pulled".to_string(),
    ));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Git sync: refs/heads/main on origin has changes this checklist hasn't pulled")
    );
    assert_eq!(state.status_expiry, None, "sticky until read");
    assert_eq!(
        state.git_sync.problem, None,
        "nothing of this session's failed"
    );
}

#[test]
fn a_save_in_place_is_announced_once_and_outlives_the_toggle_status() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_support::unique_temp_path("app", "ro-dir", None);
    fs::create_dir(&dir).unwrap();
    let path = dir.join("list.md");
    fs::write(&path, "## S\n\n- [ ] one\n- [ ] two\n- [ ] three\n").unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    if fs::write(dir.join("probe"), "").is_ok() {
        eprintln!("skipped: the directory stays writable (running as root?)");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char(' '));
    assert!(fs::read_to_string(&path).unwrap().contains("- [x] one"));
    let notice = state.status_message.clone().unwrap_or_default();
    assert!(notice.starts_with("Saved in place"), "{notice}");

    state.handle_key(KeyCode::Char(' '));
    assert_eq!(state.status_message.as_deref(), Some("Marked done"));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn dash_skips_a_task_counts_it_as_resolved_and_moves_on() {
    let path = write_real_file("## S\n\n- [ ] one\n- [ ] two\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.status_message.as_deref(), Some("Marked skipped"));
    assert!(fs::read_to_string(&path).unwrap().contains("- [-] one"));
    assert_eq!(
        state.current_item_index, 1,
        "moved on to the next open task"
    );
    let sync = state.git_sync_request().unwrap();
    assert!(sync.description.starts_with("Skip"), "{}", sync.description);
    let progress = state.document.progress();
    assert_eq!(
        (progress.done, progress.total()),
        (0, 1),
        "skipped is out of the total"
    );
    assert_eq!(progress.skipped, 1);

    state.handle_key(KeyCode::Char(' '));
    assert_eq!(
        state.view,
        View::AllComplete,
        "done plus skipped completes the list"
    );

    state.focus_item(0, 0, CursorMove::Quiet);
    state.view = View::Checklist;
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.status_message.as_deref(), Some("Marked not skipped"));
    assert!(fs::read_to_string(&path).unwrap().contains("- [ ] one"));
}

#[test]
fn dash_on_a_note_says_there_is_nothing_to_skip() {
    let path = write_real_file("## S\n\n- a note\n- [ ] task\n");
    let mut state = AppState::new(parser::parse_document(path).unwrap());
    state.focus_item(0, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to skip: this card is a note, not a task")
    );
}

#[test]
fn checking_a_critical_task_takes_a_second_space() {
    let path = write_real_file("## S\n\n- [ ] **! Drop the database**\n- [ ] next\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop"),
        "not yet"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Critical step — press Space again"))
    );
    assert!(state.git_sync_request().is_none());

    let_critical_pause_pass(&mut state);
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [x] **! Drop")
    );
    assert_eq!(state.current_item_index, 1, "moved on as usual");

    state.focus_item(0, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop"),
        "unchecking stays one press"
    );
}

#[test]
fn an_overview_click_on_a_critical_task_also_needs_a_second_click() {
    let path = write_real_file("## S\n\n- [ ] a\n- [ ] **! Reboot**\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_item(0, 1);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Reboot")
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Critical step — click its marker again")),
        "the prompt names the click that confirms, not Space: {:?}",
        state.status_message
    );
    let_critical_pause_pass(&mut state);
    state.toggle_item(0, 1);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("- [x] **! Reboot"), "{text}");
    assert!(
        text.contains("- [ ] a"),
        "the cursor's task is untouched: {text}"
    );
}

/// Backdates an armed critical step as if the user had paused before the
/// confirming press.
fn let_critical_pause_pass(state: &mut AppState) {
    let arm = state
        .critical_armed
        .as_mut()
        .expect("a critical step is armed");
    arm.at = arm.at.checked_sub(CRITICAL_PAUSE).unwrap();
}

const HELD_KEY_LIST: &str = "## L\n\n- [ ] a\n- [ ] **! Drop the database**\n- [ ] b\n- [ ] c\n";

#[test]
fn a_held_space_does_not_walk_past_a_critical_step() {
    let path = write_real_file(HELD_KEY_LIST);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    for _ in 0..10 {
        state.handle_key(KeyCode::Char(' '));
    }
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("- [x] a"), "{text}");
    assert!(text.contains("- [ ] **! Drop"), "{text}");
    assert!(text.contains("- [ ] b"), "{text}");
    assert_eq!(state.current_item_index, 1, "the cursor stays on it");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Critical step still open: too quick — pause, then press Space again to check it")
    );

    let_critical_pause_pass(&mut state);
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [x] **! Drop")
    );
    assert_eq!(state.current_item_index, 2, "moved on as usual");
}

#[test]
fn skipping_a_critical_step_takes_a_second_deliberate_minus() {
    let path = write_real_file(HELD_KEY_LIST);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Char('-'));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("- [-] a"), "{text}");
    assert!(text.contains("- [ ] **! Drop"), "{text}");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Critical step — press - again to skip it, any other key leaves it open")
    );

    state.handle_key(KeyCode::Char('-'));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop"),
        "too quick"
    );

    let_critical_pause_pass(&mut state);
    state.handle_key(KeyCode::Char('-'));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("- [-] **! Drop"), "{text}");
    assert_eq!(state.status_message.as_deref(), Some("Marked skipped"));
    assert_eq!(state.current_item_index, 2, "moved on as usual");

    state.focus_item(0, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('-'));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop"),
        "unskipping stays one press"
    );
}

#[test]
fn a_critical_step_armed_by_one_key_is_not_confirmed_by_another() {
    let path = write_real_file(HELD_KEY_LIST);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.focus_item(0, 1, CursorMove::Quiet);

    state.handle_key(KeyCode::Char(' '));
    let_critical_pause_pass(&mut state);
    state.handle_key(KeyCode::Char('-'));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop"),
        "the - only armed a skip"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Critical step — press - again"))
    );
}

#[test]
fn a_quick_second_click_leaves_a_critical_step_open() {
    let path = write_real_file(HELD_KEY_LIST);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_item(0, 1);
    state.toggle_item(0, 1);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Drop")
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some(
            "Critical step still open: too quick — pause, then click its marker again to check it"
        )
    );
}

#[test]
fn toggles_carry_the_content_before_the_first_unsynced_change_as_their_base() {
    let original = "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n";
    let path = write_real_file(original);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.toggle_current();
    let first = state.take_git_sync_request().expect("a sync is queued");
    let expected = SyncBase::Own {
        epoch: 1,
        content: Some(original.to_string()),
    };
    assert_eq!(first.base, expected);

    state.apply_git_sync_outcome(&SyncOutcome::Failed("refused".to_string()));
    state.toggle_current();
    let second = state.take_git_sync_request().expect("a sync is queued");
    assert_ne!(second.previous_content.as_deref(), Some(original));
    assert_eq!(second.base, expected, "a refused sync keeps the base");
    fs::remove_file(&path).ok();
}

#[test]
fn content_diverged_compares_the_bytes_in_hand_and_fails_open_without_them() {
    let (state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);

    assert!(!state.content_diverged(Some(WRITE_PATH_MARKDOWN.as_bytes())));
    assert!(state.content_diverged(Some(b"## Other\n\n- [ ] changed\n")));
    assert!(
        !state.content_diverged(None),
        "an unreadable file fails open; write_back reports it"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn document_contents_follows_each_save_so_it_hashes_as_the_file() {
    let (mut state, path) = state_from_markdown("## L\n\n- [X] a\n- [ ] b\n- [ ] c\n");
    let matches_disk = |state: &AppState| {
        let contents = writer::document_contents(&state.document);
        assert_eq!(contents, file_text(&path));
        assert_eq!(
            Some(hash_bytes(contents.as_bytes())),
            state.file_content_hash
        );
    };

    state.current_item_index = 0;
    state.toggle_current();
    assert_eq!(file_text(&path), "## L\n\n- [ ] a\n- [ ] b\n- [ ] c\n");
    matches_disk(&state);

    state.undo();
    assert_eq!(
        file_text(&path),
        "## L\n\n- [x] a\n- [ ] b\n- [ ] c\n",
        "re-checking an uppercase [X] writes [x], as the README promises"
    );
    matches_disk(&state);

    state.reset_all();
    assert_eq!(file_text(&path), "## L\n\n- [ ] a\n- [ ] b\n- [ ] c\n");
    matches_disk(&state);

    fs::remove_file(&path).ok();
}
