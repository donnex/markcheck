use super::*;
use crate::app::test_support::*;

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
fn moving_past_the_last_task_says_so() {
    for code in [
        KeyCode::Char('l'),
        KeyCode::Char('j'),
        KeyCode::Right,
        KeyCode::Down,
    ] {
        let mut state = AppState::new(two_list_document());
        state.current_list_index = 1;
        state.current_item_index = 1;
        state.handle_key(code);
        assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
        assert_eq!(
            state.status_message.as_deref(),
            Some("Already at the last task · Tab finds unfinished ones"),
            "{code:?}"
        );
        assert!(state.status_is_error);
    }
}

#[test]
fn moving_past_the_first_task_says_so() {
    for code in [
        KeyCode::Char('h'),
        KeyCode::Char('k'),
        KeyCode::Left,
        KeyCode::Up,
    ] {
        let mut state = AppState::new(two_list_document());
        state.handle_key(code);
        assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
        assert_eq!(
            state.status_message.as_deref(),
            Some("Already at the first task · Tab finds unfinished ones"),
            "{code:?}"
        );
        assert!(state.status_is_error);
    }
}

#[test]
fn edge_message_omits_the_tab_hint_when_nothing_else_is_unfinished() {
    let mut state = AppState::new(all_done_document());
    state.current_list_index = state.document.lists.len() - 1;
    state.current_item_index = state.current_list().items.len() - 1;
    state.view = View::Checklist;
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Already at the last task")
    );
}

#[test]
fn moving_within_the_checklist_reports_nothing() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('l'));
    assert_eq!(state.current_item_index, 1);
    assert_eq!(state.status_message, None);
}

#[test]
fn wheel_past_the_last_task_stays_quiet() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 1;
    state.current_item_index = 1;
    state.handle_scroll_down();
    assert_eq!((state.current_list_index, state.current_item_index), (1, 1));
    assert_eq!(state.status_message, None);
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
    // Completing the last item while an earlier one is still open wraps
    // to the list's first open task instead of crossing to the next list.
    let mut state = AppState::new(two_list_document());
    state.current_item_index = 2; // items 0 and 1 remain not done
    state.toggle_current();
    assert_eq!(state.current_list_index, 0, "does not cross lists");
    assert_eq!(state.current_item_index, 0, "wraps to the first open task");
    assert_eq!(shown(&state), (View::Checklist, None));
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
fn completing_a_task_still_stops_on_a_following_note() {
    let path = write_real_file("## L\n\n- [ ] a\n- a note\n- [ ] b\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());
    state.toggle_current();
    assert_eq!(state.current_item_index, 1, "the note is still shown");
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
    assert!(state.pending_g());
    state.handle_key(KeyCode::Char('l'));
    assert!(!state.pending_g(), "a non-g key cancels the pending gg");
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
    state.layout.card_max_scroll = 5; // overflow present, but j/k must still nav
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
    state.layout.card_max_scroll = 5;
    state.layout.card_viewport_height = 4; // half_page = 2, page = 3
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
fn wheel_scrolls_overflowing_card_and_navigates_otherwise() {
    let mut state = AppState::new(two_list_document());
    // No overflow: wheel navigates.
    state.handle_scroll_down();
    assert_eq!(state.current_item_index, 1);
    // Overflow: wheel scrolls, does not navigate.
    state.layout.card_max_scroll = 3;
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
    state.layout.card_max_scroll = 0;
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
