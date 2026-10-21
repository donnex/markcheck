//! Input dispatch: every key, mouse click, scroll and paste, routed by
//! overlay and screen, and the multi-key chords (`gg`, `o`/`y` then a number).

use std::rc::Rc;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::clipboard;
use crate::effect::Effect;
use crate::model::{
    AppState, CriticalArm, Overlay, OverviewTarget, PendingChord, ResetScope, View,
};

use super::search::delete_last_word;
use super::*;

/// What Esc and Ctrl-C say on a screen with nothing to cancel.
pub(super) const QUIT_HINT: &str = "Type q to quit";

/// What one key did at an armed `o`/`y` number prompt.
pub(super) enum NumberKey {
    Typing(usize),
    Picked(usize),
    Cancelled,
    Other,
}

/// One key at an armed `o`/`y` prompt choosing among `count` entries, with
/// `typed` digits so far. A digit picks at once unless a longer valid number
/// could still follow (`1` with twelve links might be `[12]`); then it waits
/// for another digit or `Enter`. A leading `0` picks `[0]`, which no card has.
pub(super) fn pick_number_key(code: KeyCode, typed: usize, count: usize) -> NumberKey {
    match code {
        KeyCode::Char('0') if typed == 0 => NumberKey::Picked(0),
        KeyCode::Char(c @ '0'..='9') => {
            let n = typed * 10 + usize::from(c as u8 - b'0');
            if n * 10 <= count {
                NumberKey::Typing(n)
            } else {
                NumberKey::Picked(n)
            }
        }
        KeyCode::Enter if typed > 0 => NumberKey::Picked(typed),
        KeyCode::Backspace if typed > 0 => NumberKey::Typing(typed / 10),
        KeyCode::Esc => NumberKey::Cancelled,
        _ => NumberKey::Other,
    }
}

/// A key or chord as the status bar names it, e.g. `x`, `Home`, `Ctrl-S`,
/// `Alt-1`, `Ctrl-Left`.
pub(super) fn chord_name(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut name = String::new();
    for (flag, prefix) in [
        (KeyModifiers::CONTROL, "Ctrl-"),
        (KeyModifiers::ALT, "Alt-"),
        (KeyModifiers::SUPER, "Super-"),
        (KeyModifiers::HYPER, "Hyper-"),
        (KeyModifiers::META, "Meta-"),
    ] {
        if modifiers.contains(flag) {
            name.push_str(prefix);
        }
    }
    match code {
        KeyCode::Char(c) if !name.is_empty() && c.is_ascii_alphabetic() => {
            name.push(c.to_ascii_uppercase())
        }
        other => name.push_str(&other.to_string()),
    }
    name
}

/// What the status bar says for a key or chord the screen doesn't bind. The
/// keys other to-do tools use to check a task or reach the first or last one
/// also name markcheck's own key for it.
pub(super) fn unbound_message(code: KeyCode, modifiers: KeyModifiers) -> String {
    let hint = match code {
        _ if !(modifiers - KeyModifiers::SHIFT).is_empty() => "",
        KeyCode::Char('x') => " — Space checks a task",
        KeyCode::Home => " — gg goes to the first task",
        KeyCode::End => " — G goes to the last task",
        _ => "",
    };
    format!(
        "{} is not bound{hint} (? lists every key)",
        chord_name(code, modifiers)
    )
}

impl AppState {
    /// Mouse wheel, routed per screen: on the checklist an overflowing card
    /// body takes absolute priority, otherwise the wheel steps between tasks
    /// like `k`/`j` (crossing list edges); it scrolls the `?` help overlay
    /// and moves the `T` picker highlight.
    pub fn handle_scroll_up(&mut self) {
        self.critical_armed = None;
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.layout.card_max_scroll > 0 => self.scroll_card_up(),
            (None, View::Checklist) => {
                self.navigate_backward();
            }
            (Some(Overlay::Help), _) => self.scroll_help_up_by(1),
            (Some(Overlay::ListPicker), _) => self.picker_move(-1),
            _ => {}
        }
    }

    pub fn handle_scroll_down(&mut self) {
        self.critical_armed = None;
        self.clear_pending_chords();
        match (self.overlay, self.view) {
            (None, View::Checklist) if self.layout.card_max_scroll > 0 => self.scroll_card_down(),
            (None, View::Checklist) => {
                self.navigate_forward();
            }
            (Some(Overlay::Help), _) => self.scroll_help_down_by(1),
            (Some(Overlay::ListPicker), _) => self.picker_move(1),
            _ => {}
        }
    }

    /// A bracketed paste: on `/` search and the `T` picker it is appended to
    /// the query as one line (line breaks and tabs become spaces, other control
    /// characters are dropped); everywhere else it is ignored with a sticky
    /// error, so a stray paste never runs its characters as commands.
    pub fn handle_paste(&mut self, text: &str) {
        self.critical_armed = None;
        self.clear_pending_chords();
        let line: String = text
            .trim_end()
            .chars()
            .filter_map(|c| match c {
                '\n' | '\r' | '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        match self.overlay {
            Some(Overlay::Search) => {
                self.search.query.push_str(&line);
                self.update_search();
            }
            Some(Overlay::ListPicker) => {
                self.picker.query.push_str(&line);
                self.picker.selection = 0;
            }
            _ => self.set_error("Paste ignored — markcheck takes single-key commands".to_string()),
        }
    }

    /// Left-click at `(col, row)`: a click inside the current card
    /// copies its command, exactly like `y` — copying its sole code candidate
    /// or showing the same hint when there are none or several. In the `T`
    /// picker a click selects a row (see `picker_click`). Clicks outside the
    /// card, or off the checklist, are ignored.
    pub fn handle_left_click(&mut self, col: u16, row: u16) {
        let armed = self.critical_armed;
        self.dispatch_left_click(col, row);
        self.disarm_unless_used(armed);
        self.show_write_notice();
    }

    pub(super) fn dispatch_left_click(&mut self, col: u16, row: u16) {
        self.clear_pending_chords();

        if self.overlay == Some(Overlay::ListPicker) {
            self.picker_click(col, row);
            return;
        }

        // A click on an overview row jumps the cursor there. Handled with no
        // overlay open (checklist + completion views), where the overview is a plain
        // navigation aid — not while a modal (Help/Search/picker/confirm) is up,
        // even though the overview is still drawn beside those overlays.
        let overview_target = self
            .overlay
            .is_none()
            .then(|| {
                self.layout
                    .overview_rows
                    .iter()
                    .find(|(rect, _)| rect.contains((col, row).into()))
                    .map(|(_, target)| *target)
            })
            .flatten();
        if let Some(target) = overview_target {
            match target {
                OverviewTarget::List(list_index) => self.jump_to_list(list_index),
                OverviewTarget::Item(list_index, item_index) => {
                    self.focus_item(list_index, item_index, CursorMove::Navigate)
                }
                // The state icon toggles in place, cursor unchanged.
                OverviewTarget::Toggle(list_index, item_index) => {
                    self.toggle_item(list_index, item_index)
                }
            }
            return;
        }

        // The card click-to-copy actions below only apply on the checklist
        // screen.
        if self.overlay.is_some() || self.view != View::Checklist {
            return;
        }
        // A click on a specific code row copies that exact fragment,
        // overriding the fenced-over-inline priority that governs `y`.
        if let Some(text) = self
            .layout
            .code_regions
            .iter()
            .find(|(rect, _)| rect.contains((col, row).into()))
            .map(|(_, text)| Rc::clone(text))
        {
            self.copy_specific(&text);
            return;
        }
        // Otherwise a click anywhere on the card copies its sole candidate,
        // exactly like `y`.
        if self
            .layout
            .card_rect
            .is_some_and(|r| r.contains((col, row).into()))
        {
            self.copy_current_code();
        }
    }

    /// Top-level key dispatch: modal overlays first, then card scroll, then
    /// global keys, then the view's bindings. The order matters.
    pub fn handle_key_with_mods(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if code == KeyCode::Char('l') && modifiers.contains(KeyModifiers::CONTROL) {
            self.redraw_screen();
            return;
        }
        let armed = self.critical_armed;
        if armed.is_some() && code == KeyCode::Esc {
            self.critical_armed = None;
            self.set_status("Critical step not checked".to_string());
            return;
        }
        self.dispatch_key(code, modifiers);
        self.disarm_unless_used(armed);
        self.show_write_notice();
    }

    /// Any key or click other than the confirming one leaves an armed
    /// critical task unchecked.
    pub(super) fn disarm_unless_used(&mut self, armed: Option<CriticalArm>) {
        if armed.is_some() && self.critical_armed == armed {
            self.critical_armed = None;
        }
    }

    pub(super) fn dispatch_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.clear_status_outside_search();

        // Ctrl-Z suspends from every screen, overlays and search input
        // included, as in other full-screen terminal programs.
        if code == KeyCode::Char('z') && modifiers.contains(KeyModifiers::CONTROL) {
            self.clear_pending_chords();
            self.effects.push(Effect::Suspend);
            return;
        }

        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.handle_ctrl_c();
            return;
        }

        // A Ctrl/Alt chord runs only where it is explicitly bound. Every
        // binding below matches on `KeyCode` alone, so without this Ctrl-S
        // would act as `s` and Ctrl-Y would confirm the reset like `y`.
        if !self.is_bound_chord(code, modifiers) {
            self.clear_pending_chords();
            self.keeping_status(|state| {
                state.set_error(unbound_message(code, modifiers));
            });
            return;
        }

        self.dispatch_bound_key(code, modifiers);
    }

    /// Everything [`dispatch_key`] does once a chord has passed
    /// [`is_bound_chord`].
    pub(super) fn dispatch_bound_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Any key other than the one an armed chord waits for cancels it.
        let pending = std::mem::take(&mut self.pending_chord);
        let pending_g = pending == PendingChord::G;

        if self.handle_pending_number_key(pending, code) {
            return;
        }

        // Confirmation modals are handled before the global q/Esc quit
        // check, so Esc cancels rather than quits.
        // An open overlay takes every key, before the global keys, so `Esc`
        // closes or cancels it rather than quitting, and typed text is never
        // run as commands.
        if let Some(overlay) = self.overlay {
            match overlay {
                Overlay::ConfirmReset | Overlay::ConfirmQuitReset => {
                    self.handle_confirm_key(overlay, code)
                }
                Overlay::Help => self.handle_help_key(code, modifiers),
                Overlay::Search => self.handle_search_key(code, modifiers),
                // Letters are filter input, so the selection moves on arrows /
                // Ctrl-N/Ctrl-P, not j/k.
                Overlay::ListPicker => self.handle_picker_key(code, modifiers),
            }
            return;
        }

        // Card-body viewport scroll. Only the Checklist screen has a
        // scrollable body; intercepted before the plain `e`/`y`/… bindings so
        // Ctrl-E/Ctrl-Y don't trigger "edit"/"yank".
        if self.view == View::Checklist && self.handle_checklist_scroll_key(code, modifiers) {
            return;
        }

        if self.handle_global_key(code, modifiers) {
            return;
        }

        match self.view {
            View::Checklist => self.handle_checklist_key(code, pending_g),
            View::ListComplete => self.handle_list_complete_key(code),
            View::AllComplete => self.handle_all_complete_key(code),
        }
    }

    /// Whether `code` with `modifiers` may reach the screen's bindings: plain
    /// and Shift-modified keys always may (capitals can arrive with SHIFT);
    /// a Ctrl chord only where the current screen binds it; Alt (and any
    /// other modifier) never, since nothing binds it.
    pub(super) fn is_bound_chord(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let chord = modifiers - KeyModifiers::SHIFT;
        if chord.is_empty() {
            return true;
        }
        if chord != KeyModifiers::CONTROL {
            return false;
        }
        let KeyCode::Char(c) = code else {
            return false;
        };
        let bound: &[char] = match (self.overlay, self.view) {
            (None, View::Checklist) => &['e', 'y', 'd', 'u', 'r', 'o'],
            (None, View::ListComplete | View::AllComplete) => &['r'],
            (Some(Overlay::Help), _) => &['e', 'y', 'd', 'u'],
            (Some(Overlay::ListPicker), _) => &['p', 'n', 'd', 'u', 'w'],
            (Some(Overlay::Search), _) => &['u', 'w'],
            (Some(Overlay::ConfirmReset | Overlay::ConfirmQuitReset), _) => &[],
        };
        bound.contains(&c)
    }

    /// `Ctrl-L`, as in vim and the shell: repaints the screen from every
    /// screen and overlay, leaving the status, armed chords and an armed
    /// critical step as they were.
    pub(super) fn redraw_screen(&mut self) {
        self.effects.push(Effect::Redraw);
        if self.status_message.is_none() {
            self.set_status("Screen redrawn".to_string());
        }
    }

    /// Raw mode delivers Ctrl-C as a key, not `SIGINT`. As in vim, it cancels
    /// an overlay or search input like `Esc`, and elsewhere says how to quit
    /// rather than quitting.
    pub(super) fn handle_ctrl_c(&mut self) {
        self.clear_pending_chords();
        match self.overlay {
            Some(Overlay::Search) => self.cancel_search(),
            Some(_) => self.overlay = None,
            None => self.set_error(QUIT_HINT.to_string()),
        }
    }

    /// An armed `o` waits for a link number and an armed `y` for a command
    /// number. Runs before the digit list jump and the global Esc. Any other key
    /// cancels the prompt and still takes its normal action, returning `false`.
    pub(super) fn handle_pending_number_key(
        &mut self,
        pending: PendingChord,
        code: KeyCode,
    ) -> bool {
        let (typed, count) = match pending {
            PendingChord::OpenLink { typed } => (
                typed,
                self.current_item().map_or(0, |item| item.link_urls().len()),
            ),
            PendingChord::CopyCode { typed } => (
                typed,
                self.current_item()
                    .map_or(0, |item| clipboard::candidates(item).len()),
            ),
            PendingChord::None | PendingChord::G => return false,
        };
        match pick_number_key(code, typed, count) {
            NumberKey::Typing(typed) => {
                self.pending_chord = match pending {
                    PendingChord::OpenLink { .. } => PendingChord::OpenLink { typed },
                    _ => PendingChord::CopyCode { typed },
                };
                true
            }
            NumberKey::Picked(n) => {
                if matches!(pending, PendingChord::OpenLink { .. }) {
                    self.open_link_number(n);
                } else {
                    self.copy_code_number(n);
                }
                true
            }
            NumberKey::Cancelled => true,
            NumberKey::Other => false,
        }
    }

    /// A failed reset leaves either dialog open, so its error is read first.
    pub(super) fn handle_confirm_key(&mut self, overlay: Overlay, code: KeyCode) {
        if overlay == Overlay::ConfirmReset {
            // Only `y` (everything), `l` (this list) and `s` (this sub-list)
            // confirm — `Enter` is too easy to hit by reflex for a rewrite.
            // Any other key cancels.
            let scope = match code {
                KeyCode::Char('y') => ResetScope::All,
                KeyCode::Char('l') => ResetScope::List,
                KeyCode::Char('s') if self.sub_list_root().is_some() => ResetScope::SubList,
                KeyCode::Char('s') => {
                    self.set_error("No sub-list here: y resets everything".to_string());
                    return;
                }
                _ => {
                    self.overlay = None;
                    return;
                }
            };
            if self.reset_count(scope) == 0 {
                self.set_error("Nothing to reset there: none of those tasks are done".to_string());
            } else if self.reset_scope(scope) {
                self.overlay = None;
            }
        } else {
            // ConfirmQuitReset, offered on quit when all done: y resets
            // then quits, n or q quits without resetting, Esc/anything else
            // cancels. A quit closes the dialog, so a wait for git sync is
            // shown over the checklist rather than under a stale question.
            match code {
                KeyCode::Char('y') => {
                    if self.reset_all() {
                        self.effects.push(Effect::Quit);
                        self.overlay = None;
                    }
                }
                KeyCode::Char('n' | 'q') => {
                    self.effects.push(Effect::Quit);
                    self.overlay = None;
                }
                _ => self.overlay = None,
            }
        }
    }

    /// The help overlay scrolls when it overflows a short terminal:
    /// the card scroll keys plus j/k and arrows scroll; every other
    /// key closes it.
    pub(super) fn handle_help_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll_help_down_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_help_up_by(1),
            KeyCode::Char('e') if ctrl => self.scroll_help_down_by(1),
            KeyCode::Char('y') if ctrl => self.scroll_help_up_by(1),
            KeyCode::Char('d') if ctrl => self.scroll_help_down_by(self.help_half_page()),
            KeyCode::Char('u') if ctrl => self.scroll_help_up_by(self.help_half_page()),
            KeyCode::PageDown => self.scroll_help_down_by(self.help_page()),
            KeyCode::PageUp => self.scroll_help_up_by(self.help_page()),
            _ => self.overlay = None,
        }
    }

    pub(super) fn handle_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Enter => self.keeping_status(Self::commit_search),
            KeyCode::Backspace => {
                self.search.query.pop();
                self.update_search();
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.clear();
                self.update_search();
            }
            KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                delete_last_word(&mut self.search.query);
                self.update_search();
            }
            // Plain typed text edits the query (Shift for capitals is fine);
            // Ctrl-combos (e.g. Ctrl-H) are ignored rather than inserting
            // their base letter into the query.
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.push(c);
                self.update_search();
            }
            _ => {}
        }
    }

    pub(super) fn handle_picker_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.picker_commit(),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Char('p') if ctrl => self.picker_move(-1),
            KeyCode::Char('n') if ctrl => self.picker_move(1),
            // Half-page jumps, matching the card-body Ctrl-D/Ctrl-U.
            KeyCode::Char('d') if ctrl => self.picker_move(self.picker_half_page()),
            KeyCode::Char('u') if ctrl => self.picker_move(-self.picker_half_page()),
            KeyCode::Char('w') if ctrl => {
                delete_last_word(&mut self.picker.query);
                self.picker.selection = 0;
            }
            KeyCode::Backspace => {
                self.picker.query.pop();
                self.picker.selection = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.picker.query.push(c);
                self.picker.selection = 0;
            }
            _ => {}
        }
    }

    /// Card-body viewport scroll, vim-style: Ctrl-E/Y one line, Ctrl-D/U
    /// half a page, PageDown/Up a page. Returns whether it handled the key.
    pub(super) fn handle_checklist_scroll_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('e') => {
                    self.scroll_card_down_by(1);
                    return true;
                }
                KeyCode::Char('y') => {
                    self.scroll_card_up_by(1);
                    return true;
                }
                KeyCode::Char('d') => {
                    self.scroll_card_down_by(self.half_page());
                    return true;
                }
                KeyCode::Char('u') => {
                    self.scroll_card_up_by(self.half_page());
                    return true;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::PageDown => {
                self.scroll_card_down_by(self.page());
                true
            }
            KeyCode::PageUp => {
                self.scroll_card_up_by(self.page());
                true
            }
            _ => false,
        }
    }

    /// The single-key actions available from any non-modal screen: help/task
    /// picker overlays, quit (with the all-done reset offer), editor, reset,
    /// undo/redo, the `1`-`9` list jump, and the Tab/Shift-L/Shift-H
    /// incomplete-task/list jumps. Returns whether it handled the key.
    pub(super) fn handle_global_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Open the help overlay from any non-modal screen, scrolled to
        // the top.
        if code == KeyCode::Char('?') {
            self.help.scroll = 0;
            self.overlay = Some(Overlay::Help);
            return true;
        }

        // Open the "go to task" overlay from any non-modal screen (`T`).
        if code == KeyCode::Char('T') {
            self.open_list_picker();
            return true;
        }

        if code == KeyCode::Char('q') {
            // When every task is done or skipped, offer to reset before
            // quitting instead of quitting outright.
            if self.document.progress().is_finished() {
                self.overlay = Some(Overlay::ConfirmQuitReset);
            } else {
                self.effects.push(Effect::Quit);
            }
            return true;
        }
        // As in vim, Esc cancels (search, the picker, help, dialogs and
        // prompts all take it first) and never quits; here, with nothing left
        // to cancel, it says how to quit rather than doing nothing.
        if code == KeyCode::Esc {
            self.set_error(QUIT_HINT.to_string());
            return true;
        }

        if code == KeyCode::Char('e') {
            let line = self.current_item().map(|item| item.line_number);
            self.effects.push(Effect::OpenEditor { line });
            return true;
        }
        if code == KeyCode::Char('R') {
            self.request_reset();
            return true;
        }

        // Undo / redo of state-changing actions. `u` undoes; `Ctrl-R`
        // redoes (vim-idiomatic). Guarded on the modifier so `u` never fires on
        // `Ctrl-U` (card scroll) and `R`/`Ctrl-R` stay distinct from `R` (reset).
        if code == KeyCode::Char('u') && !modifiers.contains(KeyModifiers::CONTROL) {
            self.undo();
            return true;
        }
        if code == KeyCode::Char('r') && modifiers.contains(KeyModifiers::CONTROL) {
            self.redo();
            return true;
        }
        if code == KeyCode::Char('o')
            && modifiers.contains(KeyModifiers::CONTROL)
            && self.view == View::Checklist
        {
            self.jump_back();
            return true;
        }

        if let KeyCode::Char(c @ '1'..='9') = code {
            let index = (c as u8 - b'1') as usize;
            self.jump_to_list(index);
            return true;
        }

        // Global jumps from any non-modal screen: Tab → next unfinished *task*
        // anywhere; Shift-L / Shift-H → next / prev unfinished *list*.
        if code == KeyCode::Tab {
            self.jump_to_next_incomplete_task();
            return true;
        }
        if code == KeyCode::Char('L') {
            self.jump_to_next_incomplete_list();
            return true;
        }
        if code == KeyCode::Char('H') {
            self.jump_to_prev_incomplete_list();
            return true;
        }

        false
    }

    /// All four home-row/arrow motions navigate between tasks: `h`/`l`/
    /// ←/→ read as the carousel sliding left/right, `j`/`k`/↑/↓ as walking the
    /// list-shaped overview. Card-body scrolling lives on Ctrl-E/Y/D/U and
    /// PageUp/Down (handled by `handle_checklist_scroll_key` before this).
    pub(super) fn handle_checklist_key(&mut self, code: KeyCode, pending_g: bool) {
        match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.step_backward()
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('j') | KeyCode::Down => {
                self.step_forward()
            }
            // `gg` (two presses) → first task; `G` → last task.
            KeyCode::Char('g') => {
                if pending_g {
                    self.go_to_first_item();
                } else {
                    self.pending_chord = PendingChord::G;
                }
            }
            KeyCode::Char('G') => self.go_to_last_item(),
            // `}`/`{` jump to the next/previous `### H3`+ sub-section, vim
            // paragraph-motion style.
            KeyCode::Char('}') => self.jump_sub_section(true),
            KeyCode::Char('{') => self.jump_sub_section(false),
            // `/` opens incremental search; `n`/`N` cycle its matches.
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.search_cycle(true),
            KeyCode::Char('N') => self.search_cycle(false),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_current(),
            KeyCode::Char('s') => self.start_current(),
            KeyCode::Char('-') => self.skip_current(),
            KeyCode::Char('y') => self.copy_current_code(),
            KeyCode::Char('o') => self.open_current_link(),
            _ => self.set_error(unbound_message(code, KeyModifiers::NONE)),
        }
    }

    pub(super) fn handle_list_complete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('l')
            | KeyCode::Right
            | KeyCode::Char('j')
            | KeyCode::Down
            | KeyCode::Enter => self.advance_to_next_incomplete_list(),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.review_completed_list()
            }
            _ => self
                .set_error("List complete — press l for the next list or h to review".to_string()),
        }
    }

    pub(super) fn handle_all_complete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('k') | KeyCode::Up => {
                self.go_to_last_item()
            }
            _ => self.set_error(
                "All tasks complete — press h to review, R to reset or q to quit".to_string(),
            ),
        }
    }
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
