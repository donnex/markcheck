use super::*;
use crate::app::test_support::*;

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
            state.link_open_request().is_none(),
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
    assert!(state.link_open_request().is_none());
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
    assert!(state.link_open_request().is_none());
    assert!(state.pending_open_link(), "armed for a link number");
    // No status message hides the links — the numbered list stays visible in
    // the panel below the card.
    assert_eq!(state.status_message, None, "links stay visible, not hidden");
}

#[test]
fn o_then_two_digits_opens_link_twelve() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    assert!(
        state.pending_open_link(),
        "1 could still become 10, 11 or 12"
    );
    assert_eq!(state.pending_number(), 1);
    assert!(state.link_open_request().is_none());
    state.handle_key(KeyCode::Char('2'));
    assert!(!state.pending_open_link(), "consumed");
    assert_eq!(state.pending_number(), 0);
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
    assert!(!state.pending_open_link());
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
    assert!(!state.pending_open_link());
    assert_eq!(
        state.take_link_open_request().as_deref(),
        Some("https://example.com/1")
    );
}

#[test]
fn o_then_a_number_past_the_last_link_says_so() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.handle_key(KeyCode::Char('3'));
    assert!(!state.pending_open_link());
    assert!(state.link_open_request().is_none());
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
    assert!(!state.pending_open_link());
    assert_eq!(state.pending_number(), 0);
    assert!(state.link_open_request().is_none());
    assert_eq!(state.overlay, Some(Overlay::Help));
}

#[test]
fn a_click_clears_a_partial_link_number() {
    let mut state = twelve_link_state();
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('1'));
    state.clear_pending_chords();
    assert_eq!(state.pending_number(), 0);
}

#[test]
fn o_then_a_digit_opens_that_link_not_a_list() {
    // The armed digit selects a link and must pre-empt the global
    // digit→list jump.
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('2'));
    assert!(!state.pending_open_link(), "consumed");
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
    // `o` arms the link-number chord; a mouse click (e.g. jumping to a
    // different card via the overview) must not leave it armed for a later
    // digit to misfire against whatever the click just landed on.
    use ratatui::layout::Rect;
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link(), "armed");

    state.layout.overview_rows = vec![(Rect::new(60, 5, 30, 1), OverviewTarget::List(0))];
    state.handle_left_click(65, 5);
    assert!(!state.pending_open_link(), "cleared by the click");
}

#[test]
fn mouse_scroll_clears_a_pending_open_link_chord() {
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    assert!(state.pending_open_link(), "armed");

    state.handle_scroll_down();
    assert!(!state.pending_open_link(), "cleared by the scroll");
}

#[test]
fn o_then_an_out_of_range_digit_reports_no_such_link() {
    let item = item_with_links(1, &["https://example.com/a", "https://example.com/b"]);
    let mut state = AppState::new(single_item_document(item));
    state.handle_key(KeyCode::Char('o'));
    state.handle_key(KeyCode::Char('5'));
    assert!(state.link_open_request().is_none());
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
    assert!(state.link_open_request().is_none());
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
    assert!(state.pending_open_link());
    state.handle_key(KeyCode::Char('j'));
    assert!(!state.pending_open_link(), "cancelled");
    assert!(state.link_open_request().is_none());
    assert_eq!(state.current_item_index, 1, "j still navigated");
}

#[test]
fn ctrl_c_cancels_a_pending_link_prompt() {
    let mut state = AppState::new(two_list_document());
    state.pending_chord = PendingChord::OpenLink { typed: 0 };
    press_ctrl_c(&mut state);
    assert!(!state.pending_open_link());
    assert_quit_hint(&state);
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
fn report_link_sent_names_the_opener_as_an_attempt() {
    let mut state = AppState::new(single_item_document(item_with_links(
        1,
        &["https://example.com/a"],
    )));
    state.report_link_sent("/usr/bin/xdg-open", "https://example.com/a", false);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to xdg-open: https://example.com/a")
    );
    assert!(!state.status_is_error);
    state.report_link_sent("/usr/bin/xdg-open", "https://example.com/a", true);
    assert_eq!(
        state.status_message.as_deref(),
        Some("Sent to xdg-open on this host: https://example.com/a")
    );
}

#[test]
fn copy_link_too_adds_the_copy_after_the_opener_report() {
    let url = "https://example.com/a";
    let mut state = AppState::new(single_item_document(item_with_links(1, &[url])));
    state.copy_route = crate::model::CopyRoute::Osc52Only;

    state.report_link_sent("xdg-open", url, true);
    state.copy_link_too(url);
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some(
            "Sent to xdg-open on this host: https://example.com/a · link also sent to clipboard (OSC 52)"
        )
    );
    assert!(!state.status_is_error);

    state.set_error("Couldn't open link (xdg-open: not found)".to_string());
    state.copy_link_too(url);
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("Couldn't open link (xdg-open: not found) · link also sent to clipboard (OSC 52)")
    );
    assert!(state.status_is_error);

    state.clear_status();
    state.copy_link_too("https://example.com/\u{7}");
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("link not copied: it contains hidden control characters")
    );
    assert!(state.status_is_error);
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
    state.settle();
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
    state.settle();
    assert_eq!(
        state.status_message.as_deref(),
        Some("No browser here — link sent to clipboard (OSC 52)")
    );

    state.copy_link_instead("https://example.com/\u{7}");
    state.settle();
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("hidden control characters"))
    );

    let huge = format!("https://example.com/{}", "a".repeat(200_000));
    state.copy_link_instead(&huge);
    state.settle();
    assert!(state.status_is_error);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.contains("too large"))
    );
}

#[test]
fn o_follows_an_anchor_link_and_ctrl_o_comes_back() {
    let path = write_real_file(
        "## Run\n\n- [ ] if it fails, see [Rollback](#rollback)\n- [ ] next\n\n## Rollback\n\n- [ ] undo it\n",
    );
    let mut state = AppState::new(parser::parse_document(path).unwrap());
    state.handle_key(KeyCode::Char('o'));
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(
        state.status_message.as_deref(),
        Some("Jumped to #rollback · Ctrl-O goes back")
    );
    assert!(
        state.link_open_request().is_none(),
        "nothing is handed to a browser"
    );

    state.handle_key_with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!((state.current_list_index, state.current_item_index), (0, 0));
    assert_eq!(state.status_message.as_deref(), Some("Jumped back"));

    state.handle_key_with_mods(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|m| m.starts_with("Nothing to go back to"))
    );
}

#[test]
fn an_anchor_link_to_no_heading_says_so() {
    let path = write_real_file("## Run\n\n- [ ] see [there](#nowhere)\n");
    let mut state = AppState::new(parser::parse_document(path).unwrap());
    state.handle_key(KeyCode::Char('o'));
    assert_eq!(
        state.status_message.as_deref(),
        Some("No section named #nowhere in this file")
    );
}

#[test]
fn open_at_moves_to_a_line_or_an_anchor_or_says_why_not() {
    use crate::model::StartAt;
    let path = write_real_file("## A\n\n- [ ] one\n- [ ] two\n\n## B\n\n- [ ] three\n");
    let mut state = AppState::new(parser::parse_document(path).unwrap());
    assert_eq!(
        state.open_at(&StartAt::Line(4)),
        Ok("Opened at line 4".to_string())
    );
    assert_eq!((state.current_list_index, state.current_item_index), (0, 1));
    assert_eq!(
        state.open_at(&StartAt::Anchor("b".to_string())),
        Ok("Opened at #b".to_string())
    );
    assert_eq!((state.current_list_index, state.current_item_index), (1, 0));
    assert_eq!(
        state.open_at(&StartAt::Anchor("c".to_string())),
        Err("no list or task at #c, so opened at the start".to_string())
    );
}
