# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Toggling, starting, resetting, undoing, or redoing a task no longer leaves
  it showing the wrong state if the save to disk fails (e.g. the file
  becomes read-only, is deleted, or disk space runs out) — the change is
  rolled back in memory to match what's actually on disk, instead of the
  checklist silently drifting out of sync with the file (and potentially
  showing a false "all complete" screen once a later change is saved).
- Toggling a task in a checklist file that didn't end in a trailing newline
  no longer adds one — the very first save used to silently change a byte
  it had no business touching.
- Git-sync (`--git-sync`) commits can no longer pick up an unrelated,
  concurrent change to the file (another toggle, or an edit made in
  `$EDITOR`) under a commit message that only describes the original one —
  each commit now always matches exactly what its message says changed.
  Quitting right after an edit or toggle also now reliably waits for that
  commit to land instead of occasionally racing the app's own exit.
- A change is no longer silently overwritten if the file changed on disk
  (another `markcheck` instance, an external editor) after it was loaded but
  before the next toggle/start/reset/undo/redo tries to save — the save is
  now refused, the file reloaded with the other change intact, and a sticky
  error asks you to retry.
- A line like `` ```done `` (a fence run followed by trailing text) inside a
  code block is no longer mistaken for closing that fence. Previously, a
  `[/]`-lookalike line genuinely still inside the block, past such a line,
  could show up in the card as `[ ]` instead of the source's actual `[/]`.

## [1.2.2] - 2026-08-20

### Fixed

- A git-sync auto-commit message that gets truncated no longer ends with a
  dangling, unclosed `"` from the task title.
- The tab bar could render a title a character or two short of its allotted
  width in a narrow terminal when list titles were very unevenly sized.

## [1.2.1] - 2026-08-19

### Changed

- Git-sync auto-commit messages are truncated to 80 characters, so a long
  task title can no longer produce an unwieldy `git log` entry.
- Git-sync's status message now names the resolved editor binary (e.g.
  `vim`), not the literal `$EDITOR`/`$VISUAL` environment variable name.

## [1.2.0] - 2026-08-19

### Fixed

- A crash (`completion_card` panic) on very short terminal heights.
- `R` (reset) ignoring items in the `Started` state.
- The overview panel's sticky section header could fail to converge when
  scrolling.
- Editor edits (`e`) now sync immediately via git-sync instead of waiting
  for a later toggle to piggyback on.
- Git-sync now reports when it's skipped because the file isn't tracked by
  git, instead of failing silently.

## [1.1.0] - 2026-08-18

### Added

- `--new` flag to create and open a starter checklist.

## [1.0.0] - 2026-08-10

Initial public release: a terminal UI for working through Markdown
checklists (`- [ ]`/`- [/]`/`- [x]`) one card at a time, with progress
written straight back into the file, live reload, undo/redo, search,
clipboard copy of task commands, and optional git-sync auto-commit.
