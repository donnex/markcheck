use std::io::Write;

use base64::Engine;

use crate::model::Item;

/// tmux truncates OSC 52 payloads around 74 KB; stay conservative.
const OSC52_MAX_BYTES: usize = 100_000;

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
    (encoded.len() <= OSC52_MAX_BYTES).then(|| format!("\x1b]52;{selection};{encoded}\x07"))
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
    fn osc52_sequence_refuses_oversized_payload() {
        let big = "x".repeat(OSC52_MAX_BYTES);
        assert_eq!(osc52_sequence(&big, 'c'), None);
    }
}
