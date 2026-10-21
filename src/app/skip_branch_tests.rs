use crate::app::test_support::*;

const BRANCH: &str = "## Deploy\n\n- If using PostgreSQL:\n  - [ ] Stop the replica\n  - [x] Run the migration\n  - [/] Restart the replica\n  - [ ] Check the logs\n- [ ] Announce\n";

fn status(state: &AppState) -> &str {
    state.status_message.as_deref().unwrap_or_default()
}

#[test]
fn dash_on_a_note_with_open_sub_tasks_asks_then_skips_them_all_as_one_change() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "If using PostgreSQL");
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmSkip));
    assert_eq!(file_text(&path), BRANCH, "nothing written before y");
    let plan = state.branch_skip().unwrap();
    assert!(!plan.reopen);
    assert_eq!(plan.sub_tasks, 3);

    state.handle_key(KeyCode::Char('y'));
    assert_eq!(state.overlay, None);
    let written = file_text(&path);
    for line in [
        "  - [-] Stop the replica",
        "  - [x] Run the migration",
        "  - [-] Restart the replica",
        "  - [-] Check the logs",
        "- [ ] Announce",
    ] {
        assert!(written.contains(line), "{line:?} in {written}");
    }
    assert_eq!(
        status(&state),
        "Skipped 3 sub-tasks of \"If using PostgreSQL:\""
    );
    assert!(!state.status_is_error);
    assert_eq!(
        state.git_sync_request().map(|s| s.description.as_str()),
        Some("Skip \"If using PostgreSQL:\" and its sub-tasks")
    );
    assert_eq!(
        state.current_item().unwrap().label(),
        "Announce",
        "moves on"
    );

    state.handle_key(KeyCode::Char('u'));
    assert_eq!(file_text(&path), BRANCH, "one undo step restores all");
    fs::remove_file(&path).ok();
}

#[test]
fn dash_on_a_fully_skipped_branch_offers_to_reopen_it() {
    let markdown = "## A\n\n- Optional:\n  - [-] one\n  - [x] two\n  - [-] three\n- [ ] after\n";
    let (mut state, path) = state_from_markdown(markdown);
    focus_text(&mut state, "Optional");
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmSkip));
    assert!(state.branch_skip().unwrap().reopen);
    state.handle_key(KeyCode::Char('y'));
    let written = file_text(&path);
    assert!(
        written.contains("  - [ ] one")
            && written.contains("  - [x] two")
            && written.contains("  - [ ] three"),
        "{written}"
    );
    assert_eq!(status(&state), "Reopened 2 sub-tasks of \"Optional:\"");
    assert_eq!(
        state.git_sync_request().map(|s| s.description.as_str()),
        Some("Reopen \"Optional:\" and its sub-tasks")
    );
    assert_eq!(state.view, View::Checklist);
    fs::remove_file(&path).ok();
}

#[test]
fn a_parent_task_is_skipped_and_reopened_with_its_sub_tasks() {
    let markdown = "## A\n\n- [ ] parent\n  - [ ] child\n- [ ] other\n";
    let (mut state, path) = state_from_markdown(markdown);
    focus_text(&mut state, "parent");
    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Char('y'));
    let written = file_text(&path);
    assert!(written.contains("- [-] parent") && written.contains("  - [-] child"));
    assert_eq!(status(&state), "Skipped \"parent\" and its 1 sub-task");

    focus_text(&mut state, "parent");
    state.handle_key(KeyCode::Char('-'));
    assert!(state.branch_skip().unwrap().reopen);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(file_text(&path), markdown);
    assert_eq!(status(&state), "Reopened \"parent\" and its 1 sub-task");
    fs::remove_file(&path).ok();
}

#[test]
fn any_other_key_cancels_the_skip_and_says_so() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "If using PostgreSQL");
    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(state.overlay, None);
    assert_eq!(status(&state), "Skip cancelled: nothing changed");
    assert_eq!(file_text(&path), BRANCH);

    let markdown = "## A\n\n- Optional:\n  - [-] one\n";
    let (mut state, other) = state_from_markdown(markdown);
    focus_text(&mut state, "Optional");
    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Esc);
    assert_eq!(status(&state), "Reopen cancelled: nothing changed");
    assert!(state.git_sync_request().is_none());
    fs::remove_file(&path).ok();
    fs::remove_file(&other).ok();
}

#[test]
fn critical_sub_tasks_stay_open_and_keep_their_own_confirmation() {
    let markdown =
        "## A\n\n- Branch:\n  - [ ] **! Drop the table**\n  - [ ] tidy up\n- [ ] after\n";
    let (mut state, path) = state_from_markdown(markdown);
    focus_text(&mut state, "Branch");
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmSkip));
    state.handle_key(KeyCode::Char('y'));
    let written = file_text(&path);
    assert!(
        written.contains("  - [ ] **! Drop the table**") && written.contains("  - [-] tidy up"),
        "{written}"
    );
    assert_eq!(
        status(&state),
        "Skipped 1 sub-task of \"Branch:\" · 1 critical step stays open: skip it on its own"
    );
    assert_eq!(
        state.current_item().unwrap().label(),
        "Drop the table",
        "moves on to the critical step left open"
    );

    focus_text(&mut state, "Branch");
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.overlay, None, "only a critical step is left open");
    assert!(state.status_is_error);
    assert_eq!(
        status(&state),
        "Critical step under \"Branch:\": skip \"Drop the table\" on its own first (- twice, pausing between)"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn several_critical_steps_are_counted() {
    let markdown = "## A\n\n- [ ] **! Root**\n  - [ ] **! One**\n  - [ ] plain\n- [ ] after\n";
    let (mut state, path) = state_from_markdown(markdown);
    state.focus_item(0, 0, CursorMove::Quiet);
    let plan = state.branch_skip().unwrap();
    assert_eq!(plan.held.len(), 2, "the critical parent is held too");
    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Char('y'));
    assert!(file_text(&path).contains("- [ ] **! Root**"));
    assert_eq!(
        status(&state),
        "Skipped 1 sub-task of \"Root\" · 2 critical steps stay open: skip each on its own"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn a_note_whose_sub_tasks_are_all_done_has_nothing_to_skip() {
    let (mut state, path) = state_from_markdown("## A\n\n- Done note:\n  - [x] one\n- [ ] b\n");
    focus_text(&mut state, "Done note");
    state.handle_key(KeyCode::Char('-'));
    assert_eq!(state.overlay, None);
    assert!(state.status_is_error);
    assert_eq!(
        status(&state),
        "Nothing to skip: every sub-task of this note is done"
    );
    fs::remove_file(&path).ok();
}

#[test]
fn a_parent_task_with_every_sub_task_resolved_toggles_on_its_own() {
    let markdown = "## A\n\n- [ ] parent\n  - [x] child\n- [ ] other\n";
    let (mut state, path) = state_from_markdown(markdown);
    focus_text(&mut state, "parent");
    assert!(state.branch_skip().is_none());
    state.handle_key(KeyCode::Char('-'));
    assert!(file_text(&path).contains("- [-] parent"));
    assert_eq!(status(&state), "Marked skipped");

    let markdown = "## A\n\n- [ ] parent\n  - [-] child\n- [ ] other\n";
    let (state, other) = state_from_markdown(markdown);
    let mut state = state;
    focus_text(&mut state, "parent");
    assert!(
        state.branch_skip().is_none(),
        "an open parent over skipped sub-tasks skips on its own"
    );
    fs::remove_file(&path).ok();
    fs::remove_file(&other).ok();
}

#[test]
fn skipping_the_last_open_branch_finishes_the_list() {
    let markdown = "## A\n\n- [x] first\n- Branch:\n  - [ ] one\n  - [ ] two\n\n## B\n\n- [ ] b\n";
    let (mut state, path) = state_from_markdown(markdown);
    focus_text(&mut state, "Branch");
    state.handle_key(KeyCode::Char('-'));
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(state.view, View::ListComplete);
    fs::remove_file(&path).ok();
}

#[test]
fn reload_cancels_a_pending_skip_confirmation_visibly() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "If using PostgreSQL");
    state.handle_key(KeyCode::Char('-'));
    touch_with_new_mtime(&path, &BRANCH.replace("- [ ] Announce", "- [x] Announce"));
    assert!(state.reload_if_changed());
    assert_eq!(state.overlay, None);
    assert_eq!(status(&state), "File changed on disk — skip cancelled");
    fs::remove_file(&path).ok();
}

#[test]
fn a_blocked_write_leaves_the_dialog_open_with_the_error() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "If using PostgreSQL");
    state.handle_key(KeyCode::Char('-'));
    state.file_gone = Some(FileGone::Deleted);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmSkip));
    assert!(state.status_is_error);
    assert_eq!(file_text(&path), BRANCH);
    fs::remove_file(&path).ok();
}

#[test]
fn a_failed_write_is_not_an_undo_point() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "If using PostgreSQL");
    state.handle_key(KeyCode::Char('-'));
    fs::write(&path, "## Deploy\n\n- [ ] changed elsewhere\n").unwrap();
    assert!(!state.apply_branch_skip());
    assert!(state.undo_stack.is_empty());
    fs::remove_file(&path).ok();
}

#[test]
fn applying_with_no_branch_left_says_so() {
    let (mut state, path) = state_from_markdown(BRANCH);
    focus_text(&mut state, "Announce");
    state.overlay = Some(Overlay::ConfirmSkip);
    assert!(!state.apply_branch_skip());
    assert_eq!(state.overlay, None);
    assert_eq!(status(&state), "Nothing to skip or reopen here any more");
    fs::remove_file(&path).ok();
}

#[test]
fn sub_task_count_names_one_and_many() {
    assert_eq!(super::sub_task_count(1), "1 sub-task");
    assert_eq!(super::sub_task_count(4), "4 sub-tasks");
}
