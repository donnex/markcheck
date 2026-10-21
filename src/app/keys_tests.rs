use super::*;
use crate::app::test_support::*;

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
    state.layout.help_max_scroll = 5; // as if the renderer measured an overflow
    state.layout.help_viewport_height = 6;
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
    assert_eq!(state.help.scroll, 5, "clamps at layout.help_max_scroll");
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

#[test]
fn shift_l_jumps_to_the_next_list() {
    let mut state = AppState::new(two_list_document());
    state.current_list_index = 0;
    state.current_item_index = 0;
    state.handle_key(KeyCode::Char('L'));
    assert_eq!(state.current_list_index, 1, "Shift-L jumps lists");
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
    assert!(!state.quit_requested());
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
fn esc_cancels_the_open_prompt_without_quitting() {
    // Regression: while armed, Esc must cancel the prompt, not fall through
    // to the global q/Esc quit.
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link());
    state.handle_key(KeyCode::Esc);
    assert!(!state.pending_open_link(), "cancelled");
    assert!(
        !state.quit_requested(),
        "esc must not quit while arming a link open"
    );
    assert!(state.link_open_request().is_none());
}

#[test]
fn mouse_click_clears_a_pending_gg_chord() {
    // A lone `g` arms the `gg` chord; a mouse click in
    // between must not leave it armed for a later `g` to be misread as the
    // second half of a stale chord.
    use ratatui::layout::Rect;
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('g'));
    assert!(state.pending_g(), "armed");

    state.layout.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::List(1))];
    state.handle_left_click(65, 5);
    assert!(!state.pending_g(), "cleared by the click");
}

#[test]
fn q_sets_should_quit() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('q'));
    assert!(state.quit_requested());
}

#[test]
fn wheel_scrolls_the_help_overlay() {
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.layout.help_max_scroll = 2;
    state.handle_scroll_down();
    state.handle_scroll_down();
    state.handle_scroll_down();
    assert_eq!(state.help.scroll, 2, "clamps at layout.help_max_scroll");
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
fn ctrl_c_closes_help() {
    let mut state = AppState::new(two_list_document());
    state.view = View::ListComplete;
    state.handle_key(KeyCode::Char('?'));
    assert_eq!(state.overlay, Some(Overlay::Help));
    press_ctrl_c(&mut state);
    assert_eq!(shown(&state), (View::ListComplete, None));
}

#[test]
fn help_ctrl_scroll_keys_and_page_up_scroll() {
    // Mirrors help_scroll_keys_scroll_instead_of_closing, but for the
    // Ctrl-E/Ctrl-Y/Ctrl-U/PageUp arms that test doesn't reach.
    let mut state = AppState::new(two_list_document());
    state.handle_key(KeyCode::Char('?'));
    state.layout.help_max_scroll = 5;
    state.layout.help_viewport_height = 6;
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
    assert!(!state.quit_requested());
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
fn an_unbound_chord_cancels_a_pending_prompt_and_names_special_keys() {
    let mut state = AppState::new(two_list_document());
    state.pending_chord = PendingChord::OpenLink { typed: 0 };
    state.handle_key_with_mods(KeyCode::Left, KeyModifiers::CONTROL);
    assert!(!state.pending_open_link());
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
            &state.effects,
            &state.search.query,
            &state.picker.query,
            state.picker.selection,
            state.help.scroll,
            &state.status_message,
        ),
        (
            state.current_list_index,
            state.current_item_index,
            state.pending_chord,
            state.document.progress(),
        )
    )
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
        state.layout.picker_viewport_height = 4;
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
            state.pending_chord = PendingChord::OpenLink { typed: 0 };
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
            state.pending_chord = PendingChord::CopyCode { typed: 0 };
            state
        },
        no_prompt,
    );
}

#[test]
fn esc_never_quits_and_says_how_to_on_every_content_screen() {
    for view in [View::Checklist, View::ListComplete, View::AllComplete] {
        let mut state = AppState::new(two_list_document());
        state.view = view;
        state.handle_key(KeyCode::Esc);
        assert!(!state.quit_requested(), "{view:?}");
        assert_eq!(state.status_message.as_deref(), Some("Type q to quit"));
        assert!(state.status_is_error, "sticky, so it is read");
        assert_eq!(state.view, view, "nothing else changes");
    }
}

#[test]
fn any_other_key_or_esc_leaves_an_armed_critical_task_open() {
    let path = write_real_file("## S\n\n- [ ] **! Fail over**\n- [ ] next\n");
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char(' '));
    state.handle_key(KeyCode::Char('j'));
    state.handle_key(KeyCode::Char('k'));
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Fail"),
        "the j disarmed it, so this Space only armed it again"
    );
    state.handle_key(KeyCode::Esc);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Critical step not checked")
    );
    assert_eq!(state.critical_armed, None);
}

#[test]
fn the_wheel_or_a_paste_leaves_an_armed_critical_task_open() {
    let path = write_real_file(
        "## S

- [ ] **! Fail over**
- [ ] next
",
    );
    let mut state = AppState::new(parser::parse_document(path.clone()).unwrap());

    state.handle_key(KeyCode::Char(' '));
    state.handle_scroll_down();
    state.handle_scroll_up();
    assert_eq!(state.critical_armed, None, "the wheel disarmed it");
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Fail"),
        "this Space only armed it again"
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Critical step — press Space again"))
    );

    state.handle_paste("text");
    assert_eq!(state.critical_armed, None, "the paste disarmed it");
    state.handle_key(KeyCode::Char(' '));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("- [ ] **! Fail"),
        "still open after the paste"
    );
}

/// What a key does with the `is_bound_chord` gate bypassed, card scroll
/// included since `observable` leaves it out.
fn ungated_outcome(
    setup: &impl Fn() -> AppState,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> String {
    let mut state = setup();
    state.dispatch_bound_key(code, modifiers);
    format!("{} {}", observable(&state), state.card_scroll)
}

/// The Ctrl-a..z chords a screen's handlers act on themselves: ungated, each
/// does something other than both an unbound key and its plain letter.
fn ctrl_chords_with_own_arm(setup: impl Fn() -> AppState) -> (Vec<char>, Vec<char>) {
    let unbound = ungated_outcome(&setup, KeyCode::F(12), KeyModifiers::NONE);
    let mut handled = Vec::new();
    let mut gated = Vec::new();
    for c in ('a'..='z').filter(|c| !matches!(c, 'c' | 'z')) {
        let code = KeyCode::Char(c);
        let chord = ungated_outcome(&setup, code, KeyModifiers::CONTROL);
        if chord != unbound && chord != ungated_outcome(&setup, code, KeyModifiers::NONE) {
            handled.push(c);
        }
        if setup().is_bound_chord(code, KeyModifiers::CONTROL) {
            gated.push(c);
        }
    }
    (handled, gated)
}

type Screen = (&'static str, fn() -> AppState);

#[test]
fn every_ctrl_chord_a_handler_acts_on_is_let_through_and_no_other() {
    let checklist = || {
        let mut state = AppState::new(two_list_document());
        state.current_item_index = 1;
        state.card_scroll = 2;
        state.layout.card_max_scroll = 6;
        state.layout.card_viewport_height = 4;
        state
    };
    let screens: [Screen; 7] = [
        ("checklist", checklist),
        ("list complete", || {
            let mut state = AppState::new(two_list_document_first_list_almost_done());
            state.view = View::ListComplete;
            state
        }),
        ("all complete", || {
            let mut state = AppState::new(all_done_document());
            state.view = View::AllComplete;
            state
        }),
        ("help", || {
            let mut state = AppState::new(two_list_document());
            state.handle_key(KeyCode::Char('?'));
            state.help.scroll = 2;
            state.layout.help_max_scroll = 6;
            state.layout.help_viewport_height = 4;
            state
        }),
        ("search", || {
            let mut state = AppState::new(two_list_document());
            state.handle_key(KeyCode::Char('/'));
            type_str(&mut state, "a task");
            state
        }),
        ("picker", || {
            let mut state = AppState::new(two_list_document());
            state.handle_key(KeyCode::Char('T'));
            type_str(&mut state, "task");
            state.picker.selection = 2;
            state.layout.picker_viewport_height = 4;
            state
        }),
        ("reset dialog", || {
            let mut state = AppState::new(two_list_document_first_list_almost_done());
            state.overlay = Some(Overlay::ConfirmReset);
            state
        }),
    ];
    for (screen, setup) in screens {
        let (handled, gated) = ctrl_chords_with_own_arm(setup);
        assert_eq!(
            handled, gated,
            "{screen}: the Ctrl chords its handlers act on (left) must be exactly \
             the ones is_bound_chord lets through (right)"
        );
    }
}
