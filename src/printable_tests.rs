use super::{is_hidden, printable, printable_line};

#[test]
fn escape_sequences_lose_their_control_characters() {
    assert_eq!(
        printable_line("Runbook\x07\x1b]52;c;ZWNobw==\x07"),
        "Runbook]52;c;ZWNobw=="
    );
    assert_eq!(printable_line("a\u{9b}31mb\u{7f}c\rd"), "a31mbcd");
    assert_eq!(printable_line("echo \u{202E}txt\u{2066}"), "echo txt");
}

#[test]
fn printable_keeps_line_breaks_and_tabs() {
    assert_eq!(
        printable("remote: one\x1b[2K\nremote:\ttwo"),
        "remote: one[2K\nremote:\ttwo"
    );
}

#[test]
fn printable_line_puts_everything_on_one_line() {
    assert_eq!(printable_line("one\ntwo\tthree"), "one two three");
}

#[test]
fn ordinary_text_is_untouched() {
    let text = "Deploy · 3/11 — markcheck: echo héllo → 世界";
    assert_eq!(printable(text), text);
    assert_eq!(printable_line(text), text);
    assert!(!is_hidden('é'));
    assert!(is_hidden('\u{0085}'));
}
