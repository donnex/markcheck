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
  - [Terminal title and exit line](#terminal-title-and-exit-line)
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
- Four task states — not started, **started/in-progress** (`[/]`),
  **skipped** (`[-]`), and done — each with its own card color and icon
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
- Reset the whole checklist, the current list or the sub-list you're in
  (with confirmation), and an offer to reset when
  you quit a fully-completed checklist
- Mouse and keyboard driven; plain Unicode icons that work on any terminal, or Nerd Font ones with `--nerd-font`
- Colors adapt to your terminal: curated 24-bit hues on truecolor terminals,
  a 256-color match otherwise, and named ANSI colors as a safe fallback.
  Truecolor is detected from `COLORTERM=truecolor`/`24bit`, or from a
  `TERM` of `xterm-kitty`, `alacritty`, `foot`, `foot-extra`, `wezterm`,
  `contour`, `xterm-ghostty` or one ending in `-direct` (so those terminals
  keep their colors over SSH, where `COLORTERM` is usually not forwarded);
  256 colors from a `TERM` containing `256color`. `--color
  auto|truecolor|256|16` (or the `color` config key) forces a palette when
  detection gets it wrong. Set `NO_COLOR` (to any non-empty value, per [no-color.org](https://no-color.org/))
  to turn color off entirely

## Installation

Pre-built binaries (Linux x86_64) are attached to each
[release](https://github.com/donnex/markcheck/releases) as a
`.tar.gz` with a `.sha256` checksum — download, verify, extract, and put
`markcheck` on your `$PATH`. The archive also holds `LICENSE` and
`THIRD-PARTY-LICENSES`, the license texts of every crate compiled into the
binary, a man page and shell completions:

```sh
cp markcheck.1 ~/.local/share/man/man1/                          # then: man markcheck
cp completions/markcheck.bash ~/.local/share/bash-completion/completions/markcheck
cp completions/_markcheck ~/.zfunc/                               # a directory on your $fpath
cp completions/markcheck.fish ~/.config/fish/completions/
```

A source build can make the same files with `markcheck --generate-man` and
`markcheck --generate-completions bash|zsh|fish`, which print to stdout. The binary is statically linked (musl), so it
runs on any x86_64 Linux, old or new, whatever its C library. For any other
platform, build from source: that needs Rust 1.88 or newer (`rustc
--version`; install or update it with [rustup](https://rustup.rs)).

```sh
# install straight from the repository into ~/.cargo/bin:
cargo install --git https://github.com/donnex/markcheck
# or from a checkout:
git clone https://github.com/donnex/markcheck
cd markcheck
cargo build --release        # binary at target/release/markcheck
cargo install --path .       # or install it into ~/.cargo/bin
```

Clipboard copy uses the system clipboard when an X11 or Wayland session is
available, and otherwise falls back to an OSC 52 terminal escape (see
[Clipboard](#clipboard)) — no manual setup either way; the Wayland clipboard
backend builds in automatically.

**Platform support:** Linux is what CI builds and tests, and the only
platform release binaries are published for. macOS is expected to work
(build from source) but is not tested anywhere, so a macOS-only regression
could go unnoticed. Windows and other non-Unix systems are not supported:
building there stops with the error "markcheck supports Unix-like systems
only".

## Usage

```sh
markcheck runbook.md
markcheck runbook.md#rollback         # open at the first task under "## Rollback"
markcheck +120 runbook.md             # open at the task on (or after) line 120
markcheck --nerd-font runbook.md      # Nerd Font icons, if your terminal has one
markcheck --no-nerd-font runbook.md   # ...or plain Unicode again, if your config enables them
markcheck --no-mouse runbook.md       # leave terminal text selection alone
markcheck --mouse runbook.md          # ...or force it back on, if your config disables it
markcheck --primary runbook.md        # also copy to the X11 PRIMARY selection
markcheck --auto-copy runbook.md      # copy a task's command as you navigate to it
markcheck --no-auto-copy runbook.md   # ...or turn it off for one run, if your config enables it
markcheck --color 256 runbook.md      # force a palette: auto, truecolor, 256 or 16
markcheck --clipboard osc52 runbook.md  # copy through the terminal only: auto, system or osc52
markcheck --no-terminal-title runbook.md  # leave the terminal title alone
markcheck --no-exit-summary runbook.md    # print nothing after quitting
markcheck --git-sync runbook.md       # commit + push on toggles/edits (needs a git repo)
markcheck --no-git-sync runbook.md    # ...or never, even under a git_sync_paths entry
markcheck --git-sync-delay 0 runbook.md  # commit every change at once instead of after 10 s
markcheck --config other.toml runbook.md  # read another config file instead
markcheck --no-config runbook.md      # ignore the config file for this run
markcheck --new runbook.md            # create a starter checklist, then open it
markcheck --version                   # print the version and build's git commit
```

`--new PATH` creates a new file at `PATH` and opens it, instead of opening an
existing one — `PATH` must not already exist and must end in `.md`
(case-insensitively). The starter content is a title derived from the
filename (`meeting-notes.md` → `# Meeting Notes`) and two blank tasks ready
to fill in. It works on filesystems without hard links too (FAT32/exFAT
USB sticks and SD cards, many network shares). `--new` and the positional
`FILE` argument are mutually exclusive — pass exactly one.

## Config file

Set defaults for the flags above so you don't have to pass them every
time, in a TOML file at `$XDG_CONFIG_HOME/markcheck/config.toml` (falling
back to `~/.config/markcheck/config.toml` if `XDG_CONFIG_HOME` isn't set).
All keys are optional; a flag passed on the command line always overrides
its config value.

```toml
nerd_font = false   # true  = --nerd-font,    false = --no-nerd-font
mouse = true        # false = --no-mouse,     true = --mouse
primary = false     # true  = --primary,      false = --no-primary
auto_copy = false   # true  = --auto-copy,    false = --no-auto-copy
color = "auto"      # = --color: "auto", "truecolor", "256" or "16"
clipboard = "auto"  # = --clipboard: "auto", "system" or "osc52"
terminal_title = true  # false = --no-terminal-title, true = --terminal-title
exit_summary = true    # false = --no-exit-summary,   true = --exit-summary
git_sync_paths = ["/home/you/checklists"]  # auto-enable --git-sync under these paths (end with /** to include nested repositories)
git_sync_delay = 10 # = --git-sync-delay: seconds without a change before git sync commits; 0 = at once, at most 86400 (a day)
```

No config file is not an error — `markcheck` just uses the built-in
defaults shown above. A config file that exists but fails to parse (bad
TOML, or a known key with the wrong type of value) **is** an error:
`markcheck` refuses to start and reports the problem, rather than silently
ignoring a setting you asked for.

An unrecognized key is **not** an error: `markcheck` starts, ignores that
key, and shows a sticky warning naming it (``Config: unknown key
`new_option` ignored — a typo, or a setting from a newer markcheck?``).
That keeps a typo visible while letting one config file be shared between
machines running different releases — a setting added after upgrading one
machine doesn't stop the older ones from starting.

`--config PATH` reads the config from `PATH` instead of the default
location (the file must exist), and `--no-config` ignores the config file
entirely for one run — built-in defaults plus whatever flags you pass —
which gets you past a broken config without editing it. The two are
mutually exclusive. `markcheck --help` also names the default location.

Each of the six boolean settings has a flag in both directions, so a config
value can always be overridden for a single run:
`--nerd-font`/`--no-nerd-font`, `--mouse`/`--no-mouse`,
`--primary`/`--no-primary`, `--auto-copy`/`--no-auto-copy`,
`--terminal-title`/`--no-terminal-title`, and
`--exit-summary`/`--no-exit-summary`.
`color`, `clipboard` and `git_sync_delay` work the same way: `--color`,
`--clipboard` and `--git-sync-delay` override them, and `--color auto` goes
back to detecting the terminal. An explicit `color` (from either) also wins
over `NO_COLOR`, per [no-color.org](https://no-color.org/).

`git_sync_paths` is the odd one out: a list of path prefixes rather than a
plain default. A file whose path starts with one of them gets git-sync
turned on automatically, the same as passing `--git-sync` for that one run.
`--git-sync` and `git_sync_paths` are additive (either one turns it on),
unlike the six booleans above, where a passed flag simply overrides the
config value. To turn it off for one run under a matching entry (a
rehearsal, working offline, committing by hand), pass `--no-git-sync`: the
file is still saved on every toggle, but nothing is committed or pushed and
the title bar shows no `⇅ git` section. It can't be combined with
`--git-sync`.

A leading `~` in a `git_sync_paths` entry means your home directory
(`$HOME`), as it would in a shell, so `"~/checklists"` works. An entry that
doesn't exist can never match anything, so `markcheck` says so at startup
(`git_sync_paths: ~/checklsts does not exist — ignored`) rather than leaving
you to wonder why git-sync never turns on.

An entry covers only the repository it names (or one it sits in). A
separate repository *nested* somewhere inside it, such as a project folder
copied in or an unpacked archive, is not synced, because git would run that
repository's own hooks and settings, chosen by whoever made it, just because
you opened a checklist there. `markcheck` says so at startup (`Git sync off:
the checklist's repository (...) is nested inside git_sync_paths entry
~/notes; ...`). To cover nested repositories too, end the entry with `/**`,
as in `"~/work/**"`; `--git-sync` on the command line also works for one
run.

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
| `# Heading` | **Document title**, in the title bar beside the filename. The first `# H1` wins; with no H1, a title made from the filename stands in (`release-checklist.md` shows **Release Checklist**). Tasks directly under the title, or before any heading, go to a first list named after the title: no heading is shown above its cards, but where lists are named (the overview, the list tabs, the `T` picker, the list-reset message and its git-sync commit) it goes by the document title, or by the filename-derived one when the file has no H1 or its first H1 comes after these tasks. |
| a later `# Heading` | A **list**, the same as a `## Heading`, so a file split into phases with `# Upgrade`, `# Verify`, `# Rollback` gets one list per phase. A `# H1` that only has `## H2` lists under it, and no tasks of its own, isn't shown. An H1 after the first tasks of the file starts a list too, even when it is the first H1 (it is then also the title). |
| `## Heading` | A **list**: its title (the **list header**) appears above the cards and in the overview, and is jumpable with `1`–`9` (and in a dedicated list-tab row on narrow terminals). |
| `### Heading` (and deeper, `####`–`######`) | A **sub-section** labeling a group of items *within* the current list: a `── Heading` divider above the group in the overview, and a dim `Outer › Inner` breadcrumb above the card. Deeper levels nest. Not a card; the heading itself isn't a cursor target, though `}` / `{` jump between sub-sections. |
| `- [ ]` / `- [/]` / `- [x]` | A **task card** — not started / started / done. An uppercase `[X]` counts as done too; it keeps its casing until toggled, and is written back as `[x]`. |
| `- [-]` | A **skipped** task — decided not to do it this run (it doesn't apply). Shown dimmed and struck through with a `⊘` marker. It counts as resolved, so a list of done and skipped tasks is complete, but it is left out of the done count, which the title bar reports apart (`☑ 7/9 (+2 skipped)`). The same marker as Obsidian's task plugins use for cancelled. |
| `1. [ ]` / `1) [/]` / `1. [x]` | An **ordered (numbered) list** has the same four task states, cards and notes as a bulleted one, and each item keeps its **step number** exactly as written (`3.`, `7)`, gaps and all): in front of the card title, or of the card body when there's no title, on the overview row, in the `T` picker (where typing `3.` finds step 3), and in the side cards. The card's ` n/m ` position still counts every card in the list, notes and sub-items included, so it can differ from the step number. |
| `- **Bold** rest of line` | The leading **bold** becomes the **card title**, shown inside the card (info icon, blue); the rest of the line is the card body. Inline `` `code` `` inside the title is shown as plain title text, not as a command chip, so `y` never copies it — put a command you want to copy in the body. A long title wraps inside the card, which grows to fit. The dimmed previous/next side cards in the wide layout show the title too, above the body. |
| `- [ ] **! Bold** rest` (a task's bold title starting with `!`) | A **critical step**: the `!` is dropped from the title, the card is framed in the warning color with a `critical` label, and its overview row starts with a `!`. Checking or skipping it takes two presses: the first `Space` only asks "Critical step — press Space again to check it", the first `-` asks for a second `-` to skip it, a first click on its overview marker asks for a second click on that marker instead, and any other key, click, scroll or paste leaves it open. The second press must come after a short pause (half a second): a quicker one, such as a held key's repeats or a reflex double press, keeps the step open and says it was too quick. Unchecking, unskipping, reset and undo stay one key. The `!` stays visible in an editor or on GitHub, so the file still says which steps are critical. |
| `- **Bold**` (bold only), **first** item of a list | The list's **banner** — a highlighted warning line (info icon, amber) shown below the list title and in the overview. Not a card. A long banner wraps onto as many rows as it needs above the card, so it is shown in full; on a terminal too short for both it and the shortest card it is the last row above the card to be cut, ending in `…`; the overview's one-row copy ends in `…` when cut. A bold-only bullet that carries code — `` `code` `` inside the bold, or a fenced or indented code block under the bullet — is never the banner: it stays a display-only card, so a code block under it is shown and copyable with `y`. Only a list that has a checkbox counts: a plain bullet list with no checkbox, which is dropped, neither supplies the banner nor takes the first-item slot from the task list after it. |
| `- **Bold**` (bold only), later in a list | A display-only card titled with the bold text. |
| `- plain text` | A display-only **note card** (rounded border). In the overview it shows a distinct note marker rather than a task circle, so information reads apart from steps. |
| a note with an indented sub-list | The note reflects its children's progress: its marker turns yellow once any sub-task is under way and green when all are done — on the card and in the overview — while staying a note (never a checkbox). |
| `` `code` `` | An inline command chip (light text on a dark gray chip, on light and dark terminal themes alike); copy it with `y`. With several, each chip gets a dim `[1]`/`[2]` marker for `y` then a digit. |
| a fenced ```` ``` ```` block | A command block shown in a box, styled like the chip; copy it with `y`. With several, each box shows `[1]`/`[2]` in its top border for `y` then a digit. A tab in a command (fenced or inline) is shown as spaces up to the next 4-column stop, so tab indentation stays visible; copying still gives the real tabs. |
| an indented (4-space) code block inside a task or note | Treated exactly like a fenced block: shown in a box and copyable with `y`. Outside a list item it is treated like any other code block: shown above the first task under its heading (see the preamble row below), otherwise counted in the startup "not shown" note. |
| a fenced block **directly after a list** (at column 0) | **Attached to that list's last card**, as if it were indented under it: shown in a box and copyable with `y`. Anything in between — a paragraph, a heading, another block — breaks the attachment, and a fence after a list that was dropped (no checkboxes) attaches nowhere; such a fence is not shown. |
| `*italic*` / `**bold**` (mid-line) / `~~strike~~` | Inline styling in the card body — rendered *italic*, **bold**, and ~~struck through~~. (A **leading** bold is still the card title, above.) |
| `[text](url)` | A **link**: the card shows the underlined `text`, the URL appears centered below the card (not on it), and `o` opens it. Several links on one card get `[1]`/`[2]` markers keyed to the list below the card. |
| `[Rollback](#rollback)` (a link to a heading in the same file) | `o` **jumps** to the first task under that heading instead of opening a browser, and `Ctrl-O` jumps back. Anchors are GitHub's: the heading lowercased, punctuation dropped, spaces as `-`, and `-1`, `-2` added to repeats, counting quoted headings and headings inside a list item as GitHub does, so links that work on GitHub work here. A heading inside a list item (`- ## Rollback`) jumps to its own card, a quoted one to the first task after it, and a percent-encoded link copied from a browser, such as `#d%C3%A9ploiement` for `## Déploiement`, matches like the raw `#déploiement`. The same anchor opens a file there from the command line: `markcheck runbook.md#rollback`. |
| a bare URL (`https://example.com/status`, `www.example.com`) and `<https://example.com>` | Also a **link**, the same as `[text](url)` with the URL as its text — as on GitHub. Trailing punctuation (`.`, `,`, `!`, an unmatched `)`, …) is left out of the URL, and a `www.` one opens as `http://`. Not inside the bold card title or inline code. |
| a leading `---` (YAML) or `+++` (TOML) front-matter block | **Ignored** — not shown anywhere, and left untouched in the file. Only a block on the very first line counts; its keys (e.g. `title:`) are not used. |
| inline HTML (`<kbd>`, `<br>`, …) and HTML blocks | **Dropped.** Inline tags are not shown on the card, though the text between them is (`<kbd>Enter</kbd>` shows as `Enter`); a block of HTML, including an `<!-- comment -->`, is not shown at all. A dropped tag still counts as a space, so `line one<br>line two` shows as `line one line two` rather than running the words together. |
| a task or note continuing in a **second paragraph** (a blank line, then indented text) | Joined to the first paragraph with a space and shown as one flowing body, the same as a line wrapped in the source. Text after a code block or a sub-list inside the item joins the same way. |
| `- ## Heading` (a heading *inside* a list item, any level) | **Not structural** — read like a bold run: a leading one is the **card title** (so a heading-only bullet is a titled note card, or the list's banner when it's the first item), a later one is bold body text. It never starts a list or sub-section, or sets the document title. |
| `> ## Heading` (a heading inside a blockquote) | **Ignored** — a quoted callout's heading never starts a list or sub-section, or sets the document title. |
| `> - [ ]` (a task inside a blockquote) | A **non-interactive** display-only card — a quoted/illustrative example (e.g. from another runbook), never a live, toggleable task. |
| a paragraph, quote or code block under a heading, **before its first task** | The section's **preamble**: shown above the card of the first task below it (and as dim rows in the overview, cut to one line each), so a "take a snapshot first" note is read where the work starts. A paragraph under the `# H1` goes to the document's first task; prose under a `## H2` and under the `### H3` that follows both go to the first task under the `### H3`. Up to six rows are shown above the card; a longer preamble ends in `…`. A tall card shrinks (its text scrolls) to make room for the rows above it; only when even the shortest card leaves too little room are rows left out, in this order: the sub-section name, the list title, then preamble rows from the end, prose before alerts, the banner last, the last row kept ending in `…`. |
| `> [!WARNING]` / `> [!CAUTION]` / `> [!IMPORTANT]` (a GitHub alert) in a preamble | Shown in the **warning color** behind the note icon, like the banner, with its label (`Warning: …`). Other quotes are shown behind a `│` bar. |

Finer points:

- **A trailing command gets its own line.** When a task *ends* with an inline
  `` `code` `` command (nothing after it), the command drops onto its own line
  below a blank line, so the lead-in and the command read as two tidy blocks.
  Inline code that has text after it stays in the sentence.
- **A wrapped command is marked.** A command too long for the card — a line
  of a fenced or indented block, or an inline command longer than a whole row
  — is split across rows, and every row that continues onto the next ends in
  a dim `↩`. A row without it ends a real line, so a long command can't be
  mistaken for two. Copying (`y` or a click) still gives the original text.
- **Copying (`y`)** copies a task's command — a single inline `` `code` `` span
  in the body (code inside the bold card title doesn't count) or a single fenced (or indented) block. When there are several, each is numbered on the
  card and `y` then a digit copies that one. Code blocks (fenced or indented)
  take priority: if a
  task has any, only they are numbered, and its inline spans can be copied by
  clicking them. A command containing control
  characters the card can't show (escape, backspace, carriage return, bidi
  overrides) is never copied either — see [Clipboard](#clipboard).
- **A link's URL is shown below the card, not on it.** The card shows only the
  underlined link text (a long URL used to wrap awkwardly across card rows); the
  destination appears centered in the space below the card — `→ <url>` for a
  single link, or a numbered `[1] <url>` / `[2] <url>` list (one per line)
  matching the `[1]`/`[2]` markers on the card when it has several links. (On a
  very short terminal with no room below the card, the full URL list is hidden
  rather than pushing the card around, but the card's border shows a compact
  `→ link` hint so you still know one is there.)
- **Opening links (`o`)** launches your `$BROWSER` (falling back to `xdg-open` /
  `open`). `$BROWSER` follows the usual convention: a colon-separated list of
  commands tried in order until one starts (`BROWSER=firefox:w3m`), where `%s`
  marks the URL's place (`BROWSER="firefox --new-tab %s"`, `%%` for a literal
  `%`) and the URL is appended when there is no `%s`. The status bar shows
  `Sent to <program>: <url>` once the opener starts, and if it exits with an
  error within a few seconds (no browser it can use, say) a sticky
  `Couldn't open link (<program> exited with code N)` replaces it. With
  exactly one link, `o` opens it.
- **No browser here (headless / SSH):** on Linux, when `$BROWSER` is unset
  and there is no graphical session (neither `$DISPLAY` nor
  `$WAYLAND_DISPLAY` is set), `o` doesn't try to launch anything. It copies
  the link to the clipboard instead — over SSH that reaches your local
  terminal through OSC 52 — and says `No browser here — link copied to
  clipboard`. Set `$BROWSER` to force an opener anyway. With **several**, the URLs stay
  listed below the card and the status bar shows `press 1–N to open · esc
  cancels` — press the number (matching the card's `[1]`/`[2]` markers) to open
  that one, or Esc to cancel. On a card with ten or more links, type the
  digits of `[10]` and up in turn: a digit opens its link at once unless a
  longer number could still follow, so with twelve links `1` waits (the
  status bar shows `open [1…] of 1–12`), then `2` opens `[12]` and `Enter`
  opens `[1]`; `Backspace` deletes the digit. With no link, the status bar says so.
- **Only `http://`, `https://` and `mailto:` links open.** Any other link —
  `file://`, `javascript:`, `data:`, or a schemeless one like `example.com` —
  still renders normally on the card, but `o` refuses it and says so. This keeps
  a checklist you didn't write from handing something unexpected to your
  browser or desktop opener.
- **With `NO_COLOR` set**, nothing is colored: commands (chips and boxes) are
  shown in reverse video, and the card's position-dot strip marks a started
  item with a half dot `◐` (a done one stays `●`), so state still reads from the
  icons and glyphs alone.
- **`[/]` (started / in-progress)** is markcheck's own convention, and `[-]`
  (skipped) the one Obsidian's task plugins use; GitHub and most other tools
  render both as literal text rather than a checkbox. Either one is a state
  only where `[ ]` would be a checkbox; anywhere else, such as after the `#`
  of a heading inside a bullet (`- # [/] Wait for the rollout`) or on the
  line after an empty `-`, it stays on the card as written.
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
  it isn't navigable. `markcheck` counts its bullets in the startup note
  (`2 bullet points outside task lists not shown`), so a "before you start"
  list written without checkboxes doesn't vanish unnoticed. Blockquoted
  checkboxes don't count, since they are never
  live tasks: a `##` list whose only checkboxes are quoted (`> - [ ]`) is
  dropped too, and a file with no other checkboxes reports that it has no
  checklist items.
- **`###`+ headings are sub-section labels, not titles** — only `#` and `##`
  are structural (document title / list), and only outside list items and
  blockquotes. `###` and deeper now label
  sub-sections *within* a list (above), rather than being ignored, but they
  never create a new list or the document title.
- **A link entirely inside the leading-bold title loses its URL.** `- [ ]
  **[text](url) more** body` shows "text more" as the card title like any
  other bold title, but the link's URL is discarded — `o` can't open it (only
  links in the body are openable). Put the link after the bold title instead
  if it needs to be openable.
- **Front matter is skipped, not read.** A `title:` key does not become the
  document title; use a `# H1` for that. A `---` ... `---` pair anywhere but
  the first line is ordinary Markdown (a horizontal rule, and possibly an
  `##`-style heading underline), exactly as in any other Markdown renderer.
- **Prose between tasks, or after the last one, is not shown.** A paragraph,
  quote, table or code block outside any list item is shown only as a
  preamble, under a heading and before its first task (above). One between
  two tasks, after the last task, or under a heading with no tasks belongs to
  no card, so `markcheck` says at startup how many it isn't showing (`2
  paragraphs and 1 quote outside task lists not shown`). Tables are read as
  ordinary paragraphs. A fenced block right after a list still attaches to
  its last item (above).
- **Files are read as UTF-8.** A leading byte-order mark (BOM), as some
  Windows editors write, is tolerated — the first heading still counts — and
  is left in place when the file is saved.

## Keybindings

The shortcuts are **vim-inspired**: `h`/`j`/`k`/`l` (and the arrows) all move
between tasks, `Ctrl-E`/`Ctrl-Y`/`Ctrl-D`/`Ctrl-U` scroll the card body like a
vim viewport, `gg`/`G` jump to the first/last task, `y` yanks (copies), and the
capitals `Shift-H`/`Shift-L` make the bigger jumps between lists.

| Key | Action |
| ----- | -------- |
| `h` / `l`, `←` / `→` | previous / next task; at a list's edge, cross to the previous/next list (landing on its first unfinished task) |
| `k` / `j`, `↑` / `↓` | previous / next task (same as `h`/`l` — a checklist reads as a list) |
| `gg` / `G` | jump to the first / last task in the document (after the first `g` the status bar shows that `gg` is pending; any other key cancels it) |
| `Ctrl-E` / `Ctrl-Y` | scroll the card body down / up one line (when it overflows) |
| `Ctrl-D` / `Ctrl-U` | scroll the card body down / up half a page |
| `PageDown` / `PageUp` | scroll the card body down / up one page |
| mouse wheel | scroll an overflowing card, otherwise previous / next task like `k`/`j` (crossing list edges). In the `?` help overlay it scrolls the help, and in the `T` overlay it moves the highlight. Over the overview panel it still moves the task cursor; the panel follows it |
| left-click a command on the card | copy that specific command; clicking elsewhere on the card acts like `y`. Needs mouse capture, so not with `--no-mouse` |
| left-click an overview row | on a task row, click its **icon** to toggle it done/not-done (cursor stays put), or its **label** to jump to that task; click a list title to jump to that list. Needs mouse capture, so not with `--no-mouse` |
| `Space` / `Enter` | toggle the current task done; after checking a task, move to the next card that isn't a done task (skipping done tasks, wrapping to the list's first open task), so pressing it again never unchecks a finished step; on an info card (nothing to toggle) advance to the next card instead; on a critical step (`**! Title**`), the first press only asks for a second, which must come after a short pause |
| `s` | mark the current task started / in progress (`[/]`) |
| `-` | skip the current task (`[-]`: it doesn't apply this run), or unskip it; skipping moves on to the next open task; on a critical step, the first press only asks for a second, after a short pause |
| `u` / `Ctrl-R` | undo / redo the last change (toggle, start, skip, or reset) |
| `Ctrl-O` | go back to where you were before `o` followed a `#section` link, as in vim |
| `Tab` | jump to the next unfinished task (anywhere in the document; wraps around, and the status bar says when it does) |
| `Shift-H` / `Shift-L` | jump to the previous / next list with unfinished tasks |
| `/` | search tasks by text — jump to the next match from the cursor as you type |
| `n` / `N` | jump to the next / previous search match (wraps around, and says so) |
| `}` / `{` | jump to the first task of the next / previous sub-section (within the current list) |
| `T` | open the "go to task" overlay: a filterable list of every task |
| `y` | copy the task's command to the clipboard |
| `y` then a number | when a card has several commands, copy command `[N]` (see below for `[10]` and up) |
| `o` | open the current card's link in your browser |
| `o` then a number | when a card has several links, open link `[N]` (see below for `[10]` and up) |
| `e` | edit the file in `$EDITOR` (at the current task's line for editors that support it) |
| `Ctrl-Z` | suspend to the shell, as in vim or less; `fg` brings markcheck back, picking up any change made to the file meanwhile |
| `Ctrl-C` | doesn't quit: as in vim, it shows a "Type q to quit" hint, or cancels search, an overlay or a confirmation like `Esc` |
| `R` | reset tasks to not-done (asks first): `y` resets everything, `l` only the current list, `s` only the sub-list the cursor is in |
| `1`–`9` | jump to list N |
| `?` | show the keybinding help overlay: every key, with a section per mode (search, the `T` list, the help itself, the reset dialogs, the `o`/`y` digit prompt, the list-complete screen). It uses the full width, with the overview hidden while it's open, and a long description wraps onto the next row. Scrollable when it doesn't fit — `j`/`k`, the arrows, `Ctrl-E`/`Y`/`D`/`U` and `PageUp`/`PageDown`; any other key closes |
| `q` | quit; while git sync is running, wait for it (`q` again stops it) |
| `Esc` | doesn't quit: as in vim, it cancels search, an overlay, a prompt or a confirmation, and otherwise shows a "Type q to quit" hint |

The `1`–`9` list jump only reaches the first nine lists — a document with a
10th `## H2` list has no single-keypress shortcut for it. Reach a distant list
with `Tab` / `Shift-H` / `Shift-L` / `T` instead. Links and commands have no
such limit: after `o` or `y`, type the digits of `[10]` and up in turn.

Holding `Ctrl` or `Alt` makes a different key, as in vim: only the `Ctrl`
chords listed above do anything. Any other chord, such as a reflexive `Ctrl-S`
or `Ctrl-Q`, does nothing and the status bar says it isn't bound, rather than
acting like the plain letter (`s` would mark the task started). `Shift` is
still fine, so `Shift-L` and `L` are the same key.

Pasting text into markcheck never runs it as commands. A paste into `/` search
or the `T` filter is added to the query as one line; anywhere else it is
ignored, with a "Paste ignored" message in the status bar, so a command pasted
into the wrong pane can't toggle, reset or quit anything. This relies on your
terminal supporting bracketed paste, which almost all current terminals do.

A message that arrives while `/` search is open, such as the summary of a
reload or a git sync failure, is shown on its own row above the search prompt,
and typing the query doesn't clear it. When you press `Enter` the match
position is added after it (`Reloaded: 1 done ("Verify") · Match 2/5 · n/N to cycle`);
`Esc` leaves the message showing.

When you finish a list a summary card appears — `l` / `Enter` moves to
the next list that still has incomplete tasks (skipping any already
done), `h` reviews the one you just finished. If you go back to review, you
can still move on: `Tab` jumps to the next unfinished task anywhere (wrapping
around to the top, with a `Wrapped to the first unfinished task` notice), `Shift-L` jumps to the next unfinished list (`Shift-H` the
previous), and `l` at the end of a list steps into the next list. At the
very last task `l`/`j` say `Already at the last task` (and `h`/`k` at the
very first, `Already at the first task`), adding `· Tab finds unfinished ones`
when any other task is still open; the mouse wheel stops there quietly.
Finishing every list shows an all-done summary. Jumping to a list (or
starting up) lands on its first not-done task.

## Behavior

### Write-back

Toggling a task rewrites only its `[ ]`/`[/]`/`[-]`/`[x]` marker character; the rest
of the file (formatting, comments, other lines) is left untouched. Writes are
atomic — a temp file is written and renamed over the original — so a crash
or full disk can never leave the runbook truncated, and the file's
permissions (standard Unix mode bits) and its group are preserved — except
the setuid, setgid and sticky bits, which a save always clears. POSIX
ACLs, extended attributes, another user's ownership, and **hard links** are
not preserved — the rename gives the file a new inode, and only ordinary
permissions, the group and (when you are its owner, or root) the owner are
carried forward onto it — which matters if the file lives somewhere
ACL-managed.

Shared checklists are the other case to know about. Keeping the group means
a runbook in a shared group directory stays writable by the rest of the
group after you save it (as long as you are in that group yourself). But a
file that belongs to someone else becomes yours on the first save, because
only root can give a file away; opening one shows a warning up front ("this
file belongs to user N — saving makes it yours"), so you can quit first. In
a sticky directory (`chmod +t`) the save of someone else's file is refused
outright and reported as a write failure.
Opening the file through a symlink writes through to the real target and
keeps the link intact.

A checklist in a folder you can't write to, but which is itself writable,
is saved **in place** instead: the file is rewritten directly, since there
is nowhere beside it to put the temp file. That keeps its inode, owner and
permissions, but a crash in the middle of that save could leave it
truncated, so the status bar says so the first time it happens in a
session (`Saved in place: ...`). When a save fails, the message says why
(`Write failed — change not saved to disk: the disk is full`, `no
permission to write the file or its folder`, and so on).

Hard links are the case worth calling out, because it changes what your
files mean rather than just their metadata: if two names point at the same
file, saving leaves the *other* name on the old content, and the two names
are separate files from then on. Most editors that save atomically behave
the same way. `markcheck` doesn't stop you, but it does say so up front —
opening a checklist with more than one hard link shows a warning before you
touch anything, so you can quit if that wasn't what you wanted.

A checklist you have made read-only (`chmod a-w`) is never saved to:
opening it shows a warning up front, and each toggle is refused with a
sticky "File is read-only — change not saved" message, leaving the file
exactly as it was. Run `chmod u+w` on it to allow saving again.

If the file changed on disk since it was last loaded — another `markcheck`
instance, or an external editor, saved to it after you did — a toggle is
refused rather than silently overwriting that change: the file is reloaded
and a sticky error asks you to retry.

### Terminal title and exit line

While `markcheck` runs, the terminal window or tab title (and tmux's pane
title) reads `Deployment Runbook · 3/11 — markcheck`, so with several
checklists or shells open you can see which tab holds which and how far each
is. The title you had before comes back when you quit, and while `e` has
your editor open or `Ctrl-Z` has you in the shell; markcheck sets its own
again when it takes the terminal back. After a normal quit,
one line on the screen says where the run stopped, for notes or a handover:
`runbook.md — Deployment Runbook: 3/11 done (1 started), stopped at "Warm the
build cache"`, or `all 11 done`. Turn either off with `terminal_title =
false` or `exit_summary = false` in the config file, or for one run with
`--no-terminal-title` or `--no-exit-summary` (`--terminal-title` and
`--exit-summary` turn them back on over the config).

### Live reload

If the file changes on disk while the app is open, `markcheck` reloads it
automatically and keeps your place on the same task, even when lines were
added or removed above it. If that task is gone, the cursor moves to the
nearest one and the status bar says so. If the same checklist is already
open in another `markcheck` (another terminal, tmux pane or SSH session on
the same machine and user), the second one warns at startup
(`already open in another markcheck (pid 4242, opened 12 min ago): its
changes show up here as reloads`), so you can stop before two of you work
the same procedure. The status bar also says what the
reload changed in task terms, such as `Reloaded: 2 done, 1 reopened
("Restart service", "Verify output") · 1 added`, so a step a colleague just
ticked (or unticked) on a shared checklist doesn't go unnoticed; an edit that
changes no task reads `Reloaded: file changed on disk`. A reload never
triggers `--auto-copy`. A
relative `Updated 10s ago` tag — prefixed with a refresh icon (`↻`, or a Nerd
Font glyph) — appears in the title bar, grouped with the progress counter on
the right, whenever the file changes — whether from your own toggles or an
external edit — and disappears two minutes later. The tag steps in 5-second
increments rather than ticking every second. Your own toggles never trigger
a spurious reload.

A reload never closes what you are typing into: an open `/` search, `T`
go-to-task filter or `?` help overlay stays open and is re-run against the
new content. A pending `R` reset or quit-reset confirmation is cancelled
instead, since it was asked about the old content, with a red `File changed
on disk — reset cancelled` (or `quit cancelled`) message.

If the file is **deleted** from under the app, `markcheck` won't recreate it
behind your back: it shows a red `File was deleted — changes cannot be
saved until it exists again` message and blocks toggling and reset until the
file comes back. Restore the
file (or save it again from your editor) and the app reloads it and resumes
normally. The same goes for a file **replaced** by a symlink, a named pipe
or anything else that isn't a regular file: `markcheck` doesn't read it or
write over it, shows a red `File was replaced by a symlink or special
file` message, and picks up again once a regular file is back.

Changes are normally picked up the moment they're saved. Where the
filesystem doesn't report them — a network share (NFS, SMB/CIFS), an sshfs
or other FUSE mount, or a system that has run out of file-watch slots —
`markcheck` still checks the file every 2 seconds, so a reload can take that
long to appear. If the file can't be watched at all, a startup message says
so (`Live reload slowed: can't watch the file (…) — checking every 2 s
instead`).

### Git sync

With `--git-sync` (or a matching `git_sync_paths` entry, above) active for a
file that's already in a git repository, every toggle, start, skip, reset
(of the whole checklist, a list or a sub-list), undo and redo is committed and pushed in the background, so a checklist shared via git
stays in sync across machines without leaving the app. It waits until 10
seconds pass without another change (the title bar shows `⇅ git · pending`
meanwhile), so a burst of ticks becomes one commit and a tick you undo
straight away is never published at all. Set `git_sync_delay` in the config
file or pass `--git-sync-delay SECONDS` to change the wait, or `0` to
commit every change at once; the wait can be at most 86400 seconds (a day),
and a longer one is refused at startup. A single change is committed as
`checklist.md: Check "Restart service"`; several are committed as
`checklist.md: 3 changes`, listing each in the commit body. Changes are
never lost: quitting sends a waiting change straight away, and changes made
while a commit/push is still running are folded into the next one. Editing
the file in `$EDITOR` (`e`) syncs too, at once rather than after the wait
(`checklist.md: Edited in vim`, naming only the editor program, never its
directory) — even if you only edit and quit without toggling anything
afterward. An editor that
exits with an error (vim's `:cq`) counts as an abort: the edit is reloaded
but not synced, and the status bar keeps the error, ending in `not synced`;
it stays unsynced until you commit it yourself (see below). The editor has to stay open until you close the file (see [Editor and reset](#editor-and-reset)): an edit saved later from a window it handed off to reaches markcheck as an outside change, which is not synced. If something *else* changes the file in the meantime — an
edit made outside markcheck entirely, or the file getting deleted — while
a change is still waiting to be synced, that sync is dropped rather than
publishing content that's since gone stale, and the reload message says so:
`Reloaded: 1 done ("Verify") — pending git sync dropped`. A sync already
running when the file changes is refused instead: `Git sync failed: the file
changed after this sync was queued, so it was not published`.

A toggle only ever publishes **markcheck's own changes**. Its commit holds
the whole file, so before committing, git sync checks that the file as it
was before your first not-yet-synced toggle is already committed (or
staged, below); if not, the file holds changes markcheck didn't make — a
draft line you haven't committed, an edit from another editor or script
that markcheck reloaded, an `e` edit whose editor exited with an error —
and the sync refuses rather than publishing them under `Check "…"`:
`Git sync failed: the checklist has changes markcheck didn't make, so
nothing was published; commit them yourself, then toggle again`. The
message stays red in the status bar and the title bar shows `not synced`
until a sync lands. Commit (or stage) those changes yourself and the next
toggle syncs normally. A refused sync for any other reason (a merge
conflict, a failing hook, the staleness refusal above) doesn't count
against your own toggles: once the problem is fixed, toggling again syncs
everything markcheck changed since the last sync. An edit saved in `e` is
yours to publish, so it syncs as described above, and so do the toggles
after it. It only ever
touches files git already tracks — a new/untracked file next to the
checklist is never picked up and never committed, and the same goes for the
checklist file itself: if it isn't tracked yet, git-sync won't `git add` it
for you — it reports `Git sync skipped: file is not tracked in git` in the
status bar instead of silently doing nothing, so it's clear a `git add` is
needed before syncing can start. A checklist whose file name isn't valid
UTF-8 isn't synced either: each sync fails with a red status-bar message
saying so, and nothing is committed. Commit messages name the
actual change (`checklist.md: Check "Restart service"`, `checklist.md:
Reset all tasks to not done`, …) rather than a generic "updated by
markcheck", so `git log`/`git blame` stay useful across hosts. Each one
also ends with a `Synced-by: markcheck` trailer line, which is how
`markcheck` later tells its own commits from yours. A failed
commit (diverged history, mid-merge, …) is reported as a red status-bar
message — `markcheck` never attempts to resolve it for you (no auto
`git pull --rebase`), so fix it in a terminal as usual. A commit that
succeeds but fails to *push* (offline, expired credentials, …) is
reported separately (`Committed, not pushed: ... (will keep retrying)`) —
nothing is lost, and `markcheck` retries the push on its own — after 30
seconds, then waiting twice as long after each further failure, up to 5
minutes between tries — as well as on the next sync of any kind and once
more right at startup, so you don't have to make another edit just to nudge
it once the network's back. The startup push only sends `markcheck`'s own
commits (`Pushed 2 earlier markcheck commits`): if a commit you made
yourself is waiting to be pushed, opening the checklist pushes nothing and
the status bar names that commit, so a change you kept local on purpose is
never published just by looking at the file. Commits made by markcheck
versions before the trailer existed count as yours here; push those by
hand once. If a retry fails for a new reason, the status
bar says `Push retry failed: ... (will keep retrying)`; one that fails the
same way as before stays quiet, so it doesn't replace whatever message you
are reading, and the title bar keeps showing `not pushed` (see below). When
a push finally lands, the status bar says `Git sync caught up: committed
and pushed`. Since the sync runs on a background thread, a slow or offline `git push`
never freezes the app during normal use. Git sync can't answer a password,
passphrase or host-key prompt in the terminal: a push or signed commit that
needs one fails at once, with the reason in the status bar, so use a
credential helper, `ssh-agent`, a cached GPG passphrase or a graphical
askpass/pinentry for a synced repository. A terminal pinentry
(`pinentry-curses`) is never shown, since markcheck doesn't pass `GPG_TTY`
on to git, except by a `gpg-agent` that was already sent to a terminal with
`gpg-connect-agent updatestartuptty`: that one draws over markcheck and
shares its keystrokes, so don't run it in the terminal markcheck uses.
Git's own messages, such as the reason a sync failed, are always shown in
English, whatever language your locale sets: markcheck reads them to find
that reason. Only the messages change; hooks still see your character set.
Quitting while a sync is still running is the one exception:
`markcheck` stays open with `Waiting for git sync… press q again to
abandon` and exits as soon as the sync finishes, so a quick `e` then `q`
doesn't leave the change unsynced. Press `q` again to stop the sync
instead: its `git` processes (hooks included) are stopped and cleaned up,
not left running. Closing the terminal or sending `SIGTERM` quits the same
way, waiting up to 5 seconds for the sync before stopping it. If that last
sync doesn't land cleanly (it fails, the commit is made but the push fails,
or it was stopped), `markcheck` prints one line on stderr after the screen
closes saying so (e.g. `markcheck: git sync failed: ...`), so you don't walk
away thinking the change was published. The exit status stays 0. A second
`SIGTERM` or `SIGINT` doesn't wait: `markcheck` exits at once with status 1,
without that stderr line and without restoring the terminal (run `reset` if
it's left in a strange state). If
`markcheck` is ever killed outright, the next start removes the temporary
files it left in the checklist's folder and the repository and says so in
the status bar.

Running plain `git push` yourself sends the whole current branch — so if
the branch already has other local commits ahead of the remote,
git-sync refuses to create its own commit on top rather than pushing your
other work for you, and names the commit in the way: `Git sync failed:
commit abc1234 "WIP: reorder" is not pushed yet and isn't markcheck's; push
or drop it first`. Push it yourself, then the next toggle syncs normally.
Commits that only change this checklist don't block a toggle, and neither
do *markcheck's own* commits for any checklist: several of them piling up
while offline, or one left behind by another checklist in the same
repository, are pushed together with the new change.
If the checklist itself has **staged changes** that differ from the file on
disk, git-sync refuses rather than committing: `Git sync failed: the
checklist has staged changes that differ from the file on disk;
committing would drop them — commit or unstage them first`. Committing the
working tree would replace that staged version in the index, leaving it in
no commit and no longer staged. Staging the checklist and *then* toggling is
fine and syncs normally — everything you staged is already part of what gets
committed (staging counts as approving those changes for publishing); only a
staged version you have since edited away is at risk.

If git-sync cannot *determine* whether the branch has unrelated unpushed
commits — a git command failing or timing out while it inspects history —
it refuses to publish rather than assuming the branch is clean: `Git sync
failed: could not check whether the branch has other unpushed commits
(...); not publishing`. Being unable to prove the branch is safe is
not the same as proving it is. (A branch with simply no upstream configured
is *not* this case — that's benign, and reported separately below.)

That check only runs before markcheck's own commit is made; if something
else commits to the branch in the brief window after that but before the
push, markcheck won't push it either — it reports `Committed, not pushed:
the repository changed after the commit was made (will keep retrying)` and
retries automatically, the same as any other failed push.
Unlike running `git push` yourself, git-sync's own push always targets the
exact commit it just made (or verified), not whatever the branch currently
is — so even in the rare case something else commits to the branch at
just the wrong moment, that commit is never swept along and published as
a side effect. Also refuses to run while the repository is
mid-merge, mid-rebase, mid-cherry-pick or mid-revert (including a
multi-commit one you've only partly worked through), on a detached `HEAD`,
or has any unresolved conflict anywhere in it — an automatic commit in any
of those states could silently interfere with work already in progress, so
it reports a status-bar error and does nothing instead, until you resolve it
yourself in a terminal. If git can't answer whether the repository is in
one of those states, git-sync refuses the same way rather than assuming
it isn't.

Whenever git-sync is actually active for the current file, `⇅ git` sits in
the title bar too, grouped with the `Updated` tag and the progress counter
on the right — the whole right-hand side reads as "what's going on with
this file right now," while the left side (title and filename) stays put as
plain identity. `⇅ git` is there from the moment the app opens, before
you've even toggled anything. (It's the same plain `⇅` symbol either way,
with or without `--nerd-font` — a couple of fancier icon attempts along
the way didn't render reliably on every terminal, so the word "git" carries
the meaning now rather than the glyph.) The text after it says where the file stands:

| Title bar | Meaning |
| --- | --- |
| `⇅ git` (green) | Git-sync is on; nothing has failed. |
| `⇅ git · pending` (yellow) | A change is waiting out the quiet period (`git_sync_delay`, 10 seconds by default) before it's committed. |
| `⇅ git · syncing…` (yellow) | A sync or push retry is running. |
| `⇅ git · Synced 8s ago` (green) | The latest change was committed and pushed; fades back to `⇅ git` after two minutes. |
| `⇅ git · not pushed` (red) | Committed, but the push failed; retrying on its own. |
| `⇅ git · not synced` (red) | The last sync failed and nothing was pushed; the status bar said why. |
| `⇅ git · not tracked` (red) | The file isn't tracked in git, so nothing is committed. |

A red state stays until a later sync commits and pushes successfully,
whatever keys you press in the meantime — the status bar's detailed message
clears on the next key, but the title bar keeps the warning. It also
replaces any earlier `Synced … ago`, so an old success never shows beside
a newer failure. The section stays at every terminal width; on one
narrower than 80 columns only the `Synced … ago` text is left out, so a
red state is never hidden by a small window. It's fully hidden — no icon, no text, no gap — when git-sync
wasn't requested or the file isn't in a repo.

`--git-sync` only activates when the file's directory is confirmed to be
inside a git work tree at startup. When the flag (or a `git_sync_paths`
match) asks for git-sync but it can't start, the status bar says why at
startup — for example `Git sync off: the file is not in a git repository`,
`Git sync off: git not found on PATH`, `Git sync off: git 2.22 or newer is
needed (found 2.17.1)`, or git's own error and suggested fix
when it refuses a repository owned by another user (`safe.directory`).

**The branch needs an upstream.** Git-sync pushes the exact commit it just
made, to the branch's configured upstream — so a branch that's never had
one set is committed to locally and reported as ``Committed, not pushed:
this branch has no upstream yet; run `git push -u` once (will keep
retrying)``. Do that once per
branch (`git push -u origin <branch>`) and syncing proceeds normally from
then on; the commits made in the meantime go out with the next one. It
deliberately won't fall back to a plain `git push` here, because that sends
the whole branch — including any unrelated local commits — which is exactly
what the unrelated-work check above exists to prevent, and that check can't
run without an upstream to compare against either.

**It pushes only where `git push` would.** The upstream is where a branch
is *fetched* from, which isn't always where `git push` sends it. If a plain
`git push` on the branch would push somewhere else, or nowhere — a branch
started from `origin/main` under another name (which `git push` refuses
under the default `push.default = simple`), or a `remote.pushDefault` /
`branch.<name>.pushRemote` that sends pushes to a fork — git-sync commits
locally and reports ``Committed, not pushed: `git push` would not push this
branch to refs/heads/main on origin; push it yourself``, rather than
publishing to the upstream.
A branch that tracks another *local* branch (made with
`git checkout -b <name> --track main`, say) has no remote to push to at all;
git-sync commits locally and reports `Committed, not pushed: this branch
tracks the local branch main, not a remote; push it yourself`, and never
moves the local branch it tracks.
When the remote has a separate `pushurl`, the branch is checked on, and
pushed to, that URL.

**Requirements.** Git-sync shells out to `git`, so it needs **git 2.22 or
newer** on `PATH`: it reads a commit's trailers and the branch's push remote
with format options older git doesn't have. With an older git, git-sync stays
off for the session and says so at startup, e.g. `Git sync off: git 2.22 or
newer is needed (found 2.17.1)`. It's developed and tested against modern git
(2.40+); no specific older version is verified in CI.

**Git-sync runs the repository's commit hooks.** That's deliberate — it's
why commits are made with a real `git commit` rather than assembled by hand,
so `pre-commit`/`commit-msg`/`post-commit` and `commit.gpgsign` all behave
as they would if you'd committed yourself. It does mean that opening a
checklist with `--git-sync` inside a repository you don't trust executes
that repository's hooks. The scope check below bounds what a hook can get
*committed*; it can't bound what a hook can *run*. Treat `--git-sync` in an
untrusted repo the same way you'd treat running `git commit` in one.

**Hooks must finish within 5 minutes.** Git-sync stops a `git commit` that
runs longer than that, hooks and signing included, and reports `Git sync
failed: git commit failed: git command timed out after 300s`. That leaves
room for a hook framework's first-run setup or a signing key waiting for you
to touch it; the limit isn't configurable. When a hook fails on its own, the
status bar shows the last line it printed, which is usually the actual error
rather than the hook's opening banner.

Each commit runs from a private, temporary git index — never your real one —
so a `git add`ed-but-not-yet-committed file of yours can never ride along
into a markcheck commit. That index still runs the repository's normal
commit hooks (and honors `commit.gpgsign`) exactly like committing by hand,
so a `pre-commit` hook that itself runs `git add` against that same
temporary index can try to pull other files into the commit alongside the
checklist — but markcheck checks for this after the commit is made and
undoes it if so, reporting `Git sync failed: a commit hook changed files
other than the checklist, so the commit was undone` rather than letting the extra
files through (let alone pushing them). The hook itself still runs
normally either way; only the resulting commit's scope is enforced.

A `post-commit` hook that makes a follow-up commit of its own (regenerating
a file, say), or a commit you make in another terminal at the same moment,
is never mistaken for markcheck's: both commits stay on the branch,
nothing is undone, and the status bar shows `Git sync failed: another commit
landed on top of markcheck's commit abc1234; both were left on the branch
and not pushed — check them before pushing`.

### Search

`/` starts an incremental search: as you type, the cursor jumps to the next
task whose text matches, counting from where you started (the task you're on
counts, and the search wraps around to the top past the last task, as vim's `/`
does), with the query and the **number of matches** shown in
the status bar (and a clear "no matches" when nothing hits — the cursor then
goes back to where the search started, as in vim). Press `Enter`
to keep the match — the status bar then reports your position (`Match 2/5`) as
`n` / `N` cycle forward / backward through the rest, wrapping around (`Match 1/5 · wrapped to top`, as vim reports `search hit BOTTOM`) — or `Esc`
to cancel and return to where you were. As in vim and `less`, an empty `/`
then `Enter` repeats the last search, moving on to its next match (or says
`No previous search`). In the prompt, `Ctrl-W` deletes the last word and
`Ctrl-U` clears the query. A search that matches nothing never moves you and
never triggers `--auto-copy`. Matching is
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
the filter, so `j`/`k` type rather than move; the mouse wheel moves it too;
`Ctrl-W` deletes the last word of the filter),
`Enter` to jump to the highlighted task, `Esc` to close. With the mouse, click
a row to highlight it and click it again to jump there.

### Clipboard

`y` copies a task's command. If the task has a single inline `` `code` ``
span in its body or a single code block (fenced or indented), that content is copied; if there is none,
nothing is copied and the status bar says so. Inline code inside the bold
card title is not a command: it shows as plain title text, so `y` and
`--auto-copy` ignore it. If there are several, `y` asks
which (see below). Copy uses
the system clipboard when available and otherwise emits an OSC 52 escape,
which travels over SSH to your local terminal emulator (kitty, alacritty,
wezterm, foot, and others; inside tmux, set `set-clipboard on`). The status
bar distinguishes a system-clipboard copy from an OSC 52 one.

Over SSH (when `SSH_CONNECTION` or `SSH_TTY` is set), `markcheck` does both:
it writes the system clipboard and also sends OSC 52, and says `Copied
(system clipboard + OSC 52)`. The "system clipboard" it finds there may not
be yours. A tmux session started on a workstation's desktop and reattached
over SSH from a laptop keeps `DISPLAY` pointing at the workstation, so a
system-clipboard copy alone would land on the other machine. To choose for
yourself, pass `--clipboard system` or `--clipboard osc52` (or set
`clipboard = "osc52"` in the config file): `osc52` always uses the escape
through your terminal and never the system clipboard, and `system` never
falls back. `auto`, the default, is the behaviour described above.

A command too large for OSC 52 (over 70 KB once encoded) can't go through
the terminal. Over SSH it is then copied to the system clipboard alone, and
the status bar says so in error color, and keeps it until the next key:
`Copied to the system clipboard only — too large to send through the
terminal (OSC 52)`, since that clipboard may not be the one you paste from.

On X11 the program that copied something has to stay running to hand it over
when you paste. markcheck holds the clipboard for as long as it runs, so a
copy is always pasteable while it is open. When you quit, a running clipboard
manager takes over the last copy; without one, X11 clipboard contents last
only while markcheck is running.

A command that contains hidden control characters — anything the card can't
display, such as escape, backspace or a carriage return, zero-width
characters such as a zero-width space, word joiner, byte-order mark, soft
hyphen or Unicode tag characters, a variation selector with no character
before it to modify, plus Unicode bidi overrides that reorder what is shown —
is refused with `Not copied: command contains hidden control characters`,
whether you press `y`, click it, or reach it with `--auto-copy`. Otherwise a
checklist could show one command and put another on your clipboard. Newlines
and tabs copy normally, and so does an emoji's variation selector.

OSC 52 is an out-of-band channel: the escape sequence rides over whatever
carries your terminal session, including a chain of SSH hops, so a copied
command is exposed to anything in that path capable of reading terminal
output. If a task's command embeds a credential or token, `y` sends it the
same way as any other command — worth keeping in mind for a runbook shared
in a less-trusted environment.

When a task has **several** commands, `y` won't guess between them. The card
numbers them — a dim `[1]`, `[2]` after each inline command, or in the top
border of each code block — and `y` shows `press 1–N to copy · esc cancels`
in the status bar; press the number to copy that command, or `Esc` to cancel.
Code blocks (fenced or indented) take priority, as for a single command: on a
task with any code block, only the blocks are numbered. With ten or more commands, type
the digits of `[10]` and up in turn, exactly as for links: `1` then `0` copies
`[10]`, and `1` then `Enter` copies `[1]`.

With the mouse, you can also **left-click the command** you want and that
specific one is copied, in full (an inline command, the trailing command, or
a code block). With two inline commands on one row, each copies its own; a
long inline command that wraps across rows copies whole from any of them.
Clicking anywhere else on the card, including the prose beside a command,
acts like `y`.

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

`u` undoes the last state-changing action — a toggle, a start (`s`), a skip
(`-`), or an `R` reset of any scope — restoring the previous state, writing it back, and jumping to the
task that changed so you can see it. `Ctrl-R` redoes what you just undid; making
a fresh change after an undo clears the redo history (the usual undo/redo rule).
The status bar confirms each step (`Undo: marked not done`, `Redo: marked done`,
`Redo: marked skipped`, `Undo: restored 3 tasks`) and says `Nothing to undo` / `Nothing to redo` when the
history is empty. With git sync on, an undo or redo is committed under a
message naming what it did, like any other change: `Undo: Uncheck "Restart
service"` for one task, `Undo: 3 tasks ("Drain node", "Restart service",
"Verify")` for several (the first three named, the rest elided with `…`). The history is in-memory and covers task state only; editing
the file **externally** (in another editor) clears it, since that edit could
change the file underneath the undo history — your own toggles never do.

### Editor and reset

`e` suspends the TUI, opens the file in `$VISUAL` / `$EDITOR` (falling back
to `vi`), and reloads on exit. The file opens on the selected task's line in
editors that take a line number on the command line: `+N file`
(`vi`/`vim`/`nvim`/`view`, `nano`, `pico`, `emacs`/`emacsclient`, `joe`,
`gedit`, `kak`, `micro`), `file:N` (Helix `hx`, Sublime Text `subl`) or
`--goto file:N` (VS Code `code`, VSCodium `codium`). Any other editor opens at
the top of the file. The editor has to keep running until you're done with the file, which
terminal editors do but most GUI editors don't unless told to: set
`EDITOR="code --wait"` (VS Code), `subl -w` (Sublime Text), `gedit --wait`,
or leave off `-n` with `emacsclient`. An editor that exits within a second
without changing the file gets a status-bar hint saying so, since its later
save would otherwise be picked up as an outside change and, with git-sync,
not committed as an edit of its own. `R` resets tasks to not-done after a
confirmation prompt that says how many done and started tasks it will clear:
press `y` to reset everything, `l` to reset only the current list (offered
with several lists), or `s` to reset only the sub-list you are in (offered
inside one), each shown with how many tasks it would change; the status
bar lists the same keys. `s` outside a sub-list keeps the dialog open with
`No sub-list here: y resets everything`; any other key cancels. When
you quit with every task already done, `markcheck` first asks whether to
reset the checklist so it's ready to run again: `y` resets and quits, `n`
quits as-is, `Esc` stays in the app. If the reset can't be saved, markcheck
stays open and shows why instead of quitting.

## Architecture

See [DESIGN.md](DESIGN.md) for the internal design and module layout.

## License

BSD-3-Clause — see [LICENSE](LICENSE).
