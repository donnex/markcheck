# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `--no-terminal-title` and `--no-exit-summary` turn off the terminal title and the line printed after quitting for one run, and `--terminal-title` and `--exit-summary` turn them back on over a config file that sets `terminal_title = false` or `exit_summary = false`. These two settings could only be changed in the config file, although the 1.3.0 notes and the README said every on/off setting has a flag in both directions, so a shared config that turned the exit line off couldn't be overridden, and there was no way to leave the title alone without writing a config file. (#163)

### Fixed

- With git sync on, an undo or redo is now committed under a message saying what it did, such as `checklist.md: Undo: Uncheck "Restart service"` or `checklist.md: Undo: 3 tasks ("Drain node", "Restart service", "Verify")`. It was committed as `Undo: 1 item` or `Redo: 3 items`, so someone reading the shared history couldn't tell which step had been reopened without comparing the file's versions, although every other change names its task. (#175)
- The reset dialog's status-bar line now lists only the keys it accepts. After `R`, it always read `y:all  l:list  s:sub-list  any other key:cancel`, even on a checklist with a single list or on a task outside any sub-list, and pressing the advertised `s` there just closed the dialog without a word. `l` and `s` now appear only when the dialog offers them, and `s` outside a sub-list keeps the dialog open with `No sub-list here: y resets everything`. (#174)
- Opening a checklist at a section or line, as in `markcheck runbook.md#rollback` or `markcheck +120 runbook.md`, now always confirms with `Opened at #rollback`. The confirmation was dropped whenever there was also a startup warning, such as text the checklist can't show or the file being open in another markcheck, so a successful jump looked the same as starting at the top. And a tick after a checklist was edited down to no tasks no longer says `File changed on disk — reloaded; change not saved, please retry`: nothing had been reloaded and retrying could never work, so the status bar now keeps saying `Reload skipped: file has no checklist items — changes cannot be saved until it has tasks again`. (#168)
- The terminal title is now right while you are in your editor or the shell, and after you come back. While `e` had the editor open or `Ctrl-Z` had you in the shell, the tab still read `Deployment Runbook · 3/11 — markcheck`, and after the editor or shell set a title of its own markcheck didn't set its title again until the next tick changed the count, so the tab named the wrong thing. Your own title is now shown while the other program has the terminal, and markcheck's comes back as soon as it takes the terminal back. (#162)
- Over SSH, copying a command too large to send through the terminal (OSC 52, over 70 KB) no longer says `Copied to clipboard`. Only the system clipboard got it, and over SSH that may be another machine's, so pasting on your own machine gave the previous contents after markcheck said the copy worked. The status bar now says `Copied to the system clipboard only — too large to send through the terminal (OSC 52)` and keeps the message until the next key, for `y`, `--auto-copy` and a link copied by `o`. (#161)
- A git sync failure now stays in the title bar on a terminal narrower than 80 columns, such as a split pane or a phone. Below that width the whole `⇅ git` section was dropped, so a failed push or an untracked checklist showed only as a status-bar message that cleared on the next key, and nothing on screen said afterwards that the checklist was `not pushed`. The section is now shown at every width; only its `Synced … ago` text is left out on a narrow terminal. (#160)
- Tasks written before a checklist's first heading are no longer listed under the made-up name `(Default)`. Once the file also had a `## Heading`, that name showed in the overview, the list tabs, the `T` picker, the list-complete screens and the `Tab` wrap message, and resetting that list with `R` then `l` said `"(Default)" reset to not done` and, with git sync on, committed `Reset "(Default)" to not done` to the shared history. The list now takes the checklist's title, its `# Heading` or, without one, the name made from the filename (`quick-tasks.md` shows `Quick Tasks`). (#159)
- `markcheck --help` and `man markcheck` now cover everything needed without the README. Both now say that `markcheck runbook.md#rollback` opens at a section and `markcheck +120 runbook.md` at a line, which only the README mentioned. The man page gains an ENVIRONMENT section listing the variables markcheck reads (`NO_COLOR`, `TERM`, `EDITOR`, `BROWSER` and others) and an EXIT STATUS section. And `--git-sync`'s help no longer says it commits after every toggle: changes are committed once `--git-sync-delay` passes with no further change. The README's install section also gains `cargo install --git https://github.com/donnex/markcheck` and a `git clone` for building from source on other platforms. (#173)
- Pressing `s` now confirms in the status bar with `Marked started` or `Marked not started`, as `Space` and `-` already did. It was the one task-state change that said nothing, and since `s` toggles, pressing it a second time silently undid it; with no color, where a started task differs only by a small `◐`, there was no sign the key had done anything. (#158)
- Git sync no longer publishes checklist edits you never asked it to sync. A tick committed and pushed the whole file under its own `Check "…"` message, so an uncommitted draft step, a local note, an edit another editor or script made, or an `e` edit you aborted with an error went to the shared remote the moment you ticked an unrelated task, with nothing in `git log` to show it. A tick now syncs only markcheck's own changes: when the file holds changes markcheck didn't make, nothing is published and the status bar says `Git sync failed: the checklist has changes markcheck didn't make, so nothing was published; commit them yourself, then toggle again`. Staging the checklist before ticking still syncs as before, an edit saved in `e` is still synced, and after a sync refused for another reason (a merge conflict, a failing hook) ticking again still syncs your ticks. (#177)
- A `TMPDIR` that is missing or can't be written to no longer turns off markcheck's safeguards or blames git. markcheck found out which user it runs as by creating a file in the temp directory, so with a bad `TMPDIR` every save silently ran without the lock that stops two markcheck windows on one checklist overwriting each other's changes, and the warnings that a checklist belongs to another user or is already open in another markcheck never appeared. Git sync was also switched off with `Git sync off: git not found on PATH` while git was installed; the message now names the temp directory it couldn't use: `Git sync off: could not run git: cannot create git scratch files in <directory>: …`. `Ctrl-Z` and stopping a stuck git command also no longer need a `kill` program, so `Couldn't suspend: couldn't run kill` can't happen. (#154)
- The `?` keybinding help now says that `R` can reset just the current list or sub-list. It still described `R` as resetting every task, although the reset dialog has offered the list and sub-list too since that was added. (#152)
- A bullet list with no checkboxes is now counted in the startup note of what markcheck isn't showing. Such a list is dropped, and while paragraphs, quotes and code blocks markcheck can't show were already counted, the bullets were not, so a "before you start" section of plain bullets vanished without a word. The note now says, for example, `2 bullet points outside task lists not shown`. (#151)
- The reset confirmations now count skipped (`[-]`) tasks. Pressing `R` when the only changed tasks were skipped said `This marks  tasks (of 4) as not started`, and with done and skipped tasks it named only the done ones and left the skipped ones out of the total, although the reset clears them too; it now says, for example, `This marks 2 done and 1 skipped tasks (of 5)`. Quitting a checklist whose tasks are all done or skipped, including one where every task was skipped, now offers the reset as a fully done one does, instead of quitting with the `[-]` markers left in the file, and undoing or redoing a skip says `marked skipped` rather than `restored`. (#150)
- Copying now refuses a command containing a zero-width character, such as a zero-width space, word joiner, byte-order mark, soft hyphen or Unicode tag characters, or a variation selector with nothing before it to modify. The card doesn't show these, so `systemctl restart nginx` with a zero-width space inside it looked right but failed when pasted, and a host name or path could silently name something else. Such a command is now refused with `Not copied: command contains hidden control characters`, like one with an escape or a bidi override. (#149)
- A huge git sync wait no longer crashes markcheck on the first tick. A `--git-sync-delay` or config-file `git_sync_delay` too large for the clock, such as a huge number meant as "never commit on its own", saved the tick and then crashed, so the change was never synced, and with such a config file every session crashed the same way. The wait can now be at most 86400 seconds (a day), and a longer one is refused at startup with `git_sync_delay must be at most 86400 seconds`. (#147)
- The `?` keybinding help can now be read in full on an 80-column terminal. It shared the screen with the overview, so on the common 80×24 most descriptions were cut off mid-word with no way to scroll sideways to the rest. The help now uses the whole width while it is open, and a description that still doesn't fit wraps onto the next row. (#251)
- A second `# Heading` in a checklist now starts a list of its own, like a `## Heading`. Before, it started nothing: its tasks joined whatever list came before it, and its name was shown nowhere, so a runbook split into `# Upgrade`, `# Verify` and `# Rollback` counted the rollback steps as part of the upgrade, and wasn't complete until they were done too. The first `# Heading` is still the title. (#250)
- A task with a long code block or command no longer freezes markcheck or runs it out of memory. Each row of the block on the current card kept its own copy of the whole block, and a long command was re-measured from its start for every row it wrapped onto, so a 10,000-line block took over three seconds and well over a gigabyte to draw on every key press, and a few hundred kilobytes could exhaust memory as soon as the checklist opened. Such a card now draws in a fraction of a second. (#245)
- A checklist that repeats the same heading many times, such as a `### Verify` under every host, no longer takes minutes to open. Each repeat checked every earlier heading once per number it tried for its link name, so 3,000 repeats took most of a minute before anything was shown, and the screen froze for as long again every time the file changed on disk. Such a file now opens at once. (#148)
- Git sync no longer takes a commit made on top of its own for its own and rewinds it off the branch. A `post-commit` hook that commits a follow-up (a regenerated file, an auto-changelog) lost that commit on every sync, as could a commit you made in another terminal at the same moment, which left the branch and reappeared as staged changes; the status bar then claimed markcheck's own commit had been undone, when it was the one still there. Both commits now stay on the branch, nothing is pushed, and the status bar says another commit landed on top of markcheck's. (#232)
- Holding `Space` or pressing it twice quickly no longer walks past a critical step (a task whose bold title starts with `!`). The confirming press could come at any speed, so a held key's repeats armed and confirmed the step in a fraction of a second, despite the release notes promising otherwise; `-` didn't ask at all, so a held `-` skipped every critical step in a list. The second press, of `Space` or `-` alike, now has to come after a short pause, and a quicker one keeps the step open and says it was too quick. (#219)
- The list title, the list's banner and a task's notes and `> [!WARNING]` alerts above it no longer vanish when a task's card is tall. On a short terminal, such as the common 80×24, a long, command-heavy step hid every row above its card at once, with nothing saying anything was hidden, so a "take a backup first" warning disappeared on exactly the step it was written for. The card now gets shorter instead (its text scrolls), and only when even the shortest card leaves too little room are rows left out: first the sub-section name and list title, then notes from the end, with the banner and alerts kept the longest and a `…` marking the cut. (#180)
- Git sync no longer draws a GPG passphrase prompt over markcheck. With signed commits (`commit.gpgsign`), a key whose passphrase wasn't cached and a terminal pinentry such as `pinentry-curses`, the first sync after a tick opened a full-screen passphrase dialog on top of the checklist, and your keystrokes were split between the two: part of the passphrase could be taken as markcheck commands, and when the dialog closed the screen was left garbled. Signing now fails at once with the reason in the status bar, as the README already said it would; a cached passphrase or a graphical pinentry keeps working. (#204)
- A critical step armed by clicking its marker in the overview now asks you to click it again, rather than to press `Space`. `Space` acts on the current card, not the clicked one, so following the old prompt checked a different task. Scrolling the mouse wheel or pasting now also leaves an armed critical step open; before, a wheel away and back hid the prompt but kept the step armed, so the next `Space` checked it in what looked like a single press. (#179)
- Git sync now says at startup when your git is too old for it, instead of appearing to work. It needs git 2.22 or newer, not git 2.0 as the README claimed; with an older git, such as the 2.17 or 2.20 still found on long-lived servers, every push was refused with a wrong reason ("`git push` would not push this branch …") and markcheck's own earlier commits were never pushed, so commits piled up locally. Git sync now stays off with `Git sync off: git 2.22 or newer is needed (found 2.17.1)`. (#169)
- Pressing your terminal's interrupt key while editing a checklist with `e` no longer ends markcheck. Emacs uses `C-g` as that key, and line editors such as `ed` use `Ctrl-C`; once markcheck quit as soon as the editor closed, and twice killed it straight away while the editor was still open, leaving the editor stranded with any unsaved work at risk. The key now goes only to the editor. (#146)
- The README no longer contradicts markcheck in several places. It said a card's 10th and later links couldn't be opened, although `o` opens them by typing both digits; it described three task states rather than four, leaving out skipped (`[-]`), and left skipping and the list and sub-list resets out of what undo and git sync cover; it said an indented code block outside a task was ignored, although under a heading it is shown above the first task like any other code block; and it said the exit status was always 0, although a second `SIGTERM` or `SIGINT` exits at once with status 1. Its git sync table now also lists the `⇅ git · pending` state shown while a change waits to be committed. (#153)
- Git sync now explains a failed sync correctly when your system language isn't English. With git's translations installed, a non-English locale made git print words such as `schwerwiegend:` instead of `fatal:`, so markcheck couldn't find the reason: the status bar showed the last hint git printed after a rejected push or refused commit, a checklist outside any repository showed git's raw text instead of `Git sync off: the file is not in a git repository`, and a push that lost a race was not explained as `the remote branch changed while pushing`. Git's messages are now always in English; the rest of your locale, such as the character set your hooks see, is unchanged. (#170)

### Security

- A checklist can no longer send escape sequences to your terminal through its title, a task's label or its file name. These were written to the terminal title, the line printed on quit and git-sync's messages without being cleaned, so a crafted checklist could, for example, silently replace your clipboard with a command of its choosing the next time you ticked a task or quit. Control characters are now dropped from all of them, and from git-sync commit messages. (#145)

## [1.3.0] - 2026-10-20

### Added

- Prose under a heading is now shown. A paragraph, quote or code block written under a heading before its first task (a "take a snapshot first" note, a `> [!WARNING]` alert, the intro under the title) appears above that task's card and in the overview, and GitHub-style `[!WARNING]`, `[!CAUTION]` and `[!IMPORTANT]` alerts are shown in the warning color. Before, everything outside a task list was dropped without a word; prose that still can't be placed (between tasks, or after the last one) is now counted at startup. (#80)
- A skipped task state, `[-]`, for a step that doesn't apply this run. Press `-` to skip the current task, or unskip it. A skipped task is dimmed and struck through, counts as resolved so the list can still complete, and is counted apart from done tasks (`☑ 7/9 (+2 skipped)`). (#54)
- Critical steps: start a task's bold title with `!` (`- [ ] **! Drop the old database**`) and checking it takes a second, deliberate `Space`, so a held key or a double press can't walk past it. Its card is framed in the warning color with a `critical` label. (#95)
- Links to a heading in the same file, such as `[Rollback](#rollback)`, open with `o` by jumping to the first task under that heading, and `Ctrl-O` jumps back, as in vim. The same anchors work when starting markcheck: `markcheck runbook.md#rollback` opens at that section, and `markcheck +120 runbook.md` at the task on or after line 120. (#114, #139)
- `R` can reset part of a checklist: in its confirmation, `l` resets only the current list and `s` only the sub-list you are in, each with the number of tasks it would change, while `y` still resets everything. (#118)
- Git sync waits until 10 seconds pass without another change before it commits and pushes, so a run of ticks becomes one commit (`checklist.md: 3 changes`, each listed in the message) and a tick undone straight away is never published. The title bar shows `⇅ git · pending` while a change waits, and quitting sends it at once. Set the wait with `git_sync_delay` in the config file or `--git-sync-delay SECONDS`; `0` commits every change at once, as before. (#56)
- A task with several commands can be copied from the keyboard: the card numbers them (a dim `[1]`, `[2]` after each inline command, or in the top border of each fenced block) and `y` then a number copies that one, just as `o` then a number opens one of several links. Numbers from 10 up work for both. (#116, #100)
- `--clipboard auto|system|osc52`, and a matching `clipboard` setting, choose where copies go. Over SSH the default `auto` now also sends the copy through the terminal (OSC 52) as well as to the system clipboard, because a tmux session reattached from another machine still copies to the clipboard of the machine it started on. (#133)
- `Ctrl-Z` suspends markcheck to your shell, as in vim and less, and `fg` brings it back, picking up any change made to the checklist meanwhile. (#107)
- The terminal title shows the open checklist and its progress (`Deployment Runbook · 3/11 — markcheck`), and your own title comes back on exit; after quitting, one line says where the run stopped (`3/11 done, stopped at "Warm the build cache"`). Turn them off with `terminal_title = false` and `exit_summary = false` in the config file. (#135)
- Opening a checklist that is already open in another markcheck (same machine, same user) warns at startup, naming the other session's process and how long it has been open. (#101)
- Steps in a numbered list show their number, exactly as written, on the card, its overview row, the side cards, the `T` picker and git-sync commit messages, and typing `3.` in the picker finds step 3. (#82)
- A bare URL in a task or note (`https://example.com/status`, `www.example.com`) is a link, underlined, listed below the card and opened with `o`, as on GitHub. (#81)
- A task's indented code block, the older way of writing a command that converters such as pandoc produce, is shown and copied like a fenced block; it used to be dropped. (#130)
- `--color auto|truecolor|256|16`, and a matching `color` setting, force a color palette when markcheck picks the wrong one. (#134)
- `NO_COLOR` (<https://no-color.org/>) is honoured: with it set, markcheck draws no color, shows commands in reverse video and tells states apart by their glyphs. (#45)
- `--config PATH` reads a different config file and `--no-config` ignores it for one run, so a broken config file no longer locks you out of every checklist. (#112)
- `--no-git-sync` opens a checklist without committing or pushing anything, even under a directory listed in `git_sync_paths`. (#111)
- `--nerd-font`, `--mouse`, `--no-primary` and `--no-auto-copy`, so every on/off setting can be overridden for one run in both directions, whichever way the config file sets it.
- A warning at startup when the checklist has more than one hard link (a save writes a new file, so the other names stop following it, as in most editors), and when it belongs to another user (saving makes it yours). (#106)
- A man page and bash, zsh and fish completions in the release archive, also printed by `markcheck --generate-man` and `--generate-completions <shell>` for a source build. (#90)
- The release archive includes `LICENSE` and `THIRD-PARTY-LICENSES` with the license texts of every crate compiled into the binary. (#132)

### Changed

- **Plain Unicode icons are now the default.** The Nerd Font icons markcheck used to start with need a patched font, and without one every task marker showed as the same blank box. If you have a Nerd Font and want its icons back, set `nerd_font = true` in the config file or pass `--nerd-font`. (#83)
- **`Esc` no longer quits.** As in vim it cancels (search, the task picker, help, a confirmation, a number prompt), and on the checklist and completion screens it shows "Type q to quit"; `Ctrl-C` does the same instead of nothing. `q` still quits. (#51, #13)
- **The release binary is statically linked**, so it runs on any x86_64 Linux; it used to need glibc 2.34 or newer. The archive is now named `markcheck-<version>-x86_64-unknown-linux-musl.tar.gz` instead of `…-linux-gnu.tar.gz`, so update any script that downloads it. Building from source needs Rust 1.88 or newer. (#44)
- **Git sync is rebuilt to commit only your checklist change and publish only what it checked**, and to refuse, with a reason in the status bar, rather than guess. You may see a refusal where 1.2.2 went ahead:

  - It needs the branch to have an upstream (`git push -u origin <branch>`, once) before it pushes, and pushes only where a plain `git push` would, never to a local branch or a remote branch that was deleted. Without an upstream the commit is still made and kept locally. (#98, #71)
  - It won't push while the branch has other unpushed commits that aren't markcheck's, since `git push` would publish them too; the message names the commit to push or drop first. Every commit markcheck makes ends with a `Synced-by: markcheck` line, and at startup it pushes any of its own commits an earlier session left unpushed (commits from earlier versions lack that line, so push those by hand once). (#62)
  - It won't commit during a merge, rebase, cherry-pick, revert or bisect, on a detached `HEAD`, with unresolved conflicts, or when the checklist has a staged version that the commit would drop. (#9)
  - A failed push is retried after 30 seconds, then less often, up to every 5 minutes, and the title bar shows `⇅ git · not pushed`, `syncing…` or `Synced … ago` instead of looking healthy while pushes fail. Commit hooks and signing get up to 5 minutes, pushes 60 seconds, and a hung git command no longer stalls syncing for the rest of the session. (#64, #53)
  - It commits through a private index, so other files you have staged are never swept in, while your commit hooks and `commit.gpgsign` still run as for a manual commit.
- A `git_sync_paths` entry turns on git sync only for the repository it names, not for repositories nested inside it, since a repository unpacked into a synced folder brings its own hooks and settings, which opening a checklist there would run. End the entry with `/**` (`"~/work/**"`) to sync them all; markcheck says at startup when a checklist is left out. An entry starting with `~` now means your home directory. (#124, #38)
- An unknown key in the config file is a warning instead of an error, so a setting added to a config shared between machines doesn't lock out a machine running an older release. A malformed file is still an error. (#112)
- `/` searches forward from the cursor, wrapping, as vim's `/` does, and reports the match's position (`Match 7/12`); it used to jump to the first match in the file. An empty `/` repeats the last search, and `Ctrl-W` and `Ctrl-U` edit the query. (#61, #131)
- `Space`/`Enter` moves on to the next task that is still open, never onto a done one, where the next press would have unchecked it. (#84)
- A reload says what changed (`Reloaded: 2 done, 1 reopened ("Restart service", "Verify output") · 1 added`), keeps the cursor on its task when lines move above it, and keeps an open search, picker or help open. (#140, #60, #24)
- `Tab` and `n`/`N` say when they wrap around the document. (#87)
- Keys that do nothing on the current screen, `Ctrl`/`Alt` chords that aren't bound, and a lone `g` all say so in the status bar instead of doing nothing (an unbound chord used to act as the plain key, so `Ctrl-S` marked a task started). (#52, #25, #104)
- The `?` help lists the keys of every mode, and the mouse wheel works in help and the `T` picker. (#74, #65)
- `o` on a headless machine (no `$BROWSER`, no display) copies the link instead of failing, and a browser that exits with an error is reported. `$BROWSER` may be a colon-separated list with `%s` for the URL, as other programs read it. (#63, #15)
- `e` opens Kakoune, micro, Helix, Sublime Text, VS Code and VSCodium on the selected task's line, and hints at the wait flag when a GUI editor returns at once. (#110, #109)
- A file without a `# Heading` is titled from its filename instead of showing "Missing document title" in red. (#86)
- Long banners and card titles wrap instead of being cut, and other shortened text ends in `…`. (#102)
- The status message for an OSC 52 copy says `Sent to clipboard (OSC 52)`, since the terminal may not have applied it, and its size limit is 70 KB, below where tmux truncates.
- Building on a non-Unix system stops with "markcheck supports Unix-like systems only". (#78)

### Fixed

- A change made in another program or another markcheck after the file was loaded is no longer overwritten by the next toggle; the save is refused and the file reloaded. A failed save no longer leaves the task showing a state the file doesn't have, and the message says why the save failed. (#46)
- A checklist in a folder you can't write to can be saved: it is rewritten in place when the file itself is writable. Long file names can be saved and created with `--new`. (#46)
- Toggling a task changes only that task's marker. An untouched `[X]` is no longer rewritten to `[x]`, a file mixing CRLF and LF keeps each line's ending, a CR-only file saves at all, and a file without a final newline doesn't get one. (#23)
- A write-protected checklist is no longer rewritten; markcheck warns when it opens one and refuses each change. (#47)
- Saving keeps the file's group, so a shared runbook directory stays writable by the team. (#106)
- A nested task indented with a tab toggles its own line, not its parent's. (#79)
- A started task (`[/]`) after a line that only looks like a code fence (in an HTML comment, or indented four spaces) no longer disappears, and a `[/]` inside a code block opened on a bullet is no longer shown as `[ ]`. A fence line with trailing text no longer closes the block. (#3)
- A heading inside a list item or a quote no longer starts a new list, a section whose only checkboxes are quoted is skipped like any section without tasks, and a list's banner comes only from a list with tasks. (#92, #27, #103)
- A YAML or TOML front-matter block at the top of the file is skipped instead of becoming a list title. (#43)
- A task's second paragraph, or text after `<br>`, no longer runs into the first without a space. (#72)
- A bold-only first bullet that carries a command stays a card with its command, instead of a banner that dropped it. (#115)
- Inline code in a task's bold title is no longer treated as a hidden command by `y` and `--auto-copy`. (#137)
- East Asian text and emoji wrap instead of running off the card, and boxes around such commands keep their border. (#12)
- A command that wraps shows `↩` on each continued row, and tabs in a command show as indentation. (#136, #85)
- A long status message wraps instead of being cut off. (#11)
- Dialogs always show the keys that answer them, even on an 80×24 terminal. (#93)
- The side cards show the task's title. (#75)
- Commands are readable on light terminal themes. (#45)
- The "All Tasks Complete" screen no longer appears while an earlier list is unfinished.
- The `R` confirmation counts started tasks too. (#26)
- `y` on "reset and quit" no longer exits when the reset can't be saved. (#7)
- Undo and redo can no longer apply an old step to a file changed on disk. (#91)
- Clicking a command copies exactly that command, in full. (#59)
- On X11 without a clipboard manager, a copy can be pasted; it used to vanish at once. (#58)
- Pasted text is ignored (or added to the search) instead of running as keystrokes. (#57)
- `--auto-copy` copies on every motion, never on a reload or a search that matched nothing. (#30, #60, #117, #138)
- The cursor stays on a later one of two `## H2` lists with the same title across a reload.
- Live reload works on network shares and FUSE mounts, after a folder is recreated, and when file watches run out, with a 2-second recheck. (#50)
- Large and deeply nested checklists no longer slow markcheck down, and an idle session no longer redraws. (#39)
- A crash prints its message on the restored terminal. Startup errors name the file and what to do, and running without a terminal says it needs one. (#49, #48)
- kitty, Alacritty, foot, WezTerm, Contour and Ghostty get full colour over SSH and `sudo`. (#134)
- `git_sync_paths` entries reached through a symlink or written as relative paths match.
- `--new` never leaves a half-written file.
- Git sync:
  - It no longer draws credential or host-key prompts over the screen and waits on them; such a push fails at once with the reason. (#2)
  - Quitting or a signal while it runs no longer leaves a `git commit` running after markcheck exits; `q` waits ("press q again to abandon"), and a problem with the last sync is printed after the screen closes. (#5, #108)
  - A commit hook that stages more than the checklist no longer gets those files committed and pushed: the commit is undone.
  - A commit can no longer pick up a later change under the first change's message.
  - Git variables inherited from your shell or a hook (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`) no longer point it at another repository.
  - A checklist named with `[`, `]`, `*` or `?` is no longer matched as a pattern against another file, and a file name that isn't valid UTF-8 is refused. (#70, #8)
  - An untracked checklist is reported at startup, even when `status.showUntrackedFiles = no` or `.gitignore` hides it, and a reason is shown when git sync can't start. (#28, #38)
  - Its `git status` no longer takes the `index.lock`, so your own git commands can't fail because a sync is running. (#119)
  - Editor edits are synced after `e` only when the editor exits successfully, and the commit message names the editor, not its path. (#94)

### Security

- markcheck no longer reads or saves a checklist that was replaced while it was open by a symlink, a named pipe or another special file. Someone who could write to the checklist's folder could swap it for a link to one of your private files, which markcheck then wrote into the shared checklist (and git sync pushed); a named pipe froze it. (#123)
- Saving clears a checklist's setuid, setgid and sticky bits; a planted file with one of them set used to come back as a special-bit file owned by whoever saved it. (#122)
- Copying refuses a command containing hidden control characters or Unicode bidi overrides, which the card can't show, so a checklist can no longer display one command while the clipboard gets another. (#4)

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
