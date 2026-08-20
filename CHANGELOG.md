# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
