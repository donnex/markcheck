//! A per-thread stand-in for the system clipboard and the terminal, so
//! tests can see what was copied where without side effects. Tests never
//! reach arboard or stdout; every copy goes through here instead.

use std::cell::RefCell;

use super::CopyOutcome;
use crate::model::CopyRoute;

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

pub(super) fn copy(text: &str, primary: bool, route: CopyRoute) -> CopyOutcome {
    with(|c| {
        let available = c.system_available;
        let mut system_attempts = std::mem::take(&mut c.system_attempts);
        let outcome = super::copy_text_with(
            text,
            primary,
            route,
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

/// Tests never create an arboard instance, so there is nothing to release.
pub fn release_system_clipboard() {}
