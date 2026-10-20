//! Golden screens (#105): every `examples/` file rendered at a narrow, a
//! standard and a wide terminal, on its start card and after a few keys,
//! compared as plain text against a reviewed snapshot. Review a change with
//! `cargo insta review` (or rerun with `INSTA_UPDATE=always` and check the
//! diff). Unicode icons and the basic palette keep the output the same on
//! every machine; only symbols are compared, never styles.

use std::path::Path;

use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::model::{AppState, IconSet};

const SIZES: [(u16, u16); 3] = [(50, 24), (80, 24), (120, 40)];

fn example_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir("examples")
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".md") && name != "README.md")
        .collect();
    names.sort();
    names
}

fn screen(state: &mut AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| super::render(frame, state)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open(name: &str) -> AppState {
    let path = Path::new("examples").join(name);
    let mut state = AppState::new(crate::parser::parse_document(path).unwrap());
    state.icons = IconSet::unicode();
    state
}

#[test]
fn every_example_matches_its_golden_screens() {
    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.bind(|| {
        for name in example_names() {
            let stem = name.trim_end_matches(".md");
            for (width, height) in SIZES {
                let mut state = open(&name);
                insta::assert_snapshot!(
                    format!("{stem}-{width}x{height}-start"),
                    screen(&mut state, width, height)
                );
                state.handle_key(KeyCode::Char('j'));
                insta::assert_snapshot!(
                    format!("{stem}-{width}x{height}-next"),
                    screen(&mut state, width, height)
                );
            }
            for (key, step, width) in [('T', "picker", 80), ('?', "help", 50), ('?', "help", 80)] {
                let mut state = open(&name);
                state.handle_key(KeyCode::Char(key));
                insta::assert_snapshot!(
                    format!("{stem}-{width}x24-{step}"),
                    screen(&mut state, width, 24)
                );
            }
        }
    });
}
