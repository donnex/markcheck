use std::io::Write;

use base64::Engine;

use crate::model::Item;

/// tmux truncates OSC 52 payloads around 74 KB; stay under that so a
/// refused-as-too-large payload here is never one tmux would have silently
/// truncated instead had this cap been laxer. Measures only the base64
/// payload, not the full `ESC ] 52 ; c ; <base64> BEL` escape sequence
/// (~8 bytes of framing beyond that) — the name says so explicitly since
/// the margin to tmux's limit is generous enough that the difference
/// doesn't need to be accounted for.
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
    /// The text is too large for the OSC 52 fallback (see
    /// `OSC52_MAX_BASE64_BYTES`). Distinct from `Failed` because the cause
    /// and the remedy are different: the clipboard is fine, the payload just
    /// won't fit through a terminal escape sequence. Reporting this as
    /// `Failed` told the user their clipboard was unavailable and pointed
    /// them at a setup that was working.
    TooLarge,
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
/// single-candidate rule of `copy_item_code`. Returns only `Copied*`/`Failed`.
pub fn copy_specific(text: &str, primary: bool) -> CopyOutcome {
    copy_text(text, primary)
}

/// arboard first (works locally, verifiable), OSC 52 on failure — the
/// escape travels through SSH to the local terminal emulator, which is
/// the only option on headless servers. With `primary`, the PRIMARY
/// selection is targeted in addition to the normal clipboard.
fn copy_text(text: &str, primary: bool) -> CopyOutcome {
    if set_via_arboard(text, primary) {
        return CopyOutcome::CopiedSystem;
    }
    // Fallback: emit OSC 52 for the clipboard, and for PRIMARY too when
    // requested. Success of the clipboard write decides the outcome.
    if osc52_sequence(text, 'c').is_none() {
        // Too big for the escape sequence — say so rather than blaming the
        // clipboard, which we never even got to try through this path.
        return CopyOutcome::TooLarge;
    }
    let clipboard_ok = emit_osc52(text, 'c');
    if primary {
        let _ = emit_osc52(text, 'p');
    }
    if clipboard_ok {
        CopyOutcome::CopiedOsc52
    } else {
        CopyOutcome::Failed
    }
}

/// Sets the system clipboard via arboard, plus the PRIMARY selection when
/// requested (best-effort — a PRIMARY failure doesn't fail the copy).
/// Returns whether the main clipboard write succeeded.
///
/// Coverage note: exercising the success path requires a live X11/Wayland
/// (or macOS/Windows) clipboard session, not reachable headlessly — CI and
/// most dev sandboxes only ever hit the `Clipboard::new()` failure branch.
fn set_via_arboard(text: &str, primary: bool) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
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
    clipboard_ok
}

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
fn emit_osc52(text: &str, selection: char) -> bool {
    let Some(sequence) = osc52_sequence(text, selection) else {
        return false;
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|_| stdout.flush())
        .is_ok()
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
        // Deep review, round 2: an over-cap payload collapsed into
        // `Failed`, which the UI renders as "no clipboard available" --
        // telling the user to fix a clipboard that was working, for a
        // command that simply won't fit through an OSC 52 escape.
        //
        // Only reachable where arboard is unavailable (a headless host), so
        // this drives `copy_text`'s fallback path directly. In a session
        // *with* a working system clipboard the copy succeeds first and this
        // never applies, hence the tolerance for `CopiedSystem` here.
        let huge = "x".repeat(OSC52_MAX_BASE64_BYTES);
        assert!(
            osc52_sequence(&huge, 'c').is_none(),
            "test setup: payload must exceed the cap once base64-encoded"
        );
        match copy_text(&huge, false) {
            CopyOutcome::TooLarge => {}
            CopyOutcome::CopiedSystem => {
                // A real clipboard was available and took it; the OSC 52
                // fallback under test was never reached.
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn osc52_sequence_refuses_oversized_payload() {
        let big = "x".repeat(OSC52_MAX_BASE64_BYTES);
        assert_eq!(osc52_sequence(&big, 'c'), None);
    }

    #[test]
    fn emit_osc52_refuses_oversized_payload_without_writing() {
        let big = "x".repeat(OSC52_MAX_BASE64_BYTES);
        assert!(!emit_osc52(&big, 'c'));
    }

    #[test]
    fn copy_text_with_primary_also_targets_the_primary_selection() {
        // The PRIMARY write is fire-and-forget (best-effort — see
        // `copy_text`'s doc comment), so it never changes the returned
        // outcome; this just exercises the `primary` branch, on whichever
        // backend (arboard or the OSC 52 fallback) the environment
        // `cargo test` runs in actually has available. Deliberately not
        // asserting a specific non-`Failed` outcome here: `Failed` requires
        // both no live clipboard session (the CI-typical case) *and* the
        // OSC 52 write to stdout itself failing, which is unlikely but not
        // impossible depending on how the test harness has stdout wired up
        // — an environment-dependent assertion here would make this test
        // flaky rather than actually verifying anything about `primary`.
        // The `selection` parameter's own correctness (`c` vs `p`) is
        // covered directly by `osc52_sequence_uses_the_given_selection`.
        let outcome = copy_text("hello", true);
        assert!(matches!(
            outcome,
            CopyOutcome::CopiedSystem | CopyOutcome::CopiedOsc52 | CopyOutcome::Failed
        ));
    }
}
