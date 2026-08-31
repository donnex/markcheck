use super::*;
use std::path::PathBuf;

fn checkbox(line_number: usize, completed: bool) -> Item {
    Item {
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

fn document_with_lists(lists: Vec<List>) -> Document {
    let max_line = lists
        .iter()
        .flat_map(|s| s.items.iter())
        .map(|i| i.line_number)
        .max()
        .unwrap_or(0);
    let file_path = unique_temp_path();
    // write_back requires the target file to already exist (it reads
    // its permissions before writing); a real, if empty, backing file lets
    // toggle/reset/undo tests exercise the actual write-success path instead
    // of always silently failing against a path that was never created.
    fs::write(&file_path, "").unwrap();
    Document {
        file_path,
        title: None,
        has_default_list: false,
        lists,
        raw_lines: vec![String::new(); max_line],
        uses_crlf: false,
        trailing_newline: true,
    }
}

/// Like `document_with_lists`, but the backing file is never created, so
/// `write_back` always fails (regression tests: a missing file is an
/// easy, portable stand-in for any write failure — permission denied, disk
/// full, etc. — since `write_back` bails at the same first `fs::metadata`
/// call regardless of *why* the file is unwritable).
fn document_with_missing_file(lists: Vec<List>) -> Document {
    let max_line = lists
        .iter()
        .flat_map(|s| s.items.iter())
        .map(|i| i.line_number)
        .max()
        .unwrap_or(0);
    Document {
        file_path: unique_temp_path(),
        title: None,
        has_default_list: false,
        lists,
        raw_lines: vec![String::new(); max_line],
        uses_crlf: false,
        trailing_newline: true,
    }
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
fn start_current_marks_started_without_advancing() {
    let mut state = AppState::new(two_list_document());
    state.start_current();
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::Started)
    );
    assert_eq!(state.current_item_index, 0, "does not auto-advance");
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn start_current_toggles_started_off() {
    let mut state = AppState::new(two_list_document());
    state.start_current();
    state.start_current();
    assert_eq!(
        state.current_item().unwrap().kind,
        ItemKind::Checkbox(TaskState::NotStarted)
    );
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Help);
    // A non-scroll key closes (scroll keys now scroll instead).
    state.handle_key(KeyCode::Char('x'));
    assert_eq!(
        state.screen,
        Screen::Checklist,
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
    assert_eq!(state.screen, Screen::Help, "j scrolls, does not close");
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
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn help_returns_to_the_screen_it_was_opened_from() {
    let mut state = AppState::new(two_list_document());
    state.screen = Screen::ListComplete;
    state.handle_key(KeyCode::Char('?'));
    assert_eq!(state.screen, Screen::Help);
    state.handle_key(KeyCode::Esc);
    assert_eq!(state.screen, Screen::ListComplete);
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
    assert_eq!(state.screen, Screen::Checklist);
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
    // Shift-L used to move silently when nothing incomplete follows.
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
    // Tab used to move silently when every task is already done.
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
    assert_eq!(state.screen, Screen::Checklist);
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
    state.focus_item(1, 0);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(state.screen, Screen::Checklist);
    // An out-of-range item clamps to the list's last item...
    state.focus_item(0, 99);
    let last = state.document.lists[0].items.len() - 1;
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (0, last)
    );
    // ...and an out-of-range list index is ignored (no move, no panic).
    state.focus_item(99, 0);
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
    assert_eq!(state.screen, Screen::Search);
    type_str(&mut state, "task 3");
    assert_eq!(state.search.query, "task 3");
    // "task 3" is the third item of the first list.
    assert_eq!((state.current_list_index, state.current_item_index), (0, 2));
    // Still on the Search screen until committed.
    assert_eq!(state.screen, Screen::Search);
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
    assert_eq!(state.screen, Screen::Search);
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
}

#[test]
fn n_without_an_active_search_is_a_noop() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('n'));
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert!(state.search.last.is_none());
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
fn committing_a_single_match_search_omits_the_cycle_nudge() {
    // The `n`/`N` nudge only makes sense with more than one match.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task 4"); // exactly one task
    state.handle_key(KeyCode::Enter);
    assert_eq!(state.status_message.as_deref(), Some("Match 1/1"));
}

// --- Go-to-task overlay (ListPicker) ---

#[test]
fn t_opens_picker_listing_all_tasks_then_filters() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    assert_eq!(state.screen, Screen::ListPicker);
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Opening"))
    );
}

#[test]
fn o_opens_every_safe_scheme_including_uppercase() {
    for url in [
        "http://example.com/a",
        "https://example.com/a",
        "mailto:ops@example.com",
        "HTTPS://EX.COM/A",
        // is_safe_link is a scheme-prefix allowlist, not a full URI
        // validator — content after a genuinely-safe scheme (however oddly
        // formed) isn't separately rejected. This is safe because the URL
        // is handed to the opener as a single argv element
        // (Command::new(...).arg(url)), never through a shell, so an
        // embedded control character can't be interpreted as an argument
        // or command separator the way it could in shell text.
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
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("Opening"))
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
    // Deep review: this used to be a silent no-op, against the UI-feedback
    // rule -- pressing `5` in a two-list document cleared the status bar
    // and did nothing else, which reads exactly like a wedged app.
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
    state.screen = Screen::ListComplete;
    state.jump_to_list(1);
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::ListComplete);
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
    assert_eq!(state.screen, Screen::AllComplete);
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
    assert_eq!(state.screen, Screen::AllComplete);
    state.toggle_current();
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn list_complete_j_advances_to_next_list() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('j'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn list_complete_k_returns_to_checklist_on_same_list() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.current_list_index, 0);
    assert_eq!(state.current_item_index, 2);
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::AllComplete);
    state.handle_key(KeyCode::Char('k'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.current_item_index, 1);
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn number_key_jumps_to_list_from_any_screen() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('2'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);

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
    assert_eq!(state.screen, Screen::ListComplete);
    // Advancing should skip fully-done B and land on C's undone item.
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_list_index, 2);
    assert_eq!(state.current_item_index, 0);
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::AllComplete);
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
fn list_complete_l_advances_h_reviews() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.current_item_index = 2;
    state.toggle_current();
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('h'));
    assert_eq!(state.screen, Screen::Checklist);
    state.toggle_current(); // untoggle
    state.toggle_current(); // re-toggle -> ListComplete again
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_list_index, 1);
    assert_eq!(state.screen, Screen::Checklist);
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
    state.screen = Screen::AllComplete;
    state.handle_key(KeyCode::Char('e'));
    assert!(state.take_editor_request());
}

// --- Reset with confirmation ---

#[test]
fn r_with_no_done_tasks_skips_prompt() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.screen, Screen::Checklist);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Nothing to reset: no tasks are done")
    );
}

#[test]
fn r_with_done_tasks_opens_confirm_prompt() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.screen, Screen::ConfirmReset);
}

#[test]
fn r_with_only_started_tasks_opens_confirm_prompt() {
    // Regression test: request_reset used to check only the Done count, so
    // a document whose only progress was Started (`[/]`) items wrongly
    // reported "nothing to reset" even though reset_all clears Started too.
    let mut state = AppState::new(two_list_document());
    state.start_current();
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.screen, Screen::ConfirmReset);
}

#[test]
fn confirm_reset_yes_clears_all_and_persists() {
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    state.handle_key(KeyCode::Char('R'));
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::ConfirmReset);
    state.handle_key(KeyCode::Esc);
    assert_eq!(state.screen, Screen::Checklist);
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
    state.screen = Screen::AllComplete;
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.screen, Screen::ConfirmReset);
    state.handle_key(KeyCode::Char('n'));
    assert_eq!(state.screen, Screen::AllComplete);
}

#[test]
fn confirm_reset_enter_does_not_confirm() {
    // Enter no longer resets — only `y` does. Any other key cancels.
    let mut state = AppState::new(two_list_document_first_list_almost_done());
    let done_before = state.document.checkbox_stats().0;
    state.handle_key(KeyCode::Char('R'));
    assert_eq!(state.screen, Screen::ConfirmReset);
    state.handle_key(KeyCode::Enter);
    assert_eq!(state.screen, Screen::Checklist, "Enter cancels");
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
    assert_eq!(state.screen, Screen::ConfirmQuitReset);
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
    assert_eq!(state.screen, Screen::ConfirmQuitReset);
    state.handle_key(KeyCode::Esc);
    assert!(!state.should_quit, "Esc cancels, stays running");
    assert_eq!(state.screen, Screen::Checklist);
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
fn copy_with_multiple_candidates_shows_alert() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["first".to_string(), "second".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Not copied: item has 2 code candidates")
    );
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

    // Ambiguous item inside the card → the multi-candidate hint.
    let mut document = two_list_document();
    document.lists[0].items[0].code_blocks = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.handle_left_click(12, 6);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Not copied: item has 2 code candidates")
    );
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

    // On the region: specific path (outcome is env-dependent, but never
    // the ambiguous hint).
    state.handle_left_click(15, 7);
    let msg = state.status_message.as_deref().unwrap_or("");
    assert!(
        !msg.contains("candidates"),
        "specific-region click overrides the ambiguous hint: {msg:?}"
    );

    // Elsewhere in the card (no region): ambiguous fallback (MVP).
    state.handle_left_click(15, 6);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Not copied: item has 2 code candidates")
    );
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

    state.screen = Screen::AllComplete;
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
    assert_eq!(state.screen, Screen::Checklist);
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
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn click_on_overview_works_from_completion_screen() {
    use ratatui::layout::Rect;
    // The overview is a plain nav aid on the completion screens too, so
    // a click there jumps to the task and drops back to the checklist.
    let mut state = AppState::new(two_list_document());
    state.screen = Screen::AllComplete;
    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 0))];

    state.handle_left_click(70, 5);
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn click_on_overview_ignored_while_a_modal_is_open() {
    use ratatui::layout::Rect;
    // Overview rows are still drawn beside the Help/Search/picker
    // overlays, but a click there must not jump the cursor mid-modal.
    let mut state = AppState::new(two_list_document());
    let before = (state.current_list_index, state.current_item_index);
    state.screen = Screen::Help;
    state.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::Item(1, 1))];

    state.handle_left_click(70, 5);
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        before,
        "modal click leaves the cursor put"
    );
    assert_eq!(state.screen, Screen::Help, "still on the modal");
}

#[test]
fn click_on_overview_marker_toggles_item_without_moving_cursor() {
    use ratatui::layout::Rect;
    // A click on the marker zone toggles that task done/not-done in
    // place — the cursor (and card view) don't move.
    let mut state = AppState::new(two_list_document());
    let before = (state.current_list_index, state.current_item_index);
    state.overview_rows = vec![(Rect::new(60, 5, 6, 1), OverviewTarget::Toggle(1, 0))];

    state.handle_left_click(61, 5);
    assert!(matches!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    ));
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
    state.screen = Screen::AllComplete;

    // Un-done the first task via a marker click → demote to the checklist.
    state.toggle_item(0, 0);
    assert_eq!(state.screen, Screen::Checklist);

    // Re-complete it from the checklist: no promotion to AllComplete.
    state.toggle_item(0, 0);
    assert_eq!(
        state.screen,
        Screen::Checklist,
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
    // Without the rollback, a failed write still left the mutation in
    // memory. That phantom-done item wouldn't trigger a completion screen
    // by itself (the failing action bails out before checking), but a
    // *later, successful* toggle of the real remaining item would count it
    // too via `list_all_done` and show a false completion screen — even
    // though the file on disk still has it as not-done.
    let document = document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, false), checkbox(3, false)],
    }]);
    let path = document.file_path.clone();
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
        state.screen,
        Screen::Checklist,
        "no premature completion screen"
    );

    // The file reappears, so toggling the *actual* remaining item succeeds.
    // Clear the sticky error first, matching what a real keypress does
    // (`handle_key_with_mods` clears status before dispatch) — otherwise
    // the previous failure's message would still be sitting there and mask
    // whether this second attempt actually succeeded.
    fs::write(&path, "").unwrap();
    state.clear_status();
    state.current_item_index = 2; // item 3, genuinely not done
    state.toggle_current();
    assert!(!state.status_is_error, "the retry succeeds");
    assert_eq!(
        state.screen,
        Screen::Checklist,
        "item 2 is still genuinely not-done, so the list must not appear complete"
    );
}

#[test]
fn toggle_refuses_and_reloads_when_disk_content_changed_since_load() {
    // Two writers sharing one file (another markcheck instance, or an
    // external editor) — the in-memory document is stale relative to disk
    // by the time we're about to write, so overwriting now would silently
    // discard whatever the other writer just did. `disk_content_diverged`
    // must catch this even though mtime/size alone can miss it on a
    // coarse-mtime filesystem.
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
    // External review, round 4: disk_content_diverged/commit_write is a
    // pre-write content check, not an atomic compare-and-swap (see
    // hash_bytes's and disk_content_diverged's doc comments in app.rs) —
    // there's a real gap between this check returning "unchanged" and
    // write_back's own rename actually landing, in which another writer
    // can still save a change that gets silently overwritten. This
    // documents that accepted gap directly, standing in for the two real
    // steps `commit_write` takes with a race manually inserted between
    // them, rather than leaving it implicit.
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
        "the external writer's content is silently overwritten here -- \
         this is the accepted gap, not a regression"
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
    // External review: a transient malformed intermediate save (common with
    // editors that write in stages) must not be recorded as the current
    // disk revision, or a later write would wrongly believe the file still
    // matches what markcheck last saw and silently overwrite the malformed
    // content instead of detecting the divergence.
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
    assert_eq!(state.status_message, None);

    state.reload_if_changed();
    assert_eq!(
        state.status_message, None,
        "toggling our own file should not look like an external change"
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
    // Regression: nothing in the Markdown format forbids two `## H2`s
    // sharing a title, but remap_position used to match by title alone —
    // `.position()` always finding the *first* occurrence — so a reload
    // while positioned on the *second* "Servers" list used to snap the
    // cursor back to the first one instead. Ranking by occurrence (this is
    // the second "Servers" list, land on the second "Servers" list again)
    // fixes that as long as the duplicates' relative order is unchanged.
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
    // Deep review, round 2, reproduced end-to-end against the real binary.
    // The "parsed fine, but no checklist items" branch declined to adopt the
    // new document -- correctly, since AppState requires at least one list --
    // but still advanced `file_content_hash` to the new on-disk content. The
    // in-memory document was then the *old* one while the fingerprint said
    // disk was current, so `disk_content_diverged` went blind and the next
    // write overwrote the user's file with stale content, silently.
    //
    // Exactly the invariant `failed_reload_does_not_advance_the_write_conflict_fingerprint`
    // pins for the sibling (failed-parse) branch.
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
        Some("File deleted — changes cannot be saved"),
    );
    assert_eq!(state.status_expiry, None, "deletion message is sticky");

    // Toggling must be blocked.
    state.toggle_current();
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — cannot save changes"),
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
    state.screen = crate::model::Screen::ConfirmReset;
    state.handle_key(KeyCode::Char('y'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("File was deleted — cannot save changes"),
    );
}

// --- Undo / redo of state-changing actions ---

#[test]
fn undo_reverts_a_toggle_and_focuses_the_task() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current(); // item 0 -> Done, auto-advances to item 1
    assert_eq!(state.current_item_index, 1);

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
    assert_eq!(state.screen, Screen::Checklist);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Undo: marked not done")
    );
    assert!(!state.status_is_error);
}

#[test]
fn redo_reapplies_an_undone_toggle() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current();
    state.undo();
    state.redo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::Done),
        "redo restores the toggle",
    );
    assert_eq!(state.status_message.as_deref(), Some("Redo: marked done"));
}

#[test]
fn undo_reverts_a_started_toggle() {
    let mut state = AppState::new(two_list_document());
    state.start_current(); // item 0 -> Started
    state.undo();
    assert_eq!(
        state.current_list().items[0].kind,
        ItemKind::Checkbox(TaskState::NotStarted),
    );
}

#[test]
fn undo_reverts_a_reset_and_reports_the_count() {
    let mut state = AppState::new(two_list_document());
    state.toggle_current(); // item 0 done (advances)
    state.current_item_index = 1;
    state.toggle_current(); // item 1 done
    state.reset_all(); // both back to not-done
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
}

#[test]
fn undo_reverts_an_overview_click_toggle() {
    let mut state = AppState::new(two_list_document());
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
        Some("File was deleted — cannot save changes"),
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

// --- Coverage gaps found by the 2026-08-19 deep code review (REVIEW.md) ---

#[test]
fn record_git_sync_sets_the_synced_timestamp() {
    let mut state = AppState::new(two_list_document());
    assert!(state.git_sync.last_at.is_none());
    state.record_git_sync();
    assert!(state.git_sync.last_at.is_some());
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
        state.screen,
        Screen::Help,
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
    assert_eq!(state.screen, Screen::Checklist, "closes on Enter");
    assert_eq!(
        (state.current_list_index, state.current_item_index),
        (before_list, before_item),
        "cursor untouched when nothing matched"
    );
}

#[test]
fn go_to_first_and_last_item_are_no_ops_on_a_list_less_document() {
    // Defensive-only branches: AppState::new requires >=1 list, but
    // go_to_first_item/go_to_last_item each guard independently, so
    // construct the (otherwise-unreachable) empty-lists state directly.
    let mut state = AppState::new(two_list_document());
    state.document.lists.clear();
    state.go_to_first_item();
    state.go_to_last_item();
    // Must not panic; nothing to assert beyond that since there is no
    // valid cursor position to check.
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
    state.screen = Screen::ListComplete;
    state.toggle_item(1, 0); // toggles the *other* list's task
    assert_eq!(
        state.document.lists[1].items[0].kind,
        ItemKind::Checkbox(TaskState::Done)
    );
    assert_eq!(
        state.screen,
        Screen::ListComplete,
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
    assert_eq!(state.screen, Screen::Checklist);
}

#[test]
fn start_current_reports_instead_of_going_silent_on_a_display_only_card() {
    // Deep review: `s` on an info card used to return with no message at
    // all, unlike every neighbouring action (`y`, `o`, `}`/`{`, `R`), which
    // all explain a no-op.
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
fn advancing_past_the_last_list_with_an_earlier_incomplete_shows_all_complete() {
    // advance_to_next_incomplete_list only searches *forward* from the
    // current list; completing the last list while an earlier one is
    // still incomplete has nowhere forward to go, so it falls through to
    // AllComplete even though list 1 (index 0) isn't actually done.
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
    assert_eq!(state.screen, Screen::ListComplete);
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.screen, Screen::AllComplete);
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
    // External review: lock in the allowlist's exact boundary.
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
    // External review raised control-character-containing URLs (e.g. an
    // embedded newline that might smuggle a second "argument" into a
    // shelled-out opener) as worth testing explicitly. `is_safe_link` only
    // ever checks the scheme prefix, so a URL like this *is* accepted —
    // that's safe here specifically because `open_link` (main.rs) passes
    // the URL to `Command::arg`, never through a shell, so there is no
    // argv-splitting on embedded whitespace/newlines to exploit. This test
    // documents that division of responsibility: `is_safe_link` guarantees
    // an allowlisted scheme (and, incidentally, that the string can't start
    // with `-`); the no-shell `Command` call is what makes anything after
    // the scheme safe to pass through unexamined.
    assert!(is_safe_link("http://example.com\n--some-option"));
    assert!(is_safe_link("http://example.com\rSet-Cookie: x"));
}
