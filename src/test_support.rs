//! Shared `#[cfg(test)]`-only helpers, used across this crate's unit test
//! modules so each doesn't hand-roll its own unique-temp-path counter.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::git_test_support::{NO_GLOBAL_GIT_CONFIG, ambient_git_vars};

pub use crate::temp_test_support::unique_temp_path;

thread_local! {
    static GIT_HOME: PathBuf = {
        let home = unique_temp_path("git", "home", None);
        fs::create_dir_all(&home).expect("create an empty HOME for git");
        home
    };
}

/// Cuts a git command off from the machine running the tests: no ambient
/// variable pointing it at another repository (a hook running the suite
/// exports `GIT_DIR`/`GIT_INDEX_FILE`) or injecting config, no global or
/// system config (signing, `core.hooksPath`, templates), and an empty `HOME`
/// and `XDG_CONFIG_HOME` so no global ignore or attributes file applies.
pub fn isolate_git(cmd: &mut Command) {
    for var in ambient_git_vars() {
        cmd.env_remove(var);
    }
    cmd.envs(NO_GLOBAL_GIT_CONFIG);
    GIT_HOME.with(|home| {
        cmd.env("HOME", home).env("XDG_CONFIG_HOME", home);
    });
}

/// A bare `git` command for a test fixture, isolated by [`isolate_git`].
pub fn git() -> Command {
    let mut cmd = Command::new("git");
    isolate_git(&mut cmd);
    cmd
}

/// The view and overlay together, for asserting that a test landed on a view
/// with no overlay open.
pub fn shown(
    state: &crate::model::AppState,
) -> (crate::model::View, Option<crate::model::Overlay>) {
    (state.view, state.overlay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_git_ignores_the_ambient_repository_and_global_config() {
        let mut cmd = git();
        let envs: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.is_some()))
            .collect();
        for var in ambient_git_vars() {
            assert!(envs.contains(&(var.to_string(), false)), "{var} kept");
        }
        for var in [
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "HOME",
            "XDG_CONFIG_HOME",
        ] {
            assert!(envs.contains(&(var.to_string(), true)), "{var} not set");
        }
        let home = GIT_HOME.with(Clone::clone);
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0, "HOME not empty");

        let global = cmd.args(["config", "--global", "--list"]).output().unwrap();
        assert!(
            global.stdout.is_empty(),
            "global config leaked in: {}",
            String::from_utf8_lossy(&global.stdout)
        );
    }

    #[test]
    fn temp_paths_are_removed_when_the_test_thread_ends() {
        let (file, dir) = std::thread::spawn(|| {
            let file = unique_temp_path("support", "file", Some("md"));
            fs::write(&file, "x").unwrap();
            let dir = unique_temp_path("support", "dir", None);
            fs::create_dir_all(dir.join("nested")).unwrap();
            fs::write(dir.join("nested").join("f"), "x").unwrap();
            (file, dir)
        })
        .join()
        .unwrap();
        assert!(!file.exists(), "file left behind: {file:?}");
        assert!(!dir.exists(), "directory left behind: {dir:?}");
    }

    #[test]
    fn temp_paths_are_removed_even_when_the_test_panics() {
        let path = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::new()));
        let seen = path.clone();
        let result = std::thread::spawn(move || {
            let file = unique_temp_path("support", "panic", None);
            fs::write(&file, "x").unwrap();
            *seen.lock().unwrap() = file;
            panic!("simulated test failure");
        })
        .join();
        assert!(result.is_err());
        let path = path.lock().unwrap().clone();
        assert!(!path.exists(), "left behind after a panic: {path:?}");
    }
}

/// An [`crate::effect::Host`] that records what it was asked to do instead
/// of touching the terminal, a process or git.
#[derive(Debug, Default)]
pub(crate) struct RecordingHost {
    pub(crate) editor_lines: Vec<Option<crate::model::LineNumber>>,
    pub(crate) suspends: usize,
    pub(crate) redraws: usize,
    pub(crate) links: Vec<String>,
    pub(crate) syncs: Vec<crate::model::PendingSync>,
    pub(crate) cancelled_syncs: usize,
    /// What `cancel_git_sync` answers: whether a waiting request existed.
    pub(crate) sync_waiting: bool,
}

impl crate::effect::Host for RecordingHost {
    fn open_editor(
        &mut self,
        _state: &mut crate::model::AppState,
        line: Option<crate::model::LineNumber>,
    ) -> anyhow::Result<()> {
        self.editor_lines.push(line);
        Ok(())
    }

    fn suspend(&mut self, _state: &mut crate::model::AppState) -> anyhow::Result<()> {
        self.suspends += 1;
        Ok(())
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        self.redraws += 1;
        Ok(())
    }

    fn open_link(&mut self, _state: &mut crate::model::AppState, url: &str) {
        self.links.push(url.to_string());
    }

    fn git_sync(&mut self, sync: crate::model::PendingSync) {
        self.syncs.push(sync);
    }

    fn cancel_git_sync(&mut self) -> bool {
        self.cancelled_syncs += 1;
        std::mem::take(&mut self.sync_waiting)
    }
}

/// Random Markdown for the parser and write-back property tests (#79).
pub mod markdown {
    use proptest::prelude::*;

    /// One source line, without its ending.
    fn line() -> impl Strategy<Value = String> {
        let word = prop::sample::select(vec![
            "step",
            "`cmd --flag`",
            "**Bold**",
            "[link](https://example.com)",
            "x",
            "naïve",
            "",
        ]);
        let marker = prop::sample::select(vec!["[ ]", "[x]", "[X]", "[/]", "[-]"]);
        let bullet = prop::sample::select(vec!["- ", "* ", "+ ", "1. ", "2) "]);
        let indent = prop::sample::select(vec!["", "  ", "    ", "\t"]);
        prop_oneof![
            3 => (indent.clone(), bullet.clone(), marker, word.clone())
                .prop_map(|(i, b, m, w)| format!("{i}{b}{m} {w}")),
            1 => (indent.clone(), bullet, word.clone()).prop_map(|(i, b, w)| format!("{i}{b}{w}")),
            1 => prop::sample::select(vec!["# Title", "## List", "### Sub", "#### Deeper"])
                .prop_map(str::to_string),
            1 => prop::sample::select(vec![
                "```", "```sh", "~~~", "    indented code", "> - [ ] quoted", "> note",
                "<div>", "</div>", "---", "", "plain text", "  continued",
            ])
            .prop_map(str::to_string),
            1 => (indent, word).prop_map(|(i, w)| format!("{i}{w}")),
        ]
    }

    /// A whole document: lines joined by one kind of line ending, an optional
    /// BOM, and an optional final newline.
    pub(crate) fn document() -> impl Strategy<Value = String> {
        let ending = prop::sample::select(vec!["\n", "\r\n", "\r"]);
        (
            prop::collection::vec(line(), 0..24),
            ending,
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(lines, ending, bom, final_newline)| {
                let mut text = String::new();
                if bom {
                    text.push('\u{feff}');
                }
                text.push_str(&lines.join(ending));
                if final_newline && !lines.is_empty() {
                    text.push_str(ending);
                }
                text
            })
    }
}
