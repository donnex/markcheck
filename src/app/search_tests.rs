use super::*;
use crate::app::test_support::*;

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
    assert!(!state.quit_requested(), "typing q in search does not quit");
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
            assert_eq!(
                state.status_message.as_deref(),
                Some("No previous search — press / to search")
            );
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
    state.settle();
    crate::clipboard::fake::with(|c| {
        assert_eq!(c.system_attempts, vec![(expected.clone(), false)]);
    });
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
    state.layout.picker_viewport_height = 10; // as if rendered with a 10-row list
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
fn esc_still_cancels_search_and_help_without_the_quit_hint() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    state.handle_key(KeyCode::Esc);
    assert_eq!(state.overlay, None);
    assert_ne!(state.status_message.as_deref(), Some("Type q to quit"));

    state.handle_key(KeyCode::Char('?'));
    state.handle_key(KeyCode::Esc);
    assert_eq!(state.overlay, None);
    assert!(!state.quit_requested());
}

#[test]
fn an_empty_search_repeats_the_last_one_and_keeps_it_for_n() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task");
    state.handle_key(KeyCode::Enter);
    let first = (state.current_list_index, state.current_item_index);

    state.handle_key(KeyCode::Char('/'));
    state.handle_key(KeyCode::Enter);
    let second = (state.current_list_index, state.current_item_index);
    assert!(
        second > first,
        "moved on to the next match: {first:?} -> {second:?}"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Match ")),
        "{:?}",
        state.status_message
    );
    assert_eq!(state.search.last.as_deref(), Some("task"));

    state.handle_key(KeyCode::Char('n'));
    assert!((state.current_list_index, state.current_item_index) != second);
}

#[test]
fn ctrl_u_and_ctrl_w_edit_the_search_query() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('/'));
    type_str(&mut state, "task one  ");
    state.handle_key_with_mods(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(state.search.query, "task ");
    state.handle_key_with_mods(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(state.search.query, "");
    type_str(&mut state, "abc");
    state.handle_key_with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(state.search.query, "");
    assert_eq!(state.overlay, Some(Overlay::Search), "still searching");
}

#[test]
fn ctrl_w_deletes_a_word_in_the_picker_and_ctrl_u_still_pages() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('T'));
    type_str(&mut state, "task one");
    state.handle_key_with_mods(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(state.picker.query, "task ");
    state.handle_key_with_mods(KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(
        state.picker.query, "task ",
        "Ctrl-U moves, it doesn't clear"
    );
}
