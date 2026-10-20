use super::*;
use crate::model::{ItemKind, TaskState};

fn item_with_code(spans: Vec<&str>, blocks: Vec<&str>) -> Item {
    Item {
        ordinal: None,
        line_number: 1,
        depth: 0,
        section: vec![],
        display_text: "task".to_string(),
        body: vec![],
        header: None,
        code_spans: spans.into_iter().map(str::to_string).collect(),
        code_blocks: blocks.into_iter().map(str::to_string).collect(),
        kind: ItemKind::Checkbox(TaskState::NotStarted),
    }
}

#[test]
fn no_code_yields_no_candidates() {
    let item = item_with_code(vec![], vec![]);
    assert!(candidates(&item).is_empty());
}

#[test]
fn multiple_spans_without_block_are_all_candidates() {
    let item = item_with_code(vec!["one", "two"], vec![]);
    assert_eq!(candidates(&item), &["one".to_string(), "two".to_string()]);
}

#[test]
fn multiple_blocks_are_the_candidates_regardless_of_spans() {
    let item = item_with_code(vec!["span"], vec!["block-a", "block-b"]);
    assert_eq!(
        candidates(&item),
        &["block-a".to_string(), "block-b".to_string()]
    );
}

#[test]
fn single_block_wins_over_spans() {
    let item = item_with_code(vec!["span-a", "span-b"], vec!["the-block"]);
    assert_eq!(candidates(&item), &["the-block".to_string()]);
}

#[test]
fn spans_are_fallback_when_no_blocks() {
    let item = item_with_code(vec!["the-span"], vec![]);
    assert_eq!(candidates(&item), &["the-span".to_string()]);
}

#[test]
fn osc52_sequence_has_expected_format() {
    // base64("hello") == "aGVsbG8="
    assert_eq!(
        osc52_sequence("hello", 'c').unwrap(),
        "\x1b]52;c;aGVsbG8=\x07"
    );
}

#[test]
fn osc52_sequence_uses_the_given_selection() {
    assert_eq!(
        osc52_sequence("hello", 'p').unwrap(),
        "\x1b]52;p;aGVsbG8=\x07"
    );
}

#[test]
fn an_oversized_payload_is_reported_as_too_large_not_as_a_missing_clipboard() {
    let huge = "x".repeat(OSC52_MAX_BASE64_BYTES);
    assert!(
        osc52_sequence(&huge, 'c').is_none(),
        "test setup: payload must exceed the cap once base64-encoded"
    );
    assert_eq!(
        copy_text(&huge, false, CopyRoute::SystemThenOsc52),
        CopyOutcome::TooLarge
    );
    assert!(fake::with(|c| c.osc52.is_empty()), "nothing is emitted");
}

#[test]
fn the_system_clipboard_is_used_when_available() {
    fake::with(|c| c.system_available = true);
    assert_eq!(
        copy_text("hello", false, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedSystem
    );
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), false)]);
        assert!(c.osc52.is_empty(), "no OSC 52 fallback after success");
    });
}

#[test]
fn osc52_is_the_fallback_without_a_system_clipboard() {
    assert_eq!(
        copy_text("hello", false, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedOsc52
    );
    fake::with(|c| assert_eq!(c.osc52, b"\x1b]52;c;aGVsbG8=\x07"));
}

#[test]
fn osc52_sequence_refuses_oversized_payload() {
    let big = "x".repeat(OSC52_MAX_BASE64_BYTES);
    assert_eq!(osc52_sequence(&big, 'c'), None);
}

#[test]
fn emit_osc52_refuses_oversized_payload_without_writing() {
    let big = "x".repeat(OSC52_MAX_BASE64_BYTES);
    let mut out = Vec::new();
    assert!(!emit_osc52(&mut out, &big, 'c'));
    assert!(out.is_empty());
}

#[test]
fn a_failed_terminal_write_is_reported_as_failed() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        copy_text_with(
            "hello",
            false,
            CopyRoute::SystemThenOsc52,
            |_, _| false,
            &mut Broken
        ),
        CopyOutcome::Failed
    );
}

#[test]
fn copy_text_with_primary_also_targets_the_primary_selection() {
    fake::with(|c| c.system_available = true);
    assert_eq!(
        copy_text("hello", true, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedSystem
    );
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), true)]);
        c.system_available = false;
        c.system_attempts.clear();
    });

    assert_eq!(
        copy_text("hello", true, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedOsc52
    );
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), true)]);
        assert_eq!(c.osc52, b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;aGVsbG8=\x07");
    });
}

#[test]
fn without_primary_only_the_clipboard_selection_is_targeted() {
    assert_eq!(
        copy_text("hello", false, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedOsc52
    );
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), false)]);
        assert_eq!(c.osc52, b"\x1b]52;c;aGVsbG8=\x07");
    });
}

#[test]
fn releasing_the_system_clipboard_leaves_copying_working() {
    release_system_clipboard();
    assert_eq!(
        copy_text("hello", false, CopyRoute::SystemThenOsc52),
        CopyOutcome::CopiedOsc52
    );
}

#[test]
fn hidden_characters_are_detected() {
    for text in [
        "ls\x1b[201~\nrm -rf ~",
        "safe\x08\x08\x08\x08evil",
        "echo ok\rrm -rf ~",
        "echo \u{202E}txt.exe",
        "a\u{2066}b",
        "a\u{0085}b",
        "systemctl restart ng\u{200B}inx",
        "ssh example\u{2060}-host",
        "\u{FEFF}ls",
        "rm -rf /tmp/a\u{00AD}b",
        "a\u{180E}b",
        "a\u{200D}b",
        "echo a\u{E0041}\u{E007F}",
        "\u{FE0F}ls",
        "ls \u{FE0F}-l",
        "ls\u{034F}\u{034F}",
        "a\u{200B}\u{FE0F}",
        "a\n\u{E0100}",
    ] {
        assert!(has_hidden_characters(text), "{text:?}");
    }
}

#[test]
fn newlines_and_tabs_are_not_hidden_characters() {
    assert!(!has_hidden_characters("make\tbuild\nmake test"));
    assert!(!has_hidden_characters("echo héllo → 世界"));
}

#[test]
fn variation_selectors_on_a_base_are_not_hidden_characters() {
    assert!(!has_hidden_characters("echo \u{2714}\u{FE0F} done"));
    assert!(!has_hidden_characters("echo \u{8FBA}\u{E0100}"));
    assert!(!has_hidden_characters("a\u{034F}b"));
}

#[test]
fn copy_refuses_zero_width_characters() {
    assert_eq!(
        copy(
            "systemctl restart ng\u{200B}inx",
            false,
            CopyRoute::SystemThenOsc52
        ),
        CopyOutcome::HiddenCharacters
    );
}

#[test]
fn copy_refuses_escape_and_carriage_return_tricks() {
    assert_eq!(
        copy("ls\x1b[201~\nrm -rf ~", false, CopyRoute::SystemThenOsc52),
        CopyOutcome::HiddenCharacters
    );
    assert_eq!(
        copy("echo ok\rrm -rf ~", true, CopyRoute::SystemThenOsc52),
        CopyOutcome::HiddenCharacters
    );
}

#[test]
fn copy_refuses_backspace_overwrites() {
    assert_eq!(
        copy(
            "safe\x08\x08\x08\x08evil",
            false,
            CopyRoute::SystemThenOsc52
        ),
        CopyOutcome::HiddenCharacters
    );
}

fn fake_copy(text: &str, system_available: bool, route: CopyRoute) -> (CopyOutcome, usize, bool) {
    fake::with(|c| {
        c.system_available = system_available;
        c.system_attempts.clear();
        c.osc52.clear();
    });
    let outcome = copy_text(text, false, route);
    fake::with(|c| (outcome, c.system_attempts.len(), !c.osc52.is_empty()))
}

#[test]
fn each_copy_route_uses_the_clipboards_it_names() {
    use CopyRoute::*;
    // (route, system available) -> (outcome, system attempts, OSC 52 sent)
    for (route, available, expected) in [
        (SystemThenOsc52, true, (CopyOutcome::CopiedSystem, 1, false)),
        (SystemThenOsc52, false, (CopyOutcome::CopiedOsc52, 1, true)),
        (Both, true, (CopyOutcome::CopiedBoth, 1, true)),
        (Both, false, (CopyOutcome::CopiedOsc52, 1, true)),
        (SystemOnly, true, (CopyOutcome::CopiedSystem, 1, false)),
        (SystemOnly, false, (CopyOutcome::Failed, 1, false)),
        (Osc52Only, true, (CopyOutcome::CopiedOsc52, 0, true)),
    ] {
        assert_eq!(
            fake_copy("hello", available, route),
            expected,
            "{route:?} {available}"
        );
    }
}

#[test]
fn too_large_for_osc52_says_only_the_system_clipboard_took_it() {
    let huge = "x".repeat(OSC52_MAX_BASE64_BYTES);
    assert_eq!(
        fake_copy(&huge, true, CopyRoute::Both),
        (CopyOutcome::CopiedSystemOnlyTooLarge, 1, false)
    );
    assert_eq!(
        fake_copy(&huge, true, CopyRoute::SystemThenOsc52).0,
        CopyOutcome::CopiedSystem
    );
    assert_eq!(
        fake_copy(&huge, false, CopyRoute::Both).0,
        CopyOutcome::TooLarge
    );
    assert_eq!(
        fake_copy(&huge, true, CopyRoute::Osc52Only).0,
        CopyOutcome::TooLarge
    );
}
