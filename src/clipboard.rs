use std::io::Write;

use base64::Engine;

use crate::model::Item;

/// tmux truncates OSC 52 payloads around 74 KB. Measures the base64 payload
/// only.
const OSC52_MAX_BASE64_BYTES: usize = 70_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// arboard succeeded — verifiable, definitely on the clipboard.
    CopiedSystem,
    /// arboard failed; an OSC 52 escape was emitted. Fire-and-forget:
    /// whether it lands depends on the terminal (and tmux config).
    CopiedOsc52,
    /// The item has no fenced blocks and no inline code spans.
    NoCandidates,
    /// More than one candidate; nothing was copied.
    Ambiguous(usize),
    /// No clipboard backend usable at all.
    Failed,
    /// Too large for OSC 52. Distinct from `Failed`: the clipboard is fine.
    TooLarge,
    /// The text contains characters the card never shows (see
    /// `has_hidden_characters`); nothing was copied.
    HiddenCharacters,
}

/// Control characters other than `\n`/`\t` are dropped when drawn and bidi
/// controls reorder text, so the paste would differ from what was read; an
/// embedded `ESC [201~` can end a bracketed paste early.
fn has_hidden_characters(text: &str) -> bool {
    text.chars().any(|c| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(
                c,
                '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
    })
}

/// Fenced code blocks take priority; inline spans are the fallback.
fn candidates(item: &Item) -> &[String] {
    if !item.code_blocks.is_empty() {
        &item.code_blocks
    } else {
        &item.code_spans
    }
}

/// Copies the item's single code candidate, or refuses: exactly one
/// candidate is required — zero or several produce no copy and a
/// distinct outcome so the UI can explain why. When `primary` is set the
/// copy also targets the X11 PRIMARY selection.
pub fn copy_item_code(item: &Item, primary: bool) -> CopyOutcome {
    let candidates = candidates(item);
    match candidates.len() {
        0 => CopyOutcome::NoCandidates,
        1 => copy_text(&candidates[0], primary),
        n => CopyOutcome::Ambiguous(n),
    }
}

/// Copies an exact string — a specific clicked code row — bypassing the
/// single-candidate rule of `copy_item_code`. Never returns `NoCandidates`
/// or `Ambiguous`.
pub fn copy_specific(text: &str, primary: bool) -> CopyOutcome {
    copy_text(text, primary)
}

/// arboard first (works locally, verifiable), OSC 52 on failure — the
/// escape travels through SSH to the local terminal emulator, which is
/// the only option on headless servers. With `primary`, the PRIMARY
/// selection is targeted in addition to the normal clipboard.
#[cfg(not(test))]
fn copy_text(text: &str, primary: bool) -> CopyOutcome {
    copy_text_with(text, primary, set_via_arboard, &mut std::io::stdout())
}

/// Tests never reach arboard or stdout; they go through `fake` instead.
#[cfg(test)]
fn copy_text(text: &str, primary: bool) -> CopyOutcome {
    fake::copy(text, primary)
}

fn copy_text_with(
    text: &str,
    primary: bool,
    set_system: impl FnOnce(&str, bool) -> bool,
    osc52_out: &mut impl Write,
) -> CopyOutcome {
    if has_hidden_characters(text) {
        return CopyOutcome::HiddenCharacters;
    }
    if set_system(text, primary) {
        return CopyOutcome::CopiedSystem;
    }
    // Fallback: emit OSC 52 for the clipboard, and for PRIMARY too when
    // requested. Success of the clipboard write decides the outcome.
    if osc52_sequence(text, 'c').is_none() {
        // Too big for the escape sequence — say so rather than blaming the
        // clipboard, which we never even got to try through this path.
        return CopyOutcome::TooLarge;
    }
    let clipboard_ok = emit_osc52(osc52_out, text, 'c');
    if primary {
        let _ = emit_osc52(osc52_out, text, 'p');
    }
    if clipboard_ok {
        CopyOutcome::CopiedOsc52
    } else {
        CopyOutcome::Failed
    }
}

/// The session's one arboard instance. On X11 the owning instance serves the
/// clipboard to whoever pastes, and dropping the last one takes the contents
/// with it unless a clipboard manager picks them up, so it must outlive each
/// copy rather than being created and dropped per copy.
#[cfg(not(test))]
static SYSTEM_CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

/// Sets the clipboard, plus PRIMARY when requested (best-effort). Returns
/// whether the main write succeeded. A failed write discards the instance
/// so the next copy starts from a fresh connection.
///
/// Not compiled into tests, which must not touch the developer's real
/// clipboard; it needs a live clipboard session anyway.
#[cfg(not(test))]
fn set_via_arboard(text: &str, primary: bool) -> bool {
    let mut slot = SYSTEM_CLIPBOARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = arboard::Clipboard::new().ok();
    }
    let Some(clipboard) = slot.as_mut() else {
        return false;
    };
    let clipboard_ok = clipboard.set_text(text.to_string()).is_ok();
    #[cfg(target_os = "linux")]
    if primary {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let _ = clipboard
            .set()
            .clipboard(LinuxClipboardKind::Primary)
            .text(text.to_string());
    }
    #[cfg(not(target_os = "linux"))]
    let _ = primary; // PRIMARY selection is X11/Wayland-only
    if !clipboard_ok {
        *slot = None;
    }
    clipboard_ok
}

/// Drops the session's arboard instance, which on X11 hands the last copy
/// to a running clipboard manager. Call once, on quit.
#[cfg(not(test))]
pub fn release_system_clipboard() {
    let clipboard = SYSTEM_CLIPBOARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(clipboard);
}

/// Tests never create an arboard instance, so there is nothing to release.
#[cfg(test)]
pub fn release_system_clipboard() {}

/// Pure, unit-testable sequence builder for an OSC 52 write to the given
/// selection (`c` = clipboard, `p` = primary). `None` if the payload
/// exceeds the size cap.
fn osc52_sequence(text: &str, selection: char) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    (encoded.len() <= OSC52_MAX_BASE64_BYTES).then(|| format!("\x1b]52;{selection};{encoded}\x07"))
}

/// Writing directly to stdout is safe here: it happens synchronously
/// between ratatui draws and produces no visible output. Bare OSC 52 is
/// emitted (no tmux passthrough wrapping) — tmux needs `set-clipboard on`.
fn emit_osc52(out: &mut impl Write, text: &str, selection: char) -> bool {
    let Some(sequence) = osc52_sequence(text, selection) else {
        return false;
    };
    out.write_all(sequence.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

/// A per-thread stand-in for the system clipboard and the terminal, so
/// tests can see what was copied where without side effects.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::CopyOutcome;

    #[derive(Default)]
    pub(crate) struct Clipboard {
        /// Whether the system clipboard accepts writes; off by default,
        /// as on a headless machine.
        pub(crate) system_available: bool,
        /// Every attempted system clipboard write, with its `primary` flag.
        pub(crate) system_attempts: Vec<(String, bool)>,
        /// Everything written to the terminal as OSC 52.
        pub(crate) osc52: Vec<u8>,
    }

    thread_local! {
        static CLIPBOARD: RefCell<Clipboard> = RefCell::default();
    }

    pub(crate) fn with<R>(f: impl FnOnce(&mut Clipboard) -> R) -> R {
        CLIPBOARD.with(|c| f(&mut c.borrow_mut()))
    }

    pub(super) fn copy(text: &str, primary: bool) -> CopyOutcome {
        with(|c| {
            let available = c.system_available;
            let mut system_attempts = std::mem::take(&mut c.system_attempts);
            let outcome = super::copy_text_with(
                text,
                primary,
                |t, p| {
                    system_attempts.push((t.to_string(), p));
                    available
                },
                &mut c.osc52,
            );
            c.system_attempts = system_attempts;
            outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKind, TaskState};

    fn item_with_code(spans: Vec<&str>, blocks: Vec<&str>) -> Item {
        Item {
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
}
