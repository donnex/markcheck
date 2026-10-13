use super::*;
use crate::test_support::shown;
use std::path::PathBuf;

fn checkbox(line_number: usize, completed: bool) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: format!("task {line_number}"),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::Checkbox(if completed {
            TaskState::Done
        } else {
            TaskState::NotStarted
        }),
    }
}

fn display_only(line_number: usize) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: format!("heading {line_number}"),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::DisplayOnly,
    }
}

fn unique_temp_path() -> PathBuf {
    crate::test_support::unique_temp_path("app", "", Some("md"))
}

/// The source lines a hand-built `lists` stands for: each item's own line
/// carries the bullet and checkbox its `kind` claims, so a write-back
/// rewrites a real marker on the right line. Lines no item claims are blank.
fn raw_lines_for(lists: &[List]) -> Vec<String> {
    let items = || lists.iter().flat_map(|list| list.items.iter());
    let max_line = items().map(|item| item.line_number).max().unwrap_or(0);
    let mut lines = vec!["\n".to_string(); max_line];
    for item in items() {
        let marker = match item.kind {
            ItemKind::Checkbox(TaskState::NotStarted) => "[ ] ",
            ItemKind::Checkbox(TaskState::Started) => "[/] ",
            ItemKind::Checkbox(TaskState::Done) => "[x] ",
            ItemKind::DisplayOnly => "",
        };
        let indent = "  ".repeat(item.depth);
        lines[item.line_number - 1] = format!("{indent}- {marker}{}\n", item.display_text);
    }
    lines
}

/// A document from hand-built `lists`, backed by a real file holding exactly
/// the Markdown those lists describe, so a write test can read the file back.
fn document_with_lists(lists: Vec<List>) -> Document {
    let document = document_with_missing_file(lists);
    fs::write(&document.file_path, writer::document_contents(&document)).unwrap();
    document
}

/// Like `document_with_lists`, but the backing file is never created, so
/// `write_back` always fails.
fn document_with_missing_file(lists: Vec<List>) -> Document {
    let raw_lines = raw_lines_for(&lists);
    Document {
        file_path: unique_temp_path(),
        title: None,
        has_default_list: false,
        lists,
        raw_lines,
    }
}

/// Writes `markdown` to a fresh temp file and loads it the way `main.rs`
/// does, so the items and the file always agree.
fn state_from_markdown(markdown: &str) -> (AppState, PathBuf) {
    let path = write_real_file(markdown);
    let state = AppState::new(parser::parse_document(path.clone()).unwrap());
    (state, path)
}

fn file_text(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn two_list_document() -> Document {
    document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, false), checkbox(2, false), checkbox(3, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(4, false), checkbox(5, false)],
        },
    ])
}

/// List 1's first two items are already completed; only the last
/// item remains, so toggling it triggers a ListComplete transition.
fn two_list_document_first_list_almost_done() -> Document {
    document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, true), checkbox(2, true), checkbox(3, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(4, false), checkbox(5, false)],
        },
    ])
}

// --- Started / in-progress task state ---

#[test]
fn a_write_is_refused_while_another_instance_holds_the_write_lock() {
    // The lock spans the hash check and the write.
    use crate::writer::{LockOutcome, WriteLock};

    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
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
    assert_eq!(state.document.checkbox_stats(), (2, 5), "started ≠ done");
}

#[test]
fn startup_selects_started_task_as_first_undone() {
    // A started task still counts as work remaining, so startup lands
    // on it rather than skipping past.
    let mut document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
    }]);
    document.lists[0].items[1].kind = ItemKind::Checkbox(TaskState::Started);
    let state = AppState::new(document);
    assert_eq!(state.current_item_index, 1, "lands on the started item");
}

// --- Help overlay ---

#[test]
fn help_key_opens_and_a_non_scroll_key_closes() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    assert_eq!(state.overlay, Some(Overlay::Help));
    // A non-scroll key closes (scroll keys now scroll instead).
    state.handle_key(KeyCode::Char('x'));
    assert_eq!(
        shown(&state),
        (View::Checklist, None),
        "a non-scroll key closes help"
    );
}

#[test]
fn help_scroll_keys_scroll_instead_of_closing() {
    // On a short help viewport, j/k and the card scroll keys move the
    // overlay rather than dismissing it; it clamps and any other key closes.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.help.max_scroll = 5; // as if the renderer measured an overflow
    state.help.viewport_height = 6;
    state.handle_key(KeyCode::Char('j'));
    assert_eq!(
        state.overlay,
        Some(Overlay::Help),
        "j scrolls, does not close"
    );
    assert_eq!(state.help.scroll, 1);
    state.handle_key(KeyCode::Down);
    assert_eq!(state.help.scroll, 2);
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.help.scroll, 1);
    state.handle_key_with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(state.help.scroll, 4, "Ctrl-D jumps half a page (3)");
    state.handle_key(KeyCode::PageDown);
    assert_eq!(state.help.scroll, 5, "clamps at help.max_scroll");
    state.handle_key(KeyCode::Char('q')); // a non-scroll key still closes
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn help_returns_to_the_screen_it_was_opened_from() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    state.handle_key(KeyCode::Char('?'));
    assert_eq!(state.overlay, Some(Overlay::Help));
    state.handle_key(KeyCode::Esc);
    assert_eq!(shown(&state), (View::ListComplete, None));
}

// --- Advancing between lists ---

#[test]
fn navigate_forward_crosses_into_next_list_at_end() {
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 2; // last item of list 0
    state.navigate_forward();
    assert_eq!(state.current_list_index, 1, "moved to next list");
    assert_eq!(state.current_item_index, 0, "first undone of that list");
}

#[test]
fn navigate_forward_clamps_at_last_list_end() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1;
    state.current_item_index = 1; // last item of the last list
    state.navigate_forward();
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1, "stays put at the very end");
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
fn navigate_backward_within_list_steps_one_item() {
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 2;
    state.navigate_backward();
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 1, "just the previous item");
}

#[test]
fn navigate_backward_clamps_at_first_list_start() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.current_item_index = 0;
    state.navigate_backward();
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 0, "stays put at the very start");
}

#[test]
fn completing_last_item_with_earlier_incomplete_stays_in_list() {
    // Regression guard: the toggle-driven advance uses navigate_next
    // (which clamps), NOT navigate_forward, so completing the last item
    // while an earlier one is unchecked must not skip to the next
    // list.
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 2; // items 0 and 1 remain not done
    state.toggle_current();
    assert_eq!(state.current_list_index, 0, "does not cross lists");
    assert_eq!(shown(&state), (View::Checklist, None));
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
fn completing_a_task_lands_on_the_next_open_task_past_done_ones() {
    let path = write_real_file("## L\n\n- [ ] a\n- [x] b\n- [ ] c\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    assert_eq!(state.current_item_index, 2, "skips done b");
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
fn completing_a_task_still_stops_on_a_following_note() {
    let path = write_real_file("## L\n\n- [ ] a\n- a note\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    assert_eq!(state.current_item_index, 1, "the note is still shown");
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
fn tab_jumps_to_next_incomplete_task_within_and_across_lists() {
    // Tab = next unfinished task anywhere. All-undone doc: from (0,0)
    // Tab steps to the next undone task in the same list.
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.current_item_index = 0;
    state.handle_key(KeyCode::Tab);
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (0, 1),
        "next undone task in the same list"
    );

    // list 0 = [done, done, undone], list 1 = [undone, undone]: from
    // the last undone task in list 0, Tab crosses into list 1.
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_list_index = 0;
    state.current_item_index = 2;
    state.handle_key(KeyCode::Tab);
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (1, 0),
        "crosses into the next list's first undone task"
    );
}

#[test]
fn tab_wraps_around_to_the_first_incomplete_task() {
    // Only undone: (0,2), (1,0), (1,1). From the last one, Tab wraps.
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_list_index = 1;
    state.current_item_index = 1;
    state.handle_key(KeyCode::Tab);
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (0, 2),
        "wraps to the first unfinished task"
    );
}

#[test]
fn n_unbound_and_shift_l_jumps_lists() {
    // `n` was dropped; Shift-L (not Tab) now jumps lists.
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.current_item_index = 0;
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(state.current_list_index, 0, "n is unbound");
    state.handle_key(KeyCode::Char('L'));
    assert_eq!(state.current_list_index, 1, "Shift-L jumps lists");
}

#[test]
fn shift_l_at_the_last_incomplete_list_reports_it() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1; // already the last (and only later) list
    state.jump_to_next_incomplete_list();
    assert_eq!(state.current_list_index, 1, "did not move");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Already at the last incomplete list")
    );
    assert!(state.status_is_error);
}

#[test]
fn shift_h_at_the_first_incomplete_list_reports_it() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0; // already the first (and only earlier) list
    state.jump_to_prev_incomplete_list();
    assert_eq!(state.current_list_index, 0, "did not move");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Already at the first incomplete list")
    );
    assert!(state.status_is_error);
}

#[test]
fn shift_l_with_no_incomplete_lists_at_all_reports_it() {
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }]));
    state.jump_to_next_incomplete_list();
    assert_eq!(
        state.status_message.as_deref(),
        Some("No lists have unfinished tasks")
    );
    assert!(state.status_is_error);
}

#[test]
fn tab_with_no_unfinished_tasks_reports_it() {
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }]));
    state.jump_to_next_incomplete_task();
    assert_eq!(state.status_message.as_deref(), Some("No unfinished tasks"));
    assert!(state.status_is_error);
}

#[test]
fn gg_goes_to_first_task_and_g_then_other_key_cancels() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1;
    state.current_item_index = 1;
    // A lone `g` then a non-`g` key cancels the pending motion.
    state.handle_key(KeyCode::Char('g'));
    assert!(state.pending_g);
    state.handle_key(KeyCode::Char('l'));
    assert!(!state.pending_g, "a non-g key cancels the pending gg");
    assert_ne!(
        (state.current_list_index, state.current_item_index),
        (0, 0),
        "single g does not jump"
    );
    // `gg` jumps to the very first task.
    state.current_list_index = 1;
    state.current_item_index = 1;
    state.handle_key(KeyCode::Char('g'));
    state.handle_key(KeyCode::Char('g'));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
}

#[test]
fn capital_g_goes_to_last_task() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.current_item_index = 0;
    state.handle_key(KeyCode::Char('G'));
    let last_list = state.document.lists.len() - 1;
    let last_item = state.document.lists[last_list].items.len() - 1;
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (last_list, last_item)
    );
}

#[test]
fn motions_are_noops_when_no_list_has_items() {
    // Lists exist but hold no checklist items (e.g. headings with only
    // prose beneath). Position motions must clamp safely and the
    // Tab-jump must bail on the empty item-order without panicking.
    let mut state = AppState::new(document_with_lists(vec![
        List {
            title: "Empty A".to_string(),
            banner: None,
            items: vec![],
        },
        List {
            title: "Empty B".to_string(),
            banner: None,
            items: vec![],
        },
    ]));
    assert!(state.current_item().is_none());

    state.go_to_last_item();
    state.jump_to_next_incomplete_task();
    state.go_to_first_item();

    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
#[should_panic(expected = "at least one list")]
fn new_panics_on_a_list_less_document_in_debug() {
    // The ≥1-list invariant is guarded by a debug_assert in the
    // constructor (active under cfg(debug_assertions), which tests run
    // with), so violating it fails loudly and immediately rather than as
    // an opaque out-of-bounds panic on the first navigation.
    let _ = AppState::new(document_with_lists(vec![]));
}

// --- Search foundation ---

fn text_item(line: usize, text: &str) -> Item {
    let mut item = checkbox(line, false);
    item.display_text = text.to_string();
    item
}

#[test]
fn find_matches_is_smart_case_and_in_document_order() {
    let state = AppState::new(document_with_lists(vec![
        List {
            title: "L1".to_string(),
            banner: None,
            items: vec![
                text_item(1, "Restart the API server"),
                text_item(2, "check disk space"),
            ],
        },
        List {
            title: "L2".to_string(),
            banner: None,
            items: vec![text_item(3, "restart the worker")],
        },
    ]));
    // Lowercase query is case-insensitive: matches "Restart" and "restart",
    // returned in document order across lists.
    assert_eq!(state.find_matches("restart"), vec![(0, 0), (1, 0)]);
    // A query with an uppercase letter is case-sensitive (smart-case).
    assert_eq!(state.find_matches("Restart"), vec![(0, 0)]);
    // Empty query and a non-match both yield nothing.
    assert!(state.find_matches("").is_empty());
    assert!(state.find_matches("zzz").is_empty());
}

#[test]
fn find_matches_searches_command_text() {
    // A task is findable by a command in its fenced block, not just prose.
    let mut item = text_item(1, "reboot the box");
    item.code_blocks = vec!["systemctl restart nginx".to_string()];
    let state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![item],
    }]));
    assert_eq!(state.find_matches("nginx"), vec![(0, 0)]);
}

#[test]
fn focus_item_lands_on_exact_item_and_clamps() {
    let mut state = AppState::new(two_list_document());
    state.focus_item(1, 0, CursorMove::Quiet);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(shown(&state), (View::Checklist, None));
    // An out-of-range item clamps to the list's last item...
    state.focus_item(0, 99, CursorMove::Quiet);
    let last = state.document.lists[0].items.len() - 1;
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (0, last)
    );
    // ...and an out-of-range list index is ignored (no move, no panic).
    state.focus_item(99, 0, CursorMove::Quiet);
    assert_eq!(state.current_list_index, 0);
}

fn type_str(state: &mut AppState, text: &str) {
    for c in text.chars() {
        state.handle_key(KeyCode::Char(c));
    }
}

#[test]
fn slash_enters_search_and_typing_jumps_to_first_match() {
    // two_list_document items are "task 1".."task 5" across lists.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    assert_eq!(state.overlay, Some(Overlay::Search));
    type_str(&mut state, "task 3");
    assert_eq!(state.search.query, "task 3");
    // "task 3" is the third item of the first list.
    assert_eq!((state.current_list_index, state.current_item_index), (0, 2));
    // Still on the Search screen until committed.
    assert_eq!(state.overlay, Some(Overlay::Search));
}

#[test]
fn search_ignores_control_modified_keys() {
    // A Ctrl-combo (e.g. Ctrl-H) must not insert its base letter into the
    // query — only plain/Shift typing edits it.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    state.handle_key_with_mods(KeyCode::Char('h'), KeyModifiers::CONTROL);
    assert_eq!(state.search.query, "");
    state.handle_key(KeyCode::Char('h')); // plain typing still works
    assert_eq!(state.search.query, "h");
}

#[test]
fn search_query_captures_command_keys_as_text() {
    // A command key like `q` typed into the query is literal text, not a
    // quit — the search branch owns all input.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "q?j");
    assert_eq!(state.search.query, "q?j");
    assert!(!state.should_quit, "typing q in search does not quit");
    assert_eq!(state.overlay, Some(Overlay::Search));
}

#[test]
fn search_backspace_edits_query_and_rejumps() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 4"); // jumps to (1, 0)
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    state.handle_key(KeyCode::Backspace); // "task " -> first match again
    assert_eq!(state.search.query, "task ");
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
}

#[test]
fn search_enter_commits_and_n_cycles_matches() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task"); // matches all five, cursor at (0,0)
    state.handle_key(KeyCode::Enter);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(state.search.last.as_deref(), Some("task"));
    // n advances through matches in document order, then wraps.
    for expected in [(0, 1), (0, 2), (1, 0), (1, 1), (0, 0)] {
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(
            (state.current_list_index, state.current_item_index),
            expected
        );
    }
    // N goes backward and wraps.
    state.handle_key(KeyCode::Char('N'));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
}

#[test]
fn search_esc_restores_the_pre_search_cursor() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1;
    state.current_item_index = 1;
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 1"); // jumps to (0, 0)
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    state.handle_key(KeyCode::Esc);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
}

#[test]
fn n_without_an_active_search_stays_put_and_says_so() {
    for key in ['n', 'N'] {
        let mut state = AppState::new(two_list_document());
        state.handle_key(KeyCode::Char(key));
        assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
        assert!(state.search.last.is_none());
        assert_eq!(
            state.status_message.as_deref(),
            Some("No previous search — press / to search")
        );
        assert!(state.status_is_error);
    }
}

#[test]
fn unbound_key_on_list_complete_hints_at_the_screen_keys() {
    for key in [KeyCode::Char(' '), KeyCode::Char('/'), KeyCode::Char('y')] {
        let mut state = AppState::new(two_list_document());
        state.view = View::ListComplete;
        state.handle_key(key);
        assert_eq!(shown(&state), (View::ListComplete, None));
        assert_eq!(
            state.status_message.as_deref(),
            Some("List complete — press l for the next list or h to review")
        );
        assert!(state.status_is_error);
    }
}

#[test]
fn unbound_key_on_all_complete_hints_at_the_screen_keys() {
    for key in [KeyCode::Char(' '), KeyCode::Char('l'), KeyCode::Enter] {
        let mut state = AppState::new(two_list_document());
        state.view = View::AllComplete;
        state.handle_key(key);
        assert_eq!(shown(&state), (View::AllComplete, None));
        assert_eq!(
            state.status_message.as_deref(),
            Some("All tasks complete — press h to review, R to reset or q to quit")
        );
        assert!(state.status_is_error);
    }
}

#[test]
fn search_reports_match_position_and_no_match_feedback() {
    // A committed search is never silent — it reports the position...
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task"); // matches all five tasks
    state.handle_key(KeyCode::Enter);
    // ...with a nudge to `n`/`N` on commit when there's more than one.
    assert_eq!(
        state.status_message.as_deref(),
        Some("Match 1/5 · n/N to cycle")
    );
    // ...and `n` updates the reported position (no nudge once cycling).
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(state.status_message.as_deref(), Some("Match 2/5"));
    // A committed query with no matches gives a sticky "no matches".
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "zzz");
    state.handle_key(KeyCode::Enter);
    assert_eq!(
        state.status_message.as_deref(),
        Some("No matches for \"zzz\"")
    );
    assert!(state.status_expiry.is_none(), "no-match message is sticky");
}

#[test]
fn search_from_mid_document_lands_on_the_next_match_after_the_cursor() {
    // Like vim's `/`, the search runs forward from the cursor rather than
    // restarting at the top of the document.
    let mut state = AppState::new(two_list_document());
    state.focus_item(0, 2, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "t");
    assert_eq!((state.current_list_index, state.current_item_index), (0, 2));
    type_str(&mut state, "ask");
    assert_eq!((state.current_list_index, state.current_item_index), (0, 2));
    state.handle_key(KeyCode::Enter);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Match 3/5 · n/N to cycle")
    );
    state.handle_key(KeyCode::Char('n'));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(state.status_message.as_deref(), Some("Match 4/5"));
}

#[test]
fn search_wraps_to_the_first_match_when_none_follow_the_cursor() {
    let path = write_real_file("## L\n\n- [ ] `restart a`\n- [ ] `restart b`\n- [ ] `check`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.focus_item(0, 2, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "restart");
    assert_eq!(state.current_item_index, 0);
    state.handle_key(KeyCode::Enter);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Match 1/2 · n/N to cycle")
    );
    fs::remove_file(&path).ok();
}

#[test]
fn committing_a_single_match_search_omits_the_cycle_nudge() {
    // The `n`/`N` nudge only makes sense with more than one match.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 4"); // exactly one task
    state.handle_key(KeyCode::Enter);
    assert_eq!(state.status_message.as_deref(), Some("Match 1/1"));
}

#[test]
fn a_search_that_stops_matching_returns_the_cursor_to_its_origin() {
    // Like vim's incremental search: once the query matches nothing, the
    // cursor goes back to where the search began instead of staying on the
    // last matching prefix's card, and committing leaves it there.
    let mut state = AppState::new(two_list_document());
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 1");
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    type_str(&mut state, "x");
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    state.handle_key(KeyCode::Enter);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    assert_eq!(
        state.status_message.as_deref(),
        Some("No matches for \"task 1x\"")
    );
    assert!(state.status_is_error);
}

#[test]
fn an_emptied_search_returns_the_cursor_to_its_origin() {
    let mut state = AppState::new(two_list_document());
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 1");
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    for _ in 0..6 {
        state.handle_key(KeyCode::Backspace);
    }
    assert_eq!(state.search.query, "");
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    state.handle_key(KeyCode::Enter);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn a_failed_or_empty_search_commit_never_auto_copies() {
    for query in ["task 1x", ""] {
        let mut document = two_list_document();
        for list in &mut document.lists {
            for item in &mut list.items {
                item.code_spans = vec![format!("cmd-{}", item.line_number)];
            }
        }
        let mut state = AppState::new(document);
        state.auto_copy = true;
        state.focus_item(1, 1, CursorMove::Quiet);
        crate::clipboard::fake::with(|c| {
            c.system_attempts.clear();
            c.osc52.clear();
        });
        state.handle_key(KeyCode::Char('/'));
        type_str(&mut state, query);
        state.handle_key(KeyCode::Enter);
        assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
        crate::clipboard::fake::with(|c| {
            assert!(c.system_attempts.is_empty(), "{query:?}: nothing copied");
            assert!(c.osc52.is_empty(), "{query:?}: nothing sent over OSC 52");
        });
        if query.is_empty() {
            assert_eq!(state.status_message, None);
        } else {
            assert_eq!(
                state.status_message.as_deref(),
                Some("No matches for \"task 1x\"")
            );
            assert!(state.status_is_error);
        }
    }
}

#[test]
fn a_successful_search_commit_auto_copies_the_match() {
    let mut document = two_list_document();
    for list in &mut document.lists {
        for item in &mut list.items {
            item.code_spans = vec![format!("cmd-{}", item.line_number)];
        }
    }
    let expected = document.lists[1].items[0].code_spans[0].clone();
    let mut state = AppState::new(document);
    state.auto_copy = true;
    crate::clipboard::fake::with(|c| c.system_attempts.clear());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 4");
    crate::clipboard::fake::with(|c| assert!(c.system_attempts.is_empty()));
    state.handle_key(KeyCode::Enter);
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![(expected.clone(), false)]);
    });
}

// --- Bracketed paste ---

#[test]
fn paste_on_the_checklist_changes_nothing_and_says_so() {
    let mut state = AppState::new(two_list_document());
    state.navigate_next();
    let before = fs::read_to_string(&state.document.file_path).unwrap();
    let states = |state: &AppState| -> Vec<ItemKind> {
        state
            .document
            .lists
            .iter()
            .flat_map(|l| l.items.iter().map(|i| i.kind))
            .collect()
    };
    let states_before = states(&state);
    state.handle_paste("systemctl restart example-service\nRy q");
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 1));
    assert_eq!(states(&state), states_before);
    assert_eq!(
        fs::read_to_string(&state.document.file_path).unwrap(),
        before
    );
    assert!(!state.should_quit);
    assert!(state.status_is_error);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Paste ignored — markcheck takes single-key commands")
    );
}

#[test]
fn paste_on_the_help_overlay_is_ignored_and_keeps_it_open() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.handle_paste("x");
    assert_eq!(state.overlay, Some(Overlay::Help));
    assert!(state.status_is_error);
}

#[test]
fn paste_into_search_fills_the_query_as_one_line() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    state.handle_paste("task\t4\u{7}\r\n");
    assert_eq!(state.search.query, "task 4");
    assert_eq!(state.overlay, Some(Overlay::Search));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert!(!state.status_is_error);
}

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

// --- Go-to-task overlay (ListPicker) ---

#[test]
fn t_opens_picker_listing_all_tasks_then_filters() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    assert_eq!(state.overlay, Some(Overlay::ListPicker));
    assert_eq!(
        state.picker_matches().len(),
        5,
        "all tasks listed unfiltered"
    );
    type_str(&mut state, "task 4");
    assert_eq!(state.picker.query, "task 4");
    assert_eq!(state.picker_matches(), vec![(1, 0)], "filtered to one task");
    assert_eq!(
        state.picker.selection, 0,
        "selection reset on filter change"
    );
}

#[test]
fn brace_keys_jump_between_sub_sections_with_feedback() {
    // `}` / `{` move to the first item of the next / previous
    // sub-section within the current list, and report where they land or
    // that there's nowhere further to go.
    let sub = |level: u8, text: &str| crate::model::SubHeading {
        level,
        text: text.to_string(),
    };
    let with_section = |line: usize, section: Vec<crate::model::SubHeading>| {
        let mut item = text_item(line, &format!("task {line}"));
        item.section = section;
        item
    };
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![
            with_section(1, vec![]),                // 0
            with_section(2, vec![sub(3, "Alpha")]), // 1: starts Alpha
            with_section(3, vec![sub(3, "Alpha")]), // 2
            with_section(4, vec![sub(3, "Beta")]),  // 3: starts Beta
        ],
    }]));
    // Starts at item 0; `}` lands on the first sub-section start.
    assert_eq!(state.current_item_index, 0);
    state.handle_key(KeyCode::Char('}'));
    assert_eq!(state.current_item_index, 1);
    assert_eq!(state.status_message.as_deref(), Some("Sub-section: Alpha"));
    // `}` again lands on the next.
    state.handle_key(KeyCode::Char('}'));
    assert_eq!(state.current_item_index, 3);
    assert_eq!(state.status_message.as_deref(), Some("Sub-section: Beta"));
    // No further sub-section: cursor holds, error feedback shown.
    state.handle_key(KeyCode::Char('}'));
    assert_eq!(state.current_item_index, 3);
    assert!(state.status_is_error);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Already at the last sub-section")
    );
    // `{` walks back to the previous start.
    state.handle_key(KeyCode::Char('{'));
    assert_eq!(state.current_item_index, 1);
    assert_eq!(state.status_message.as_deref(), Some("Sub-section: Alpha"));
    // `{` before the first sub-section: holds with feedback.
    state.handle_key(KeyCode::Char('{'));
    assert_eq!(state.current_item_index, 1);
    assert!(state.status_is_error);
}

#[test]
fn brace_keys_report_when_the_list_has_no_sub_sections() {
    // A list with no `### H3`+ sub-sections gives an explicit notice
    // rather than a silent no-op.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('}'));
    assert!(state.status_is_error);
    assert_eq!(
        state.status_message.as_deref(),
        Some("No sub-sections in this list")
    );
}

#[test]
fn picker_filter_matches_sub_section_name_without_leaking_into_search() {
    // The go-to-task picker filters on the item's `### H3`+ sub-section
    // path too, so "pre-flight" finds the task under it — but that section
    // text must NOT leak into the main `/` search.
    let mut item = text_item(1, "confirm the staging host is idle");
    item.section = vec![crate::model::SubHeading {
        level: 3,
        text: "Pre-flight checks".to_string(),
    }];
    let plain = text_item(2, "run the deploy");
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "Deploy".to_string(),
        banner: None,
        items: vec![item, plain],
    }]));

    // The picker filter matches the sub-section name → only its task.
    state.picker.query = "pre-flight".to_string();
    assert_eq!(state.picker_matches(), vec![(0, 0)]);
    // The main `/` search does not see the section text at all.
    assert!(state.find_matches("pre-flight").is_empty());
    // A body-text query still works in both the picker and search.
    state.picker.query = "staging".to_string();
    assert_eq!(state.picker_matches(), vec![(0, 0)]);
    assert_eq!(state.find_matches("staging"), vec![(0, 0)]);
}

#[test]
fn picker_enter_jumps_to_the_selected_task() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    type_str(&mut state, "task 5");
    assert_eq!(state.picker_matches(), vec![(1, 1)]);
    state.handle_key(KeyCode::Enter);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
}

#[test]
fn picker_selection_moves_with_arrows_and_ctrl_np_and_clamps() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    assert_eq!(state.picker.selection, 0);
    state.handle_key(KeyCode::Down);
    assert_eq!(state.picker.selection, 1);
    state.handle_key_with_mods(KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(state.picker.selection, 2);
    state.handle_key(KeyCode::Up);
    state.handle_key_with_mods(KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert_eq!(state.picker.selection, 0);
    state.handle_key(KeyCode::Up); // clamps at the top
    assert_eq!(state.picker.selection, 0);
}

#[test]
fn picker_ctrl_d_u_move_half_a_page() {
    let items: Vec<Item> = (1..=12)
        .map(|n| text_item(n, &format!("item {n}")))
        .collect();
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items,
    }]));
    state.handle_key(KeyCode::Char('T'));
    state.picker.viewport_height = 10; // as if rendered with a 10-row list
    state.handle_key_with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(state.picker.selection, 5, "Ctrl-D jumps half a page down");
    state.handle_key_with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(state.picker.selection, 0, "Ctrl-U jumps half a page up");
}

#[test]
fn picker_typing_j_and_k_filters_rather_than_moving() {
    // Because the overlay filters by typing, j/k are query input, not moves.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    state.handle_key(KeyCode::Char('j'));
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.picker.query, "jk");
    assert_eq!(
        state.picker.selection, 0,
        "typing does not move the selection"
    );
}

#[test]
fn picker_esc_closes_without_moving_the_cursor() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1;
    state.current_item_index = 1;
    state.handle_key(KeyCode::Char('T'));
    state.handle_key(KeyCode::Esc);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
}

// --- Open link (o) ---

fn item_with_links(line: usize, urls: &[&str]) -> Item {
    let mut item = checkbox(line, false);
    item.body = urls
        .iter()
        .map(|u| crate::model::BodySpan::Link {
            text: "link".to_string(),
            url: u.to_string(),
        })
        .collect();
    item
}

fn single_item_document(item: Item) -> Document {
    document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![item],
    }])
}

#[test]
fn o_requests_opening_a_single_link_and_reports_it() {
    let mut state = AppState::new(single_item_document(item_with_links(
        1,
        &["https://example.com/a"],
    )));
    state.handle_key(KeyCode::Char('o'));
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/a")
    );
    assert_eq!(
        state.status_message, None,
        "the main loop reports the outcome once the opener has run"
    );
}

#[test]
fn o_opens_a_bare_url_in_the_task_text() {
    let path =
        write_real_file("## S\n\n- [ ] Check the dashboard at https://example.com/status.\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key(KeyCode::Char('o'));
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/status")
    );
    fs::remove_file(&path).ok();
}

#[test]
fn o_opens_every_safe_scheme_including_uppercase() {
    for url in [
        "http://example.com/a",
        "https://example.com/a",
        "mailto:ops@example.com",
        "HTTPS://EX.COM/A",
        // Only the scheme is checked; the URL reaches the opener as one argv
        // element, never through a shell.
        "http://example.com/a\0/../etc/passwd",
        "https://example.com/a\n-oProxyCommand=x",
    ] {
        let mut state = AppState::new(single_item_document(item_with_links(1, &[url])));
        state.handle_key(KeyCode::Char('o'));
        assert_eq!(
            state.take_link_open_request().as_deref(),
            Some(url),
            "{url} should open"
        );
    }
}

#[test]
fn o_refuses_unsafe_schemes_with_a_sticky_hint() {
    // file:// is excluded deliberately: an opener handed a .desktop file or
    // an executable can run it, not just display it.
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "example.com/no-scheme",
        "--some-flag",
        // The scheme must be the literal start of the string — leading
        // whitespace or a stray BOM (e.g. a copy-paste artifact) must not
        // let a URL sneak past the allowlist by starting "close enough" to
        // a safe scheme.
        " http://evil.com",
        "\thttps://evil.com",
        "\u{FEFF}https://evil.com",
        // Matching is deliberately ASCII-only (`to_ascii_lowercase`), not
        // full Unicode case folding — a visually-similar non-ASCII
        // lookalike (Turkish dotted capital İ) must not be treated as
        // equivalent to ASCII 'h'/'H' and sneak past the allowlist.
        "İTTP://evil.com",
        "HTTP\u{130}://evil.com",
    ] {
        let mut state = AppState::new(single_item_document(item_with_links(1, &[url])));
        state.handle_key(KeyCode::Char('o'));
        assert!(
            state.link_open_request.is_none(),
            "{url} should not be opened"
        );
        assert_eq!(
            state.status_message.as_deref(),
            Some(format!("Not opened: unsupported link type ({url})").as_str())
        );
        assert!(state.status_expiry.is_none(), "sticky");
    }
}

#[test]
fn o_with_no_link_gives_a_sticky_hint() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('o'));
    assert!(state.link_open_request.is_none());
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to open: card has no link")
    );
    assert!(state.status_expiry.is_none(), "sticky");
}

#[test]
fn o_with_multiple_links_arms_a_digit_without_hiding_them() {
    // `o` on a multi-link card doesn't open outright — it arms a digit
    // (matching the card's [N] markers). Crucially it sets NO status
    // message, so the numbered URL list stays visible in the status bar
    // instead of being hidden by a modal prompt.
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.link_open_request.is_none());
    assert!(state.pending_open_link, "armed for a link number");
    // No status message hides the links — the numbered list stays visible in
    // the panel below the card.
    assert_eq!(state.status_message, None, "links stay visible, not hidden");
}

fn twelve_link_state() -> AppState {
    let urls: Vec<String> = (1..=12)
        .map(|n| format!("https://example.com/{n}"))
        .collect();
    let urls: Vec<&str> = urls.iter().map(String::as_str).collect();
    AppState::new(single_item_document(item_with_links(1, &urls)))
}

#[test]
fn o_then_two_digits_opens_link_twelve() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    assert!(state.pending_open_link, "1 could still become 10, 11 or 12");
    assert_eq!(state.pending_number, 1);
    assert!(state.link_open_request.is_none());
    state.handle_key(KeyCode::Char('2'));
    assert!(!state.pending_open_link, "consumed");
    assert_eq!(state.pending_number, 0);
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/12")
    );
}

#[test]
fn o_then_a_digit_that_cannot_grow_opens_at_once_on_a_long_card() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('3'));
    assert!(!state.pending_open_link);
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/3")
    );
}

#[test]
fn o_then_one_and_enter_opens_link_one_of_twelve() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Enter);
    assert!(!state.pending_open_link);
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/1")
    );
}

#[test]
fn backspace_at_the_open_prompt_deletes_the_last_digit() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Backspace);
    assert!(state.pending_open_link, "still armed");
    assert_eq!(state.pending_number, 0);
    state.handle_key(KeyCode::Char('7'));
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/7")
    );
}

#[test]
fn o_then_a_number_past_the_last_link_says_so() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Char('3'));
    assert!(!state.pending_open_link);
    assert!(state.link_open_request.is_none());
    assert_eq!(
        state.status_message.as_deref(),
        Some("No link [13] on this card")
    );
}

#[test]
fn another_key_after_a_partial_number_cancels_and_acts() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Char('?'));
    assert!(!state.pending_open_link);
    assert_eq!(state.pending_number, 0);
    assert!(state.link_open_request.is_none());
    assert_eq!(state.overlay, Some(Overlay::Help));
}

#[test]
fn a_click_clears_a_partial_link_number() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.clear_pending_chords();
    assert_eq!(state.pending_number, 0);
}

#[test]
fn esc_cancels_the_open_prompt_without_quitting() {
    // Regression: while armed, Esc must cancel the prompt, not fall through
    // to the global q/Esc quit.
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link);
    state.handle_key(KeyCode::Esc);
    assert!(!state.pending_open_link, "cancelled");
    assert!(
        !state.should_quit,
        "esc must not quit while arming a link open"
    );
    assert!(state.link_open_request.is_none());
}

#[test]
fn o_then_a_digit_opens_that_link_not_a_list() {
    // The armed digit selects a link and must pre-empt the global
    // digit→list jump.
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('2'));
    assert!(!state.pending_open_link, "consumed");
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/b")
    );
    assert_eq!(
        state.status_message, None,
        "the main loop reports the outcome once the opener has run"
    );
}

#[test]
fn mouse_click_clears_a_pending_open_link_chord() {
    // `o` arms pending_open_link; a mouse click (e.g. jumping to a
    // different card via the overview) must not leave it armed for a later
    // digit to misfire against whatever the click just landed on.
    use ratatui::layout::Rect;
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link, "armed");

    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::List(0))];
    state.handle_left_click(65, 5);
    assert!(!state.pending_open_link, "cleared by the click");
}

#[test]
fn mouse_scroll_clears_a_pending_open_link_chord() {
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link, "armed");

    state.handle_scroll_down();
    assert!(!state.pending_open_link, "cleared by the scroll");
}

#[test]
fn mouse_click_clears_a_pending_gg_chord() {
    // A lone `g` arms pending_g for the `gg` chord; a mouse click in
    // between must not leave it armed for a later `g` to be misread as the
    // second half of a stale chord.
    use ratatui::layout::Rect;
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('g'));
    assert!(state.pending_g, "armed");

    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::List(1))];
    state.handle_left_click(65, 5);
    assert!(!state.pending_g, "cleared by the click");
}

#[test]
fn o_then_an_out_of_range_digit_reports_no_such_link() {
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('5'));
    assert!(state.link_open_request.is_none());
    assert_eq!(
        state.status_message.as_deref(),
        Some("No link [5] on this card")
    );
}

#[test]
fn o_then_a_digit_for_an_unsafe_scheme_is_refused() {
    let item = item_with_links(1, &["https://example.com/a", "file:///etc/passwd"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('2'));
    assert!(state.link_open_request.is_none());
    assert_eq!(
        state.status_message.as_deref(),
        Some("Not opened: unsupported link type (file:///etc/passwd)")
    );
}

#[test]
fn o_then_a_non_digit_cancels_the_prompt_and_acts_normally() {
    // A non-digit after `o` soft-cancels (like the gg chord) and still does
    // its normal job — here `j` navigates.
    let mut doc = single_item_document(item_with_links(
        1,
        &["https://example.com/a", "https://example.com/b"],
    ));
    doc.lists[0].items.push(checkbox(2, false));
    let mut state = AppState::new(doc);
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link);
    state.handle_key(KeyCode::Char('j'));
    assert!(!state.pending_open_link, "cancelled");
    assert!(state.link_open_request.is_none());
    assert_eq!(state.current_item_index, 1, "j still navigated");
}

#[test]
fn shift_h_and_l_jump_between_incomplete_lists() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.handle_key(KeyCode::Char('L')); // next incomplete list
    assert_eq!(state.current_list_index, 1);
    state.handle_key(KeyCode::Char('H')); // previous incomplete list
    assert_eq!(state.current_list_index, 0);
}

#[test]
fn navigate_next_stops_at_last_item() {
    let mut state = AppState::new(two_list_document());
    state.navigate_next();
    state.navigate_next();
    state.navigate_next();
    state.navigate_next();
    assert_eq!(state.current_item_index, 2);
}

#[test]
fn navigate_prev_stops_at_first_item() {
    let mut state = AppState::new(two_list_document());
    state.navigate_prev();
    state.navigate_prev();
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn navigate_next_stops_on_display_only_items() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![checkbox(1, false), display_only(2), checkbox(3, false)],
    }]);
    let mut state = AppState::new(document);
    state.navigate_next();
    assert_eq!(state.current_item_index, 1);
    assert_eq!(state.current_item().unwrap().kind, ItemKind::DisplayOnly);
}

#[test]
fn jump_to_list_out_of_range_reports_instead_of_moving() {
    // Out of range must not be a silent no-op.
    let mut state = AppState::new(two_list_document());
    state.jump_to_list(5);
    assert_eq!(state.current_list_index, 0, "the cursor must not move");
    assert_eq!(
        state.status_message.as_deref(),
        Some("No list 6: this document has 2 lists")
    );
    assert!(state.status_is_error, "sticky, error-colored");
}

#[test]
fn jump_to_list_out_of_range_message_is_singular_for_one_list() {
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "Only".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
    }]));
    state.jump_to_list(3);
    assert_eq!(
        state.status_message.as_deref(),
        Some("No list 4: this document has 1 list")
    );
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
fn toggle_current_on_display_only_advances_without_editing() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![display_only(1), checkbox(2, false)],
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
fn completing_last_item_in_list_shows_list_complete() {
    let document = document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    // current_item_index must not advance past the list boundary.
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn completing_all_lists_shows_all_complete() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_current();
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn uncompleting_item_returns_to_checklist_screen() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_current();
    assert_eq!(shown(&state), (View::AllComplete, None));
    state.toggle_current();
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
}

#[test]
fn list_with_only_display_only_items_never_completes() {
    let document = document_with_lists(vec![List {
        title: "List 1".to_string(),
        banner: None,
        items: vec![display_only(1), display_only(2)],
    }]);
    let mut state = AppState::new(document);
    state.navigate_next();
    state.toggle_current();
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn list_complete_j_advances_to_next_list() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('j'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn list_complete_k_returns_to_checklist_on_same_list() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 2);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn all_complete_k_returns_to_last_list_last_item() {
    let document = document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, true)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(2, false), checkbox(3, false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.jump_to_list(1);
    state.navigate_next();
    state.toggle_current();
    state.navigate_prev();
    state.toggle_current();
    assert_eq!(shown(&state), (View::AllComplete, None));
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn number_key_jumps_to_list_from_any_screen() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('2'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn list_with_zero_items_does_not_panic() {
    let document = document_with_lists(vec![
        List {
            title: "Empty list".to_string(),
            banner: None,
            items: vec![],
        },
        List {
            title: "Real list".to_string(),
            banner: None,
            items: vec![checkbox(1, false)],
        },
    ]);
    let mut state = AppState::new(document);
    // Startup skips the empty list to the first undone item; force
    // the cursor onto the empty list to exercise the defensive path.
    state.current_list_index = 0;
    state.current_item_index = 0;
    assert!(state.current_item().is_none());

    // None of these should panic when the current list is empty.
    state.navigate_next();
    state.navigate_prev();
    state.toggle_current();
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char(' '));
    assert_eq!(shown(&state), (View::Checklist, None));

    // Jumping to the list that does have items still works.
    state.jump_to_list(1);
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
}

#[test]
fn q_sets_should_quit() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('q'));
    assert!(state.should_quit);
}

// --- First-undone selection ---

#[test]
fn startup_selects_first_undone_item() {
    let document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true), checkbox(3, false)],
    }]);
    let state = AppState::new(document);
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 2);
}

#[test]
fn startup_skips_fully_done_first_list() {
    let document = document_with_lists(vec![
        List {
            title: "Done".to_string(),
            banner: None,
            items: vec![checkbox(1, true)],
        },
        List {
            title: "Todo".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
        },
    ]);
    let state = AppState::new(document);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn startup_all_done_falls_back_to_first_item() {
    let document = document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }]);
    let state = AppState::new(document);
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn jump_to_list_lands_on_first_undone() {
    let document = document_with_lists(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, false)],
        },
        List {
            title: "B".to_string(),
            banner: None,
            items: vec![checkbox(2, true), checkbox(3, false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('2'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1, "skips the done item");
}

#[test]
fn complete_list_advance_skips_to_next_incomplete() {
    let document = document_with_lists(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, false)],
        },
        List {
            title: "B (all done)".to_string(),
            banner: None,
            items: vec![checkbox(2, true)],
        },
        List {
            title: "C".to_string(),
            banner: None,
            items: vec![checkbox(3, false)],
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
fn complete_last_incomplete_list_advance_goes_all_complete() {
    let document = document_with_lists(vec![
        List {
            title: "A".to_string(),
            banner: None,
            items: vec![checkbox(1, true)],
        },
        List {
            title: "B".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
        },
    ]);
    let mut state = AppState::new(document); // starts on B's undone item
    state.toggle_current(); // completes everything -> AllComplete
    assert_eq!(shown(&state), (View::AllComplete, None));
}

// --- Card-focused navigation and card scrolling ---

#[test]
fn h_l_and_horizontal_arrows_navigate() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_item_index, 1);
    state.handle_key(KeyCode::Right);
    assert_eq!(state.current_item_index, 2);
    state.handle_key(KeyCode::Char('h'));
    assert_eq!(state.current_item_index, 1);
    state.handle_key(KeyCode::Left);
    assert_eq!(state.current_item_index, 0);
}

#[test]
fn j_k_and_vertical_arrows_navigate() {
    // j/k and ↑/↓ now walk the task list like h/l, they no longer
    // scroll the card body.
    let mut state = AppState::new(two_list_document());
    state.card_max_scroll = 5; // overflow present, but j/k must still nav
    state.handle_key(KeyCode::Char('j'));
    assert_eq!(state.current_item_index, 1);
    state.handle_key(KeyCode::Down);
    assert_eq!(state.current_item_index, 2);
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.current_item_index, 1);
    state.handle_key(KeyCode::Up);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(state.card_scroll, 0, "navigation keys must not scroll");
}

#[test]
fn ctrl_and_page_keys_scroll_the_card_and_clamp() {
    // Card-body scrolling lives on Ctrl-E/Y (line), Ctrl-D/U (half
    // page) and PageDown/Up (page); none of them navigate.
    let mut state = AppState::new(two_list_document());
    state.card_max_scroll = 5;
    state.card_viewport_height = 4; // half_page = 2, page = 3
    let ctrl = |c| (KeyCode::Char(c), KeyModifiers::CONTROL);

    let (code, m) = ctrl('e'); // one line down
    state.handle_key_with_mods(code, m);
    assert_eq!(state.card_scroll, 1);
    let (code, m) = ctrl('d'); // half page down (2)
    state.handle_key_with_mods(code, m);
    assert_eq!(state.card_scroll, 3);
    state.handle_key(KeyCode::PageDown); // page down (3) → clamps at 5
    assert_eq!(state.card_scroll, 5, "clamps at card_max_scroll");
    let (code, m) = ctrl('e');
    state.handle_key_with_mods(code, m);
    assert_eq!(state.card_scroll, 5, "stays clamped at bottom");
    assert_eq!(state.current_item_index, 0, "scrolling must not navigate");

    let (code, m) = ctrl('y'); // one line up
    state.handle_key_with_mods(code, m);
    assert_eq!(state.card_scroll, 4);
    let (code, m) = ctrl('u'); // half page up (2)
    state.handle_key_with_mods(code, m);
    assert_eq!(state.card_scroll, 2);
    state.handle_key(KeyCode::PageUp); // page up (3) → clamps at 0
    assert_eq!(state.card_scroll, 0, "clamps at zero");
}

#[test]
fn navigation_resets_card_scroll() {
    let mut state = AppState::new(two_list_document());
    state.card_max_scroll = 5;
    state.card_scroll = 3;
    state.navigate_next();
    assert_eq!(state.card_scroll, 0);
    assert_eq!(state.card_max_scroll, 0);
}

#[test]
fn wheel_scrolls_overflowing_card_and_navigates_otherwise() {
    let mut state = AppState::new(two_list_document());
    // No overflow: wheel navigates.
    state.handle_scroll_down();
    assert_eq!(state.current_item_index, 1);
    // Overflow: wheel scrolls, does not navigate.
    state.card_max_scroll = 3;
    state.handle_scroll_down();
    assert_eq!(state.card_scroll, 1);
    assert_eq!(state.current_item_index, 1);
    state.handle_scroll_up();
    assert_eq!(state.card_scroll, 0);
    assert_eq!(state.current_item_index, 1);
}

#[test]
fn wheel_up_navigates_to_previous_item_when_card_fits() {
    // Mirror image of the scroll-down case: with no card overflow, a
    // wheel-up navigates to the previous item rather than scrolling.
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 1;
    state.card_max_scroll = 0;
    state.handle_scroll_up();
    assert_eq!(state.current_item_index, 0);
    assert_eq!(state.card_scroll, 0);
}

#[test]
fn wheel_crosses_list_edges_like_j_and_k() {
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 2;
    state.handle_scroll_down();
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    state.handle_scroll_up();
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (0, 0),
        "lands on the previous list's first unfinished task, like k"
    );
}

#[test]
fn wheel_scrolls_the_help_overlay() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.help.max_scroll = 2;
    state.handle_scroll_down();
    state.handle_scroll_down();
    state.handle_scroll_down();
    assert_eq!(state.help.scroll, 2, "clamps at help.max_scroll");
    state.handle_scroll_up();
    assert_eq!(state.help.scroll, 1);
    assert_eq!(
        state.overlay,
        Some(Overlay::Help),
        "the wheel doesn't close help"
    );
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
}

#[test]
fn wheel_moves_the_picker_highlight() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    state.handle_scroll_down();
    state.handle_scroll_down();
    assert_eq!(state.picker.selection, 2);
    state.handle_scroll_up();
    assert_eq!(state.picker.selection, 1);
    assert_eq!(state.overlay, Some(Overlay::ListPicker));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
}

#[test]
fn wheel_is_ignored_on_the_confirmation_card() {
    let mut state = AppState::new(two_list_document());
    state.overlay = Some(Overlay::ConfirmReset);
    state.handle_scroll_down();
    assert_eq!(state.overlay, Some(Overlay::ConfirmReset));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
}

#[test]
fn list_complete_l_advances_h_reviews() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('h'));
    assert_eq!(shown(&state), (View::Checklist, None));
    state.toggle_current(); // untoggle
    state.toggle_current(); // re-toggle -> ListComplete again
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(shown(&state), (View::Checklist, None));
}

// --- Editor request ---

#[test]
fn e_requests_editor_and_is_consumed_once() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('e'));
    assert!(state.take_editor_request());
    assert!(!state.take_editor_request(), "request is consumed");
}

#[test]
fn e_requests_editor_from_completion_screen() {
    let mut state = AppState::new(two_list_document());
    state.view = View::AllComplete;
    state.handle_key(KeyCode::Char('e'));
    assert!(state.take_editor_request());
}

// --- Suspend (Ctrl-Z) ---

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
    assert!(!state.pending_g);
}

// --- Ctrl-C ---

fn press_ctrl_c(state: &mut AppState) {
    state.handle_key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL);
}

fn assert_quit_hint(state: &AppState) {
    assert_eq!(state.status_message.as_deref(), Some("Type q to quit"));
    assert!(state.status_is_error);
    assert!(!state.should_quit);
}

#[test]
fn ctrl_c_on_checklist_shows_quit_hint() {
    let mut state = AppState::new(two_list_document());
    press_ctrl_c(&mut state);
    assert_quit_hint(&state);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn ctrl_c_on_list_complete_shows_quit_hint() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    press_ctrl_c(&mut state);
    assert_quit_hint(&state);
    assert_eq!(shown(&state), (View::ListComplete, None));
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
fn ctrl_c_cancels_search_input_like_esc() {
    let mut state = AppState::new(two_list_document());
    let origin = (state.current_list_index, state.current_item_index);
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task");
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::Checklist, None));
    assert_eq!(state.search.query, "task", "no 'c' typed into the query");
    assert_eq!((state.current_list_index, state.current_item_index), origin);
}

#[test]
fn ctrl_c_closes_the_task_picker() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    assert_eq!(state.overlay, Some(Overlay::ListPicker));
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn ctrl_c_closes_help() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    state.handle_key(KeyCode::Char('?'));
    assert_eq!(state.overlay, Some(Overlay::Help));
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::ListComplete, None));
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
    assert!(!state.should_quit);
}

#[test]
fn ctrl_c_cancels_a_pending_link_prompt() {
    let mut state = AppState::new(two_list_document());
    state.pending_open_link = true;
    press_ctrl_c(&mut state);
    assert!(!state.pending_open_link);
    assert_quit_hint(&state);
}

#[test]
fn finish_suspend_failure_is_a_sticky_error() {
    let mut state = AppState::new(two_list_document());
    state.finish_suspend(Err("kill exited with exit status: 1".to_string()));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Couldn't suspend: kill exited with exit status: 1")
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
fn finish_suspend_reloads_an_edit_made_while_suspended() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_suspend(Ok(()));
    assert_eq!(state.current_list().items.len(), 2);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: file changed on disk")
    );
    fs::remove_file(&path).ok();
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn finish_editor_with_no_change_is_silent_and_queues_no_sync() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.finish_editor("vi", None, Ok(exit_status(0)), EDITOR_WAITED);
    assert_eq!(state.status_message, None);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[cfg(unix)]
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

#[cfg(unix)]
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
        Some("vi exited with exit status: 1; Reloaded: file changed on disk — not synced")
    );
    assert!(state.status_is_error);
    assert_eq!(state.status_expiry, None);
    assert!(state.take_git_sync_request().is_none());
    fs::remove_file(&path).ok();
}

#[cfg(unix)]
#[test]
fn finish_editor_without_git_sync_does_not_mention_syncing() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.finish_editor("vi", None, Ok(exit_status(1 << 8)), Duration::ZERO);
    assert_eq!(
        state.status_message.as_deref(),
        Some("vi exited with exit status: 1; Reloaded: file changed on disk")
    );
    fs::remove_file(&path).ok();
}

#[cfg(unix)]
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
        Some("Bad $EDITOR/$VISUAL — using vi; Reloaded: file changed on disk")
    );
    assert!(state.status_is_error);
    assert!(state.take_git_sync_request().is_some());
    fs::remove_file(&path).ok();
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

// --- Reset with confirmation ---

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
    assert!(!state.should_quit, "Esc cancels, does not quit");
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

fn all_done_document() -> Document {
    document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }])
}

#[test]
fn quit_when_all_done_opens_confirm_quit_reset() {
    // q on an all-done file offers to reset instead of quitting.
    let mut state = AppState::new(all_done_document());
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    assert!(!state.should_quit, "prompt shown, not quit yet");
}

#[test]
fn confirm_quit_reset_yes_resets_and_quits() {
    let mut state = AppState::new(all_done_document());
    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('y'));
    assert!(state.should_quit, "y quits");
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

    assert!(!state.should_quit, "a refused reset must not quit");
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

    assert!(!state.should_quit, "a failed reset must not quit");
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
    assert!(state.should_quit, "n quits");
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
    assert!(!state.should_quit, "Esc cancels, stays running");
    assert_eq!(shown(&state), (View::Checklist, None));
}

#[test]
fn auto_copy_stays_silent_without_a_single_candidate() {
    // Auto-copy announces only a successful copy. With no code
    // candidate, or an ambiguous set, it stays silent (the success paths
    // touch a real clipboard backend, so they're manual-only).
    let mut state = AppState::new(two_list_document());
    state.auto_copy = true;
    state.maybe_auto_copy();
    assert_eq!(state.status_message, None, "no code candidate -> silent");

    // Ambiguous: put two candidates on the current item.
    let (si, ii) = (state.current_list_index, state.current_item_index);
    state.document.lists[si].items[ii].code_blocks = vec!["one".to_string(), "two".to_string()];
    state.maybe_auto_copy();
    assert_eq!(state.status_message, None, "ambiguous set -> silent");
}

#[test]
fn auto_copy_fires_on_every_navigation_motion() {
    // Every motion that lands on a card honours --auto-copy, not just
    // h/l: each card carries one command, and the fake clipboard records
    // which one was copied.
    type Position = (usize, usize);
    let cases: &[(&str, View, Position, &[KeyCode], Position)] = &[
        ("l", View::Checklist, (0, 1), &[KeyCode::Char('l')], (0, 2)),
        ("h", View::Checklist, (0, 2), &[KeyCode::Char('h')], (0, 1)),
        ("j", View::Checklist, (0, 1), &[KeyCode::Char('j')], (0, 2)),
        ("k", View::Checklist, (0, 2), &[KeyCode::Char('k')], (0, 1)),
        (
            "l across lists",
            View::Checklist,
            (0, 2),
            &[KeyCode::Char('l')],
            (1, 0),
        ),
        ("Tab", View::Checklist, (0, 1), &[KeyCode::Tab], (0, 2)),
        (
            "Tab wraps",
            View::Checklist,
            (1, 1),
            &[KeyCode::Tab],
            (0, 1),
        ),
        (
            "gg",
            View::Checklist,
            (1, 1),
            &[KeyCode::Char('g'), KeyCode::Char('g')],
            (0, 0),
        ),
        ("G", View::Checklist, (0, 1), &[KeyCode::Char('G')], (1, 1)),
        (
            "digit",
            View::Checklist,
            (0, 1),
            &[KeyCode::Char('2')],
            (1, 0),
        ),
        ("L", View::Checklist, (0, 1), &[KeyCode::Char('L')], (1, 0)),
        ("H", View::Checklist, (1, 1), &[KeyCode::Char('H')], (0, 1)),
        ("n", View::Checklist, (0, 1), &[KeyCode::Char('n')], (1, 0)),
        (
            "T + Enter",
            View::Checklist,
            (1, 1),
            &[KeyCode::Char('T'), KeyCode::Enter],
            (0, 0),
        ),
        (
            "all-complete h",
            View::AllComplete,
            (0, 0),
            &[KeyCode::Char('h')],
            (1, 1),
        ),
        (
            "all-complete k",
            View::AllComplete,
            (0, 0),
            &[KeyCode::Char('k')],
            (1, 1),
        ),
    ];
    for &(name, screen, (from_list, from_item), keys, (to_list, to_item)) in cases {
        let mut document = two_list_document();
        document.lists[0].items[0].kind = ItemKind::Checkbox(TaskState::Done);
        for list in &mut document.lists {
            for item in &mut list.items {
                item.code_spans = vec![format!("cmd-{}", item.line_number)];
            }
        }
        let expected = document.lists[to_list].items[to_item].code_spans[0].clone();
        let mut state = AppState::new(document);
        state.auto_copy = true;
        state.search.last = Some("task 4".to_string());
        state.current_list_index = from_list;
        state.current_item_index = from_item;
        state.view = screen;
        crate::clipboard::fake::with(|c| c.system_attempts.clear());

        for &key in keys {
            state.handle_key(key);
        }

        assert_eq!(
            (state.current_list_index, state.current_item_index),
            (to_list, to_item),
            "{name}: lands on the target card"
        );
        crate::clipboard::fake::with(|c| {
            assert_eq!(
                c.system_attempts,
                vec![(expected.clone(), false)],
                "{name}: copies the landing card's command"
            );
        });
        let status = state.status_message.as_deref().unwrap_or_default();
        assert!(
            status.ends_with("Sent to clipboard (OSC 52)"),
            "{name}: confirms the copy, got {status:?}"
        );
    }
}

#[test]
fn a_wrap_message_keeps_the_auto_copy_confirmation_beside_it() {
    let mut document = two_list_document();
    for list in &mut document.lists {
        for item in &mut list.items {
            item.code_spans = vec![format!("cmd-{}", item.line_number)];
        }
    }
    let mut state = AppState::new(document);
    state.auto_copy = true;
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Tab);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Wrapped to the first unfinished task (List 1) · Sent to clipboard (OSC 52)")
    );
}

#[test]
fn a_wrap_message_gives_way_to_a_sticky_auto_copy_error() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["rm\u{202e}x".to_string()];
    let mut state = AppState::new(document);
    state.auto_copy = true;
    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Tab);
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some(HIDDEN_CHARACTERS_MESSAGE)
    );
    assert!(state.status_is_error);
}

#[test]
fn tab_says_when_it_wraps_to_the_top_and_stays_quiet_otherwise() {
    let mut state = AppState::new(two_list_document());
    state.focus_item(0, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Tab);
    assert_eq!((state.current_list_index, state.current_item_index), (0, 2));
    assert_eq!(state.status_message, None, "a forward step needs no notice");

    state.focus_item(1, 1, CursorMove::Quiet);
    state.handle_key(KeyCode::Tab);
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Wrapped to the first unfinished task (List 1)")
    );
    assert!(!state.status_is_error, "a wrap is a notice, not an error");
    assert!(
        state.status_expiry.is_some(),
        "the wrap notice is ephemeral"
    );
}

#[test]
fn tab_on_the_only_unfinished_task_reports_the_wrap_back_to_it() {
    let mut document = two_list_document();
    for list in &mut document.lists {
        for item in &mut list.items {
            item.kind = ItemKind::Checkbox(TaskState::Done);
        }
    }
    document.lists[1].items[0].kind = ItemKind::Checkbox(TaskState::Started);
    let mut state = AppState::new(document);
    state.focus_item(1, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Tab);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Wrapped to the first unfinished task (List 2)")
    );
}

#[test]
fn n_and_n_say_when_the_search_wraps_around() {
    let mut state = AppState::new(two_list_document());
    state.search.last = Some("task".to_string());
    state.focus_item(1, 0, CursorMove::Quiet);
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(state.status_message.as_deref(), Some("Match 5/5"));
    state.handle_key(KeyCode::Char('n'));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Match 1/5 · wrapped to top")
    );
    state.handle_key(KeyCode::Char('N'));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Match 5/5 · wrapped to bottom")
    );
    state.handle_key(KeyCode::Char('N'));
    assert_eq!(state.status_message.as_deref(), Some("Match 4/5"));
}

#[test]
fn quiet_cursor_moves_do_not_auto_copy() {
    // Cancelling a search restores the cursor without copying.
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["cmd".to_string()];
    let mut state = AppState::new(document);
    state.auto_copy = true;
    state.handle_key(KeyCode::Char('/'));
    state.handle_key(KeyCode::Char('5'));
    state.handle_key(KeyCode::Esc);
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(shown(&state), (View::Checklist, None));
    crate::clipboard::fake::with(|c| assert!(c.system_attempts.is_empty()));
}

// Copy alert paths (0 or many candidates) never touch a clipboard
// backend, so they are safe to test headless. The Copied* paths are
// manual verification.

#[test]
fn copy_with_no_code_shows_alert() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to copy: item has no code")
    );
}

#[test]
fn y_with_multiple_candidates_arms_a_digit_prompt() {
    // `y` on a multi-command card copies nothing yet: it arms a digit
    // (matching the card's [N] markers) and sets no status message, so the
    // status bar shows the prompt.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["first".to_string(), "second".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    assert!(state.pending_copy_code, "armed for a command number");
    assert_eq!(state.status_message, None);
    crate::clipboard::fake::with(|c| assert!(c.system_attempts.is_empty()));
}

#[test]
fn y_then_two_digits_copies_command_ten() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = (1..=10).map(|n| format!("cmd-{n}")).collect();
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char('1'));
    assert!(state.pending_copy_code, "1 could still become 10");
    state.handle_key(KeyCode::Char('0'));
    assert!(!state.pending_copy_code, "consumed");
    assert_eq!(state.current_list_index, 0, "no list jump");
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("cmd-10".to_string(), false)]);
    });
}

#[test]
fn y_then_a_digit_copies_that_fenced_block_not_a_list() {
    // The armed digit selects a command and must pre-empt the global
    // digit→list jump.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["first".to_string(), "second".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char('2'));
    assert!(!state.pending_copy_code, "consumed");
    assert_eq!(state.current_list_index, 0, "no list jump");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to clipboard (OSC 52)")
    );
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("second".to_string(), false)]);
    });
}

#[test]
fn y_then_a_digit_copies_that_inline_command() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["stop-svc".to_string(), "start-svc".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char('1'));
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("stop-svc".to_string(), false)]);
    });
}

#[test]
fn y_then_an_out_of_range_digit_reports_no_such_command() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char('3'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("No command [3] on this card")
    );
    assert!(state.status_is_error);
    crate::clipboard::fake::with(|c| assert!(c.system_attempts.is_empty()));
}

#[test]
fn esc_cancels_the_copy_prompt_without_quitting() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Esc);
    assert!(!state.pending_copy_code, "cancelled");
    assert!(!state.should_quit, "esc must not quit while arming a copy");
    crate::clipboard::fake::with(|c| assert!(c.system_attempts.is_empty()));
}

#[test]
fn y_then_a_non_digit_cancels_the_prompt_and_acts_normally() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    let before = state.current_item_index;
    state.handle_key(KeyCode::Char('y'));
    state.handle_key(KeyCode::Char('j'));
    assert!(!state.pending_copy_code, "cancelled");
    assert_ne!(state.current_item_index, before, "j still navigated");
}

#[test]
fn mouse_input_clears_a_pending_copy_prompt() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    assert!(state.pending_copy_code, "armed");
    state.handle_scroll_down();
    assert!(!state.pending_copy_code, "cleared by the scroll");
}

#[test]
fn click_inside_card_copies_current_command() {
    use ratatui::layout::Rect;
    // A click inside the card behaves like `y`. No-code item →
    // the same "nothing to copy" hint (success paths are manual-only).
    let mut state = AppState::new(two_list_document());
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(15, 8);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to copy: item has no code")
    );

    // Ambiguous item inside the card → `y`'s digit prompt.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(12, 6);
    assert!(state.pending_copy_code, "armed like `y`");
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
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.code_regions = vec![(Rect::new(11, 7, 38, 1), "b".to_string())];

    // On the region: the specific path, never the ambiguous hint.
    state.handle_left_click(15, 7);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to clipboard (OSC 52)")
    );
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("b".to_string(), false)]);
    });

    // Elsewhere in the card (no region): `y`'s digit prompt.
    state.handle_left_click(15, 6);
    assert!(state.pending_copy_code);
}

#[test]
fn click_outside_card_or_off_checklist_is_ignored() {
    use ratatui::layout::Rect;
    let mut state = AppState::new(two_list_document());
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
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
    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 1))];

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
    state.overview_rows = vec![(Rect::new(60, 3, 30, 1), OverviewTarget::List(1))];

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
    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 0))];

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
    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 1))];

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
    state.overview_rows = vec![(Rect::new(60, 5, 6, 1), OverviewTarget::Toggle(1, 0))];

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
        items: vec![checkbox(1, true), checkbox(2, true)],
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
fn no_code_copy_hint_is_sticky() {
    // A `y` on a no-code item is feedback to a deliberate keypress,
    // so it sticks (no expiry) rather than fading after 4s.
    let mut state = AppState::new(two_list_document());
    state.card_rect = Some(ratatui::layout::Rect::new(10, 5, 40, 10));
    state.handle_left_click(15, 8); // no-code item -> "Nothing to copy"
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to copy: item has no code")
    );
    assert_eq!(state.status_expiry, None, "the no-code hint is sticky");
    assert!(
        state.status_is_error,
        "a 'nothing to copy' hint is an error"
    );
}

#[test]
fn set_error_is_sticky_and_flagged() {
    // set_error carries no expiry (sticky) and marks the message an
    // error so the status bar renders it red.
    let mut state = AppState::new(two_list_document());
    state.set_error("Something failed".to_string());
    assert_eq!(state.status_message.as_deref(), Some("Something failed"));
    assert_eq!(state.status_expiry, None, "error messages are sticky");
    assert!(state.status_is_error);
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
fn toggle_current_reports_error_and_does_not_advance_when_write_fails() {
    // A write_back failure must surface as an error, not a silent
    // "Marked done" success — and must not advance the cursor/screen either,
    // since the toggle wasn't actually saved.
    let document = document_with_missing_file(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, false), checkbox(2, false)],
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
        items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
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

#[cfg(unix)]
#[test]
fn toggle_is_refused_on_a_write_protected_checklist() {
    use std::os::unix::fs::PermissionsExt;
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
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
    assert!(state.git_sync.pending.is_none());
}

#[test]
fn toggle_succeeds_normally_when_disk_content_is_unchanged() {
    // Sanity check alongside the conflict test above: when nothing external
    // touched the file, the new hash check must not itself become a false
    // positive that blocks every ordinary write.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, false)],
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
        items: vec![checkbox(1, false)],
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
        items: vec![checkbox(1, false), checkbox(2, false)],
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
        items: vec![checkbox(1, false)],
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

fn write_real_file(contents: &str) -> PathBuf {
    let path = unique_temp_path();
    fs::write(&path, contents).unwrap();
    path
}

/// Forces a distinct mtime by nudging it a full second into the future,
/// avoiding flakiness on filesystems with coarse mtime resolution.
fn touch_with_new_mtime(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let new_mtime = metadata.modified().unwrap() + std::time::Duration::from_secs(1);
    let file = fs::File::open(path).unwrap();
    file.set_modified(new_mtime).unwrap();
}

#[test]
fn reload_picks_up_external_change() {
    let path = write_real_file("## List 1\n\n- [ ] `task one`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    touch_with_new_mtime(&path, "## List 1\n\n- [ ] `task one`\n- [ ] `task two`\n");
    state.reload_if_changed();

    assert_eq!(state.current_list().items.len(), 2);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: file changed on disk")
    );

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
    assert!(!state.should_quit);
    assert_eq!(
        state.status_message.as_deref(),
        Some("File changed on disk — quit cancelled")
    );

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
    assert_eq!(
        state.status_message.as_deref(),
        Some("Reloaded: file changed on disk")
    );
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
        Some("Reloaded: file changed on disk")
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
        Some("Reloaded: file changed on disk — task not found, cursor moved")
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
        Some("Reloaded: file changed on disk — task not found, cursor moved")
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
        Some("Reloaded: file changed on disk")
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
            .is_some_and(|m| m.contains("File changed on disk")),
        "the refusal must be reported: {:?}",
        state.status_message
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reload_detects_deleted_file_and_blocks_writes() {
    let path = write_real_file("## S\n\n- [ ] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.reload_if_changed();

    assert!(state.file_deleted, "flag must be set after deletion");
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
    assert!(state.file_deleted);

    // Restore the file.
    touch_with_new_mtime(&path, "## S\n\n- [ ] `task`\n");
    state.reload_if_changed();

    assert!(!state.file_deleted, "flag must clear when file is restored");
    assert_eq!(
        state.status_message.as_deref(),
        Some("File restored — reloaded"),
    );

    fs::remove_file(&path).ok();
}

#[test]
fn reset_is_blocked_when_file_deleted() {
    let path = write_real_file("## S\n\n- [x] `task`\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    fs::remove_file(&path).unwrap();
    state.file_deleted = true;
    // Reach reset_all via the ConfirmReset screen's 'y' path.
    state.overlay = Some(Overlay::ConfirmReset);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — changes cannot be saved until it exists again"),
    );
}

// --- Keypress to file: every writing action lands on the right line ---

const WRITE_PATH_MARKDOWN: &str = "\
# Title

## First

Intro paragraph with a stray [ ] that is not a task.

- [ ] **Alpha** first task
  body line with [x] in it
- [X] Bravo
- plain note
- [/] Charlie

## Second

- [ ] Delta
";

/// `WRITE_PATH_MARKDOWN` with each `(from, to)` line swapped, asserting every
/// `from` occurs exactly once so a typo can't make the expectation vacuous.
fn markdown_with(edits: &[(&str, &str)]) -> String {
    let mut text = WRITE_PATH_MARKDOWN.to_string();
    for (from, to) in edits {
        assert_eq!(text.matches(from).count(), 1, "{from:?} must be unique");
        text = text.replace(from, to);
    }
    text
}

fn focus_text(state: &mut AppState, text: &str) {
    let (list, item) = state
        .document
        .lists
        .iter()
        .enumerate()
        .find_map(|(l, list)| {
            list.items
                .iter()
                .position(|item| item.display_text.contains(text))
                .map(|i| (l, i))
        })
        .unwrap();
    state.current_list_index = list;
    state.current_item_index = item;
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
fn an_overview_marker_click_writes_that_task_only() {
    use ratatui::layout::Rect;
    let (mut state, path) = state_from_markdown(WRITE_PATH_MARKDOWN);
    state.overview_rows = vec![(Rect::new(60, 5, 6, 1), OverviewTarget::Toggle(1, 0))];
    state.handle_left_click(61, 5);
    assert_eq!(
        file_text(&path),
        markdown_with(&[("- [ ] Delta", "- [x] Delta")])
    );
}

#[test]
fn quit_reset_confirmed_with_y_writes_every_task_open_and_quits() {
    let all_done = "## L\n\n- [x] one\n- [X] two\n";
    let (mut state, path) = state_from_markdown(all_done);
    state.handle_key(KeyCode::Char('q'));
    assert_eq!(state.overlay, Some(Overlay::ConfirmQuitReset));
    state.handle_key(KeyCode::Char('y'));
    assert!(state.should_quit);
    assert_eq!(file_text(&path), "## L\n\n- [ ] one\n- [ ] two\n");
}

#[test]
fn quit_without_reset_leaves_the_file_untouched() {
    let all_done = "## L\n\n- [x] one\n- [X] two\n";
    let (mut state, path) = state_from_markdown(all_done);
    state.handle_key(KeyCode::Char('q'));
    state.handle_key(KeyCode::Char('n'));
    assert!(state.should_quit);
    assert_eq!(file_text(&path), all_done);
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

// --- Undo / redo of state-changing actions ---

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
    state.file_deleted = true;
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
fn help_ctrl_scroll_keys_and_page_up_scroll() {
    // Mirrors help_scroll_keys_scroll_instead_of_closing, but for the
    // Ctrl-E/Ctrl-Y/Ctrl-U/PageUp arms that test doesn't reach.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.help.max_scroll = 5;
    state.help.viewport_height = 6;
    state.handle_key(KeyCode::PageDown);
    assert_eq!(state.help.scroll, 5, "clamps at max_scroll");
    state.handle_key_with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(state.help.scroll, 2, "Ctrl-U jumps half a page (3) up");
    state.handle_key(KeyCode::PageUp);
    assert_eq!(state.help.scroll, 0, "clamps at 0");
    state.handle_key_with_mods(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(state.help.scroll, 1, "Ctrl-E scrolls down one line");
    state.handle_key_with_mods(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(state.help.scroll, 0, "Ctrl-Y scrolls up one line");
    assert_eq!(
        state.overlay,
        Some(Overlay::Help),
        "none of these close the overlay"
    );
}

#[test]
fn picker_backspace_edits_the_query() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    type_str(&mut state, "task");
    assert_eq!(state.picker.query, "task");
    state.handle_key(KeyCode::Backspace);
    assert_eq!(state.picker.query, "tas");
}

#[test]
fn picker_move_is_a_no_op_with_no_matches() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    type_str(&mut state, "nonexistent task text");
    assert_eq!(state.picker_matches().len(), 0);
    state.handle_key(KeyCode::Down); // picker_move: must not panic or move
    assert_eq!(state.picker.selection, 0);
}

#[test]
fn picker_enter_with_no_matches_closes_without_jumping() {
    let mut state = AppState::new(two_list_document());
    let (before_list, before_item) = (state.current_list_index, state.current_item_index);
    state.handle_key(KeyCode::Char('T'));
    type_str(&mut state, "nonexistent task text");
    state.handle_key(KeyCode::Enter);
    assert_eq!(shown(&state), (View::Checklist, None), "closes on Enter");
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (before_list, before_item),
        "cursor untouched when nothing matched"
    );
}

#[test]
fn toggle_item_is_blocked_when_file_deleted() {
    let mut state = AppState::new(two_list_document());
    state.file_deleted = true;
    state.toggle_item(0, 0);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
        "blocked toggle must not change state"
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
        items: vec![checkbox(1, false)],
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
            items: vec![checkbox(1, true)],
        },
        List {
            title: "Other list".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
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
        items: vec![display_only(1), checkbox(2, false)],
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
        items: vec![checkbox(1, false), display_only(2)],
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
fn advancing_past_the_last_list_wraps_back_to_an_earlier_incomplete_list() {
    // Completing the last list wraps back to an earlier incomplete one.
    let document = document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.current_list_index = 1;
    state.current_item_index = 0;
    state.toggle_current(); // completes list 2 -> ListComplete
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(
        shown(&state),
        (View::Checklist, None),
        "must not claim completion while a list is untouched"
    );
    assert_eq!(state.current_list_index, 0, "wrapped back to List 1");
}

#[test]
fn advancing_shows_all_complete_only_when_every_list_is_done() {
    // The other half of the rule above: once nothing is left anywhere,
    // AllComplete is still exactly what should appear.
    let document = document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, true)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(2, false)],
        },
    ]);
    let mut state = AppState::new(document);
    state.current_list_index = 1;
    state.current_item_index = 0;
    state.toggle_current(); // completes the only remaining work
    assert_eq!(shown(&state), (View::AllComplete, None));

    // And reaching it via the ListComplete `l` path agrees.
    state.view = View::ListComplete;
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn undo_redo_skip_display_only_items_in_the_snapshot() {
    // state_snapshot/apply_snapshot must ignore DisplayOnly items (nothing
    // to undo/redo on a note card) even when one sits alongside checkboxes.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![display_only(1), checkbox(2, false)],
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
        items: vec![checkbox(1, false)],
    }]);
    let mut state = AppState::new(document);
    state.toggle_current();
    state.undo();
    state.file_deleted = true;
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
        items: vec![checkbox(1, false)],
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
fn is_safe_link_accepts_the_allowlisted_schemes_case_insensitively() {
    for url in [
        "http://example.com",
        "https://example.com",
        "mailto:user@example.com",
        "HTTP://example.com",
        "HTTPS://EXAMPLE.COM",
        "MAILTO:user@example.com",
        "HtTpS://example.com",
    ] {
        assert!(is_safe_link(url), "should accept: {url:?}");
    }
}

#[test]
fn is_safe_link_rejects_non_allowlisted_or_malformed_urls() {
    for url in [
        "ftp://example.com",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "-http://example.com",
        "",
        " http://example.com",
        "http:/example.com",
    ] {
        assert!(!is_safe_link(url), "should reject: {url:?}");
    }
}

#[test]
fn is_safe_link_only_checks_the_scheme_prefix_not_the_rest_of_the_url() {
    // Accepted because `open_link` never uses a shell; see `is_safe_link`.
    assert!(is_safe_link("http://example.com\n--some-option"));
    assert!(is_safe_link("http://example.com\rSet-Cookie: x"));
}

#[test]
fn an_external_reload_drops_a_queued_git_sync_request() {
    // A reload drops the queued sync request for the replaced content.
    let path = unique_temp_path();
    fs::write(&path, "## L\n\n- [ ] alpha\n- [ ] beta\n").unwrap();
    let document = crate::parser::parse_document(path.clone()).unwrap();
    let mut state = AppState::new(document);

    state.toggle_current();
    assert!(
        state.git_sync.pending.is_some(),
        "test setup: the toggle must have queued a sync request"
    );

    // Somebody else rewrites the file, and the watcher picks it up.
    touch_with_new_mtime(&path, "## L\n\n- [ ] alpha\n- [x] beta\n");
    assert!(
        state.reload_if_changed(),
        "the external edit must be adopted"
    );

    assert!(
        state.git_sync.pending.is_none(),
        "a request describing content that is no longer on disk must not survive the reload"
    );

    fs::remove_file(&path).ok();
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
        state.pending_open_link,
        "test setup: the digit prompt must be armed"
    );
    state.pending_copy_code = true;

    // The file changes underneath and the reload is adopted.
    touch_with_new_mtime(
        &path,
        "## L\n\n- [ ] plain first task\n- [ ] see [c](https://example.com/c)\n",
    );
    assert!(state.reload_if_changed(), "the reload must be adopted");

    assert!(
        !state.pending_open_link,
        "a reload must disarm the chord rather than leave it pointed at new content"
    );
    assert!(!state.pending_g, "and the same for a pending gg");
    assert!(!state.pending_copy_code, "and for y's digit prompt");

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
fn every_copy_path_refuses_a_command_with_hidden_characters() {
    use ratatui::layout::Rect;
    // The card drops control characters when drawn, so copying them would
    // put something on the clipboard the user never saw. `y`, a click on a
    // code row and `--auto-copy` all refuse, with a sticky error.
    let expected = Some("Not copied: command contains hidden control characters");
    let hidden = "ls\x1b[201~\nrm -rf ~".to_string();

    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec![hidden.clone()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(state.status_message.as_deref(), expected);
    assert!(state.status_is_error);

    let mut state = AppState::new(two_list_document());
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.code_regions = vec![(Rect::new(11, 7, 38, 1), hidden.clone())];
    state.handle_left_click(15, 7);
    assert_eq!(state.status_message.as_deref(), expected);

    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["echo ok\rrm -rf ~".to_string()];
    let mut state = AppState::new(document);
    state.auto_copy = true;
    state.maybe_auto_copy();
    assert_eq!(state.status_message.as_deref(), expected);
}

// --- Unbound Ctrl/Alt chords ---

const CHORD_TEST_FILE: &str =
    "## List 1\n\n- [ ] task one\n- [x] task two\n\n## List 2\n\n- [ ] task three\n";

#[test]
fn ctrl_s_does_not_start_the_task_or_write_the_file() {
    let path = write_real_file(CHORD_TEST_FILE);
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.handle_key_with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(fs::read_to_string(&path).unwrap(), CHORD_TEST_FILE);
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Ctrl-S is not bound (? lists every key)")
    );
    assert!(state.status_is_error);
    fs::remove_file(&path).ok();
}

#[test]
fn ctrl_q_and_alt_digit_do_not_quit_or_jump() {
    let mut state = AppState::new(two_list_document());
    state.handle_key_with_mods(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(!state.should_quit);
    state.handle_key_with_mods(KeyCode::Char('2'), KeyModifiers::ALT);
    assert_eq!(state.current_list_index, 0);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Alt-2 is not bound (? lists every key)")
    );
    state.handle_key_with_mods(KeyCode::Char('L'), KeyModifiers::SHIFT);
    assert_eq!(state.current_list_index, 1, "Shift still passes through");
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
fn ctrl_e_on_a_completion_screen_does_not_open_the_editor() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    state.handle_key_with_mods(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert!(!state.editor_requested);
    assert_eq!(shown(&state), (View::ListComplete, None));
    state.view = View::AllComplete;
    state.handle_key_with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(shown(&state), (View::AllComplete, None));
}

#[test]
fn an_unbound_chord_cancels_a_pending_prompt_and_names_special_keys() {
    let mut state = AppState::new(two_list_document());
    state.pending_open_link = true;
    state.handle_key_with_mods(KeyCode::Left, KeyModifiers::CONTROL);
    assert!(!state.pending_open_link);
    assert_eq!(state.current_item_index, 0, "Ctrl-Left does not navigate");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Ctrl-Left is not bound (? lists every key)")
    );
    assert_eq!(
        chord_name(
            KeyCode::Char('x'),
            KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META
        ),
        "Super-Hyper-Meta-X"
    );
}

#[test]
fn bound_ctrl_chords_still_reach_their_screen() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    state.handle_key(KeyCode::Char('u'));
    state.view = View::ListComplete;
    state.handle_key_with_mods(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(
        state.document.lists[0].items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
        "Ctrl-R redoes from a completion screen"
    );
}

#[test]
fn report_link_sent_names_the_opener_as_an_attempt() {
    let mut state = AppState::new(single_item_document(item_with_links(
        1,
        &["https://example.com/a"],
    )));
    state.report_link_sent("/usr/bin/xdg-open", "https://example.com/a");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to xdg-open: https://example.com/a")
    );
    assert!(!state.status_is_error);
}

#[test]
fn copy_link_instead_reports_each_clipboard_outcome() {
    let url = "https://example.com/a";
    let mut state = AppState::new(single_item_document(item_with_links(1, &[url])));

    crate::clipboard::fake::with(|c| {
        c.system_available = true;
        c.system_attempts.clear();
    });
    state.copy_link_instead(url);
    assert_eq!(
        state.status_message.as_deref(),
        Some("No browser here — link copied to clipboard")
    );
    assert!(!state.status_is_error);
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![(url.to_string(), false)]);
    });

    crate::clipboard::fake::with(|c| c.system_available = false);
    state.copy_link_instead(url);
    assert_eq!(
        state.status_message.as_deref(),
        Some("No browser here — link sent to clipboard (OSC 52)")
    );

    state.copy_link_instead("https://example.com/\u{7}");
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("hidden control characters"))
    );

    let huge = format!("https://example.com/{}", "a".repeat(200_000));
    state.copy_link_instead(&huge);
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("too large"))
    );
}

// --- The `?` overlay documents every handled key ---

/// The names a `?` overlay key column may use for `code` with `modifiers`.
fn help_key_names(code: KeyCode, modifiers: KeyModifiers) -> Vec<String> {
    if modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = code
    {
        return vec![format!("Ctrl-{}", c.to_ascii_uppercase())];
    }
    let name = |s: &str| vec![s.to_string()];
    match code {
        KeyCode::Char(' ') => name("space"),
        KeyCode::Char('1'..='9') => name("1-9"),
        KeyCode::Char('g') => vec!["g".to_string(), "gg".to_string()],
        KeyCode::Char(c) if c.is_ascii_uppercase() => vec![c.to_string(), format!("Shift-{c}")],
        KeyCode::Char(c) => vec![c.to_string()],
        KeyCode::Up => name("↑"),
        KeyCode::Down => name("↓"),
        KeyCode::Left => name("←"),
        KeyCode::Right => name("→"),
        KeyCode::PageUp => name("PageUp"),
        KeyCode::PageDown => name("PageDn"),
        other => vec![format!("{other:?}")],
    }
}

/// Every key the probe tries: named keys, printable ASCII (unless `typing`,
/// where letters are text input), and Ctrl-a..z. Ctrl-C and Ctrl-Z act on
/// every screen and are checked against the main rows separately.
fn probe_keys(typing: bool) -> Vec<(KeyCode, KeyModifiers)> {
    let none = KeyModifiers::NONE;
    let mut keys: Vec<(KeyCode, KeyModifiers)> = [
        KeyCode::Esc,
        KeyCode::Enter,
        KeyCode::Backspace,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Delete,
        KeyCode::Insert,
    ]
    .into_iter()
    .chain((1..=11).map(KeyCode::F))
    .map(|code| (code, none))
    .collect();
    if !typing {
        keys.extend((' '..='~').map(|c| (KeyCode::Char(c), none)));
    }
    keys.extend(
        ('a'..='z')
            .filter(|c| !matches!(c, 'c' | 'z'))
            .map(|c| (KeyCode::Char(c), KeyModifiers::CONTROL)),
    );
    keys
}

fn observable(state: &AppState) -> String {
    format!(
        "{:?} {:?}",
        (
            state.view,
            state.overlay,
            state.should_quit,
            state.suspend_requested,
            state.editor_requested,
            &state.search.query,
            &state.picker.query,
            state.picker.selection,
            state.help.scroll,
            &state.status_message,
        ),
        (
            state.current_list_index,
            state.current_item_index,
            state.pending_g,
            state.pending_open_link,
            state.pending_copy_code,
            &state.link_open_request,
            state.document.checkbox_progress(),
        )
    )
}

/// The key-column words of the `?` overlay sections titled `titles`.
fn help_words(titles: &[&str]) -> Vec<&'static str> {
    crate::ui::HELP_SECTIONS
        .iter()
        .filter(|(title, _)| titles.contains(title))
        .flat_map(|(_, rows)| rows.iter())
        .flat_map(|(key, _)| key.split_whitespace())
        .collect()
}

/// Presses each probe key on a fresh `setup()` state, and requires every key
/// whose outcome differs from `baseline`'s to be named in `titles`' rows.
fn assert_handled_keys_have_help_rows(
    mode: &str,
    titles: &[&str],
    typing: bool,
    setup: impl Fn() -> AppState,
    baseline: impl Fn(KeyCode, KeyModifiers) -> String,
) {
    let words = help_words(titles);
    let mut missing = Vec::new();
    for (code, modifiers) in probe_keys(typing) {
        let mut state = setup();
        if !state.is_bound_chord(code, modifiers) {
            continue;
        }
        state.handle_key_with_mods(code, modifiers);
        if observable(&state) == baseline(code, modifiers) {
            continue;
        }
        let names = help_key_names(code, modifiers);
        if !names.iter().any(|n| words.contains(&n.as_str())) {
            missing.push(names[0].clone());
        }
    }
    assert!(
        missing.is_empty(),
        "{mode}: handled keys with no `?` overlay row: {missing:?}"
    );
}

/// Baseline for an overlay or screen: what an unbound key (F12) does there.
fn unbound_key_outcome(setup: &impl Fn() -> AppState) -> String {
    let mut state = setup();
    state.handle_key(KeyCode::F(12));
    observable(&state)
}

fn assert_mode_documented(mode: &str, titles: &[&str], typing: bool, setup: impl Fn() -> AppState) {
    let baseline = unbound_key_outcome(&setup);
    assert_handled_keys_have_help_rows(mode, titles, typing, setup, |_, _| baseline.clone());
}

#[test]
fn every_checklist_key_has_a_help_row() {
    assert_mode_documented("checklist", &[""], false, || {
        let mut state = AppState::new(two_list_document());
        state.current_item_index = 1;
        state
    });
}

#[test]
fn every_completion_screen_key_has_a_help_row() {
    let titles = ["", "On a list-complete screen"];
    assert_mode_documented("list complete", &titles, false, || {
        let mut state = AppState::new(two_list_document_first_list_almost_done());
        state.view = View::ListComplete;
        state
    });
    assert_mode_documented("all complete", &titles, false, || {
        let mut state = AppState::new(all_done_document());
        state.view = View::AllComplete;
        state
    });
}

#[test]
fn every_overlay_key_has_a_help_row() {
    assert_mode_documented("help", &["In this help"], false, || {
        let mut state = AppState::new(two_list_document());
        state.handle_key(KeyCode::Char('?'));
        state
    });
    assert_mode_documented("search", &["In / search"], true, || {
        let mut state = AppState::new(two_list_document());
        state.handle_key(KeyCode::Char('/'));
        type_str(&mut state, "task");
        state
    });
    assert_mode_documented("picker", &["In the T go-to-task list"], true, || {
        let mut state = AppState::new(two_list_document());
        state.handle_key(KeyCode::Char('T'));
        type_str(&mut state, "task");
        state.picker.selection = 2;
        state.picker.viewport_height = 4;
        state
    });
    let dialogs = ["Reset dialogs (R, or q when all done)"];
    assert_mode_documented("reset dialog", &dialogs, false, || {
        let mut state = AppState::new(two_list_document_first_list_almost_done());
        state.overlay = Some(Overlay::ConfirmReset);
        state
    });
    assert_mode_documented("quit-reset dialog", &dialogs, false, || {
        let mut state = AppState::new(all_done_document());
        state.overlay = Some(Overlay::ConfirmQuitReset);
        state
    });
}

#[test]
fn every_digit_prompt_key_has_a_help_row() {
    // A prompt's keys are the ones that act differently from the same key
    // pressed with no prompt pending.
    let fresh = || {
        let mut state = AppState::new(two_list_document());
        state.current_item_index = 1;
        state
    };
    let no_prompt = |code, modifiers| {
        let mut state = fresh();
        state.handle_key_with_mods(code, modifiers);
        observable(&state)
    };
    let titles = ["Waiting for a number after o / y"];
    assert_handled_keys_have_help_rows(
        "o prompt",
        &titles,
        false,
        || {
            let mut state = fresh();
            state.pending_open_link = true;
            state
        },
        no_prompt,
    );
    assert_handled_keys_have_help_rows(
        "y prompt",
        &titles,
        false,
        || {
            let mut state = fresh();
            state.pending_copy_code = true;
            state
        },
        no_prompt,
    );
}

#[test]
fn every_screen_help_lists_ctrl_c_and_ctrl_z() {
    let words = help_words(&[""]);
    assert!(words.contains(&"Ctrl-C") && words.contains(&"Ctrl-Z"));
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
