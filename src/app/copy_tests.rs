use super::*;
use crate::app::test_support::*;

#[test]
fn a_sub_section_jump_keeps_its_name_beside_the_auto_copy_confirmation() {
    let mut item = text_item(2, "task 2");
    item.section = vec![crate::model::SubHeading {
        level: 3,
        text: "Alpha".to_string(),
    }];
    item.code_spans = vec!["make".to_string()];
    let mut state = AppState::new(document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![text_item(1, "task 1"), item],
    }]));
    state.auto_copy = true;
    state.handle_key(KeyCode::Char('}'));
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sub-section: Alpha · Sent to clipboard (OSC 52): make")
    );
}

#[test]
fn an_auto_copy_confirmation_never_hides_an_error() {
    let mut state = AppState::new(two_list_document());
    state.set_error("Something failed".to_string());
    state.apply_copy_outcome(CopyKind::Auto, "make", CopyOutcome::CopiedSystem);
    assert_eq!(state.status_message.as_deref(), Some("Something failed"));
    assert!(state.status_is_error);

    state.clear_status();
    state.apply_copy_outcome(CopyKind::Auto, "make", CopyOutcome::Failed);
    assert_eq!(state.status_message, None, "a failed auto-copy is silent");
}

#[test]
fn auto_copy_stays_silent_without_a_single_candidate() {
    // Auto-copy announces only a successful copy. With no code
    // candidate, or an ambiguous set, it stays silent.
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
            &[
                KeyCode::Char('T'),
                KeyCode::Up,
                KeyCode::Up,
                KeyCode::Up,
                KeyCode::Up,
                KeyCode::Enter,
            ],
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
        state.settle();

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
            status.ends_with(&format!("Sent to clipboard (OSC 52): {expected}")),
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
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Wrapped to the first unfinished task (List 1) · Sent to clipboard (OSC 52): cmd-1")
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
    state.settle();
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some(HIDDEN_CHARACTERS_MESSAGE)
    );
    assert!(state.status_is_error);
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
    assert!(state.pending_copy_code(), "armed for a command number");
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
    assert!(state.pending_copy_code(), "1 could still become 10");
    state.handle_key(KeyCode::Char('0'));
    state.settle();
    assert!(!state.pending_copy_code(), "consumed");
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
    state.settle();
    assert!(!state.pending_copy_code(), "consumed");
    assert_eq!(state.current_list_index, 0, "no list jump");
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to clipboard (OSC 52): second")
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
    state.settle();
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
    assert!(!state.pending_copy_code(), "cancelled");
    assert!(
        !state.quit_requested(),
        "esc must not quit while arming a copy"
    );
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
    assert!(!state.pending_copy_code(), "cancelled");
    assert_ne!(state.current_item_index, before, "j still navigated");
}

#[test]
fn mouse_input_clears_a_pending_copy_prompt() {
    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["a".to_string(), "b".to_string()];
    let mut state = AppState::new(document);
    state.handle_key(KeyCode::Char('y'));
    assert!(state.pending_copy_code(), "armed");
    state.handle_scroll_down();
    assert!(!state.pending_copy_code(), "cleared by the scroll");
}

#[test]
fn no_code_copy_hint_is_sticky() {
    // A `y` on a no-code item is feedback to a deliberate keypress,
    // so it sticks (no expiry) rather than fading after 4s.
    let mut state = AppState::new(two_list_document());
    state.layout.card_rect = Some(ratatui::layout::Rect::new(10, 5, 40, 10));
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
    state.settle();
    assert_eq!(state.status_message.as_deref(), expected);
    assert!(state.status_is_error);

    let mut state = AppState::new(two_list_document());
    state.layout.card_rect = Some(Rect::new(10, 5, 40, 10));
    state.layout.code_regions = vec![(Rect::new(11, 7, 38, 1), hidden.as_str().into())];
    state.handle_left_click(15, 7);
    state.settle();
    assert_eq!(state.status_message.as_deref(), expected);

    let mut document = two_list_document();
    document.lists[0].items[0].code_spans = vec!["echo ok\rrm -rf ~".to_string()];
    let mut state = AppState::new(document);
    state.auto_copy = true;
    state.maybe_auto_copy();
    state.settle();
    assert_eq!(state.status_message.as_deref(), expected);
}

#[test]
fn a_copy_to_both_clipboards_says_so() {
    let mut state = AppState::new(two_list_document());
    state.apply_copy_outcome(CopyKind::Command, "ls", CopyOutcome::CopiedBoth);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Copied (system clipboard + OSC 52): ls")
    );
    state.apply_copy_outcome(CopyKind::LinkFallback, "x", CopyOutcome::CopiedBoth);
    assert!(
        state
            .status_message
            .as_deref()
            .unwrap()
            .contains("system clipboard + OSC 52")
    );
}

#[test]
fn a_copy_too_large_for_osc52_says_only_the_system_clipboard_has_it() {
    const SYSTEM_ONLY: &str =
        "Copied to the system clipboard only — too large to send through the terminal (OSC 52)";
    let mut state = AppState::new(two_list_document());
    state.apply_copy_outcome(
        CopyKind::Command,
        "ls",
        CopyOutcome::CopiedSystemOnlyTooLarge,
    );
    assert_eq!(state.status_message.as_deref(), Some(SYSTEM_ONLY));
    assert!(state.status_is_error);

    state.clear_status();
    state.set_status("Match 1/2".to_string());
    state.apply_copy_outcome(CopyKind::Auto, "ls", CopyOutcome::CopiedSystemOnlyTooLarge);
    assert_eq!(
        state.status_message.as_deref(),
        Some(format!("Match 1/2 · {SYSTEM_ONLY}").as_str())
    );
    assert!(state.status_is_error);

    state.clear_status();
    state.apply_copy_outcome(CopyKind::Auto, "ls", CopyOutcome::CopiedSystemOnlyTooLarge);
    assert_eq!(state.status_message.as_deref(), Some(SYSTEM_ONLY));

    state.clear_status();
    state.apply_copy_outcome(
        CopyKind::LinkFallback,
        "x",
        CopyOutcome::CopiedSystemOnlyTooLarge,
    );
    assert!(
        state
            .status_message
            .as_deref()
            .unwrap()
            .contains("link copied to the system clipboard only")
    );
    assert!(state.status_is_error);
}

#[test]
fn each_copy_outcome_that_falls_short_is_reported_as_an_error() {
    let cases = [
        (
            CopyKind::Command,
            CopyOutcome::TooLarge,
            "Not copied: this command is too large for the terminal clipboard",
        ),
        (
            CopyKind::Command,
            CopyOutcome::Failed,
            "Copy failed: no clipboard available",
        ),
        (
            CopyKind::LinkFallback,
            CopyOutcome::Failed,
            "No browser or clipboard here — link: https://example.com/x",
        ),
    ];
    for (kind, outcome, expected) in cases {
        let mut state = AppState::new(two_list_document());
        state.apply_copy_outcome(kind, "https://example.com/x", outcome);
        assert_eq!(state.status_message.as_deref(), Some(expected));
        assert!(state.status_is_error, "{expected}");
    }
}

#[test]
fn an_auto_copy_to_both_clipboards_says_so() {
    let mut state = AppState::new(two_list_document());
    state.apply_copy_outcome(CopyKind::Auto, "ls", CopyOutcome::CopiedBoth);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Copied (system clipboard + OSC 52): ls")
    );
    assert!(!state.status_is_error);
}

#[test]
fn each_extra_link_copy_outcome_follows_the_opener_report() {
    let cases = [
        (
            CopyOutcome::CopiedSystem,
            "link also copied to clipboard",
            false,
        ),
        (
            CopyOutcome::CopiedOsc52,
            "link also sent to clipboard (OSC 52)",
            false,
        ),
        (
            CopyOutcome::CopiedBoth,
            "link also copied (system clipboard + OSC 52)",
            false,
        ),
        (
            CopyOutcome::CopiedSystemOnlyTooLarge,
            "link also copied, but only to the system clipboard: too large to send through the terminal (OSC 52)",
            true,
        ),
        (
            CopyOutcome::TooLarge,
            "link not copied: too large for the terminal clipboard",
            true,
        ),
        (
            CopyOutcome::HiddenCharacters,
            "link not copied: it contains hidden control characters",
            true,
        ),
        (
            CopyOutcome::Failed,
            "link not copied: no clipboard available",
            true,
        ),
    ];
    for (outcome, copied, is_error) in cases {
        let mut state = AppState::new(two_list_document());
        state.set_status("Sent to open on this host: https://example.com/x".to_string());
        state.apply_copy_outcome(CopyKind::LinkAlso, "https://example.com/x", outcome);
        assert_eq!(
            state.status_message.as_deref(),
            Some(format!("Sent to open on this host: https://example.com/x · {copied}").as_str())
        );
        assert_eq!(state.status_is_error, is_error, "{copied}");
    }
}

#[test]
fn a_copy_confirmation_previews_the_first_line_and_counts_the_rest() {
    let mut state = AppState::new(two_list_document());
    let cases = [
        ("make", "Copied to clipboard: make"),
        (
            "kubectl rollout restart deploy/example\nkubectl rollout status deploy/example",
            "Copied to clipboard: kubectl rollout restart deploy/example (+1 line)",
        ),
        (
            "cd /srv/example\n./build.sh\n./deploy.sh",
            "Copied to clipboard: cd /srv/example (+2 lines)",
        ),
        ("\tmake  all ", "Copied to clipboard: make  all"),
    ];
    for (text, expected) in cases {
        state.apply_copy_outcome(CopyKind::Command, text, CopyOutcome::CopiedSystem);
        assert_eq!(state.status_message.as_deref(), Some(expected), "{text:?}");
        assert!(!state.status_is_error);
    }
}

#[test]
fn a_long_first_line_is_cut_with_an_ellipsis() {
    let mut state = AppState::new(two_list_document());
    let long = "x".repeat(PREVIEW_MAX_CHARS + 10);
    state.apply_copy_outcome(CopyKind::Command, &long, CopyOutcome::CopiedOsc52);
    let expected = format!(
        "Sent to clipboard (OSC 52): {}…",
        "x".repeat(PREVIEW_MAX_CHARS - 1)
    );
    assert_eq!(state.status_message.as_deref(), Some(expected.as_str()));

    let exact = "y".repeat(PREVIEW_MAX_CHARS);
    state.apply_copy_outcome(CopyKind::Command, &exact, CopyOutcome::CopiedOsc52);
    assert_eq!(
        state.status_message,
        Some(format!("Sent to clipboard (OSC 52): {exact}")),
        "a line exactly at the cap is shown whole"
    );
}

#[test]
fn an_auto_copy_confirmation_previews_the_command_too() {
    let mut state = AppState::new(two_list_document());
    state.apply_copy_outcome(CopyKind::Auto, "make\ntest", CopyOutcome::CopiedSystem);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Copied to clipboard: make (+1 line)")
    );
}

#[test]
fn the_first_osc52_copy_inside_tmux_says_what_tmux_needs() {
    let mut state = AppState::new(two_list_document());
    state.multiplexer = Multiplexer::Tmux;
    state.apply_copy_outcome(CopyKind::Command, "make", CopyOutcome::CopiedOsc52);
    assert_eq!(
        state.status_message.as_deref(),
        Some(
            "Sent to clipboard (OSC 52): make · in tmux this needs set-clipboard on or allow-passthrough on"
        )
    );
    state.apply_copy_outcome(CopyKind::Command, "make", CopyOutcome::CopiedOsc52);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to clipboard (OSC 52): make"),
        "only once a session"
    );
}

#[test]
fn the_tmux_hint_waits_for_a_copy_that_used_osc52() {
    let mut state = AppState::new(two_list_document());
    state.multiplexer = Multiplexer::Tmux;
    state.apply_copy_outcome(CopyKind::Command, "ls", CopyOutcome::CopiedSystem);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Copied to clipboard: ls")
    );
    state.set_error("Something failed".to_string());
    state.apply_copy_outcome(CopyKind::Auto, "ls", CopyOutcome::CopiedOsc52);
    assert!(
        !state.tmux_hint_shown,
        "a hidden auto-copy doesn't use it up"
    );
    state.clear_status();
    state.apply_copy_outcome(CopyKind::Auto, "ls", CopyOutcome::CopiedBoth);
    assert_eq!(
        state.status_message.as_deref(),
        Some(&*format!(
            "Copied (system clipboard + OSC 52): ls{TMUX_HINT}"
        ))
    );
}

#[test]
fn every_osc52_copy_kind_carries_the_tmux_hint() {
    let cases = [
        (CopyKind::Auto, CopyOutcome::CopiedOsc52),
        (CopyKind::Command, CopyOutcome::CopiedBoth),
        (CopyKind::LinkFallback, CopyOutcome::CopiedOsc52),
        (CopyKind::LinkFallback, CopyOutcome::CopiedBoth),
        (CopyKind::LinkAlso, CopyOutcome::CopiedOsc52),
        (CopyKind::LinkAlso, CopyOutcome::CopiedBoth),
    ];
    for (kind, outcome) in cases {
        let mut state = AppState::new(two_list_document());
        state.multiplexer = Multiplexer::Tmux;
        state.apply_copy_outcome(kind, "https://example.com", outcome.clone());
        let status = state.status_message.clone().unwrap_or_default();
        assert!(
            status.ends_with(TMUX_HINT),
            "{kind:?} {outcome:?}: {status}"
        );
        assert!(!state.status_is_error, "{kind:?} {outcome:?}");
    }
}

#[test]
fn outside_tmux_an_osc52_copy_has_no_hint() {
    for multiplexer in [Multiplexer::None, Multiplexer::Screen] {
        let mut state = AppState::new(two_list_document());
        state.multiplexer = multiplexer;
        state.apply_copy_outcome(CopyKind::Command, "make", CopyOutcome::CopiedOsc52);
        assert_eq!(
            state.status_message.as_deref(),
            Some("Sent to clipboard (OSC 52): make")
        );
    }
}
