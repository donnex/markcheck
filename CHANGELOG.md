# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A task with several commands can now be copied from the keyboard. The card
  numbers them — a dim `[1]`, `[2]` after each inline command, or in the top
  border of each fenced block — and `y` then a digit copies that command,
  just as `o` then a digit opens one of several links. Previously `y` refused
  with "item has N code candidates" and the only way to copy one was a mouse
  click, which isn't available with `--no-mouse`, on many terminal and tmux
  setups, or to anyone who doesn't use a mouse. (#116)

- `Ctrl-Z` suspends markcheck and drops you back to your shell, the way it
  does in vim, less and most other full-screen terminal programs; `fg`
  brings it back, picking up any change made to the checklist meanwhile.
  Previously the key did nothing, so running a step's command without a
  second terminal meant quitting and starting markcheck again. (#107)

- Support for the `NO_COLOR` convention (<https://no-color.org/>): when the
  `NO_COLOR` environment variable is set to any non-empty value, markcheck
  draws no color at all. Commands are shown in reverse video, and states are
  told apart by their icons and glyphs (a started item's position dot becomes
  a half dot `◐`). Previously the variable was ignored and you got the full
  color scheme regardless. (#45)

- A warning when the checklist you opened has more than one hard link.
  Saving writes a new file over the path, so the other names keep the old
  content and quietly stop being the same file — the same thing happens in
  most editors, but markcheck now tells you up front rather than letting you
  find out later. Your file is still fully editable; it is a warning, not a
  refusal.

- `--nerd-font`, `--mouse`, `--no-primary` and `--no-auto-copy`, so every
  boolean setting can now be overridden for a single run in *both*
  directions, whichever way your config file sets it. Previously each
  setting had only one flag: `primary`/`auto_copy` could be turned on but
  not off, and `nerd_font`/`mouse` off but not on — so with, say,
  `nerd_font = false` in your config there was no way to get Nerd Font
  glyphs back for one run short of editing the config, despite the README
  saying a passed flag always wins. It now does.

### Changed

- Git-sync messages read as one sentence instead of two. Every failure used
  to arrive doubly-labelled — `Git sync failed: git-sync: ...` — and the
  push failures said "committed locally" twice. The internal prefix is gone
  and the wording is plainer throughout: "a rebase is in progress" rather
  than "repository has a rebase in progress", and the messages that only
  made sense if you knew markcheck's internals now say what happened.
- The two different messages for a deleted checklist are now one, and it
  tells you the situation is recoverable: "File was deleted — changes cannot
  be saved until it exists again".

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

- Card text written in East Asian scripts (Chinese, Japanese, Korean) or
  with emoji now wraps onto further lines instead of losing its end off the
  right edge of the card. These characters take two terminal columns each,
  but were measured as one, so a line up to twice the card's width was drawn
  as if it fit and the rest could never be read, not even by scrolling. List
  tabs, the header title, breadcrumbs and section dividers had the same
  mis-measurement and could overflow or cut such text wrongly. (#12)

- `Ctrl-C` now responds instead of silently doing nothing. Outside an
  overlay it shows "Type q to quit", as vim does, rather than quitting; in
  search, the "go to task" list, the help overlay or a confirmation it
  cancels, just like `Esc`. Previously the key had no effect anywhere, so
  the reflexive way out of a terminal program made markcheck look hung. (#13)

- `markcheck --new` now works on filesystems without hard links, such as
  FAT32/exFAT USB sticks and SD cards and many network shares. Previously it
  always failed there with "cannot create …: Operation not permitted"; it
  still never overwrites a file that already exists. (#14)

- A status message too long for the bottom row now wraps onto a few more
  rows instead of being cut off at the right edge. Previously the end of the
  longest messages was never visible: the "Another markcheck is saving this
  file" refusal names a lock file to remove by hand if it persists, and that
  path could not be read on any terminal, while git sync (`--git-sync`)
  failures were often cut off right where the reason began. (#11)

- Git sync (`--git-sync`)'s message for a remote branch that has moved ahead
  now reads "... is not based on; pull first, then it will sync".
  Previously a run of 18 spaces sat in the middle of it, which on a typical
  terminal width pushed the useful half — what to do about it — off the end
  of the status bar. (#10)

- Git sync (`--git-sync`) no longer commits in the middle of a multi-commit
  cherry-pick or revert that you concluded one step of with a plain
  `git commit` rather than `--continue`, and no longer commits when git fails
  to say whether a merge, rebase or conflict is in progress. Previously both
  were taken as "nothing in progress" and an automatic commit could land in
  the middle of your work; syncing is now refused with a message saying
  why. (#9)

- Git sync (`--git-sync`) no longer commits a stray file when the checklist's
  file name isn't valid UTF-8. Previously it rewrote the name with
  replacement characters and committed and pushed the new content under that
  made-up name on every toggle, while the real checklist was never updated in
  git. Syncing such a file is now refused with a message saying why. (#8)

- Answering `y` to "reset and quit" (offered when you quit a checklist with
  every task done) no longer exits when the reset can't be saved. Previously
  markcheck quit anyway, so the reason — another instance saving, the file
  changed or deleted on disk, a write error — was never shown, and the
  checklist opened as already complete next time. It now stays open with the
  error shown. (#7)

- Two markcheck instances saving the same checklist could both write at once,
  silently losing one toggle, on systems whose `ps` doesn't support `-p` —
  BusyBox, as found in Alpine and other minimal images. markcheck checks
  whether the instance holding the save lock is still running, and misread
  `ps`'s usage error as "that instance is gone", so it took the lock from a
  live owner. On Linux the check now reads `/proc` and needs no `ps` at all;
  elsewhere a `ps` error keeps the lock. This also fixes the opposite case on
  Linux systems without `ps` installed (slim and distroless containers), where
  a lock left by a markcheck killed mid-save was never recovered and every
  later save was refused. (#6)

- A command too long for the card now shows a dim `↩` at the end of every
  row it wraps onto. Previously the wrapped part was drawn exactly like a new
  line, so on a narrow terminal a single command — say a delete whose path
  wrapped mid-way — read as two separate commands, which matters when typing
  it by hand rather than copying it. Copied text was always correct. (#136)

- A task's indented code block — the older Markdown way to write a command,
  indented four spaces instead of fenced with backticks, and what converters
  such as pandoc produce — is now shown on the card and copied with `y`, just
  like a fenced block. Previously it was silently dropped: the card showed only
  the lead-in ("Restart the service:"), the command was missing from the card,
  the overview and search, and `y` said there was nothing to copy. (#130)

- With git sync on, markcheck no longer leaves a stale `.git/index.lock`
  behind that breaks your own git commands. A git command that ran past
  markcheck's time limit was killed outright, which could leave the lock file
  in place, and from then on every `git add`, `git commit` or `git checkout`
  failed with "index.lock: File exists" until you deleted it by hand. markcheck
  now asks git to stop first so it can clean up, and if it still has to
  force-kill one, the error message says how to clear the lock. The
  `git status` that git sync runs after every toggle also no longer takes the
  lock at all, so your own git commands in another terminal can no longer fail
  because a sync happened to be running. (#119)

- A list's first bullet that is only bold text but carries a command — a
  fenced code block under it, or `` `code` `` inside the bold — is now shown as
  a note card with its command visible and copyable with `y`. It used to
  become the list's one-line warning banner, which silently dropped the
  fenced block altogether and showed inline code as plain, uncopyable text, so
  a precondition like "export this first" never showed the command to run.
  (#115)

- Quitting markcheck right after a change no longer hides what happened to
  that change's git sync. With git sync on, the final sync runs while
  markcheck is exiting, and if it failed, committed but couldn't push, or
  was still running when markcheck gave up waiting, the screen just closed
  as if all was well. markcheck now prints one line on the terminal after it
  closes naming the problem, so you know the checklist wasn't published.
  (#108)

- Saving a checklist no longer moves it out of its group. Every toggle
  writes a fresh file over the old one, and that file used to take the
  group of whoever ran markcheck, so in a shared runbook directory the
  first save could leave the rest of the team unable to write the file. The
  group is now carried over (provided you belong to it), and opening a file
  owned by another user shows a warning that saving will make it yours,
  since only root can keep the original owner. (#106)

- Long text is no longer cut off silently at the edge of the screen. A list's
  warning banner (a list's leading bold-only bullet) and a card's title now
  wrap onto as many rows as they need, so the whole warning and the whole
  step name are always readable; before, both were cut mid-word, which could
  hide a precondition at the end of a warning while still reading as
  complete. Places that stay one row — the list heading, the overview's
  banner and task rows, and the `T` "go to task" picker — now end in `…`
  when shortened, and the picker keeps its list and sub-section context
  visible beside a long task name. A long document title no longer pushes
  the file name out of the title bar. (#102)

- Git-sync (`--git-sync`) no longer pushes where a plain `git push` would
  not. It used to always push to the branch's upstream under the upstream's
  name, so a branch started from `origin/main` under another name had its
  checklist commits published straight to `main`, and a fork setup
  (`remote.pushDefault` or `branch.<name>.pushRemote`) had them pushed into
  the shared repository instead of your fork. In those cases the commit now
  stays local with a message asking you to push it yourself. A remote with
  a separate push URL (`pushurl`) is now checked on that URL too, rather
  than failing every push as out of date. (#98)

- Undo (`u`) and redo (`Ctrl-R`) can no longer overwrite another program's
  edits. When the checklist file had been changed by something else, the
  first `u` correctly reloaded the file and saved nothing, but a second `u`
  then applied the old undo step to the new content by line number and saved
  it — unchecking tasks you never touched. The reload now clears the undo and
  redo history as intended, so the next `u` says "Nothing to undo". (#91)

- Clicking a command on a card now copies exactly that command, in full.
  Previously, with two inline commands on the same row, clicking the second
  copied the first; and a long inline command that wrapped across several
  rows copied only the piece on the clicked row, putting a truncated command
  on the clipboard. Clicking the prose beside a command now behaves like `y`
  instead of copying the command next to it. (#59)

- On Linux X11 desktops without a clipboard manager, a command copied with
  `y` (or a click, or `--auto-copy`) can now actually be pasted. Previously
  markcheck said "Copied to clipboard" but the copy vanished at once, because
  on X11 the copying program has to stay around to hand the text over and
  markcheck let go of it straight away. It now holds the clipboard for as long
  as it runs; without a clipboard manager, the copy lasts until you quit
  markcheck. (#58)

- Pasting text into markcheck no longer runs it as commands. Previously a
  paste arrived as one keystroke per character, so a shell command pasted
  into the markcheck pane by mistake would toggle tasks with its spaces,
  start tasks, open the editor, and could reset the checklist or quit — each
  change written to the file. A paste into `/` search or the `T` filter now
  fills in the query; anywhere else it is ignored with a "Paste ignored"
  message. (#57)

- Commands are now readable on light terminal themes. Inline commands and
  command blocks set a dark gray background but left the text in the
  terminal's own color, which on a light theme is black — dark text on a dark
  box. They now always use light text on that background. (#45)

- A file reload no longer closes the `/` search prompt, the `T` go-to-task
  filter or the `?` help overlay. When the checklist changed on disk while
  one of them was open, markcheck switched back to the checklist without
  saying so, and every key typed after that ran as a command: typing a word
  into the search could open your editor, or mark a task started and save
  the file (and, with git sync on, commit and push it). These now stay open
  across a reload. A pending reset or quit confirmation is cancelled instead,
  with a message saying the file changed, so a following `y` can no longer
  act on a dialog that had quietly disappeared. (#24)
- A YAML or TOML front-matter block at the top of a checklist (the
  `---` ... `---` properties block that Obsidian, Hugo and Jekyll write) is now
  skipped. It used to be read as a section heading, so every task showed up
  under a list titled with the raw metadata text, which also leaked into git
  sync commit messages. The block is still left untouched in the file. (#43)

- Git-sync no longer commits a checklist into a different file when the
  checklist's name contains `[`, `]`, `*` or `?`. Git read the name as a
  pattern, so ticking a box in `notes[1].md` in a repository that also
  tracked `notes1.md` overwrote `notes1.md` with the checklist, committed
  and pushed it, and reported success, while the checklist itself was never
  committed. Names are now matched exactly, also when `GIT_ICASE_PATHSPECS`
  or `GIT_GLOB_PATHSPECS` is set in your shell, and a lookup that still
  matches more than one file stops the sync with an error. (#70)

- A started task (`- [/] ...`) no longer disappears from the checklist when
  something earlier in the file merely looks like a code fence — a line of
  backticks inside an HTML comment, or indented four spaces. Everything
  after such a line used to be treated as code, so a started task there
  vanished outright or showed up as a plain note reading `[/] ...`. The
  reverse case is fixed too: a `[/]` line inside a code block that opens on
  a list bullet (`` - ``` ``) is no longer shown — and copied with `y` — as
  `[ ]`. Started tasks are now recognised by the same Markdown parser that
  reads the rest of the file, so the two can no longer disagree. (#3)

- Git-sync will not recreate or overwrite a remote branch that changed while
  it was pushing. The previous fix checked with the remote first, but the
  check and the push were separate steps, so a branch deleted in between
  could still be recreated. The push now tells git what it expects the
  branch to be, and git refuses if that is no longer true.

- Git-sync no longer recreates a branch that has been deleted on the remote.
  Your clone remembers the branch until you next fetch, so a toggle used to
  push it straight back — republishing history somebody had removed, without
  being asked. It now checks with the remote first and, if the branch is
  gone, keeps the commit local and tells you rather than pushing. The same
  applies when the remote can't be reached to check.

- Git-sync now stores exactly the content `git add` would, in repositories
  that normalise line endings (`core.autocrlf`, or a `.gitattributes` `text`
  setting). It previously bypassed that normalisation and committed the file
  verbatim, so a repository shared with Windows users could receive content
  git itself would never have written. The same bug also made git-sync
  wrongly refuse to sync — reporting that a staged version of the checklist
  would be lost — when the staged version was in fact identical.

- Git-sync no longer leaks a couple of threads and file handles per commit
  when a repository's hooks start a background process. Over a long session
  against such a repository this could accumulate until the process ran out
  of file handles; syncing now costs nothing that isn't released.

- Git-sync no longer publishes unrelated local commits when your branch has
  upstream *settings* but no remote-tracking ref yet — configured by hand,
  or pruned. The check that refuses to publish unrelated work saw no
  upstream to compare against and stood down, while the push went ahead and
  created the branch on the remote. Both now agree on what having an
  upstream means; run `git push -u` once and syncing proceeds as normal.
- Git-sync now ignores git environment variables inherited from wherever
  you launched `markcheck`. With `GIT_DIR` or `GIT_WORK_TREE` exported — the
  common bare dotfiles-repo setup, or anything run from a git hook,
  `git rebase --exec` or `git bisect run` — git-sync could read one
  repository and commit and push into a different one. An inherited
  `GIT_INDEX_FILE` also made a tracked checklist look untracked. Every git
  command it runs now resolves the repository from the checklist's own
  directory and nothing else.
- Git-sync no longer reports a failure when someone else edits the checklist
  in the moment between your toggle being saved and its commit being made.
  The queued commit described content the external edit had already
  replaced, so it was correctly refused — but shown as a git-sync error for
  a situation where nothing had gone wrong. The stale request is now
  discarded quietly when the external change is picked up.
- A slow save can no longer cost you a toggle. When two copies of
  `markcheck` had the same checklist open, one that took more than thirty
  seconds to write — a stalled disk, a network or FUSE filesystem — was
  treated as crashed, and the other started writing alongside it, so one of
  the two changes was silently lost. A save is now only ever taken over
  from a process that has actually exited.
- Two copies of `markcheck` open on the same checklist can no longer lose a
  toggle. Both could previously check that the file was unchanged and then
  both save, with the second overwriting the first's change. Saving is now
  serialised between instances on the same machine; if another instance is
  mid-save you get `Another markcheck is saving this file — change not
  saved, please retry` rather than a silently lost toggle.
- Git-sync can no longer remove a commit it didn't make. When a sync had to
  be rolled back — because a commit hook changed files beyond the checklist,
  or because another process committed at exactly the wrong moment — it
  rewound the branch all the way to where it started, taking any commit that
  had landed in between with it. It now rewinds only its own commit and
  leaves everything else on the branch, reporting what it found.
- Git-sync now tells you when the checklist isn't tracked by git even if
  your git configuration hides that. With `status.showUntrackedFiles = no`,
  or when the checklist matches a `.gitignore` rule, git-sync saw nothing to
  do and stayed quiet — so it looked like it was working while it could
  never commit anything. It now reports `file is not tracked in git` in
  those cases too.
- A git-sync commit waiting to be pushed is no longer held up indefinitely
  by unrelated sync failures. If something else kept failing on every
  toggle — a refusal to publish unrelated commits, say — each failure reset
  the waiting push's retry timer, so it never came due. The retry timer is
  now only reset by the retry itself failing.
- Git-sync no longer drops a staged version of the checklist. If you had
  `git add`ed the checklist and then edited it further, the automatic commit
  was built from the working tree and the index was repointed at it — so the
  version you staged ended up in no commit and no longer staged, recoverable
  only as a dangling object. Git-sync now refuses in that case and tells you
  to commit or unstage it first. Staging and then toggling still syncs
  normally, since nothing is lost there.
- Git-sync no longer discards a version of the checklist you staged while a
  background sync was running. If you ran `git add` on the file (or your
  editor did) mid-sync, markcheck's index bookkeeping afterwards replaced
  your staged version with its own. Your file's contents were never
  affected, but the staging had to be redone; it is now left alone whenever
  the entry changed after the sync began.
- Git-sync now refuses to push when it *cannot tell* whether the branch has
  unrelated unpushed commits, instead of treating an unanswerable check as
  an all-clear. Previously any git failure during that check — a timeout on
  a large history, say — read exactly like "nothing unrelated here" and the
  push went ahead. A branch with no upstream configured is still not treated
  as a failure: committing proceeds as before, and the push declines with
  its usual "run `git push -u` once" message.
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

### Security

- Saving a checklist now clears its setuid, setgid and sticky bits, keeping
  the rest of its permissions as before. Each save writes a brand-new file
  owned by whoever ticked the box, so a checklist planted in a shared,
  group-writable directory with one of those bits set came back as a
  special-bit file owned by the person who saved it, with content someone
  else chose — exactly what the system normally prevents by clearing those
  bits when anyone but a file's owner writes to it. (#122)

- Copying a command (`y`, clicking a command on the card, or `--auto-copy`)
  now refuses one that contains hidden control characters — escape,
  backspace, carriage return and the like, or Unicode bidi overrides — and
  says so in the status bar instead. The card never shows those characters,
  so a malicious or corrupted checklist could display a harmless command
  while the clipboard received a different one, which could even run on
  paste. Newlines and tabs are still copied as before. (#4)

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
