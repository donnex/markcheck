use crate::app::test_support::*;

#[test]
fn paste_into_the_picker_filters_and_resets_the_selection() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    state.handle_key(KeyCode::Down);
    type_str(&mut state, "task");
    state.handle_paste(" 4\n");
    assert_eq!(state.picker.query, "task 4");
    assert_eq!(state.picker_matches(), vec![(1, 0)]);
    assert_eq!(state.picker.selection, 0);
    assert_eq!(state.overlay, Some(Overlay::ListPicker));
}

#[test]
fn jump_to_list_resets_item_index_and_screen() {
    let mut state = AppState::new(two_list_document());
    state.navigate_next();
    state.view = View::ListComplete;
    state.jump_to_list(1);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn q_offers_the_reset_when_every_task_is_skipped() {
    let mut skipped = checkbox(1, false);
    skipped.kind = ItemKind::Checkbox(TaskState::Skipped);
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![skipped],
    }]));
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    assert!(!state.quit_requested());
}

#[test]
fn q_quits_straight_away_with_no_tasks_touched() {
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![],
    }]));
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, None);
    assert!(state.quit_requested());
}

#[test]
fn navigation_resets_card_scroll() {
    let mut state = AppState::new(two_list_document());
    state.layout.card_max_scroll = 5;
    state.card_scroll = 3;
    state.navigate_next();
    assert_eq!(state.card_scroll, 0);
    assert_eq!(state.layout.card_max_scroll, 0);
}

#[test]
fn ctrl_c_on_all_complete_shows_quit_hint_without_reset_prompt() {
    let mut state = AppState::new(all_done_document());
    state.view = View::AllComplete;
    press_ctrl_c(&mut state);
    assert_quit_hint(&state);
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn ctrl_c_cancels_reset_confirmation() {
    let mut state = AppState::new(all_done_document());
    state.view = View::AllComplete;
    state.overlay = Some(Overlay::ConfirmReset);
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::AllComplete, None));
    assert_eq!(
        state.document.checkbox_stats().0,
        state.document.checkbox_stats().1
    );
}

#[test]
fn ctrl_c_cancels_quit_reset_confirmation_without_quitting() {
    let mut state = AppState::new(all_done_document());
    state.view = View::AllComplete;
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::AllComplete, None));
    assert!(!state.quit_requested());
}

#[test]
fn r_with_no_done_tasks_skips_prompt() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to reset: no tasks are done")
    );
}

#[test]
fn r_with_done_tasks_opens_confirm_prompt() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
}

#[test]
fn r_with_only_started_tasks_opens_confirm_prompt() {
    // A document whose only progress is Started still has something to reset.
    let mut state = AppState::new(two_list_document());
    state.start_current();
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
}

#[test]
fn confirm_reset_yes_clears_all_and_persists() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(state.document.checkbox_stats().0, 0, "all tasks not done");
    // No spurious reload from our own write.
    state.reload_if_changed();
    assert_ne!(
        state.status_message.as_deref(),
        Some("Reloaded: file changed on disk")
    );
}

#[test]
fn confirm_reset_cancel_restores_screen_and_leaves_tasks() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    let done_before = state.document.checkbox_stats().0;
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    state.handle_key(KeyCode::Esc);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert!(!state.quit_requested(), "Esc cancels, does not quit");
    assert_eq!(state.document.checkbox_stats().0, done_before);
}

#[test]
fn confirm_reset_from_all_complete_cancels_back_to_all_complete() {
    let document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true)],
    }]);
    let mut state = AppState::new(document);
    state.view = View::AllComplete;
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn confirm_reset_enter_does_not_confirm() {
    // Enter no longer resets — only `y` does. Any other key cancels.
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    let done_before = state.document.checkbox_stats().0;
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    state.handle_key(KeyCode::Enter);
    assert_eq!(shown(&state), (View::Checklist, None), "Enter cancels");
    assert_eq!(
        state.document.checkbox_stats().0,
        done_before,
        "Enter must not reset"
    );
}

#[test]
fn quit_when_all_done_opens_confirm_quit_reset() {
    // q on an all-done file offers to reset instead of quitting.
    let mut state = AppState::new(all_done_document());
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    assert!(!state.quit_requested(), "prompt shown, not quit yet");
}

#[test]
fn confirm_quit_reset_yes_resets_and_quits() {
    let mut state = AppState::new(all_done_document());
    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('y'));
    assert!(state.quit_requested(), "y quits");
    assert_eq!(state.document.checkbox_stats().0, 0, "y resets all tasks");
}

#[test]
fn confirm_quit_reset_yes_stays_running_when_the_file_changed_on_disk() {
    let document = all_done_document();
    let path = document.file_path.clone();
    let mut state = AppState::new(document);
    let external_content = "## Other\n\n- [x] external change\n";
    fs::write(&path, external_content).unwrap();

    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('y'));

    assert!(!state.quit_requested(), "a refused reset must not quit");
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("File changed on disk")),
        "got {:?}",
        state.status_message
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), external_content);
    fs::remove_file(&path).ok();
}

#[test]
fn confirm_quit_reset_yes_stays_running_when_the_write_fails() {
    let mut state = AppState::new(document_with_missing_file(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }]));

    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('y'));

    assert!(!state.quit_requested(), "a failed reset must not quit");
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Write failed")),
        "got {:?}",
        state.status_message
    );
    assert_eq!(
        state.overlay,
        Some(Overlay::ConfirmQuitReset),
        "y retries, n quits"
    );
    assert_eq!(state.document.checkbox_stats().0, 2, "nothing was reset");
}

#[test]
fn confirm_quit_reset_no_quits_without_resetting() {
    let mut state = AppState::new(all_done_document());
    let done_before = state.document.checkbox_stats().0;
    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('n'));
    assert!(state.quit_requested(), "n quits");
    assert_eq!(
        state.document.checkbox_stats().0,
        done_before,
        "n must not reset"
    );
}

#[test]
fn confirm_quit_reset_esc_cancels() {
    let mut state = AppState::new(all_done_document());
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    state.handle_key(KeyCode::Esc);
    assert!(!state.quit_requested(), "Esc cancels, stays running");
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn ephemeral_status_expires_after_timeout() {
    // A copy/info message auto-clears once its deadline passes.
    let mut state = AppState::new(two_list_document());
    state.set_status("Copied to clipboard".to_string());
    let exp = state
        .status_expiry
        .expect("ephemeral message has an expiry");
    // Just before the deadline: still shown.
    state.expire_status(exp - std::time::Duration::from_secs(1));
    assert_eq!(state.status_message.as_deref(), Some("Copied to clipboard"));
    // At the deadline: cleared, along with the expiry.
    state.expire_status(exp);
    assert_eq!(state.status_message, None);
    assert_eq!(state.status_expiry, None);
}

#[test]
fn sticky_error_never_auto_expires() {
    // A sticky message (failure or "keypress did nothing" feedback)
    // persists until the next input (no expiry).
    let mut state = AppState::new(two_list_document());
    state.set_error("Something failed".to_string());
    assert_eq!(state.status_expiry, None, "sticky messages carry no expiry");
    assert!(state.status_is_error, "set_error is flagged as an error");
    state.expire_status(std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Something failed"),
        "sticky message survives auto-expiry"
    );
}

#[test]
fn passive_status_clears_the_error_flag() {
    // A later passive confirmation must not stay red — each setter
    // resets the error flag.
    let mut state = AppState::new(two_list_document());
    state.set_error("Something failed".to_string());
    assert!(state.status_is_error);
    state.set_status("Copied to clipboard".to_string());
    assert!(!state.status_is_error, "set_status clears the error flag");
    state.set_error("Once more".to_string());
    state.clear_status();
    assert!(!state.status_is_error, "clear_status clears the error flag");
}

#[test]
fn reset_all_reports_error_when_write_fails() {
    let document = document_with_missing_file(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, true)],
    }]);
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('y'));
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Write failed")),
        "got {:?}",
        state.status_message
    );
    assert!(state.status_is_error);
    // The in-memory reset must be rolled back since it was never saved —
    // otherwise the UI would show every task done-reset while the file on
    // disk still has the original (done) state, with nothing left to
    // reconcile the two for the rest of the session.
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    );
}

#[test]
fn reset_with_nothing_done_is_flagged_error() {
    // An `R` that has nothing to reset is a "nothing happened" case.
    let mut state = AppState::new(two_list_document());
    state.request_reset();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to reset: no tasks are done")
    );
    assert!(state.status_is_error);
}

#[test]
fn reload_cancels_a_pending_reset_confirmation_visibly() {
    let path = write_real_file("## L\n\n- [x] `alpha`\n- [ ] `bravo`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));

    let changed = "## L\n\n- [x] `alpha`\n- [x] `bravo`\n- [ ] `charlie`\n";
    touch_with_new_mtime(&path, changed);
    assert!(state.reload_if_changed());

    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(
        state.status_message.as_deref(),
        Some("File changed on disk — reset cancelled")
    );
    assert!(state.status_is_error);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(fs::read_to_string(&path).unwrap(), changed);

    fs::remove_file(&path).ok();
}

#[test]
fn reload_cancels_a_pending_quit_reset_confirmation_visibly() {
    let path = write_real_file("## L\n\n- [x] `alpha`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));

    touch_with_new_mtime(&path, "## L\n\n- [x] `alpha`\n- [ ] `bravo`\n");
    assert!(state.reload_if_changed());

    assert_eq!(shown(&state), (View::Checklist, None));
    assert!(!state.quit_requested());
    assert_eq!(
        state.status_message.as_deref(),
        Some("File changed on disk — quit cancelled")
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reset_is_blocked_when_file_deleted() {
    let path = write_real_file("## S\n\n- [x] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.file_gone = Some(FileGone::Deleted);
    // Reach reset_all via the ConfirmReset screen's 'y' path.
    state.overlay = Some(Overlay::ConfirmReset);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — changes cannot be saved until it exists again"),
    );
}

#[test]
fn reset_confirmed_with_y_writes_every_task_open() {
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    state.handle_key(KeyCode::Char('y'));
    let reset = markdown_with(&[
        ("- [X] Bravo", "- [ ] Bravo"),
        ("- [/] Charlie", "- [ ] Charlie"),
    ]);
    assert_eq!(file_text(&path), reset);

    state.handle_key(KeyCode::Char('u'));
    assert_eq!(file_text(&path), WRITE_PATH_MARKDOWN, "u restores the file");
    state.handle_key_with_mods(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(file_text(&path), reset, "Ctrl-R writes the reset again");
}

#[test]
fn quit_reset_confirmed_with_y_writes_every_task_open_and_quits() {
    let all_done = "## L\n\n- [x] one\n- [X] two\n";
    let (mut state, path) = state_from_markdown(all_done);
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    state.handle_key(KeyCode::Char('y'));
    assert!(state.quit_requested());
    assert_eq!(file_text(&path), "## L\n\n- [ ] one\n- [ ] two\n");
}

#[test]
fn quit_without_reset_leaves_the_file_untouched() {
    let all_done = "## L\n\n- [x] one\n- [X] two\n";
    let (mut state, path) = state_from_markdown(all_done);
    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('n'));
    assert!(state.quit_requested());
    assert_eq!(file_text(&path), all_done);
}

#[test]
fn ctrl_y_does_not_confirm_the_reset() {
    let path = write_real_file(CHORD_TEST_FILE);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    state.handle_key_with_mods(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    assert_eq!(fs::read_to_string(&path).unwrap(), CHORD_TEST_FILE);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_ne!(fs::read_to_string(&path).unwrap(), CHORD_TEST_FILE);
    fs::remove_file(&path).ok();
}

#[test]
fn reset_also_clears_skipped_tasks() {
    let path = write_real_file("## S\n\n- [-] one\n- [ ] two\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(
        state.overlay,
        Some(Overlay::ConfirmReset),
        "there is something to reset"
    );
    state.handle_key(KeyCode::Char('y'));
    assert!(fs::read_to_string(&path).unwrap().contains("- [ ] one"));
}

#[test]
fn r_then_l_resets_only_the_current_list() {
    let path = write_real_file("## A\n\n- [x] a1\n- [x] a2\n\n## B\n\n- [x] b1\n- [ ] b2\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.reset_count(ResetScope::List), 1);
    state.handle_key(KeyCode::Char('l'));
    let written = fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("- [x] a1") && written.contains("- [x] a2"),
        "{written}"
    );
    assert!(written.contains("- [ ] b1"), "{written}");
    assert_eq!(
        state.status_message.as_deref(),
        Some("\"B\" reset to not done")
    );
    assert_eq!(
        state.git_sync_request().map(|s| s.description.as_str()),
        Some("Reset \"B\" to not done")
    );
    state.handle_key(KeyCode::Char('u'));
    assert!(
        fs::read_to_string(&path).unwrap().contains("- [x] b1"),
        "one undo step"
    );
}

#[test]
fn r_then_l_names_the_pre_heading_list_after_the_document_title() {
    let path = write_real_file("# Weekly ops\n\n- [x] a1\n\n## B\n\n- [ ] b1\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.focus_item(0, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('l'));
    assert!(fs::read_to_string(&path).unwrap().contains("- [ ] a1"));
    assert_eq!(
        state.status_message.as_deref(),
        Some("\"Weekly ops\" reset to not done")
    );
    assert_eq!(
        state.git_sync_request().map(|s| s.description.as_str()),
        Some("Reset \"Weekly ops\" to not done")
    );
    fs::remove_file(&path).ok();
}

#[test]
fn r_then_s_resets_the_sub_list_the_cursor_is_in() {
    let path = write_real_file(
        "## A\n\n- [x] parent\n  - [x] child one\n  - [/] child two\n- [x] other\n",
    );
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.focus_item(0, 2, CursorMove::Quiet);
    assert_eq!(
        state.sub_list_root(),
        Some(0),
        "a child's sub-list is its parent's"
    );
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.reset_count(ResetScope::SubList), 3);
    state.handle_key(KeyCode::Char('s'));
    let written = fs::read_to_string(&path).unwrap();
    assert!(written.contains("- [ ] parent") && written.contains("  - [ ] child two"));
    assert!(
        written.contains("- [x] other"),
        "outside the sub-list: {written}"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("\"parent\" and its sub-tasks"))
    );

    state.focus_item(0, 3, CursorMove::Quiet);
    assert_eq!(
        state.sub_list_root(),
        None,
        "a top-level task with no sub-tasks"
    );
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('s'));
    assert_eq!(
        state.overlay,
        Some(Overlay::ConfirmReset),
        "s without a sub-list keeps the dialog open"
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("No sub-list here: y resets everything")
    );
    assert!(state.status_is_error);
}

#[test]
fn a_scoped_reset_with_nothing_to_reset_says_so_and_stays_open() {
    let path = write_real_file("## A\n\n- [x] a\n\n## B\n\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path).unwrap());
    state.focus_item(1, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Nothing to reset there"))
    );
}
