# markcheck

> ⚠️ **AI-generated code — no guarantees.**
> This project was written by AI (Claude). It comes with **no guarantees** of
> correctness, security, or fitness for any purpose, and **it may not have been
> reviewed by a human**. Read the code and test it yourself before relying on
> it — use at your own risk.

A pilot-checklist TUI (terminal UI), written in Rust, for Markdown runbooks.
Point it at a Markdown file full of `- [ ]` tasks and work through them one
card at a time — like a pilot running a checklist — with progress written
straight back into the file. No database, no export step: **the Markdown
file is the state** — it's still just a text file when you close the app.

![markcheck: navigating a checklist, toggling a task, and using the "go to task" overlay](docs/demo.gif)

## Contents

- [Why](#why)
- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
- [Config file](#config-file)
- [Markdown](#markdown)
  - [Not supported (and how it degrades)](#not-supported-and-how-it-degrades)
- [Keybindings](#keybindings)
- [Behavior](#behavior)
  - [Write-back](#write-back)
  - [Live reload](#live-reload)
  - [Git sync](#git-sync)
  - [Search](#search)
  - [Clipboard](#clipboard)
  - [Undo and redo](#undo-and-redo)
  - [Editor and reset](#editor-and-reset)
- [Architecture](#architecture)
- [License](#license)

## Why

Repeating sysadmin/devops procedures — server upgrades, maintenance
runbooks, release steps — tend to live in Markdown. `markcheck` turns any
such task list into a step-by-step checklist you drive from the keyboard,
checking items off as you go — close the app, edit the file in your
editor, re-open it later, and the checkboxes are just `[x]` in the file.

That's the main use case, but there's nothing devops-specific about a
`- [ ]` list — `markcheck` works just as well as a keyboard-driven front
end for **any** Markdown checklist: a personal to-do list, a recipe, a
packing list, an onboarding guide, a project plan. If it's a Markdown
task list already, `markcheck` gives it the same one-card-at-a-time
workflow and file-backed state.

## Features

- Card-based, one-step-at-a-time navigation with a prev/current/next stack
- Starts on the first not-done task, so you resume where work remains
- Three task states — not started, **started/in-progress** (`[/]`), and done
  — each with its own card color and icon
- Lists from `##` headings, jumpable with number keys
- Incremental text search (`/`) to jump to a task by title, body, or command
- A filterable "go to task" overlay (`T`) listing every task at once
- Progress at a glance: an overall bar under the title, a pilot-style
  position strip on the current card, and a `done/total` counter that fades
  from yellow to green as you complete tasks
- Scrollbars on anything that overflows — a long card body, the overview
  list, and the help overlay
- Commands shown right in the card — inline `` `code` `` and fenced blocks
  are visible and highlighted, not hidden
- Atomic write-back on every toggle — the file is never left half-written,
  and permissions are preserved
- Live reload when the file changes on disk (edit it elsewhere, see it update)
- Optional git sync (`--git-sync`): commit and push the file after a toggle
  or manual edit (`e`), when it's already in a git repo — a lightweight way
  to keep a checklist in sync across hosts that share it via git
- One-key clipboard copy of a task's command, with an SSH-friendly fallback;
  optional auto-copy on navigation and PRIMARY-selection support
- Open the file in `$EDITOR` without leaving the app
- Undo/redo (`u` / `Ctrl-R`) for every state change — a mistoggle or an
  accidental reset is one keystroke away from recovery
- Reset the whole checklist (with confirmation), and an offer to reset when
  you quit a fully-completed checklist
- Mouse and keyboard driven; works on a plain terminal or a Nerd Font one
- Colors adapt to your terminal: curated 24-bit hues on truecolor terminals,
  a 256-color match otherwise, and named ANSI colors as a safe fallback

## Installation

Pre-built binaries (Linux x86_64) are attached to each
[release](https://github.com/donnex/markcheck/releases) as a
`.tar.gz` with a `.sha256` checksum — download, verify, extract, and put
`markcheck` on your `$PATH`. For any other platform, build from source:
requires a stable Rust toolchain ([rustup](https://rustup.rs)).

```sh
cargo build --release        # binary at target/release/markcheck
# or install into ~/.cargo/bin:
cargo install --path .
```

Clipboard copy uses the system clipboard when an X11 or Wayland session is
available, and otherwise falls back to an OSC 52 terminal escape (see
[Clipboard](#clipboard)) — no manual setup either way; the Wayland clipboard
backend builds in automatically.

**Platform support:** Linux and macOS only — this is what's tested and what
CI/releases build for. Windows is not supported: the `o` link-opener falls
back to `xdg-open`, and the editor fallback (when neither `$VISUAL` nor
`$EDITOR` is set) is `vi`, neither of which exists on a stock Windows
install. `cargo install --path .` will build on Windows, but will fail at
runtime the first time either fallback is hit.

## Usage

```sh
markcheck runbook.md
markcheck --no-nerd-font runbook.md   # plain-Unicode icons
markcheck --no-mouse runbook.md       # leave terminal text selection alone
markcheck --primary runbook.md        # also copy to the X11 PRIMARY selection
markcheck --auto-copy runbook.md      # copy a task's command as you navigate to it
markcheck --git-sync runbook.md       # commit + push on toggles/edits (needs a git repo)
markcheck --new runbook.md            # create a starter checklist, then open it
markcheck --version                   # print the version and build's git commit
```

`--new PATH` creates a new file at `PATH` and opens it, instead of opening an
existing one — `PATH` must not already exist and must end in `.md`
(case-insensitively). The starter content is a title derived from the
filename (`meeting-notes.md` → `# Meeting Notes`) and two blank tasks ready
to fill in. `--new` and the positional `FILE` argument are mutually
exclusive — pass exactly one.

## Config file

Set defaults for the four flags above so you don't have to pass them every
time, in a TOML file at `$XDG_CONFIG_HOME/markcheck/config.toml` (falling
back to `~/.config/markcheck/config.toml` if `XDG_CONFIG_HOME` isn't set).
All keys are optional; a flag passed on the command line always overrides
its config value.

```toml
nerd_font = true    # false = --no-nerd-font
mouse = true        # false = --no-mouse
primary = false     # true  = --primary
auto_copy = false   # true  = --auto-copy
git_sync_paths = ["/home/you/checklists"]  # auto-enable --git-sync under these paths
```

No config file is not an error — `markcheck` just uses the built-in
defaults shown above. A config file that exists but fails to parse (bad
TOML, or an unrecognized key — a likely typo) **is** an error: `markcheck`
refuses to start and reports the problem, rather than silently ignoring a
setting you asked for.

`git_sync_paths` is the odd one out: a list of path prefixes rather than a
plain default. A file whose path starts with one of them gets git-sync
turned on automatically, the same as passing `--git-sync` for that one run.
`--git-sync` and `git_sync_paths` are additive (either one turns it on),
unlike the other four flags, where a passed flag simply overrides the
config value.

## Markdown

`markcheck` reads an ordinary Markdown task list and treats a handful of
constructs specially. Example:

```markdown
# Server upgrade

## Prepare workspace

- **Run on the staging host only**
- [ ] `refresh-cache`
- [/] run `build-tool sync --profile default` and wait for it to finish
- [x] `restart-service`

### Post-checks

- [ ] `verify-output`

## Second workspace

- [ ] First check the status: `check-status example-host`
```

How each piece maps to the UI:

| Markdown | How it's shown |
| ---------- | ---------------- |
| `# Heading` | **Document title**, in the title bar beside the filename. The first `# H1` wins; with no H1, a bold-red **"Missing document title"** stands in. |
| `## Heading` | A **list**: its title (the **list header**) appears above the cards and in the overview, and is jumpable with `1`–`9` (and in a dedicated list-tab row on narrow terminals). |
| `### Heading` (and deeper, `####`–`######`) | A **sub-section** labeling a group of items *within* the current list: a `── Heading` divider above the group in the overview, and a dim `Outer › Inner` breadcrumb above the card. Deeper levels nest. Not a card; the heading itself isn't a cursor target, though `}` / `{` jump between sub-sections. |
| `- [ ]` / `- [/]` / `- [x]` | A **task card** — not started / started / done. |
| `- **Bold** rest of line` | The leading **bold** becomes the **card title**, shown inside the card (info icon, blue); the rest of the line is the card body. |
| `- **Bold**` (bold only), **first** item of a list | The list's **banner** — a highlighted warning line (info icon, amber) shown below the list title and in the overview. Not a card. |
| `- **Bold**` (bold only), later in a list | A display-only card titled with the bold text. |
| `- plain text` | A display-only **note card** (rounded border). In the overview it shows a distinct note marker rather than a task circle, so information reads apart from steps. |
| a note with an indented sub-list | The note reflects its children's progress: its marker turns yellow once any sub-task is under way and green when all are done — on the card and in the overview — while staying a note (never a checkbox). |
| `` `code` `` | An inline command chip; copy it with `y`. |
| a fenced ```` ``` ```` block | A command block shown in a box; copy it with `y`. |
| `*italic*` / `**bold**` (mid-line) / `~~strike~~` | Inline styling in the card body — rendered *italic*, **bold**, and ~~struck through~~. (A **leading** bold is still the card title, above.) |
| `[text](url)` | A **link**: the card shows the underlined `text`, the URL appears centered below the card (not on it), and `o` opens it. Several links on one card get `[1]`/`[2]` markers keyed to the list below the card. |
| `> - [ ]` (a task inside a blockquote) | A **non-interactive** display-only card — a quoted/illustrative example (e.g. from another runbook), never a live, toggleable task. |

Finer points:

- **A trailing command gets its own line.** When a task *ends* with an inline
  `` `code` `` command (nothing after it), the command drops onto its own line
  below a blank line, so the lead-in and the command read as two tidy blocks.
  Inline code that has text after it stays in the sentence.
- **Copying (`y`)** works when a task has exactly one command — a single inline
  `` `code` `` span or a single fenced block. With none or several, nothing is
  copied and the status bar explains why.
- **A link's URL is shown below the card, not on it.** The card shows only the
  underlined link text (a long URL used to wrap awkwardly across card rows); the
  destination appears centered in the space below the card — `→ <url>` for a
  single link, or a numbered `[1] <url>` / `[2] <url>` list (one per line)
  matching the `[1]`/`[2]` markers on the card when it has several links. (On a
  very short terminal with no room below the card, the full URL list is hidden
  rather than pushing the card around, but the card's border shows a compact
  `→ link` hint so you still know one is there.)
- **Opening links (`o`)** launches your `$BROWSER` (falling back to `xdg-open` /
  `open`). With exactly one link, `o` opens it. With **several**, the URLs stay
  listed below the card and the status bar shows `press 1–N to open · esc
  cancels` — press the number (matching the card's `[1]`/`[2]` markers) to open
  that one, or Esc to cancel. With no link, the status bar says so.
- **Only `http://`, `https://` and `mailto:` links open.** Any other link —
  `file://`, `javascript:`, `data:`, or a schemeless one like `example.com` —
  still renders normally on the card, but `o` refuses it and says so. This keeps
  a checklist you didn't write from handing something unexpected to your
  browser or desktop opener.
- **`[/]` (started / in-progress)** is markcheck's own convention; GitHub and
  most tools render it as literal text rather than a checkbox.
- **A blockquoted checkbox is never a live task.** `> - [ ] ...` reads as a
  quoted example (e.g. illustrating another runbook's output) rather than
  something to actually do, so it always renders as a plain, non-toggleable
  note card — even if it would otherwise have qualified as the list's banner
  or been checked/started in the source file.
- **Nested / sub-lists are supported.** An indented checklist under a bullet
  (a task *or* a plain note) keeps its hierarchy: every item is a card you can
  navigate and toggle, the overview draws color-coded depth guides (`│`) — each
  sub-list gets its own color, and each list starts on a different one, so
  nesting is glanceable and separate sub-lists don't blend — and a sub-item's
  card shows its parent chain as a matching color-coded breadcrumb (e.g.
  `Prepare › Sub ›`).
- **`### H3`+ headings group items into sub-sections.** Within a list, an
  `### Heading` (or deeper `####`–`######`) starts a labeled group: the overview
  draws a `── Heading` divider above the following cards, and each card shows the
  active path as a dim `Outer › Inner` breadcrumb above it. Deeper levels nest
  under shallower ones; a new heading at the same or a shallower level starts a
  fresh group. A sub-heading with no items under it (immediately followed by
  another heading) shows nothing. Sub-sections are just labels — not cards, and
  they don't change the progress counts — but you can jump between them with
  `}` / `{`, filter for them in the `T` picker, and the overview pins the
  full current path — the list name plus every active sub-heading — at the top
  of the panel once those rows scroll out of view, so a long group stays
  labeled with its complete context.

### Not supported (and how it degrades)

- **A list with no checkboxes is skipped** — a `##` list (or the pre-heading
  area) whose list contains no `- [ ]` items is dropped entirely: no tab, and
  it isn't navigable.
- **`###`+ headings are sub-section labels, not titles** — only `#` and `##`
  are structural (document title / list). `###` and deeper now label
  sub-sections *within* a list (above), rather than being ignored, but they
  never create a new list or the document title.
- **A link entirely inside the leading-bold title loses its URL.** `- [ ]
  **[text](url) more** body` shows "text more" as the card title like any
  other bold title, but the link's URL is discarded — `o` can't open it (only
  links in the body are openable). Put the link after the bold title instead
  if it needs to be openable.
- **Files are read as UTF-8.**

## Keybindings

The shortcuts are **vim-inspired**: `h`/`j`/`k`/`l` (and the arrows) all move
between tasks, `Ctrl-E`/`Ctrl-Y`/`Ctrl-D`/`Ctrl-U` scroll the card body like a
vim viewport, `gg`/`G` jump to the first/last task, `y` yanks (copies), and the
capitals `Shift-H`/`Shift-L` make the bigger jumps between lists.

| Key | Action |
| ----- | -------- |
| `h` / `l`, `←` / `→` | previous / next task; at a list's edge, cross to the previous/next list (landing on its first unfinished task) |
| `k` / `j`, `↑` / `↓` | previous / next task (same as `h`/`l` — a checklist reads as a list) |
| `gg` / `G` | jump to the first / last task in the document |
| `Ctrl-E` / `Ctrl-Y` | scroll the card body down / up one line (when it overflows) |
| `Ctrl-D` / `Ctrl-U` | scroll the card body down / up half a page |
| `PageDown` / `PageUp` | scroll the card body down / up one page |
| mouse wheel | scroll an overflowing card, otherwise navigate tasks |
| left-click a command on the card | copy that specific command; clicking elsewhere on the card copies its sole command (same as `y`). Needs mouse capture, so not with `--no-mouse` |
| left-click an overview row | on a task row, click its **icon** to toggle it done/not-done (cursor stays put), or its **label** to jump to that task; click a list title to jump to that list. Needs mouse capture, so not with `--no-mouse` |
| `Space` / `Enter` | toggle the current task done; on an info card (nothing to toggle) advance to the next card instead |
| `s` | mark the current task started / in progress (`[/]`) |
| `u` / `Ctrl-R` | undo / redo the last change (toggle, start, or reset) |
| `Tab` | jump to the next unfinished task (anywhere in the document; wraps around) |
| `Shift-H` / `Shift-L` | jump to the previous / next list with unfinished tasks |
| `/` | search tasks by text — jump to the first match as you type |
| `n` / `N` | jump to the next / previous search match |
| `}` / `{` | jump to the first task of the next / previous sub-section (within the current list) |
| `T` | open the "go to task" overlay: a filterable list of every task |
| `y` | copy the task's command to the clipboard |
| `o` | open the current card's link in your browser |
| `o` then `1`–`9` | when a card has several links, open link `[N]` |
| `e` | edit the file in `$EDITOR` (at the current task's line for editors that support it) |
| `R` | reset all tasks to not-done (asks first) |
| `1`–`9` | jump to list N |
| `?` | show the keybinding help overlay (scrollable — `j`/`k`/arrows/`Ctrl-D`/`U` — when it doesn't fit; any other key closes) |
| `q` / `Esc` | quit |

Both digit shortcuts above (`1`–`9` list jump, `o` then `1`–`9` link open) only
reach the first nine items — a document with a 10th `## H2` list, or a card
with a 10th link, has no single-keypress (or click) shortcut for it. Reach a
distant list with `Tab` / `Shift-H` / `Shift-L` / `T` instead; a 10th+ link has
no way to open it directly (it's still listed, just not openable, in the
panel below the card).

When you finish a list a summary card appears — `l` / `Enter` moves to
the next list that still has incomplete tasks (skipping any already
done), `h` reviews the one you just finished. If you go back to review, you
can still move on: `Tab` jumps to the next unfinished task anywhere (wrapping
around), `Shift-L` jumps to the next unfinished list (`Shift-H` the
previous), and `l` at the end of a list steps into the next list.
Finishing every list shows an all-done summary. Jumping to a list (or
starting up) lands on its first not-done task.

## Behavior

### Write-back

Toggling a task rewrites only its `[ ]`/`[/]`/`[x]` marker character; the rest
of the file (formatting, comments, other lines) is left untouched. Writes are
atomic — a temp file is written and renamed over the original — so a crash
or full disk can never leave the runbook truncated, and the file's
permissions are preserved. Opening the file through a symlink writes
through to the real target and keeps the link intact.

If the file changed on disk since it was last loaded — another `markcheck`
instance, or an external editor, saved to it after you did — a toggle is
refused rather than silently overwriting that change: the file is reloaded
and a sticky error asks you to retry.

### Live reload

If the file changes on disk while the app is open, `markcheck` reloads it
automatically and keeps your place on the same task where possible. A
relative `Updated 10s ago` tag — prefixed with a refresh icon (`↻`, or a Nerd
Font glyph) — appears in the title bar, grouped with the progress counter on
the right, whenever the file changes — whether from your own toggles or an
external edit — and disappears two minutes later. The tag steps in 5-second
increments rather than ticking every second. Your own toggles never trigger
a spurious reload.

If the file is **deleted** from under the app, `markcheck` won't recreate it
behind your back: it shows a red `File deleted — changes cannot be saved`
message and blocks toggling and reset until the file comes back. Restore the
file (or save it again from your editor) and the app reloads it and resumes
normally.

### Git sync

With `--git-sync` (or a matching `git_sync_paths` entry, above) active for a
file that's already in a git repository, every toggle/start/reset/undo/redo
commits and pushes the change in the background, so a checklist shared via
git stays in sync across machines without leaving the app. Changes are never
lost, but if several land while a previous commit/push is still in flight,
they coalesce into one commit rather than one apiece — the commit message
then names only the last of them, though its content still reflects every
change up to that point. Editing the file
in `$EDITOR` (`e`) syncs too, as its own commit (`checklist.md: Edited in
$EDITOR`) — even if you only edit and quit without toggling anything
afterward. It only ever
touches files git already tracks — a new/untracked file next to the
checklist is never picked up and never committed, and the same goes for the
checklist file itself: if it isn't tracked yet, git-sync won't `git add` it
for you — it reports `Git sync skipped: file is not tracked in git` in the
status bar instead of silently doing nothing, so it's clear a `git add` is
needed before syncing can start. Commit messages name the
actual change (`checklist.md: Check "Restart service"`, `checklist.md:
Reset all tasks to not done`, …) rather than a generic "updated by
markcheck", so `git log`/`git blame` stay useful across hosts. A failed
commit (diverged history, mid-merge, …) is reported as a red status-bar
message — `markcheck` never attempts to resolve it for you (no auto
`git pull --rebase`), so fix it in a terminal as usual. A commit that
succeeds but fails to *push* (offline, no upstream configured, …) is
reported separately (`Git commit saved locally; push failed, will retry`) —
nothing is lost, and `markcheck` retries the push on its own every 30
seconds, on the next sync of any kind, and once more right at startup, so
you don't have to make another edit just to nudge it once the network's
back. Since the sync runs on a background thread, a slow or offline `git push`
never freezes the app during normal use. Quitting right after a toggle or
edit is the one exception: `markcheck` waits up to 5 seconds for that last
sync to finish before exiting, so a quick `e` then `q` doesn't leave the
change unsynced.

`git push` sends the whole current branch, the same as running it yourself
with no arguments — if you have other local commits ahead of the remote
(from work outside markcheck), those get pushed too, not just the checklist
commit. Also refuses to run while the repository is mid-merge, mid-rebase,
mid-cherry-pick, on a detached `HEAD`, or has any unresolved conflict
anywhere in it — an automatic commit in any of those states could silently
interfere with work already in progress, so it reports a status-bar error
and does nothing instead, until you resolve it yourself in a terminal.

Whenever git-sync is actually active for the current file, `⇅ git` sits in
the title bar too, grouped with the `Updated` tag and the progress counter
on the right — the whole right-hand side reads as "what's going on with
this file right now," while the left side (title and filename) stays put as
plain identity. `⇅ git` is there from the moment the app opens, before
you've even toggled anything. (It's the same plain `⇅` symbol either way,
with or without `--no-nerd-font` — a couple of fancier icon attempts along
the way didn't render reliably on every terminal, so the word "git" carries
the meaning now rather than the glyph.) After a successful sync it's
followed by `· Synced 8s ago`, fading back to just `⇅ git` after two
minutes. It's fully hidden — no icon, no text, no gap — when git-sync
wasn't requested or the file isn't in a repo.

`--git-sync` only activates when the file's directory is confirmed to be
inside a git work tree at startup; on a file that isn't in a repo, the flag
(or a `git_sync_paths` match) simply has no effect.

Each commit runs from a private, temporary git index — never your real one —
so a `git add`ed-but-not-yet-committed file of yours can never ride along
into a markcheck commit. That index still runs the repository's normal
commit hooks (and honors `commit.gpgsign`) exactly like committing by hand,
so a `pre-commit` hook that itself runs `git add` against that same
temporary index can try to pull other files into the commit alongside the
checklist — but markcheck checks for this after the commit is made and
undoes it if so, reporting `Git sync failed: ...a commit hook modified
files beyond the checklist; sync aborted` rather than letting the extra
files through (let alone pushing them). The hook itself still runs
normally either way; only the resulting commit's scope is enforced.

### Search

`/` starts an incremental search: as you type, the cursor jumps to the first
task whose text matches, with the query and the **number of matches** shown in
the status bar (and a clear "no matches" when nothing hits). Press `Enter`
to keep the match — the status bar then reports your position (`Match 2/5`) as
`n` / `N` cycle forward / backward through the rest, wrapping around — or `Esc`
to cancel and return to where you were. Matching is
**smart-case** (case-insensitive unless your query has an uppercase letter) and
covers a task's title, its body text, *and* its commands — so you can find a
step by the command it runs, not just its description.

`T` opens a **"go to task" overlay** — a filterable list of every card (tasks
and note cards alike) with its state marker and a dim `— {list} › {sub-section}`
suffix (the list name, when there's more than one, then the item's `### H3`+
sub-section path), for when you want to see the whole checklist at once instead
of the single-card carousel. Type to filter (same smart-case matching, with a
live `N items` / match count — and you can filter by a sub-section name to
find the tasks under it), move the highlight with the arrows or
`Ctrl-N`/`Ctrl-P` (and `Ctrl-D`/`Ctrl-U` for half-page jumps; letters go to
the filter, so `j`/`k` type rather than move), `Enter` to jump to the
highlighted task, `Esc` to close.

### Clipboard

`y` copies a task's command. If the task has a single inline `` `code` ``
span or a single fenced code block, that content is copied; if there are
none or several, nothing is copied and the status bar says why. Copy uses
the system clipboard when available and otherwise emits an OSC 52 escape,
which travels over SSH to your local terminal emulator (kitty, alacritty,
wezterm, foot, and others; inside tmux, set `set-clipboard on`). The status
bar distinguishes a system-clipboard copy from an OSC 52 one.

OSC 52 is an out-of-band channel: the escape sequence rides over whatever
carries your terminal session, including a chain of SSH hops, so a copied
command is exposed to anything in that path capable of reading terminal
output. If a task's command embeds a credential or token, `y` sends it the
same way as any other command — worth keeping in mind for a runbook shared
in a less-trusted environment.

When a task has **several** commands, `y` won't guess between them — instead
**left-click the row** of the command you want and that specific one is
copied (an inline command, the trailing command, or a fenced block). Clicking
anywhere else on the card copies its sole command, the same as `y`.

Passive status messages — copy confirmations, reloads, resets — clear
themselves after a few seconds. Messages that answer a keypress stay until your
next one, so you won't miss them: a failed copy or reload, or a `y`/`R` that
found nothing to do (no code to copy, or nothing to reset).

`--primary` additionally copies to the X11 PRIMARY selection (middle-click
paste) alongside the normal clipboard. `--auto-copy` copies a task's
command automatically whenever you navigate to its card, with no manual
`y`. It confirms with the same status message as `y` on a successful copy,
and stays silent when a card has no command or an ambiguous one.

### Undo and redo

`u` undoes the last state-changing action — a toggle, a start (`s`), or a whole
`R` reset — restoring the previous state, writing it back, and jumping to the
task that changed so you can see it. `Ctrl-R` redoes what you just undid; making
a fresh change after an undo clears the redo history (the usual undo/redo rule).
The status bar confirms each step (`Undo: marked not done`, `Redo: marked done`,
`Undo: restored 3 tasks`) and says `Nothing to undo` / `Nothing to redo` when the
history is empty. The history is in-memory and covers task state only; editing
the file **externally** (in another editor) clears it, since that edit could
change the file underneath the undo history — your own toggles never do.

### Editor and reset

`e` suspends the TUI, opens the file in `$VISUAL` / `$EDITOR` (falling back
to `vi`), and reloads on exit. For editors that use the `+N file` convention
(`vi`/`vim`/`nvim`/`view`, `nano`, `pico`, `emacs`/`emacsclient`, `joe`,
`gedit`) the file opens on the selected task's line; other editors (VS Code,
Sublime, Helix, …) open at the top of the file. `R` resets every task to not-done after a
confirmation prompt (press `y` to confirm — any other key cancels). When
you quit with every task already done, `markcheck` first asks whether to
reset the checklist so it's ready to run again: `y` resets and quits, `n`
quits as-is, `Esc` stays in the app.

## Architecture

See [DESIGN.md](DESIGN.md) for the internal design and module layout.

## License

BSD-3-Clause — see [LICENSE](LICENSE).
