# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `--nerd-font`, `--mouse`, `--no-primary` and `--no-auto-copy`, so every
  boolean setting can now be overridden for a single run in *both*
  directions, whichever way your config file sets it. Previously each
  setting had only one flag: `primary`/`auto_copy` could be turned on but
  not off, and `nerd_font`/`mouse` off but not on — so with, say,
  `nerd_font = false` in your config there was no way to get Nerd Font
  glyphs back for one run short of editing the config, despite the README
  saying a passed flag always wins. It now does.

### Changed

- Three keypresses that used to do nothing at all now say why. Pressing a
  number for a list that doesn't exist reports `No list 5: this document has
  2 lists`; `s` on a note card reports that there's nothing to start; and
  `space`/`enter` on a trailing note card — which normally pages forward,
  but has nowhere to go there — says so instead of looking wedged.
- Git-sync (`--git-sync`) now requires the branch to have an upstream set
  (`git push -u origin <branch>`, once) before it will push. Without one it
  used to fall back to a plain `git push`, which — depending on your git
  config — could quietly succeed and send the *whole* branch, unrelated
  local commits included, precisely because the "refuse to publish unrelated
  work" guard also can't do its job without an upstream to compare against.
  Your commits are still made locally either way; set the upstream once and
  they go out on the next sync.
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

- Git-sync can no longer publish an unrelated commit that lands while it is
  deciding what to push. Both push paths checked that the branch had no
  unrelated unpushed work, then resolved the branch tip again a moment later
  and pushed whatever they found — so a commit made in between (by you, an
  IDE, or another tool) could be published without ever having been checked.
  Each path now resolves the tip once and pushes that exact commit, so what
  gets published is always what was verified.
- A fenced command block containing wide characters (CJK, and other
  double-width glyphs) no longer renders with its right-hand border missing.
  The box was padded by counting characters rather than the cells they
  actually occupy, so such a row overflowed its own frame and the closing
  edge was clipped away.
- The "All Tasks Complete" screen no longer appears while a list still has
  unfinished tasks. Finishing the *last* list while an earlier one was
  untouched jumped straight to the completion summary — which then listed
  that untouched list as `0 / 1` and a total of `1 / 2` under a heading
  claiming everything was done. It now moves you to the unfinished list
  instead, and only claims completion when nothing is left anywhere.
- Opening a checklist with `--git-sync` no longer commits and pushes changes
  you hadn't committed yet. If you edited the file in an editor and left the
  change uncommitted, simply opening it in `markcheck` — no toggle, nothing
  but quitting again — committed *and* published that edit, labelled `Catch
  up a pending push`, which described nothing about it. Startup now only
  ever retries pushing a commit an earlier session left behind, and can no
  longer create one. Your uncommitted work stays yours to commit.
- Copying a very large command on a machine with no system clipboard (over
  SSH, say) now says the command is too large for the terminal clipboard,
  instead of reporting that no clipboard is available — which pointed at a
  setup that was working fine.
- Toggling a task in a file that uses classic Mac (CR-only) line endings now
  actually saves. Every task was treated as being on the same line, so the
  writes overwrote each other and nothing reached the file — while the app
  showed the task as done, so there was no sign anything had gone wrong.
- A file that mixes CRLF and LF line endings no longer has all of them
  rewritten to CRLF on the first toggle. Each line now keeps the ending it
  was written with, so a toggle changes the one checkbox and nothing else —
  which also keeps git-sync's commits down to the line that actually changed.
- Git-sync (`--git-sync`) no longer spins if the repository disappears out
  from under it — moved, deleted, or on a network share that dropped. With a
  push already waiting to be retried, it would retry many times a second,
  forever, burning CPU and flashing an error, with nothing able to stop it.
  A failed retry now waits out the normal 30-second backoff like any other,
  and picks up where it left off once the repository is back.
- Editing an open checklist down to nothing — clearing it out to start over,
  say — can no longer cost you that edit. markcheck declines to load a file
  with no tasks in it (there'd be nothing to show), but it used to record
  that file as the version it was holding, so the next toggle happily wrote
  its own stale copy straight over your rewrite, with no warning. It now
  keeps the toggle from saving and tells you why, and the notice that the
  file has no tasks stays on screen instead of fading after a few seconds.
- Git-sync (`--git-sync`) can no longer delete the branch it was meant to
  push to. If the one `git` command that reads the current commit happened
  to fail (a timeout, a wedged filesystem), the push was built with an empty
  commit id, which git reads as "delete this branch on the remote" — it
  succeeded, and markcheck reported it as a successful sync. Branches other
  than the remote's default one were at risk, which is to say any topic
  branch. Git-sync now reports a failure and retries instead, and refuses to
  push at all unless it has a real commit to name.
- Git-sync (`--git-sync`) no longer risks rewinding somebody else's commit
  off the branch when a slow commit hook runs past markcheck's timeout. It
  used to treat "the branch moved" as proof its own commit was what moved
  it — which isn't true if another process (or a hook) committed during the
  same moment — and could then undo that unrelated commit while cleaning up.
  It now confirms the commit is genuinely its own, by both content and
  ancestry, before touching anything; when it can't confirm that, the sync
  is reported as failed and simply retried, leaving history alone.
- Git-sync (`--git-sync`) no longer leaves the checklist file looking
  perpetually modified in `git status` after every toggle — it used to show
  up as both staged and not-staged changes, starting from the very first
  sync, because the commit was built without ever updating the file's own
  entry in the real index. That entry is now kept in sync with each commit.
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
- Git-sync (`--git-sync`) no longer risks publishing an unrelated local
  commit when nothing new needs to be committed for the checklist itself
  (e.g. a coalesced sync re-checking already-committed content) — the
  same "refuse rather than push unrelated work" guard now applies
  whenever git-sync is about to push, not only when it's about to create
  a new commit.
- Git-sync (`--git-sync`) no longer keeps retrying a push in the
  background forever after whatever it was trying to push has been
  superseded by a newer commit — the abandoned retry is now actually
  cleared, instead of silently re-attempting (and re-abandoning) it on
  every tick indefinitely.
- Git-sync (`--git-sync`)'s unrelated-work guard no longer misses a commit
  that touches something outside the checklist if a later commit reverts
  that exact change — it now checks each unpushed commit individually
  instead of the net difference between upstream and the branch tip,
  since a revert can make an unrelated change disappear from that net
  diff while the commit (and the one that reverted it) are still in the
  history that gets pushed. A merge commit anywhere in that range is now
  also treated as unrelated and refused, rather than left unexamined.
- Git-sync (`--git-sync`)'s per-command timeout no longer risks hanging
  indefinitely after the `git` process itself has already exited, if a
  commit hook left behind a background process still holding its
  output open — the wait for that output is now itself bounded, closing
  a gap in the timeout it was already supposed to guarantee.
- Git-sync (`--git-sync`) no longer risks building a commit that silently
  drops every other tracked file from its tree if resolving the current
  commit merely fails transiently (a wedged filesystem, a slow disk) —
  that case is no longer treated the same as a genuinely empty repository.

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
