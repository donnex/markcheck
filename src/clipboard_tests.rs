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
    assert_eq!(copy_item_code(&item, false), CopyOutcome::NoCandidates);
}

#[test]
fn multiple_spans_without_block_are_ambiguous() {
    let item = item_with_code(vec!["one", "two"], vec![]);
    assert_eq!(copy_item_code(&item, false), CopyOutcome::Ambiguous(2));
}

#[test]
fn multiple_blocks_are_ambiguous_regardless_of_spans() {
    let item = item_with_code(vec!["span"], vec!["block-a", "block-b"]);
    assert_eq!(copy_item_code(&item, false), CopyOutcome::Ambiguous(2));
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
    assert_eq!(copy_text(&huge, false), CopyOutcome::TooLarge);
    assert!(fake::with(|c| c.osc52.is_empty()), "nothing is emitted");
}

#[test]
fn the_system_clipboard_is_used_when_available() {
    fake::with(|c| c.system_available = true);
    assert_eq!(copy_text("hello", false), CopyOutcome::CopiedSystem);
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), false)]);
        assert!(c.osc52.is_empty(), "no OSC 52 fallback after success");
    });
}

#[test]
fn osc52_is_the_fallback_without_a_system_clipboard() {
    assert_eq!(copy_text("hello", false), CopyOutcome::CopiedOsc52);
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
        copy_text_with("hello", false, |_, _| false, &mut Broken),
        CopyOutcome::Failed
    );
}

#[test]
fn copy_text_with_primary_also_targets_the_primary_selection() {
    fake::with(|c| c.system_available = true);
    assert_eq!(copy_text("hello", true), CopyOutcome::CopiedSystem);
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), true)]);
        c.system_available = false;
        c.system_attempts.clear();
    });

    assert_eq!(copy_text("hello", true), CopyOutcome::CopiedOsc52);
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), true)]);
        assert_eq!(c.osc52, b"\x1b]52;c;aGVsbG8=\x07\x1b]52;p;aGVsbG8=\x07");
    });
}

#[test]
fn without_primary_only_the_clipboard_selection_is_targeted() {
    assert_eq!(copy_text("hello", false), CopyOutcome::CopiedOsc52);
    fake::with(|c| {
        assert_eq!(c.system_attempts, vec![("hello".to_string(), false)]);
        assert_eq!(c.osc52, b"\x1b]52;c;aGVsbG8=\x07");
    });
}

#[test]
fn releasing_the_system_clipboard_leaves_copying_working() {
    release_system_clipboard();
    assert_eq!(copy_text("hello", false), CopyOutcome::CopiedOsc52);
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
fn copy_item_code_refuses_a_command_with_hidden_characters() {
    let item = item_with_code(vec![], vec!["ls\x1b[201~\nrm -rf ~"]);
    assert_eq!(copy_item_code(&item, false), CopyOutcome::HiddenCharacters);
    let item = item_with_code(vec!["echo ok\rrm -rf ~"], vec![]);
    assert_eq!(copy_item_code(&item, true), CopyOutcome::HiddenCharacters);
}

#[test]
fn copy_specific_refuses_a_command_with_hidden_characters() {
    assert_eq!(
        copy_specific("safe\x08\x08\x08\x08evil", false),
        CopyOutcome::HiddenCharacters
    );
}
