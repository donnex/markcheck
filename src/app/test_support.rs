//! Fixtures shared by the `app` tests.

pub(super) use std::fs;
pub(super) use std::path::PathBuf;

pub(super) use crossterm::event::{KeyCode, KeyModifiers};

pub(super) use super::*;
pub(super) use crate::effect::{self, Effect};
pub(super) use crate::git_sync::SyncOutcome;
pub(super) use crate::model::*;
pub(super) use crate::parser;
pub(super) use crate::test_support::{RecordingHost, shown};

impl AppState {
    /// Treats the key as unmodified, so tests can drive input without
    /// constructing `KeyModifiers`.
    pub fn handle_key(&mut self, code: KeyCode) {
        self.handle_key_with_mods(code, KeyModifiers::NONE);
    }
}

/// Peeks at and takes the effects a test cares about, the way the main loop
/// would drain them.
impl AppState {
    pub(super) fn disk_content_diverged(&self) -> bool {
        self.content_diverged(
            crate::writer::read_checklist(&self.document.file_path)
                .ok()
                .as_deref(),
        )
    }

    pub(super) fn quit_requested(&self) -> bool {
        self.effects.contains(&Effect::Quit)
    }

    pub(super) fn editor_requested(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::OpenEditor { .. }))
    }

    pub(super) fn link_open_request(&self) -> Option<&str> {
        self.effects.iter().find_map(|e| match e {
            Effect::OpenLink(url) => Some(url.as_str()),
            _ => None,
        })
    }

    pub(super) fn git_sync_request(&self) -> Option<&PendingSync> {
        self.effects.iter().find_map(|e| match e {
            Effect::GitSync(sync) => Some(sync),
            _ => None,
        })
    }

    pub(super) fn take_editor_request(&mut self) -> bool {
        self.take_effect(|e| matches!(e, Effect::OpenEditor { .. }))
            .is_some()
    }

    pub(super) fn take_suspend_request(&mut self) -> bool {
        self.take_effect(|e| *e == Effect::Suspend).is_some()
    }

    pub(super) fn take_link_open_request(&mut self) -> Option<String> {
        match self.take_effect(|e| matches!(e, Effect::OpenLink(_)))? {
            Effect::OpenLink(url) => Some(url),
            _ => unreachable!(),
        }
    }

    pub(super) fn take_git_sync_request(&mut self) -> Option<PendingSync> {
        match self.take_effect(|e| matches!(e, Effect::GitSync(_)))? {
            Effect::GitSync(sync) => Some(sync),
            _ => unreachable!(),
        }
    }

    pub(super) fn pending_g(&self) -> bool {
        self.pending_chord == PendingChord::G
    }

    pub(super) fn pending_open_link(&self) -> bool {
        matches!(self.pending_chord, PendingChord::OpenLink { .. })
    }

    pub(super) fn pending_copy_code(&self) -> bool {
        matches!(self.pending_chord, PendingChord::CopyCode { .. })
    }

    pub(super) fn pending_number(&self) -> usize {
        match self.pending_chord {
            PendingChord::OpenLink { typed } | PendingChord::CopyCode { typed } => typed,
            PendingChord::None | PendingChord::G => 0,
        }
    }

    pub(super) fn take_effect(&mut self, wanted: impl Fn(&Effect) -> bool) -> Option<Effect> {
        let index = self.effects.iter().position(wanted)?;
        Some(self.effects.remove(index))
    }

    /// Runs the queued effects against the fake clipboard and a recording
    /// host, as the main loop's drain would.
    pub(super) fn settle(&mut self) -> RecordingHost {
        let mut host = RecordingHost::default();
        effect::drain(self, &mut host).expect("the recording host never fails");
        host
    }
}

pub(super) fn checkbox(line_number: usize, completed: bool) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: format!("task {line_number}"),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::Checkbox(if completed {
            TaskState::Done
        } else {
            TaskState::NotStarted
        }),
    }
}

pub(super) fn display_only(line_number: usize) -> Item {
    Item {
        ordinal: None,
        line_number,
        depth: 0,
        section: vec![],
        display_text: format!("heading {line_number}"),
        body: vec![],
        header: None,
        code_spans: vec![],
        code_blocks: vec![],
        kind: ItemKind::DisplayOnly,
    }
}

pub(super) fn unique_temp_path() -> PathBuf {
    crate::test_support::unique_temp_path("app", "", Some("md"))
}

/// The source lines a hand-built `lists` stands for: each item's own line
/// carries the bullet and checkbox its `kind` claims, so a write-back
/// rewrites a real marker on the right line. Lines no item claims are blank.
pub(super) fn raw_lines_for(lists: &[List]) -> Vec<String> {
    let items = || lists.iter().flat_map(|list| list.items.iter());
    let max_line = items().map(|item| item.line_number).max().unwrap_or(0);
    let mut lines = vec!["\n".to_string(); max_line];
    for item in items() {
        let marker = match item.kind {
            ItemKind::Checkbox(TaskState::NotStarted) => "[ ] ",
            ItemKind::Checkbox(TaskState::Started) => "[/] ",
            ItemKind::Checkbox(TaskState::Done) => "[x] ",
            ItemKind::Checkbox(TaskState::Skipped) => "[-] ",
            ItemKind::DisplayOnly => "",
        };
        let indent = "  ".repeat(item.depth);
        lines[item.line_number - 1] = format!("{indent}- {marker}{}\n", item.display_text);
    }
    lines
}

/// A document from hand-built `lists`, backed by a real file holding exactly
/// the Markdown those lists describe, so a write test can read the file back.
pub(super) fn document_with_lists(lists: Vec<List>) -> Document {
    let document = document_with_missing_file(lists);
    fs::write(&document.file_path, writer::document_contents(&document)).unwrap();
    document
}

/// Like `document_with_lists`, but the backing file is never created, so
/// `write_back` always fails.
pub(super) fn document_with_missing_file(lists: Vec<List>) -> Document {
    let raw_lines = raw_lines_for(&lists);
    Document {
        file_path: unique_temp_path(),
        title: None,
        has_default_list: false,
        lists,
        preambles: Default::default(),
        unshown: Default::default(),
        anchors: Vec::new(),
        critical: Default::default(),
        raw_lines,
    }
}

/// Writes `markdown` to a fresh temp file and loads it the way `main.rs`
/// does, so the items and the file always agree.
pub(super) fn state_from_markdown(markdown: &str) -> (AppState, PathBuf) {
    let path = write_real_file(markdown);
    let state = AppState::new(parser::parse_document(path.clone()).unwrap());
    (state, path)
}

pub(super) fn file_text(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap()
}

pub(super) fn two_list_document() -> Document {
    document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, false), checkbox(2, false), checkbox(3, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(4, false), checkbox(5, false)],
        },
    ])
}

/// List 1's first two items are already completed; only the last
/// item remains, so toggling it triggers a ListComplete transition.
pub(super) fn two_list_document_first_list_almost_done() -> Document {
    document_with_lists(vec![
        List {
            title: "List 1".to_string(),
            banner: None,
            items: vec![checkbox(1, true), checkbox(2, true), checkbox(3, false)],
        },
        List {
            title: "List 2".to_string(),
            banner: None,
            items: vec![checkbox(4, false), checkbox(5, false)],
        },
    ])
}

pub(super) fn text_item(line: usize, text: &str) -> Item {
    let mut item = checkbox(line, false);
    item.display_text = text.to_string();
    item
}

pub(super) fn type_str(state: &mut AppState, text: &str) {
    for c in text.chars() {
        state.handle_key(KeyCode::Char(c));
    }
}

pub(super) fn item_with_links(line: usize, urls: &[&str]) -> Item {
    let mut item = checkbox(line, false);
    item.body = urls
        .iter()
        .map(|u| crate::model::BodySpan::Link {
            text: "link".to_string(),
            url: u.to_string(),
        })
        .collect();
    item
}

pub(super) fn single_item_document(item: Item) -> Document {
    document_with_lists(vec![List {
        title: "L".to_string(),
        banner: None,
        items: vec![item],
    }])
}

pub(super) fn twelve_link_state() -> AppState {
    let urls: Vec<String> = (1..=12)
        .map(|n| format!("https://example.com/{n}"))
        .collect();
    let urls: Vec<&str> = urls.iter().map(String::as_str).collect();
    AppState::new(single_item_document(item_with_links(1, &urls)))
}

pub(super) fn press_ctrl_c(state: &mut AppState) {
    state.handle_key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL);
}

pub(super) fn assert_quit_hint(state: &AppState) {
    assert_eq!(state.status_message.as_deref(), Some("Type q to quit"));
    assert!(state.status_is_error);
    assert!(!state.quit_requested());
}

pub(super) fn all_done_document() -> Document {
    document_with_lists(vec![List {
        title: "S".to_string(),
        banner: None,
        items: vec![checkbox(1, true), checkbox(2, true)],
    }])
}

pub(super) fn write_real_file(contents: &str) -> PathBuf {
    let path = unique_temp_path();
    fs::write(&path, contents).unwrap();
    path
}

/// Forces a distinct mtime by nudging it a full second into the future,
/// avoiding flakiness on filesystems with coarse mtime resolution.
pub(super) fn touch_with_new_mtime(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let new_mtime = metadata.modified().unwrap() + std::time::Duration::from_secs(1);
    let file = fs::File::open(path).unwrap();
    file.set_modified(new_mtime).unwrap();
}

pub(super) const WRITE_PATH_MARKDOWN: &str = "\
# Title

## First

Intro paragraph with a stray [ ] that is not a task.

- [ ] **Alpha** first task
  body line with [x] in it
- [X] Bravo
- plain note
- [/] Charlie

## Second

- [ ] Delta
";

/// `WRITE_PATH_MARKDOWN` with each `(from, to)` line swapped, asserting every
/// `from` occurs exactly once so a typo can't make the expectation vacuous.
pub(super) fn markdown_with(edits: &[(&str, &str)]) -> String {
    let mut text = WRITE_PATH_MARKDOWN.to_string();
    for (from, to) in edits {
        assert_eq!(text.matches(from).count(), 1, "{from:?} must be unique");
        text = text.replace(from, to);
    }
    text
}

pub(super) fn focus_text(state: &mut AppState, text: &str) {
    let (list, item) = state
        .document
        .lists
        .iter()
        .enumerate()
        .find_map(|(l, list)| {
            list.items
                .iter()
                .position(|item| item.display_text.contains(text))
                .map(|i| (l, i))
        })
        .unwrap();
    state.current_list_index = list;
    state.current_item_index = item;
}

pub(super) const CHORD_TEST_FILE: &str =
    "## List 1\n\n- [ ] task one\n- [x] task two\n\n## List 2\n\n- [ ] task three\n";

/// The key-column words of the `?` overlay sections titled `titles`.
pub(super) fn help_words(titles: &[&str]) -> Vec<&'static str> {
    crate::ui::HELP_SECTIONS
        .iter()
        .filter(|(title, _)| titles.contains(title))
        .flat_map(|(_, rows)| rows.iter())
        .flat_map(|(key, _)| key.split_whitespace())
        .collect()
}
