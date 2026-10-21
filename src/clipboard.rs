use std::io::Write;

use base64::Engine;

use crate::model::{CopyRoute, Item};

/// tmux truncates OSC 52 payloads around 74 KB. Measures the base64 payload
/// only.
const OSC52_MAX_BASE64_BYTES: usize = 70_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// arboard succeeded — verifiable, definitely on the clipboard.
    CopiedSystem,
    /// An OSC 52 escape was emitted, without the system clipboard (it failed,
    /// or the route skips it). Fire-and-forget: whether it lands depends on
    /// the terminal (and tmux config).
    CopiedOsc52,
    /// Both the system clipboard and OSC 52 (`CopyRoute::Both`).
    CopiedBoth,
    /// `CopyRoute::Both`, but too large for OSC 52: only the system
    /// clipboard has it, which over SSH may not be the user's.
    CopiedSystemOnlyTooLarge,
    /// No clipboard backend usable at all.
    Failed,
    /// Too large for OSC 52. Distinct from `Failed`: the clipboard is fine.
    TooLarge,
    /// The text contains characters the card never shows (see
    /// `has_hidden_characters`); nothing was copied.
    HiddenCharacters,
}

/// Control characters other than `\n`/`\t` and zero-width format characters
/// are dropped when drawn and bidi controls reorder text, so the paste would
/// differ from what was read; an embedded `ESC [201~` can end a bracketed
/// paste early. A variation selector or U+034F is only refused where it has
/// no base to modify.
fn has_hidden_characters(text: &str) -> bool {
    let mut previous: Option<char> = None;
    text.chars().any(|c| {
        let hidden = c != '\n' && c != '\t' && crate::printable::is_hidden(c)
            || is_invisible_format(c)
            || is_modifier(c) && previous.is_none_or(|p| !is_base(p));
        previous = Some(c);
        hidden
    })
}

fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2060}'..='\u{2064}'
            | '\u{206A}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn is_modifier(c: char) -> bool {
    matches!(
        c,
        '\u{034F}' | '\u{180B}'..='\u{180D}' | '\u{180F}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}'
    )
}

fn is_base(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !is_invisible_format(c) && !is_modifier(c)
}

/// The commands `y` chooses between, in card order: code blocks (fenced or
/// indented) take priority; inline spans are the fallback.
pub fn candidates(item: &Item) -> &[String] {
    if !item.code_blocks.is_empty() {
        &item.code_blocks
    } else {
        &item.code_spans
    }
}

/// Copies `text` along `route`, also to the X11 PRIMARY selection when
/// `primary` is set.
pub fn copy(text: &str, primary: bool, route: CopyRoute) -> CopyOutcome {
    copy_text(text, primary, route)
}

/// arboard is verifiable and works locally; the OSC 52 escape travels
/// through SSH to the local terminal emulator, the only option on headless
/// servers. `route` says which to use (see [`CopyRoute`]).
#[cfg(not(test))]
fn copy_text(text: &str, primary: bool, route: CopyRoute) -> CopyOutcome {
    copy_text_with(
        text,
        primary,
        route,
        set_via_arboard,
        &mut std::io::stdout(),
    )
}

fn copy_text_with(
    text: &str,
    primary: bool,
    route: CopyRoute,
    set_system: impl FnOnce(&str, bool) -> bool,
    osc52_out: &mut impl Write,
) -> CopyOutcome {
    if has_hidden_characters(text) {
        return CopyOutcome::HiddenCharacters;
    }
    let system_ok = match route {
        CopyRoute::Osc52Only => false,
        _ => set_system(text, primary),
    };
    match route {
        CopyRoute::SystemOnly => {
            return if system_ok {
                CopyOutcome::CopiedSystem
            } else {
                CopyOutcome::Failed
            };
        }
        CopyRoute::SystemThenOsc52 if system_ok => return CopyOutcome::CopiedSystem,
        _ => {}
    }
    // OSC 52 for the clipboard, and for PRIMARY too when requested. Success
    // of the clipboard write decides the outcome.
    if osc52_sequence(text, 'c').is_none() {
        return if system_ok {
            CopyOutcome::CopiedSystemOnlyTooLarge
        } else {
            CopyOutcome::TooLarge
        };
    }
    let osc52_ok = emit_osc52(osc52_out, text, 'c');
    if primary {
        let _ = emit_osc52(osc52_out, text, 'p');
    }
    match (system_ok, osc52_ok) {
        (true, true) => CopyOutcome::CopiedBoth,
        (true, false) => CopyOutcome::CopiedSystem,
        (false, true) => CopyOutcome::CopiedOsc52,
        (false, false) => CopyOutcome::Failed,
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

#[cfg(test)]
#[path = "clipboard_test_support.rs"]
pub(crate) mod fake;
#[cfg(test)]
use fake::copy as copy_text;
#[cfg(test)]
pub use fake::release_system_clipboard;

#[cfg(test)]
#[path = "clipboard_tests.rs"]
mod tests;
