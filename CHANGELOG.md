# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Git-sync (`--git-sync`) now refuses to sync (rather than pushing
  everything) if the branch already has local commits unrelated to the
  checklist that haven't reached the remote yet — `git push` always sends
  the whole branch, so without this a checklist toggle could publish
  unrelated work you weren't ready to push. Push the unrelated commits
  yourself first; the next toggle syncs normally. Several of markcheck's
  *own* commits accumulating while offline is unaffected.
- The OSC 52 clipboard fallback's status message now says `Sent to
  clipboard (OSC 52)` instead of `Copied to clipboard (OSC 52)` — writing
  the escape sequence out doesn't confirm the terminal or multiplexer
  actually applied it, unlike a direct system-clipboard copy, so the wording
  no longer implies a guarantee it can't back up.

### Fixed

- Git-sync (`--git-sync`) no longer risks getting permanently stuck if a
  `git` command hangs (a broken SSH connection, a stuck credential helper, a
  commit hook that never returns). Every git-sync operation is now bounded
  by a timeout — generous for the local steps, longer for the network-bound
  push — after which it's killed and reported as a normal sync failure
  instead of silently blocking every future sync attempt.
- Git-sync (`--git-sync`) now detects and undoes a commit that a `pre-commit`
  hook expanded beyond the checklist file (e.g. a hook that runs
  `git add -A` or stages formatter output), instead of committing — and
  pushing — whatever the hook added alongside your change.
- Git-sync (`--git-sync`)'s automatic push retry no longer risks silently
  reverting a newer commit — it used to replay the original file content
  through the same commit-or-skip logic a fresh edit uses, which could
  build a brand new commit from that stale content if something else had
  committed to the repository in the meantime. A retry now targets the
  specific commit it's trying to push and gives up cleanly (instead of
  recommitting) if that commit is no longer current.
- Git-sync (`--git-sync`) no longer leaves a commit stranded local-only
  forever if its push fails (offline, expired auth, etc.) and you don't
  happen to make another checklist edit afterward. A failed push is now
  retried automatically every 30 seconds, retried immediately the next time
  anything else triggers a sync, and given one more chance right at
  startup — and the status message now makes clear the commit is safe and
  a retry is coming, rather than reading as a flat, unexplained failure.
- Reloading a file after an editor or another process leaves it briefly
  unreadable mid-save no longer risks a later save silently overwriting
  that content instead of detecting the conflict — a failed reload no
  longer advances the fingerprint markcheck uses to notice the file
  changed underneath it.
- Git-sync (`--git-sync`) no longer risks committing files you'd separately
  `git add`ed but hadn't committed yet — it used to build its commit from
  the repository's real index, which could silently sweep up anything else
  staged there under a commit message describing only the checklist change.
  It now uses a private, temporary index that never touches yours.
- Git-sync now refuses to run — instead of committing — while the
  repository is mid-merge, mid-rebase, mid-cherry-pick, on a detached
  `HEAD`, or has any unresolved conflict anywhere in it. Previously, an
  automatic sync during an unresolved merge conflict on the checklist file
  could silently "resolve" it and advance past the merge, leaving the
  repository in a confusing half-finished state.
- Git-sync commits now run the repository's normal commit hooks (and honor
  `commit.gpgsign`) again, the same as committing by hand — a prior
  optimization bypassed them entirely.
- Reloading a file with two `## H2` lists sharing the same title (nothing
  in Markdown forbids that) no longer snaps the cursor back to the first
  of them if you were positioned on a later one.
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
- The OSC 52 clipboard fallback's payload cap is now 70 KB instead of
  100 KB, so it can no longer report a copy as sent when tmux (which
  truncates around 74 KB) would actually have cut it short.
- A `git_sync_paths` config entry reached through a symlink (or written as a
  relative path) now correctly matches, instead of silently never
  activating git-sync for files under it.
- `--new` no longer risks leaving a partially-written checklist file behind
  if the write fails partway through — the template is written and fsynced
  to a temp file first, then placed atomically, the same crash-safety
  guarantee toggling an existing file already had.
- Git-sync (`--git-sync`)'s hook-scope check no longer risks undoing a
  concurrent commit (another markcheck instance, a human, an IDE) along
  with the one it was actually meant to roll back — the rollback is now
  guarded so it only ever undoes the exact commit git-sync itself made,
  refusing instead if the branch has moved on since.
- Git-sync (`--git-sync`) no longer risks pushing a commit that landed on
  the branch after its own commit was made and verified but before the
  push ran — it now refuses to push (retrying automatically, same as any
  other failed push) rather than sending whatever's there.
- Git-sync (`--git-sync`) no longer misreports a commit as failed if the
  subprocess timeout kills it after the commit itself already succeeded
  (e.g. a slow `post-commit` hook still running past the deadline) —
  it now checks whether the commit actually landed before deciding.
- Git-sync (`--git-sync`) no longer commits and pushes stale content if
  the file changes outside markcheck (an external edit, or a deletion)
  while a sync is queued or in flight — it now refuses (`file changed
  since this request was queued`) rather than silently publishing a
  snapshot that's since gone out of date. Toggling or editing again syncs
  the file's actual current content normally.
- Git-sync (`--git-sync`)'s push — both a fresh commit and an automatic
  retry — now targets the exact commit it verified, rather than sending
  whatever the local branch currently is. A commit landing at just the
  wrong moment (between the check and the push) can no longer ride along
  and get published as a side effect.

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
