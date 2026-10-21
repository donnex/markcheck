# markcheck — Project Specification

A TUI application for pilot-style Markdown checklists, aimed at sysadmin/devops repeating workflows (server upgrades, maintenance runbooks). It reads a Markdown file, shows its tasks as navigable cards, and writes each task's state back to the same file as `[ ]` / `[/]` / `[x]` / `[-]`.

Each section below states the current contract first, in the present tense. How a design came to be — the bug or review that forced it — is in [Design history](#design-history) at the end, one line per decision, and in `git log`.

---

## Project Structure

```text
markcheck/
├── Cargo.toml          # rust-version is the minimum Rust; check.yml's msrv job builds with it
├── build.rs            # embeds the git short SHA for --version
├── about.toml, about.hbs, deny.toml   # license allow-lists (cargo-about, cargo-deny)
├── .cargo/mutants.toml # cargo-mutants scope and exclusions for the guard mutation tests
├── examples/           # sample checklists covering every Markdown feature; golden-test fixtures
├── .gitattributes      # keeps examples/crlf-bom.md's CRLF line endings out of git's normalisation
├── tests/              # cli.rs (binary, non-interactive), pty.rs (binary under a PTY), common/
└── src/
    ├── main.rs          # Wiring only: terminal setup, signals, event loop, effect host, startup messages
    ├── cli.rs           # clap `Cli`, `Settings` and the flag > config > default resolution; man page and completions
    ├── launch.rs        # Editor/opener resolution, git-sync path matching, startup and exit messages
    ├── config.rs        # Config file: TOML defaults for the flags
    ├── model.rs         # Data types (Item, List, Document, AppState, IconSet, Palette, ...)
    ├── parser.rs        # Markdown → Document
    ├── app/             # AppState's behaviour, one file per concern, each with its sibling *_tests.rs
    │   ├── mod.rs       # AppState::new, status messages, shared helpers
    │   ├── navigation.rs # Cursor motions, screen transitions, card scrolling
    │   ├── search.rs    # `/` search, the `T` picker, help scrolling
    │   ├── write.rs     # Toggle/start/skip, critical steps, set_state/commit_write, git-sync reporting
    │   ├── reload.rs    # Reload from disk, cursor remapping, return from the editor or a suspend
    │   ├── reset.rs     # `R` reset by scope
    │   ├── history.rs   # Undo/redo snapshots
    │   ├── copy.rs      # `y` and auto-copy, copy outcomes
    │   ├── links.rs     # `o`, anchors, the Ctrl-O jump-back stack
    │   ├── keys.rs      # Key, mouse and paste dispatch; chords
    │   └── test_support.rs # Fixtures shared by the app tests
    ├── effect.rs        # Effect queue: every outward action AppState asks for, and its drain
    ├── writer.rs        # Write-back (atomic, ownership- and mode-preserving), write lock, session markers
    ├── scaffold.rs      # `--new`: create a starter checklist atomically
    ├── printable.rs     # Strip terminal controls from text written outside ratatui
    ├── watcher.rs       # File watch plus timed recheck for external changes
    ├── clipboard.rs     # Copy: system clipboard and/or OSC 52 along a CopyRoute
    ├── git_env.rs       # REDIRECTING_GIT_VARS, include!d by git_sync/process.rs and build.rs
    ├── test_support.rs  # Shared test fixtures and the property-test Markdown generator
    ├── temp_test_support.rs # Self-removing temp paths, shared with tests/common by #[path]
    ├── git_test_support.rs # The git variables every test strips, built on git_env.rs and shared with tests/common by #[path]
    ├── *_tests.rs       # Each module's tests, in a sibling file (so coverage can leave them out)
    ├── git_sync/        # Background git commit+push after write-back
    │   ├── mod.rs       # GitSync: request queue and quiet period, poll and wait, retry, abandon, status/exit messages
    │   ├── shared.rs    # Leaf: SyncOutcome, request-hash lock, race points
    │   ├── sync.rs      # run_sync: one request end to end; commit messages
    │   ├── guards.rs    # The refusals: everything that stops a sync
    │   ├── commit.rs    # Commit construction through a temporary index
    │   ├── push.rs      # Publishing, retry, startup catch-up
    │   ├── inspect.rs   # Read-only repository queries
    │   ├── process.rs   # Bounded, isolated git subprocesses in their own session
    │   └── test_support.rs # Shared fixtures and the race harness
    └── ui/
        ├── mod.rs       # render(), layout, title bar, the shared state marker
        ├── cards.rs     # Card stack, preambles, completion and confirmation screens, help
        ├── overview.rs  # Right-side overview list (click to jump or toggle)
        ├── statusbar.rs # Bottom legend / status message
        └── snapshots/   # insta golden screens (golden_tests.rs)
```

---

## Dependencies

```toml
ratatui = "0.30"
crossterm = "0.29"
pulldown-cmark = { version = "0.13", default-features = false }
arboard = { version = "3", features = ["wayland-data-control"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
notify = "8"
base64 = "0.23"
shell-words = "1"
toml = "1"
serde = { version = "1", features = ["derive"] }
sha2 = "0.11"
libc = "0.2"
signal-hook = "0.4"
clap_complete = "4"
clap_mangen = "0.3"
```

- `pulldown-cmark` parses the Markdown; markcheck never guesses block structure from raw text.
- `arboard` is the system clipboard; its `wayland-data-control` feature adds the native Wayland backend (`wl-clipboard-rs`), without which arboard only has X11 on Linux and silently misses Wayland paste targets while reporting success. arboard gates the feature to Unix-not-macOS itself.
- `notify` watches the file (`watcher.rs`).
- `shell-words` splits `$EDITOR`/`$VISUAL`/`$BROWSER` the way a shell would.
- `toml`/`serde` read the config file.
- `sha2` backs `model::hash_bytes`, the one content digest used by the write-time conflict check and by git sync. A cryptographic digest is more than either needs, at negligible cost for checklist sizes.
- `libc` provides `setsid` for git-sync children, `O_NOFOLLOW`/`O_NONBLOCK` for reading the checklist and its lock, `ELOOP`, `geteuid` for the current user (`writer::current_uid`: the lock directory and the foreign-owner warning), `killpg` for a timed-out or abandoned git group (`process::signal_group`) and `kill(getpid(), SIGTSTP)` for `Ctrl-Z` (`main::stop_self`). Those calls are the crate's only `unsafe` blocks: `setsid` in `pre_exec`, and one-line calls taking plain integers and touching no memory. None spawns a process or needs a writable temp directory, so a bad `$TMPDIR` can't turn off the write lock and no `kill` binary is needed.
- `signal-hook` turns `SIGTERM`/`SIGHUP`/`SIGINT` into the normal quit path.
- `clap_complete` and `clap_mangen` render the shell completions and man page from the `clap` definition, through the hidden `--generate-completions <shell>` and `--generate-man` flags (`cli::completions`, `cli::man_page`), so neither drifts from `--help`. The man page adds only the config file location, an ENVIRONMENT section (the variables markcheck reads) and an EXIT STATUS section, and points to the README for keybindings and Markdown handling. The `+N` and `FILE#anchor` start targets are taken out of argv before clap parses it (`take_start_line`, `split_anchor`), so the `FILE` argument's help names them for `--help`, the man page and the completions.
- Git sync shells out to the system `git` (`std::process::Command`) rather than a git library: it reuses the user's SSH agent and credential helpers, and needs only plumbing commands.
- No calendar or timezone crate: every time shown is relative ("Updated 10s ago", "opened 12 min ago").

Dev-dependencies: `portable-pty` (PTY tests), `proptest` (property tests), `insta` (golden screens).

---

## Core Data Types (`src/model.rs`)

The fields live in `src/model.rs` and its doc comments (and `cargo doc`); this section gives each type's role and invariants.

- **`TaskState`** — `NotStarted`, `Started` (`[/]`), `Done` (`[x]`/`[X]`) and `Skipped` (`[-]`). `is_resolved()` is done or skipped: nothing left to decide. Started counts as unresolved. The state also carries its own wording, one exhaustive `match` each, so a new state fails to compile until it is named everywhere (#185): `toggle(target)` (the target, or back to not started when already there, shared by Space, `s` and `-`), `marked()` (`done`, `started`, `skipped`, `not done`), `toggle_message(new)` (`Marked skipped` going in, `Marked not skipped` coming out) and `commit_verb()` (`Check`, `Start`, `Skip`, `Uncheck`, for git-sync commit messages).
- **`Progress`** — task counts by state (`done`, `started`, `skipped`, `open`) from `List::progress`/`Document::progress`, with `total()` (everything but skipped: the `done/total` every counter shows), `resolved()` (done or skipped), `tasks()` (all of them) and `is_finished()` (every task resolved, and at least one). The title bar, progress bar, completion screens, reset dialogs, quit-reset gate, terminal title and exit line all read it.
- **`ItemKind`** — `Checkbox(TaskState)` or `DisplayOnly` (a note, a blockquoted example, a bold-only bullet that isn't the banner).
- **`Item`** — one card. `line_number` (1-based) is the line `write_back` rewrites and the key every lookup uses (preambles, critical steps, undo snapshots). `depth` is nesting metadata, not a tree. `header` is the card title (a leading bold run, with a critical `!` already removed), `display_text`/`body` the rest, `code_spans`/`code_blocks` its commands, `ordinal` the ordered-list marker as written, and `section` the `### H3`+ path it sits under. Items of a list are a flat `Vec` in document order.
- **`List`** — one `## H2` (or the synthesized default list before any H2): `title`, an optional `banner`, and `items`. Every kept list has at least one live checkbox. Tree helpers (`descendants`, `parent_chain`, `has_children`, `item_links`, `info_parent_state`) derive nesting from depths; `progress` counts its tasks by state (`Progress`). A display-only parent's state (`info_parent_state`) aggregates over its non-skipped children.
- **`Document`** — the whole file: `title` (first `# H1`), `lists`, `raw_lines` (every line with its own terminator, so write-back reproduces the file byte for byte), `preambles` (prose before a heading's first task, keyed by that task's line), `unshown` (counts of blocks that can't be shown), `anchors` (GitHub-style heading anchors with the lines their targets are searched from) and `critical` (lines of critical tasks). `anchor_target`/`first_task_from` resolve navigation targets.
- **`View`** and **`Overlay`** — the base screen (`Checklist`, `ListComplete`, `AllComplete`) and an optional modal layer over it (`ConfirmReset`, `ConfirmQuitReset`, `Help`, `Search`, `ListPicker`). An open overlay gets every key first; closing it is `overlay = None`.
- **`AppState`** — everything the event loop and the renderer share: the document, the cursor (`current_list_index`, `current_item_index`), view and overlay, the status message (`status_message`, `status_expiry` — `None` is sticky — and `status_is_error`), the `effects` queue, the file's last known `file_mtime`/`file_size`/`file_content_hash`, `file_gone`, undo/redo stacks and `history_epoch`, the jump stack, an armed critical task, git-sync display state, and the per-feature sub-structs `SearchState`, `PickerState`, `HelpState`, and `layout`.
- **`ViewLayout`** (`AppState.layout`, #142) — the geometry the renderer measures and the handlers read back: the card's scroll limit, viewport height and area, its copy targets (`code_regions`), the overview's click rows (`overview_rows`), the picker's viewport, row area and scroll offset, and help's scroll limit and viewport. `ui::render` fills it every frame. A handler only empties a part whose subject changed (navigation zeroes `card_max_scroll`, opening the picker clears its rows) until the next frame measures it again, so a key pressed in between never acts on geometry from something no longer on screen. Before the first frame it is empty: a scroll moves nothing, a click hits nothing. The event loop always draws before it reads input, so that state is never seen in use.
- **`PendingSync`** — one git-sync request: the exact `content` expected on disk, its `content_hash`, the `previous_content` read just before the write (for the staged-target guard), a `description` for the commit message, `immediate` (skip the quiet period), and its `base` (`SyncBase`: `Own { epoch, content }` for markcheck's own change, `Edited { epoch }` for an `e` edit the user saved; see Git Sync's Requests).
- **`StateSnapshot`** — every checkbox's state keyed by line: the unit of undo/redo, capped at `UNDO_HISTORY_CAP`.
- **`IconSet`** — the glyphs. `unicode()` (the default) and `nerd()` (Font Awesome range, Nerd Fonts v3); `sync` is the plain `⇅` in both.
- **`ColorDepth`**, **`Palette`**, **`ColorChoice`** — see Color palette below. **`ClipboardChoice`**/**`CopyRoute`**, **`ResetScope`**, **`StartAt`**, **`FileGone`**, **`PreambleBlock`**/**`PreambleKind`**/**`Unshown`** are covered in their sections.

**Color palette:** the UI's semantic colors (done, started, `code`, current, note, error, warning) come from a `Palette` resolved once at startup: curated `Color::Rgb` hues on truecolor terminals, the nearest `Color::Indexed(n)` on 256-color ones, named ANSI-16 otherwise (which also follows the user's theme). `code` is a whole `Style`: a subtle gray background with an explicit light foreground (leaving the foreground to the terminal made code black-on-slate on light themes). Nesting guides use `Palette::depth_color(slot)`, a fixed color cycle indexed by a sub-list's document-wide ordinal, and the progress counter fades from the started color to the done color (`progress_rgb_parts`, whose stops are the truecolor palette's own). Capability comes from the pure `depth_from_env`: a non-empty `NO_COLOR` selects `Mono` (every role `Color::Reset`, `code` reversed, a started dot drawn `◐`; `Palette::is_mono()` gates these fallbacks); `COLORTERM=truecolor|24bit`, a `TERM` in `TRUECOLOR_TERMS` (kitty, alacritty, foot, wezterm, contour, ghostty) or ending in `-direct` is truecolor — the list exists because `COLORTERM` is the signal most often lost (OpenSSH doesn't forward it by default, `sudo` strips it); a `TERM` containing `256color` is 256-color. terminfo isn't read. `--color auto|truecolor|256|16` (config `color`) forces a depth, over `NO_COLOR` too. All `ui/` code reads `state.palette`; nothing hardcodes `Color::*`.

---

## Header Taxonomy

markcheck recognizes five kinds of "header," each a title on a container. This vocabulary is **canonical** across the code, this document, and the README:

| Level | Markdown | What it is | Code | User-facing |
| ------- | ---------- | ----------- | ------ | ------------- |
| **Document header** | `# H1` | the whole file's title | `Document.title` | shown as the title in the title bar; a later `# H1` is a list header (#250) |
| **List header** | `## H2` | a list's title | `List.title` | "list"; shown above the cards + in the overview |
| **Sub-section header** | `### H3`–`###### H6` | a labeled group of items *within* an H2 list | `Item.section` (a `Vec<SubHeading>`) | overview `── divider`; dim ` › `-joined breadcrumb above the card |
| **Card header** | leading `**bold**` on a task | a task card's title | `Item.header` | "card title"; shown inside the card (info icon, blue) |
| **Sub-list header** | a nested/indented list under an item | the parent chain of a nested item | `List::parent_chain` → `breadcrumb_line` | depth-colored breadcrumb above the nested card |

The H2-level container is called **`List`** everywhere: identifiers (`lists`, `current_list_index`, `jump_to_list`, `View::ListComplete`), the UI ("List Complete", "jump to list N") and the docs. `List.title` is the list header, as `Document.title` and `Item.header` are theirs.

---

## Markdown Parsing (`src/parser.rs`)

`parse_document` reads the file through `writer::read_checklist` (no-follow, non-blocking, regular files only), decodes it as UTF-8 (a BOM is kept and tolerated), and hands it to `parse_source`. Parsing is pulldown-cmark with `PARSE_OPTIONS` (task lists and strikethrough; not tables) and `into_offset_iter()`, inside `TextMergeWithOffset` so a run of text pulldown splits arrives as one event.

**One line model.** `split_lines_keeping_endings` breaks on `\r\n`, a lone `\n` and a lone `\r` (CommonMark's three line endings) and keeps each line's terminator. `raw_lines` is that split, and `LineCounter` turns byte offsets into 1-based line numbers counting the same three endings, so an offset and a `raw_lines` index always name the same line and write-back can rejoin by plain concatenation.

**Front matter.** A leading YAML (`---` … `---`/`...`) or TOML (`+++` … `+++`) block is blanked to spaces (line endings kept) before anything else — unhandled, CommonMark reads the opening `---` as a thematic break and the closing one as a setext underline, turning the metadata into a `## H2` list title — so offsets still index `raw_lines` and the block round-trips byte for byte. `blank_front_matter` detects it with a probe pass using only the metadata options (`FRONT_MATTER_OPTIONS`); those options stay out of `PARSE_OPTIONS`, because pulldown-cmark accepts a metadata block at any block start and would swallow a `---` … `---` span mid-file. Its keys are not read.

**`[/]` and `[-]` markers.** pulldown-cmark recognizes only `[ ]` and `[x]`. `extract_extended_markers` finds each `[/]` (started) or `[-]` (skipped) exactly where pulldown would report a task marker — the first `Text` after `Start(Item)`, past a `Start(Paragraph)` or `Start(Heading)` (a lone `-` line under a task's text makes it a setext heading, and pulldown still reads a marker there), followed by whitespace — rewrites those three bytes to `[ ]`, and records offset and state. It repeats on the rewritten text until a round finds nothing new, because a rewrite can change block structure: as text, `[/]` forms a paragraph that swallows an ordered sub-list starting at `2)` below it, which as a marker it leaves alone. Each round only turns lookalikes into markers, so it ends. Those positions are only candidates: an ATX heading inside an item (`- # [/] h`) or an item whose first line is empty (`-` then an indented `[/] b`) puts a lookalike there that pulldown doesn't report as a marker. So once the rounds settle, one more parse of the rewritten text collects the offsets of its `TaskListMarker`s, and every recorded offset not among them gets its original three bytes back and is dropped from the map; restoring a non-marker gives back exactly the source text, so it can't create a new marker. The real parse runs on the rewritten text; a `TaskListMarker` at a recorded offset takes the recorded state. Because every rewrite that survives is a marker pulldown itself reports, a lookalike in code, HTML, a fence-looking line or any other place CommonMark doesn't treat as one shows on the card as written. A blockquoted `[/]` bullet is rewritten like any other, so it surfaces as a non-interactive card (below) instead of being dropped with a list that seems to have no checkbox. `extract_extended_markers_boundary_sweep_agrees_with_pulldown` and the property tests pin that agreement.

**The event walk.** `parse_source` drives a `ParseState`, whose `handle` dispatches each event family to one method (`on_heading_start`/`end`, `on_list_start`/`end`, `on_item_start`/`end`, `on_task_marker`, `on_code_block_end`, `on_text`/`on_break`/`on_code`); per-item text, styling and links accumulate on an `ItemBuilder` frame, kept on a stack so nested items don't clobber their parent.

- **Headings outside items and quotes** are structural. `## H2` starts a new `List`; the first non-empty `# H1` sets `Document.title`, and any `# H1` except one that comes before every task with no title set yet also starts a `List` like an H2 (`start_list`), so a file written in `#`-level phases doesn't pour every later phase into the last `##` list (#250); `###`–`######` with text update the sub-section stack (pop entries at the same or deeper level, push). H1 and H2 clear the stack. Every structural heading also gets a GitHub-style anchor in `Document::anchors` (see Open link) and opens a preamble scope. A quoted heading and a heading inside a list item get an anchor too, and nothing else, so the repeat numbering matches GitHub's.
- **A heading inside an item** (`- ## Rollback`, or in its later paragraphs) is read as a bold run: a leading one is the card title (and so can be the banner), a later one bold body text. **A heading in a quote outside any item** is discarded. Neither opens a list, sets the title or touches the stack: a heading item that opened a `List` would take the rest of the enclosing list's items with it, emptying (and dropping) the real one.
- **An item** records its line from its first non-blank byte (pulldown starts a tab-indented nested item's range at the newline before it), its `depth`, its ordered-list marker, and a snapshot of the sub-section stack. The first top-level item after a heading claims the pending preamble.
- **A task marker** makes the item a checkbox (state from the marker, or the recorded `[/]`/`[-]`) and sets `list_had_checkbox_syntax`, independently of the item's final kind, so a quoted example beside live tasks keeps its Markdown list. An item opened inside a blockquote is forced to `DisplayOnly` at its end whatever its marker: a quoted checkbox is an example, not a live task.
- **A leading bold run** becomes `header` (the card title); bold later in the text is inline styling. Inline code inside the title folds into it as plain text and sets `header_has_code`; it is no copy candidate, since it's drawn without a chip.
- **Emphasis, bold and strikethrough** toggle the builder's `TextStyle`, producing `BodySpan::Styled` runs.
- **Links** buffer their text (which still flows into `display_text`, so search finds it) and emit `BodySpan::Link`. A link entirely inside the bold title loses its URL (known limitation). **Bare URLs** — `http://`, `https://`, `www.` at a word boundary with a dotted domain, ending at whitespace or `<`, trailing punctuation and an unmatched `)` trimmed — become links too (`find_bare_urls`), shown as written (a `www.` one opens as `http://`); text in a link, the title or code is never scanned.
- **Inline code** pushes to `code_spans` and `BodySpan::Code`. **A code block** (fenced or indented) inside an item goes to its `code_blocks`. Outside one, a fenced block directly after a kept list, with only other fenced blocks between, attaches to that list's last item (`attach_fence_to_last_item`, `keeps_fence_attachment`); a fence after a dropped list attaches nowhere; any other top-level code block is a preamble block or unshown.
- **Breaks and boundaries**: a soft or hard break is a space. A boundary inside an item (a paragraph edge, the end of a nested block or sub-list, dropped HTML) marks a pending separator that becomes one space before the next text, code or link, only when text precedes it, so a second paragraph or `a<br>b` doesn't fuse onto the first.
- **The end of an item** classifies it: banner, critical task, or an ordinary item pushed to the current list's buffer.

**List banner.** The first top-level bullet of a list that is a non-checkbox bullet consisting of one bold span becomes the list's `banner` and emits no item. It must have no children (the banner emits no item to parent them), not be inside a quote, and not carry code (inline in the bold, or a block under it). The candidate is buffered with its Markdown list and committed only if that Markdown list had checkbox syntax, so a dropped checkbox-less list neither supplies the banner nor takes the slot. A banner attaches to the default list too.

**Critical steps (#95).** At an item's end, a checkbox whose title starts with `!` followed by more text has the `!` removed from its title and its line added to `Document::critical`. The file keeps the visible `!`.

**Sub-sections (`### H3`+).** Each item snapshots the active stack into `Item.section`, so a sub-heading no item references simply never appears (empty sub-sections drop). Sub-sections are display context only: not navigable, not in `/` search, not counted, never written. The `T` picker shows and filters by them (through `picker_match_text`, not `Item::search_text`). The overview draws a `── Text` divider before each group and pins the active path as a sticky header; the card shows it as a breadcrumb (see UI Layout).

**Nested lists.** Items are pushed at `End(Item)`, so a nested subtree arrives post-order; each flushed list is sorted by `line_number`. Depth is metadata; every item at any depth is a navigable card that toggles its own line. The overview draws one `│` guide per level in `Palette::depth_color(slot)`, where `slot` is the sub-list's document-wide ordinal (`sublist_base` plus `List::sublist_slot`), so every sub-list in the document gets its own color (a blue → purple → cyan → orange cycle, so hierarchy reads by which color rather than by fading), with no collision until the cycle wraps. The guides keep the marker prefix's width, so the click toggle zone doesn't move. Banner detection only looks at depth-0 bullets. `List::item_links` derives parents, slots and info-parent states in one linear pass, and `AncestorSlots` keeps only the guide levels that fit the panel, so a frame stays linear even for absurd depths. A nested card shows its parent chain as a breadcrumb in its sub-list's color.

**Preambles (#80).** Outside every item, list and heading, a paragraph, a quote (its paragraphs `\n`-joined; `as_alert` turns a leading `[!WARNING]`, `[!CAUTION]` or `[!IMPORTANT]` into `PreambleKind::Alert` with a `Warning: …` label) or an unattached code block is a candidate preamble block. While `preamble_open` (after a heading, or at the start of the file, until the next task) it waits in `pending_preamble` with the level of its heading; otherwise it is counted in `Unshown`. A heading closes the scope of any same-level or deeper heading (`open_preamble_scope`): blocks left there without a task are unshown, while a parent's blocks carry on, so prose under a `## H2` and under the `### H3` after it both reach the first task under the `### H3`, and an intro under the `# H1` reaches the document's first task. The first top-level item after a heading claims them (`claim_preamble`); `finish` maps each claim onto the first task actually kept at or after its line in its list (the claiming bullet may have become the banner), and counts claims and pending blocks with nowhere to go as unshown. `Document::preambles` keys the blocks by task line; A top-level Markdown list with no checkbox syntax, outside any blockquote, is dropped at its end (`on_list_end`), and its top-level items (the would-be banner included) are counted as `Unshown::bullets` (#151). `Document::unshown` feeds the startup message (`launch::unshown_warning`: `2 paragraphs, 1 quote and 2 bullet points outside task lists not shown`). A preamble is drawn only while its task is current, so navigation and counts are unchanged. Tables aren't enabled, so a table reads as a paragraph.

**Lists kept.** Items before the first `## H2` form a synthesized default list (`has_default_list`), titled with the document's `# H1` when that H1 opened the file (`default_title`), else with `scaffold::derive_title` of the filename, so the overview, list tabs, `T` picker, completion screens and the `R` `l` status and git-sync commit message name it as the user would (#159); an H1 that comes after those tasks names its own list and so is not reused. `finish` drops every list with no `ItemKind::Checkbox` item — checked on item kinds, so a list whose only checkboxes are quoted (and so `DisplayOnly`) is dropped too — which makes "every list in a `Document` has at least one checkbox" an invariant. Without it, a quoted-only list would show as a vacuously complete `0 / 0` row with a jump key that lands nowhere, and a quoted-only file would open instead of reporting no tasks. A file with none reports "No checklist items found" at startup. Duplicate items are told apart by `line_number`, never content.

---

## Markdown Write-Back (`src/writer.rs`)

**Only the task marker changes.** `write_back` copies `raw_lines`, and for every checkbox item `set_task_marker` replaces the leftmost `[ ]`/`[x]`/`[X]`/`[/]`/`[-]` on its line with its state's marker (`[ ]`, `[/]`, `[x]`, `[-]`), leaving a marker already in that state as written, so an untouched `[X]` keeps its casing. The lines rejoin by concatenation, so every line ending and the final newline (or its absence) come straight from the source. Returns `Saved { contents, in_place }`.

**Atomic and durable.** The source's metadata is read first with `symlink_metadata`: any error (including `NotFound`, so a deleted file is never recreated) aborts with nothing written, a non-regular path is refused (`NotRegularFile`), and a write-protected file (all write bits clear) is refused with `PermissionDenied` — editors honour read-only (vim's `E45`), and a rename, which needs only a writable directory, would otherwise replace it and copy the `0444` mode onto the new content. The app shows the sticky `File is read-only — change not saved (chmod u+w to allow saving)` and announces the condition at startup. The new content goes to a temp file beside it (`write_temp` through `write_new_temp`/`write_new_file`: `create_new` with mode `0600`, retried once at a fresh name rather than deleting a colliding file that may be another process's; contents written; `fchown` to the source's UID and GID, falling back to the GID alone; the source's ordinary bits `mode & 0o777` applied through the handle, never setuid, setgid or sticky (the new file belongs to whoever saved it, and the kernel clears those bits when anyone but the owner writes a file for the same reason); fsync), renamed over the checklist, and the directory fsynced. A crash leaves either the old file or the new one. Temp names start `.<name>.markcheck-tmp-` with the name cut to 160 bytes at a character boundary (`temp_name_stem`, shared by `save_temp_prefix`/`new_temp_prefix` and the startup sweep), so a long name still fits the 255-byte limit.

**A read-only folder (#46).** When the temp file can't be created (`PermissionDenied`) but the checklist itself is writable, `write_in_place` rewrites it through a no-follow, regular-file-checked handle and returns `in_place: true`, keeping inode, owner and ACLs; the app says so once per session. `rewrite` writes from offset 0 over the old bytes, then `set_len` to the new length (for a file that changed length in a race) and fsyncs. It never truncates first: a save only swaps 3-byte markers, so the content has the length the hash check just matched, and a write that fails or a crash part-way leaves every byte old or new, still a valid checklist (#199). If that fails, its own error is reported, not the temp file's `PermissionDenied`; `rewrite` takes a `Rewritable` (implemented for `File`) so a test can fail the write part-way.

**Failures say why.** `commit_write` rolls the change back and shows `Write failed — change not saved to disk: <reason>`, with `describe_write_error`'s plain words for permission, a full disk, quota, a read-only mount, a missing file or folder and a too-long name (the `io::Error` text otherwise), or the read-only and not-regular messages for those cases.

**Ownership and links.** The group is kept where the saver may set it, so a shared group checklist stays writable by the group. Another user's UID can only be kept by root: `foreign_owner` (comparing the owner with the UID a new file gets) adds "this file belongs to user N — saving makes it yours" to the startup warning. The rename gives the path a new inode, so a hard-linked alias stops tracking the checklist: `hard_link_count` warns at startup rather than refusing, as vim, emacs and VS Code break hard links the same way. ACLs, extended attributes and security labels are not carried over. A sticky directory refuses the rename over another user's file (`EPERM`), reported as a write failure.

**Symlinks.** `main.rs` canonicalizes the CLI path once, so a file opened through a symlink is written through to its target, the link stays intact, and the watcher watches the target's directory. The title bar shows the target's name. A symlink retargeted mid-session keeps writing to the startup target; one swapped in for the file itself is refused (see Reload From Disk).

Called synchronously on every write; the file is never observed half-written.

---

## App State & Navigation (`src/app/`)

`AppState::new` requires a document with at least one list (startup refuses a list-less file and a reload keeps the last good document, so this always holds; `current_list()` indexes without a fallback and a `debug_assert!` states it). The cursor starts on the first unresolved task: the first list's, else the first anywhere, else item 0. `+N` and `file.md#anchor` then move it (`open_at`, see Open link).

**One cursor-move primitive.** `focus_item(list, item, CursorMove)` sets both indices (clamping, ignoring an empty or out-of-range list), sets `view = Checklist` without touching the overlay, resets card scroll and, for `CursorMove::Navigate`, runs `maybe_auto_copy`. `CursorMove::Quiet` is for moves that aren't navigation (undo focusing a changed task, `Esc` restoring the search origin, a reload). Every motion goes through it. `focus_and_report` clears the status, moves, then sets the motion's message, so an auto-copy confirmation appends to it (`Match 1/5 · wrapped to top · Copied to clipboard`) rather than replacing it.

**Motions.** `navigate_next`/`navigate_prev` move one item and clamp at a list's edge; `navigate_forward`/`navigate_backward` (`l`/`j`/`→`/`↓`, `h`/`k`/`←`/`↑`) cross into the adjacent list instead, landing on its first unresolved task. The cursor stops on every item, notes included. `Tab` and `Shift-H`/`Shift-L`, `gg`/`G`, `}`/`{` and the digit list jumps are covered under Keybindings. `advance_to_next_incomplete_list` (the list-complete screen's `l`/`Enter`) skips complete lists, wraps to an earlier one, and reaches `AllComplete` only when no list anywhere has unresolved work; `render_all_complete` derives each row's marker from that list's own counts, so the summary can't claim a list done that isn't.

**Card scrolling.** `scroll_card_down_by`/`scroll_card_up_by` move `card_scroll` within `[0, layout.card_max_scroll]`, which the renderer recomputes each frame (and clamps `card_scroll` to, when a reflow shrinks the body); `half_page`/`page` come from `layout.card_viewport_height`. The mouse wheel scrolls an overflowing card, otherwise navigates; over help it scrolls, over the picker it moves the highlight. The wheel isn't hit-tested, so wheeling over the overview moves the task cursor too.

**Toggles.** Every change of task state goes through `set_state(list, item, state)`: snapshot, set, `commit_write`, push the undo point. `toggle_current` (Space/Enter) toggles done (`toggle(Done)`: done → not started, anything else → done), confirms with `Marked done`/`Marked not done` (`toggle_message`), and after completing calls `maybe_transition_screen`: list complete → `ListComplete`, all complete → `AllComplete`, otherwise `advance_to_next_open` (the next item that isn't resolved, wrapping to the list's first open task, never onto a done task). On a note it advances instead. `toggle_item` (an overview marker click) toggles in place and only demotes a completion screen it invalidated. `start_current` (`s`) toggles started in place, confirming with `Marked started`/`Marked not started`. `skip_current` (`-`) toggles skipped and moves on like completing, confirming with `Marked skipped`/`Marked not skipped`. Checking or skipping a critical task first passes `confirm_critical` (see Markdown Parsing): the first press arms it (`critical_armed`, a `CriticalArm` of position, `CriticalAction` and the `Instant` of the latest press) with a sticky prompt naming the action that confirms (`Space` again, `-` again, or for an overview click another click on that marker, since `Space` acts on the cursor's card rather than the clicked one), and the same action on the same task confirms it only once `CRITICAL_PAUSE` (500 ms) has passed since the last press (#219). A quicker repeat keeps it armed, restarts the pause and says `too quick`, so a held key's auto-repeat (which arrives as ordinary presses) or a double press never gets through; tests backdate the armed `Instant` instead of sleeping. A different action re-arms for itself rather than confirming. The key and click wrappers (`disarm_unless_used`) disarm it on any other input, as do the wheel and paste handlers, which bypass those wrappers; `Esc` disarms with `Critical step not checked`.

**`commit_write`, the one write path.** Every mutating action ends here:

1. Take the `WriteLock` (below). `Busy` rolls back with `Another markcheck is saving this file — change not saved, please retry (if this persists, remove <lock path>)`.
2. Read the file once (`read_checklist`) and `content_diverged()`: hash those bytes and compare with `file_content_hash`. A mismatch means another writer changed it since we last looked; roll back, `force_reload()` (a reload bypassing the mtime/size short-circuit) and, if it adopted the new content, show `File changed on disk — reloaded; change not saved, please retry`. A declined reload keeps its own sticky error (`Reload skipped: …`, `Reload failed: …`) rather than claiming a reload, since retrying can't work until the file is fixed.
3. Keep the same bytes as the pre-write content (for git sync's staged-target guard, and as `git_sync.unsynced_base` when none is held), never a second read, so it is the revision the check matched; then `write_back`. On failure roll back (`apply_snapshot`, so memory never diverges from disk) and show why (see Write-Back). A refused non-regular path sets `FileGone::NotRegular`.
4. On success refresh `file_mtime`/`file_size` (the self-write filter), `file_content_hash` from the content written, and `last_update_at`; note an in-place save once (`write_notice`, shown after the action's own status); queue `Effect::GitSync` with the change description.

The fingerprint is always taken from the document's own bytes (`writer::document_contents`), never from a second read, so it can't describe a different revision than the one in memory.

**The write lock.** `writer::WriteLock` is held across the divergence check and the write, making them one compare-and-swap between markcheck instances. The lock file is `markcheck-write-lock-<digest of the canonical path>` in a directory private to the user (`lock_dir`: `$XDG_RUNTIME_DIR` when it's a real directory owned by the user and closed to others, else `<temp>/markcheck-<uid>` created `0700`, both checked by `lock_dir_in`), so every spelling of the file shares one lock, a `git add -A` hook never sees it, and another user can neither plant nor read one. It holds the owner's PID and a random token. An existing lock is read with `open_regular` (no-follow, non-blocking, regular files only) capped at 256 bytes; one that isn't a regular file, or a directory that isn't private, makes the lock `Unavailable` and the write proceeds unlocked (the content check still runs). A stale lock is recovered only when its owner is provably gone (`pid_is_gone`: on Linux `/proc/<pid>` missing while `/proc/self` exists; elsewhere `ps -p <pid> -o pid=` with an empty listing, nothing on stderr and exit 0 or 1 — BusyBox's `ps` has no `-p` and exits 1 on everything), never by age, so a slow live holder is never displaced; anything short of proof honours the lock. Not `kill -0`, which can't tell "no such process" from "not yours to signal". `acquire` returns `LockOutcome::{Acquired, Busy, Unavailable}`, so a caller can't confuse "someone else is writing" with "locking isn't available here". Recovery is remove-then-`create_new`, and the winner keeps it only if a re-read still finds its own token. `Drop` removes the lock only while it still holds this guard's token. The cost: a dead owner whose PID was recycled blocks saving until the file is removed, which is why the message names it. It serialises one user's instances on one machine; a network share between machines is not covered.

**Other actions.** `request_reset`/`reset_scope` (Reset), `undo`/`redo` (Undo / redo), `finish_editor`/`finish_suspend` (Editor integration, Suspend), `apply_copy_outcome` (Effects), `follow_anchor`/`jump_back`/`open_at` (Open link).

**Feedback channels.** `set_status` (ephemeral, auto-expires after `STATUS_TIMEOUT`), `set_notice` (sticky, not an error: a state that lasts, such as waiting on git sync to quit or an armed critical step) and `set_error` (sticky, error-colored: failures and "nothing happened"). `clear_status` runs at the start of every key.

---

## Effects (`src/effect.rs`)

`AppState` never touches the clipboard, a process, git or the terminal. A handler that needs one pushes an `Effect` onto `AppState.effects`:

```rust
pub enum Effect {
    Copy { text: String, primary: bool, kind: CopyKind }, // y, click, y+digit, --auto-copy, o's no-browser fallback
    OpenEditor { line: Option<LineNumber> },              // e, at the current task's line
    Suspend,                                              // Ctrl-Z
    Redraw,                                               // Ctrl-L
    OpenLink(String),                                     // o on an http(s)/mailto link
    GitSync(PendingSync),                                 // every successful write, and an edit made in `e`
    Quit,                                                 // q, or the quit-reset prompt's y (after a successful reset) / n
}
```

The main loop calls `effect::drain(state, host)` once per iteration, after the input event. It pops effects in order, including any queued while draining (a link with no browser falls back to a copy; an `e` edit queues a sync), through one exhaustive `match`, so a new effect doesn't compile until it is handled. Copies run in `drain` itself (`clipboard::copy` along `state.copy_route`, then `apply_copy_outcome`); everything that owns the terminal or a worker goes through the `effect::Host` trait, which `main.rs`'s `TerminalHost` implements. Tests drain against `test_support::RecordingHost` and a per-thread fake clipboard. `Drained { ran_any, quit }` tells the loop whether to redraw and whether to stop; a sync queued alongside `Quit` has already reached the worker by then.

`CopyKind` decides how an outcome is reported: `Command` (`y`, a click, `y` + digit) reports every outcome; `Auto` announces only a success (appended to the motion's message) or a hidden-characters refusal; `LinkFallback` words each outcome around the link.

`GitSync` has slot semantics: `queue_git_sync` replaces one still in the queue. An external reload that brings different content calls `cancel_git_sync`, which removes one still in the queue and queues `Effect::CancelGitSync`, since markcheck never commits an edit it didn't make: by the time a reload is seen, a toggle's request has usually left the queue and is waiting out the quiet period in `GitSync::queued`, so the host's `cancel_git_sync` calls `GitSync::cancel_queued`, which drops that waiting request (a running sync and an armed push retry are left alone) and says whether there was one; when there was, `drain` calls `report_dropped_git_sync`, which ends the reload's message with `— pending git sync dropped` (#178). A reload whose content matches what markcheck last had cancels nothing, since its request is still valid. An `e` edit's `request_external_edit_sync` removes the cancel its own reload queued, so a waiting toggle folds into the edit's commit instead.

---

## Reload From Disk (`src/watcher.rs`, `src/app/reload.rs`)

Edits made outside markcheck (another terminal, `git pull`, a colleague's session) are picked up live.

**Watching, plus a slow poll.** `FileWatcher` (`notify`) watches the file's **parent directory**, non-recursive, filtered to the file's name: a watch on the path itself is lost when the file is replaced by temp-then-rename, which `write_back` and most editors do. An error or a `Rescan` event counts as a possible change. `ReloadTrigger::check` runs `reload_if_changed` on an event or every `RECHECK_INTERVAL` (2 s) regardless, covering a watch that failed to start (inotify limits), a network or FUSE filesystem that never reports remote edits, or a directory recreated under the watch; when the timer finds a change no event reported, it sets the watch up afresh. `check` reports whether anything on screen may have changed, so the idle recheck never forces a redraw. A watch that can't be set up is announced at startup (`Live reload slowed: can't watch the file (<error>) — checking every 2 s instead`).

**Self-write filtering.** `file_mtime` and `file_size` are refreshed after each of our own writes; `reload_if_changed` reloads only when the file's current mtime or size differs. The size cross-check narrows the coarse-mtime window in which a same-instant edit could be missed; the moment of writing is protected separately by the content check in `commit_write`.

**`reload_if_changed`.** The path is classified with `symlink_metadata`, never followed: `NotFound` sets `FileGone::Deleted` (`File was deleted — changes cannot be saved until it exists again`), anything but a regular file `FileGone::NotRegular` (`File was replaced by a symlink or special file — not loaded; …`), each sticky and set once; other errors are transient and retried next tick. While `file_gone` is set, every mutating action refuses through `blocked_by_deletion` with that message, so a deleted file is never recreated and a symlink or FIFO swapped in is never read or written (`read_checklist` and `write_back` refuse them too, so a swap between ticks is caught by the write). A regular file reappearing clears the flag and reloads unconditionally (`File restored — reloaded`). Otherwise an unchanged mtime and size returns early, and a change runs `reload_from_disk`.

**`reload_from_disk`.** It parses the file fresh; a parse with no lists (`Reload skipped: file has no checklist items — changes cannot be saved until it has tasks again`) or an error (`Reload failed: <reason>`) keeps the last good document, sticky. On success, before swapping the document in, it:

- remaps the cursor (`remap_position`): the same list (by title and its rank among same-titled lists) and the same task by content (`header`, `display_text`, `depth`; the nearest to the old line among several), else the item on the old line, else the nearest; the last case and a vanished list add `— task not found, cursor moved` to the message;
- summarises what changed (`reload_changes`): tasks matched by list, label and depth as a multiset (duplicates pair up in order, a task that only moved lines isn't a change, an edited one reads as removed plus added), state moves counted by direction (done, started, reopened, skipped) with the first two names, and tasks added and removed, as in `Reloaded: 2 done, 1 reopened ("Restart service", "Verify output") · 1 added`; with no task change, `Reloaded: file changed on disk`;
- keeps an open overlay open (`keep_overlay_across_reload`): `Search` re-runs against the new content with its `Esc` origin remapped, `ListPicker` and `Help` stay with clamped state, and a reset or quit-reset dialog is cancelled with a sticky `File changed on disk — reset cancelled` (or `quit cancelled`), since it was asked about the old content; a completion view returns to `Checklist`.

It then clears undo/redo (and bumps `history_epoch`), the jump stack and any armed critical step, cancels a queued git-sync request, disarms pending chords, and takes `file_content_hash` from the adopted document. A reload never auto-copies.

**Another session on the same checklist (#101).** At startup `writer::SessionMarker::register` writes `markcheck-open-<digest>-<pid>` in the lock directory and lists the other markers for that file. Markers whose PID is provably gone are removed; each live one adds `already open in another markcheck (pid 4242, opened 12 min ago): its changes show up here as reloads` to the startup message. The marker is removed on exit. Same user and machine only.

---

## Recurring defect classes

Every deep review of this project has found new bugs, and by round three of
the third pass a pattern was unmistakable: the same *shapes* keep coming
back, usually in code written to fix an earlier instance of the same shape.
This list is the audit trail turned into a checklist, so a future review
starts from what has already bitten us instead of rediscovering it. Each
entry names the shape, what it cost, and what to grep for.

1. **Symbolic identity re-derived instead of captured.** Asking git the same
   question twice — `HEAD` for a check and `HEAD` again for the action — is
   asking about two different commits. Cost: the fast path and the startup
   catch-up published commits they had never validated. Cure: resolve once,
   name that SHA everywhere. *Look for:* the same query issued twice, a
   literal `"HEAD"` anywhere but a capture.

2. **Two reads of one file assumed to describe each other.** The same shape
   for the filesystem. Cost: the content fingerprint described a revision
   the in-memory document wasn't, so the next write silently overwrote a
   newer edit; and a stat taken after a parse made an external change look
   already seen. Cure: derive from the bytes you already have
   (`Document.raw_lines` concatenates to its own source), or cross-check.

3. **Failure collapsed into a verdict.** `unwrap_or`, `.ok()`, and an
   ambiguous `None` turn "couldn't tell" into "fine". Cost: an unanswerable
   history check read as "nothing unrelated here" and published; an empty
   `rev-list` read as a root commit, whose undo path *deletes the branch*.
   Cure: three states, not two — and pick the fail direction per caller,
   since refusing is the consequential action for some and pushing is for
   others. *Look for:* every `unwrap_or`/`.ok()` on a fallible git call.

4. **One question answered from two sources.** Cost: `push` read the
   *config* for "is there an upstream?" while the guard resolved `@{u}`;
   when they disagreed the guard stood down and the push published unrelated
   work. Cure: one authority per question.

5. **Acting on something no longer yours.** Cost: a group kill aimed at a
   reaped (possibly recycled) PID; a rollback that rewound commits markcheck
   never made; a lock guard deleting another instance's lock; a realignment
   overwriting a version the user had staged. Cure: check ownership
   immediately before the destructive step, and prefer leaving a mess you
   can report over destroying something you can't restore.

6. **Unbounded waits.** Cost: `join()` on a pipe thread that a lingering
   descendant kept alive forever, defeating the timeout on every subprocess.
   Cure: bound every wait, and spend one budget across a teardown rather
   than one per step. *Look for:* `join()`, `recv()`, `read_to_end`.

7. **Superseded state left live.** Cost: a stale retry timestamp firing
   every frame; a queued sync request describing content that no longer
   existed; an armed input chord pointed at a card the user never chose.
   Cure: whatever a reload invalidates, clear it in the same place.

8. **Ambient environment trusted.** Cost: an inherited `GIT_DIR` made
   git-sync read one repository and commit into another. Cure: neutralise
   what redirects the target; leave what is the user's to set.

9. **Tools asked questions they cannot answer.** Cost: `git status` was the
   untracked oracle, and it says nothing at all for a gitignored file or
   under `status.showUntrackedFiles=no` — so git-sync sat silent forever.
   Cure: ask the component that actually knows (here, the index).

10. **Superseded prose left in present tense.** Not a runtime bug, but the
    same failure mode in documentation, and it caused real confusion: this
    document described a bare `git push` for several rounds after the code
    had stopped doing that. Cure: when behaviour changes, rewrite the
    paragraph that described it rather than appending a newer one below.

11. **A test that pins an outcome but not its cause.** Cost: both
    `undo_commit` compare-and-swap tests asserted only `result.is_err()`
    alongside an unchanged `HEAD`. That pair is satisfied by *any* failure,
    so an `undo_commit` gutted to return `Err` unconditionally passed both —
    they proved the branch was intact without proving the CAS is what kept it
    intact. Cure: assert on the reason (here, that the message came from
    `update-ref`), not just the outcome. See "Verifying the guards" below for
    how these were found.

### Verifying the guards

The safety guards in `git_sync/guards.rs` and `writer.rs` are the code most likely to rot silently: they run only on paths ordinary use never reaches, so a guard that stopped working wouldn't show in normal testing. Class 11 above is what that rot looks like from the test side.

The check is mutation testing with [`cargo-mutants`](https://mutants.rs/), configured in `.cargo/mutants.toml`: `cargo mutants` mutates every function in those two files (a comparison flipped, a guard made always-true, a body replaced by a default) and reruns the unit tests (`--bins`) for each. **Every mutant not excluded must be caught.** The exclusions are listed in the config with the reason each can't be caught: mutants that produce identical behaviour (`^` for `|` on flags that share no bits), effects no test can observe (a directory fsync), and branches reached only by a race between two instances. `.github/workflows/mutants.yml` runs it weekly and on demand, since it reruns the suite once per mutant (about an hour), and keeps the report. Run it after changing a guard, and prefer it to a coverage number: coverage proves a line ran, a surviving mutant proves nothing checked what it did.

The first complete run (2026-10-20) tested 214 mutants: 199 caught and 13 unviable (they don't compile). The other two were fixed rather than excluded. One was a filter in `SessionMarker::register` that `pid_is_gone` already covered, so it was removed. The other was a FIFO test that hung instead of failing, so it now runs under a deadline (`within_seconds`) and a regression into a blocking open fails it.

---

## Git Sync (`src/git_sync/`)

After markcheck writes the checklist, git sync commits that change and pushes it, so a checklist shared through git stays in step across machines. It is off by default, turned on by `--git-sync` (this run) or a `git_sync_paths` entry in the config (see Config File; a plain entry covers only the repository it names), and off for one run with `--no-git-sync`. Its promise: **markcheck commits exactly the change it made and publishes exactly one commit it validated, or it refuses and says why.**

**Modules, in one direction only.**

```text
process  ->  inspect  ->  guards  ->  commit  ->  sync  ->  mod (GitSync)
                            |           |          |
                            +---------> push ------+

shared: imported by any of them, imports none of them
```

`shared.rs` is the leaf (`SyncOutcome`, `lock_hash`, the test race points); `process.rs` runs git; `inspect.rs` asks the repository questions; `guards.rs` decides what must not happen; `commit.rs` builds the commit; `push.rs` publishes one; `sync.rs` sequences a request; `mod.rs` holds `GitSync`. Nothing calls upward, and only `GitSync`, `SyncOutcome`, `status_message` and `exit_report` are visible outside the module.

### Activation

`GitSync::detect` first runs `git version` and refuses a git older than `MIN_GIT_VERSION` (2.22: `%(trailers:key=…,valueonly,separator=…)` arrived then, and `%(push:remotename)` in 2.16) with `git 2.22 or newer is needed (found <version>)`, since below it the trailer check never recognises markcheck's own commits and the push-remote check refuses every push with a wrong reason (#169); output it can't parse is let through. It then runs `git rev-parse --is-inside-work-tree` and `--show-toplevel` in the file's directory and stores the root as `repo_root`; every later command runs from there, naming the checklist by its absolute path or `ls-files --full-name`. Failure (not a repository, inside `.git`, no `git`, a git too old, a refused repository) leaves the feature off for the session with a sticky `Git sync off: <reason>`; for a refusal the reason is git's whole stderr, which ends with the `safe.directory` command that fixes it. `main.rs` then applies `launch::git_sync_scope`: an entry without `/**` accepts only a repository rooted at the entry or above it, because a repository nested inside it may have come from elsewhere and git would run its hooks and config (`uploadpack`, `core.sshCommand`, `core.fsmonitor`). `detect` runs only `git version` and `rev-parse`, which run none of those, so the scope check comes before any command a nested repository could hijack. At startup `sweep_interrupted_syncs` removes temp indexes a killed markcheck left, and `request_catch_up_push` starts the catch-up below.

### Requests and the worker

`AppState` never runs git: `commit_write` queues `Effect::GitSync(PendingSync)` after every successful write, and `finish_editor` queues one (`immediate`, described `Edited in <program name>`, never its directory) after an `e` edit that reloaded and exited successfully. A non-zero editor exit is an abort, reloaded but not synced. A change reloaded from elsewhere queues nothing: markcheck never commits an edit it didn't make, and a reload with different content cancels a request still queued or waiting in `GitSync` (`Effect::CancelGitSync`, see Effects).

**Own changes only (#177).** A toggle's commit is built from the whole file, so it must not carry an edit markcheck didn't make: an uncommitted draft line, an outside edit markcheck reloaded, an `e` edit whose editor failed. `AppState.git_sync.unsynced_base` remembers the content markcheck's own unsynced changes started from: `commit_write` takes the pre-write content when none is held, and `request_external_edit_sync` takes the saved `e` content; each new base bumps `base_epoch`. It survives failed syncs, so toggling again after a refusal still syncs, and `reload_from_disk` drops it when the reload brings content that differs from what markcheck last had. Each request carries `SyncBase::Own { epoch, content }` (or `Edited { epoch }` for the `e` edit). `GitSync` remembers the epoch of the last `e` edit (`edited_epoch`: toggles on that base may publish the edit too) and what the last content sync committed and on which base (`landed`, from `in_flight` once the outcome is `Synced`, `Skipped` or `CommittedNotPushed`), and checks a later request on that base against that content instead, since its commit moved `HEAD` past the base. `run_sync` gets the result as `Publish::Everything` or `Publish::OwnChangesSince(base)`.

`GitSync::request` folds a request into one `queued` slot — the latest content, hash and `base`, the earliest `previous_content` (what the batch replaces), every description in order — due `delay` after the last request (`--git-sync-delay`, `git_sync_delay`, default 10 s; `0` for at once; at most `MAX_GIT_SYNC_DELAY`, a day, enforced by clap's `value_parser` range and by `load_config`), or at once when `immediate`; a delay too long to add to `Instant::now()` leaves `due` as `None`, waiting for `flush`. `tick` (each frame) and `poll` start it once due and nothing else runs, on its own thread, reporting a `SyncOutcome` over a channel. Only one sync runs at a time per process, so two commits never race on one index. `flush` makes the waiting request due now; every quit path calls it. `batch_message` keeps one description as the subject and lists several in the body (`checklist.md: 3 changes`). Commit subjects are `<file>: <change>` (`checklist.md: Check "Restart service"`), capped at 80 characters by cutting the change with `…` and closing a cut quote. Every markcheck commit ends with a `Synced-by: markcheck` trailer.

`run_reporting_panics` turns a worker panic into `Failed`, so `busy` can never stick; `lock_hash` tolerates a poisoned hash mutex.

### Running git (`process.rs`)

- **Bounded.** Every command runs through `run_with_timeout`: `PLUMBING_TIMEOUT` (10 s) for local plumbing, `PUSH_TIMEOUT` (60 s) for `push` and `ls-remote`, `COMMIT_TIMEOUT` (300 s) for `git commit`, whose hooks and signing can be slow. On timeout the whole process group gets `SIGTERM` (git's handlers remove its locks), then `SIGKILL` after `KILL_GRACE` (1 s), with a hint to remove `.git/index.lock` when the kill was needed. The group is signalled only while its leader is unreaped.
- **In its own session.** `spawn_in_new_session` calls `setsid()` in `pre_exec` (see Dependencies for the crate's `unsafe` calls), so a child has no controlling terminal: an HTTPS or SSH prompt that opens `/dev/tty` fails at once and is reported, rather than drawing over the TUI and waiting out the timeout; graphical askpass keeps working. `GIT_TERMINAL_PROMPT=0` makes git's own prompt fail cleanly. A session alone doesn't stop a curses pinentry, which `gpg-agent` opens on the terminal `GPG_TTY` names, by path, so `git_command` also removes `GPG_TTY`: a signed commit whose key isn't cached then fails with git's `gpg failed to sign the data`, while a graphical pinentry keeps working. Not covered: an agent already pointed at a terminal by `gpg-connect-agent updatestartuptty` still draws its pinentry there.
- **Isolated.** `git_command` removes `REDIRECTING_GIT_VARS` (from `git_env.rs`, shared with `build.rs`: `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, …, which hooks and `rebase --exec` export and which would make git read one repository and commit into another) and sets `GIT_OPTIONAL_LOCKS=0` (and removes `GPG_TTY`, above), so the background `git status` never takes the user's `index.lock`. It also sets `LC_MESSAGES=C` and removes `LANGUAGE` (`untranslated_messages`), because `failure_reason`, `describe_reason` and `detect_git_error` match git's English `fatal:`/`error:`/`hint:` prefixes and wording, which a translated git changes; only messages change, so git and the hooks `git commit` runs keep the user's character set. A set `LC_ALL` would override `LC_MESSAGES`, so it is removed and its value given to each other `LC_*` category instead. Commands taking the checklist as a pathspec use `literal_pathspec_command` (`GIT_LITERAL_PATHSPECS=1`, the glob and icase variables removed), so `notes[1].md` can't match `notes1.md`.
- **Output to files, not pipes.** stdin, stdout and stderr are `0600` scratch files in the temp directory, removed by a `Drop` guard, so a descendant that outlives git (ssh, a credential helper, a hook) can't block a reader, and no drain threads exist. A scratch file that can't be created (a `$TMPDIR` that is missing or unwritable) fails the command with `cannot create git scratch files in <dir>: …`, never with the `NotFound` kind, so `detect_spawn_error` doesn't report it as `git not found on PATH`.
- **Tracked for abandoning.** Each `GitSync` owns a `Children` registry that its worker threads install thread-locally; a child is spawned, reaped and signalled only under its lock, and an abandoned registry starts no further git.

### One sync, end to end (`run_sync`)

1. **`repo_sync_blocked`** refuses during a merge, cherry-pick, revert, bisect, sequencer run or rebase (marker files under the git dir), on a detached `HEAD`, and with unresolved conflicts anywhere, and when any of that can't be checked.
2. **Trackedness** comes from the index (`index_entry`: tracked, untracked, or failed), not from `status`, which says nothing for an ignored file or under `showUntrackedFiles=no`. Untracked is `SkippedUntracked`, reported and never `git add`ed. A file name that isn't valid UTF-8 is refused rather than converted. `status --porcelain` then says whether there is anything to do.
3. **The tip** is resolved once (`resolve_parent`, which tells "no commits yet" from a failed lookup) and that SHA is used for every later check, the blob read, the push and the new commit's parent.
4. **The unpushed history** is checked before either path that can push (`unpushed_history_blocks`, see Publishing).
5. **Fast path:** if the tip already holds this content (compared as blob SHAs through `hash-object --path`, so clean filters apply), only push when ahead.
6. **Staleness:** the file on disk must match the newest request git sync knows of (`latest_requested_hash`), read through `read_checklist`; anything else is an outside edit or deletion and is refused ("the file changed after this sync was queued, so it was not published").
7. **Own changes only:** for `Publish::OwnChangesSince`, the base must be `HEAD`'s blob or the staged one (`outside_changes_would_be_published`, failing closed on a base that couldn't be read), else refuse ("the checklist has changes markcheck didn't make, so nothing was published; commit them yourself, then toggle again"). The staged copy counts so that staging the checklist and then toggling syncs normally.
8. **Staged target:** if the index holds a version of the checklist that differs from what markcheck loaded (`previous_content`), committing would drop it; refuse (`staged_target_would_be_lost`).
9. **Commit through a temporary index** (`commit_via_temp_index`): a fresh `GIT_INDEX_FILE` under the git dir (`markcheck-index-<pid>-<hex>`) is seeded from the tip's tree, the expected blob is staged at the checklist's path, `HEAD` and `repo_sync_blocked` are re-checked, and a plain `git commit` runs, so the user's staged work never rides along while hooks and signing still run. The new commit is never taken to be whatever `HEAD` is afterwards (a `post-commit` hook, or the user in another terminal, can commit on top first, and a timed-out commit may or may not have landed): `own_commit_since` finds it as the oldest commit on `HEAD`'s first-parent chain since the parent that holds exactly this blob and carries the `Synced-by: markcheck` trailer, and it is adopted only while it is still `HEAD`. With something on top it is reported and left in place with the other commit, neither undone nor pushed; not found at all, the branch is left alone and that is reported. The temp index (and its `.lock`) is removed on every path.
10. **Scope:** `verify_commit_scope` requires the new commit (and anything between the parent and it) to touch only the checklist; a hook that staged more gets the commit undone (`undo_commit`: `update-ref` compare-and-swap to the commit's own first parent — for a root commit, deleting the branch ref — refusing when the parents can't be read, never resetting to the captured parent). A scope that can't be checked leaves the commit in place and says so.
11. **Real index:** `align_real_index_entry` points the real index's entry for the checklist at the committed blob, unless the user changed that entry meanwhile, so `git status` doesn't show the file as both staged and modified.
12. **Push**, only if `HEAD` is still the commit just made (`push_if_head_unchanged`; otherwise `CommittedNotPushed`).

Commit content always comes from the request, never from re-reading the working tree, so a concurrent write is never absorbed under the wrong message. Hooks run as for a manual `git commit`, which is why a nested repository needs an explicit opt-in.

### Publishing (`push.rs`)

markcheck publishes **one explicitly named commit**, the validated tip, as `git push <remote> --force-with-lease=<ref>:<remote tip> <sha>:<ref>`, never a bare `git push`, and only after:

- **an upstream exists and agrees.** `@{u}` must resolve (the cached tracking ref is the one authority for "is there an upstream"), `@{push}` must be the same ref with the same remote (`push_goes_to_upstream`), so `pushRemote`, `push.default` and triangular setups are respected rather than reimplemented, a remote of `.` (a local upstream) is refused, and so is a remote with no configured push URL. No upstream: the commit stays local and the message says to `git push -u` once.
- **the remote still has the branch.** `ls-remote` (matching the ref exactly) is asked; a branch deleted upstream is not recreated (`CommittedNotPushed`).
- **there is a commit to push.** An empty SHA would format as `<remote> :<branch>`, git's delete-a-remote-ref form, which exits 0; `push` refuses it before building any refspec.
- **it is a fast-forward.** `is_ancestor(remote tip, sha)`, false when it can't tell; with that, the lease can only reject (a branch that moved or vanished in between, reported as "the remote branch changed while pushing (stale info)"), never force.
- **the unpushed range is publishable.** Pushing any commit publishes its ancestry, so `unpushed_history` walks `@{u}..tip` commit by commit (a net diff would miss a change and its revert). It shares `range_commits` with the scope check (`range_has_unrelated_commits`): one `git log -z --name-only` lists every commit of a range with its parents, paths, `Synced-by` trailer and subject, however long the range, so a day of offline commits costs one process rather than two per commit. Each commit's record opens with an empty field, which no path can be, so a file named like a SHA is never read as the next commit; the options a user's config could flip (`--root`, `--no-renames`, `--no-color`, `--no-show-signature`, `--ignore-submodules=none`, `--encoding=UTF-8`) are passed explicitly, and output that doesn't parse is an error, so the guard refuses (#166). A commit is publishable when it is markcheck's own — the `Synced-by: markcheck` trailer and exactly one `.md` path changed — or, for a toggle's sync (`Publishable::OwnOrThisChecklist`), when it touches only this checklist; an empty commit touches nothing, so it never qualifies, since pushing it could start a pipeline the user meant to start later (#198). A merge never is. The first commit that fails comes back named (`commit abc1234 "WIP: reorder" is not pushed yet and isn't markcheck's; push or drop it first`). A check that can't be answered refuses; only a missing upstream proceeds (commit, don't push).

**Outcomes.** `Synced`, `Skipped`, `SkippedUntracked`, `CommittedNotPushed { message, commit }` (the commit is safe locally; a retry is armed), `Failed` (nothing committed, or a commit left in place and named), `RetryFailed`, `RetryAbandoned`, and for the startup catch-up `CaughtUp { pushed }` and `NotCaughtUp`. `failure_reason` picks the line that explains a failure: a push's rejection line or a remote hook's message, else git's `fatal:`/`error:`, else the last line (hook frameworks print their banner first), from stderr or, when that's empty, stdout.

**Retries.** `GitSync` remembers the SHA of the last `CommittedNotPushed` and `retry_push_if_due` pushes that commit (never rebuilding it from content) after `PUSH_RETRY_INTERVAL` (30 s), doubling up to 5 minutes for repeated failures of the same commit; a waiting request makes it stand aside, since that sync pushes the commit too. `retry_commit` gives up quietly (`RetryAbandoned`) when `HEAD` has moved on. Only a retry's own failure (`RetryFailed`) restarts its backoff.

**Startup catch-up.** `catch_up_push` pushes a commit an earlier session left unpushed and can never commit. It refuses a blocked repository, reports an untracked checklist, requires a positively known unpushed commit (anything unanswerable is a quiet `Skipped`), and publishes only markcheck's own commits (`Publishable::OwnOnly`): a commit the user made and kept local is named in a sticky `NotCaughtUp` instead of being published by opening the file. A push reports `Pushed N earlier markcheck commits`.

### Reporting

`AppState::apply_git_sync_outcome` is the one place an outcome reaches the UI: `git_sync::status_message` gives the status-bar text, sticky and error-colored (`Git sync failed: …`, `Committed, not pushed: … (will keep retrying)`, `Push retry failed: … (will keep retrying)`; nothing for `Synced`/`Skipped`/`RetryAbandoned`), and the title bar keeps the state — `⇅ git` while active, then `pending`, `syncing…`, `Synced … ago` (fading after 2 minutes), or an error-colored `not pushed`/`not synced`/`not tracked` until a later sync lands (a failure also clears the last success, so it can't vouch for newer changes). A retry failing the same way as before only updates the title bar.

### Quitting

- **`q` during a sync** keeps the TUI open with the sticky `Waiting for git sync… press q again to abandon` and exits once the sync settles; other keys re-show the notice and nothing new is queued. A second `q` abandons.
- **`Ctrl-Z`** waits up to `QUIT_WAIT` for the sync before stopping, without abandoning it; see Suspend.
- **`SIGTERM`, `SIGHUP`, `SIGINT`** (`StopSignals`; a second `SIGTERM`/`SIGINT` exits at once) end the loop without drawing (after `SIGHUP` the terminal is gone), wait up to `QUIT_WAIT` (5 s), then abandon.
- **Abandoning** (`GitSync::abandon`) sends `SIGTERM` to the running git group, gives the worker `ABANDON_GRACE` (2 s) to unwind through its own cleanup (temp index, scratch files), then `SIGKILL`; no further git starts. No orphaned `git commit` is left to move `HEAD` after markcheck is gone.
- **The exit report**: the last outcome seen goes to `exit_report`, and anything but `Synced`/`Skipped`/`RetryAbandoned` (or a sync still running) prints one line on stderr after the screen closes. The exit status stays 0, except that a second `SIGTERM`/`SIGINT` (`register_conditional_shutdown`) exits at once with status 1, with no report and the terminal not restored. A pending retry's backoff isn't waited for; the startup catch-up picks the commit up next time.
- **What a kill leaves** (`SIGKILL`, OOM, power loss) is swept at the next start: `writer::sweep_interrupted_saves` removes `.<name>.markcheck-tmp-*` and `-new-*` beside the checklist, and `sweep_interrupted_syncs` removes `markcheck-index-*` (and `.lock`) in the git dir, both through `remove_leftovers`: only regular files named `<pid>-<hex>…` whose PID is provably gone.

### Testing races

`RacePoint` names points inside a sync (after history validation, before the commit, before index alignment, before the push) where a test runs a real concurrent git process at exactly that moment (`RaceHook`). Hooks are thread-local, and outside `cfg(test)` `race_point` is empty. Interleaving found defects that pre-arranged state never could.

**Not covered:** there is no repository-wide lock across processes. Each step's own check (the parent re-check, the exact-SHA scope check, the CAS undo, the `HEAD` checks before pushing, the explicit refspec and lease) fails safe on interference from another process, but a new git-sync feature must bring its own guard.

---

## UI Layout (`src/ui/`)

```text
┌────────────────────────────────────────────────────────────────────────────┐
│  Doc Title │ file.md        ↻ Updated 10s ago │ ⇅ git · Synced 8s ago │ ☑ 3/12 │  title bar
├─────────────────────────────────────────────┬──────────────────────────────┤
│                                             │ Overview───────────────────┐ │
│         ╔ ☐ ═══════════════════ 2/7 ╗        │ • [1] List 1            │ │
│  ┌ ☑ ───║       ▪ Card title        ║─ ☐ ──┐│   ☑ task 1                 │ │
│  │ prev ║                           ║ next ││   ❯ task 2                 │ │
│  │(tuck)║    centered body text     ║(tuck)││   ☐ task 3                 │ │
│  └──────║                           ║──────┘│                            │ │
│         ╚═════════ • • • ═══════════╝        │                            │ │
│                                             │                            │ │
├─────────────────────────────────────────────┴──────────────────────────────┤
│  hjkl:nav  space:toggle  /:search  y:copy  ?:help  q:quit                     │  status bar
└────────────────────────────────────────────────────────────────────────────┘
```

Everything is drawn fresh each frame from `AppState` by `ui::render`; the renderer writes back only `state.layout` (see `ViewLayout`) and the clamp of `card_scroll` to the card's new scroll limit. Colors come from `state.palette`, glyphs from `state.icons`. **Everything that fits text to a row measures display cells, not characters** (`display_width`, `char_width`, `split_at_width`, `truncate`, `split_width`): a wide glyph takes two cells.

**Layout.** The working area is capped at 120 columns and centered. Vertically: title bar (1 row), a list-tab row (narrow layout with several lists only), the progress bar (1 row), the main area, and the status bar (one row, more for a long message). The main area splits into the cards, a 2-column gap, and the 30-column overview.

**Responsive tiers.** Below `OVERVIEW_MIN_WIDTH` (60) the overview is hidden and there's a single full-width card; the list tabs then get their own row when there are several lists (`render_list_tabs`: the current list a pill, the others dim; `fit_tab_titles` shows every title in full when they fit, cuts them proportionally to no less than 3 characters each when they don't, and falls back to the current tab alone, cut to 20). With the overview but a card area under `NARROW_THRESHOLD` (80) there is still a single card; wider gets the three-card stack.

**Title bar.** Left: static identity, `{file icon} {document title} │ {filename}` — the H1 (or a title from the file stem, styled the same) in `palette.current` bold and the filename dim. `split_width` guarantees the filename up to `FILENAME_MIN_WIDTH` (24) columns and cuts the title with `…`. Right: the dynamic cluster, sized to its content with the counter outermost so it never moves — the `↻ Updated 10s ago` tag (dim yellow; our write or a reload; `Updated just now` under 5 s, then floored to 5 s steps; hidden under 80 columns and after 2 minutes), the git-sync section (below), and `☑ done/total` with `(+N skipped)` when any are, whose color fades from started-yellow to done-green with completion (`Palette::progress_color`, interpolated on truecolor, snapped to the 256 cube, stepped on ANSI-16).

**Git-sync section.** Present for the whole session when git sync is active (`git_sync.active`, fixed at startup). `{⇅} git`, then by precedence: `· not pushed`/`· not synced`/`· not tracked` in `palette.error` while a problem stands, `· syncing…` while a sync runs, `· pending` while a change waits out the quiet period, `· Synced 8s ago` in green while recent (`relative_label`, shared with the update tag). The section is drawn at every width, so a failure stays on screen in a narrow terminal; under 80 columns (`UPDATE_LABEL_MIN_WIDTH`) only the `· Synced 8s ago` text is left out, as the update tag is. The word "git" carries the meaning; `⇅` is the same plain glyph in both icon sets.

**Progress bar.** One row under the title bar: done `━` in `palette.done`, started `━` in `palette.started`, the rest dim `─`, from `Document::progress` (skipped tasks are outside its `total()`), proportional via `progress_fill`.

**Above the card.** A block of reserved rows, centered together with the card (`render_checklist`), top to bottom: the current list's `## H2` title (`render_list_heading`, one row cut with `…`; absent for the default list), the banner (`banner_rows`, word-wrapped in full in `palette.warning` bold behind the note icon), the current task's preamble (`preamble_rows`, up to `MAX_PREAMBLE_ROWS` = 6, the last ending in `…` when cut; an alert in the warning color behind the note icon, a quote behind a `│` bar, code in the code style), the `### H3`+ breadcrumb (`render_section_breadcrumb`, dim, ` › `-joined, cut to the card width), then one blank row. The block has priority over the card's height (`fit_heading`): the card shrinks, its body scrolling, down to `MIN_CARD_HEIGHT`, and only if the block still doesn't fit above that are rows dropped, in order the blank row, the breadcrumb, the list title, then preamble rows from the end (prose before alerts), and banner rows last, the last row kept in a cut part ending in `…`. A main area shorter than `MIN_CARD_HEIGHT` shows only the card.

**Card stack.** The center card is `CENTER_CARD_PCT` (72%) of the row and as tall as its content (`desired_card_height`: the wrapped title and body at the width it will render, plus borders, clamped to `[MIN_CARD_HEIGHT, ~60% of the area]`, computed each frame through the same wrapping functions as the drawing, so scroll limits are exact). All word wrapping (`wrap_text` for plain text, `wrap_tokens` for styled card bodies, `wrap_modal_lines` for dialogs) goes through one greedy wrapper, `greedy_wrap`, which measures words in display cells, hard-splits a word wider than the row, and leaves a cell for a continuation marker when the caller asks; `hard_wrap` is the one character-level splitter, for fenced code. The side cards are tucked one row down, two rows shorter, and run `STACK_OVERLAP` columns under the center card from its own edges. Drawn prev, next, `Clear` on the center rect, center.

**Card anatomy.** Task cards have a thick border, notes a rounded one. The top border carries the state marker (`state_marker`: one mapping from kind and info-parent state to glyph and color, shared with the picker, the overview and the dots) on the left, ` n/m ` on the right, and for a critical step still to do a centered ` critical `, with the whole frame in the warning color. The border takes the state accent (`accent_color`: done green, started yellow, an idle note blue, a display-only parent the aggregate state of its sub-list); it is bold on the center card and dim on side cards. Inside: an optional parent-chain breadcrumb (nested items, in the sub-list's depth color), the title (`{note icon} {step number} {header}`, bold in the default foreground, wrapped and centered) and a blank row, then the body. The body has no checkbox, sits inside `BODY_PAD` columns, is centered vertically when it fits and scrolls (`card_scroll`, with a scrollbar and a ` first–last/total ` indicator) when it doesn't. Prose is centered up to `PROSE_CENTER_MAX_LINES` lines, else left-aligned; emphasis, bold and strikethrough map to italic, bold and crossed-out; inline code uses the `code` style chip; a link shows only its underlined text (the URL is listed below the card), with a dim `[N]` after each when there are several. A trailing command (the body's last meaningful span is inline code) moves to its own line. Fenced blocks are boxes in the `code` style spanning the body width, left-aligned (plain lines when the card is too narrow for a box), hard-wrapped with a dim `↩` on every continued row (`soft_wrapped`; `wrap_tokens` does the same for a split chip); tabs are shown as spaces to the next `TAB_WIDTH` (4) stop (`expand_tabs`) while copies keep the real tabs. With two or more copy candidates, each chip gets a dim `[N]` and each box `┌─[N]──┐`. A done body is dimmed; a skipped one is dimmed and struck through. The bottom border carries the position dots (`position_dots`: current `◉`, done `●`, started `●` (`◐` without color), skipped `-`, pending `·` dim, a note `·` blue), dropped before the scroll indicator when both don't fit. Side cards are plain dim text: the title, a blank row, `display_text`.

**Below the card.** The links of the current card, numbered when there are several (`render_link_panel_below`), or a hint when they don't fit.

**Overview.** A progress view that takes no keys but whose rows are click targets. Rows: `{list icon} [n] Title` per list (only with several lists; single-list documents de-indent), a banner row, `── Text` sub-section dividers before each group (`new_sub_headings`, `section_divider`), dim one-line preamble rows (an alert keeps its color), and item rows: `│` depth guides, the state marker (done > started > skipped > note > current `❯` > pending, so state shows even on the current row; a nested current marker takes its sub-list's color under the reversed highlight), a warning-colored `!` for a critical step still to do, and `Item::label()` cut with `…`. Done labels are dimmed, skipped ones dimmed and struck through. When groups scroll off the top, their list header and active sub-section path are pinned as a **sticky header** in reserved rows above the list, computed by iterating render and scroll offset to a fixed point (at most 4 passes), never covering a row. Each visible row's `Rect` and target go to `state.layout.overview_rows`: a task row is split into the marker zone (`Toggle`) and the label (`Item`); notes and list titles map whole; banners, dividers, preambles and the sticky header aren't clickable. Cleared when the overview is hidden. The panel title is just ` Overview `: progress is counted once, in the title bar.

**Scrollbars.** Cards, the overview and help share `render_scrollbar`, drawn over the right border only on overflow; `content_length` is the number of scroll positions (`total - viewport + 1`), so the thumb lands flush at the bottom.

**Modal cards.** The completion screens, both confirmations, help and the picker replace the cards area. `modal_card` word-wraps the descriptive lines and the key lines separately and sizes the card to them; on a short terminal `fit_modal_rows` trims the description and ends it with `…`, never a `Press …` line. Borders: the completion screens and the quit-reset green, the reset confirmation yellow (`palette.started`), help rounded in `palette.current`. Border titles are padded a space each side (` Title `) — a convention for every bordered widget.

**Status bar.** The status message when set, otherwise the legend for the view (`KEYBIND_LEGEND`, `LIST_COMPLETE_LEGEND`, `ALL_COMPLETE_LEGEND`, and per-overlay legends such as the reset dialog's `confirm_legend`), cut to the width: `hjkl:nav  space:toggle  /:search  y:copy  ?:help  q:quit` on the checklist, `l/enter:next list  h:review  ?:help  q:quit` and `h:review  R:reset  ?:help  q:quit` on the completion screens. A long message wraps (`statusbar::height`) to at most `MAX_STATUS_ROWS` (6) and a quarter of the terminal, since the actionable part of the longest ones (a lock path, a git error) is at the end. Errors (`status_is_error`) render in `palette.error`. Ephemeral messages expire after `STATUS_TIMEOUT` (4 s; `expire_status` each tick); sticky ones last until the next input. The digit prompts and the pending-`g` prompt use their own one-row slot. The search prompt does too, but a message set while search is open goes on its own rows above it (`height` counts the prompt row), since search is the one mode a user stays in while the file, git sync or an opener can report something.

**Terminal title** (when on): see Terminal Setup.

---

## Completion Screens

The base `view` (`Checklist`, `ListComplete`, `AllComplete`) and the optional `overlay` are separate: an open overlay takes every key first, closing it returns to the view underneath, and only overlay handling assigns `overlay` (opening one, its own keys closing it, `Ctrl-C`, and a reload cancelling a confirmation it made stale).

`maybe_transition_screen()` runs after every completed task (checked or skipped): every checkbox of the current list resolved → `ListComplete`, every list resolved → `AllComplete`, otherwise `advance_to_next_open`.

**ListComplete card** — a green modal over the cards area: a check heading, the list's title and `done / total tasks completed`, and the keys (`l` / `Enter` for the next unfinished list, `h` to review). Its legend lists only those.

**AllComplete screen** — a green modal summarising every list (each row's marker from that list's own counts) and the total, with `R` to reset, `h` to review and `q` to quit. Quitting with everything done first offers the quit-reset (see Reset).

```text
┌──────────────────────────────────────────────┐   ┌────────────────────────────────────────────────┐
│             ✓  List Complete                 │   │             ✓  All Tasks Complete              │
│            ──────────────────────            │   │            ────────────────────────            │
│   Prepare workspace                          │   │   ✓  Prepare workspace       7 / 7             │
│   7 / 7 tasks completed                      │   │   ✓  Second workspace        5 / 5             │
│                                              │   │   Total: 12 / 12 tasks                         │
│   Press  l / Enter  to go to next list       │   │                                                │
│   Press  h  to review tasks                  │   │   Press  R  to reset · h to review · q to quit │
└──────────────────────────────────────────────┘   └────────────────────────────────────────────────┘
```

---

## Keybindings

| Key | `Checklist` | `ListComplete` | `AllComplete` |
| ----- | ------------- | ------------------- | --------------- |
| `h` / `←`, `k` / `↑` | prev item (prev list at start) | back to the checklist, cursor on the task that completed the list | the last task of the last list |
| `l` / `→`, `j` / `↓` | next item (next list at end) | next unfinished list | — |
| `Ctrl-E` / `Ctrl-Y` | scroll the card body down / up one line | — | — |
| `Ctrl-D` / `Ctrl-U` | scroll the card body down / up half a page | — | — |
| `PageDown` / `PageUp` | scroll the card body down / up one page | — | — |
| `gg` / `G` | first / last task in the document | — | — |
| wheel | scroll an overflowing card, else prev/next item (crosses lists) | — | — |
| `Enter`, `Space` | toggle done, then the next open task (on a note: advance) | `Enter`: next unfinished list | — |
| `s` | toggle started | — | — |
| `-` | toggle skipped | — | — |
| `/` | incremental search | — | — |
| `n` / `N` | next / prev search match | — | — |
| `}` / `{` | next / prev `### H3`+ sub-section, within the list | — | — |
| `T` | go-to-task picker | same | same |
| `y`, then `1`–`9`… | copy the card's command; with several, pick `[N]` | — | — |
| `o`, then `1`–`9`… | open the card's link; with several, pick `[N]` | — | — |
| `Ctrl-O` | back to before the last `#anchor` jump | — | — |
| `Tab` | next unfinished task anywhere, wrapping | same | same |
| `Shift-H` / `Shift-L` | prev / next unfinished list | same | same |
| `1`–`9` | jump to list N | same | same |
| `e` | edit the file in `$EDITOR` | same | same |
| `R` | reset (all, list or sub-list) | same | same |
| `u` / `Ctrl-R` | undo / redo the last state change | same | same |
| `?` | help | same | same |
| `q` | quit (all done: offer a reset first; waits for a running git sync, `q` again abandons it) | same | same |
| `Esc`, `Ctrl-C` | sticky `Type q to quit` hint | same | same |
| `Ctrl-Z` | suspend to the shell | same | same |
| `Ctrl-L` | repaint the whole screen | same | same |

A `—` cell is never silent: any key a completion screen doesn't bind shows a sticky hint naming that screen's keys (`List complete — press l for the next list or h to review`, `All tasks complete — press h to review, R to reset or q to quit`).

**Dispatch order.** `handle_key_with_mods` first takes `Ctrl-L` (`redraw_screen`, on every screen and overlay, search input included): it queues `Effect::Redraw` and returns before anything else, so a pending chord, an armed critical step and the status message stay as they were, and only an empty status bar gets an ephemeral `Screen redrawn`. `TerminalHost::redraw` calls `repaint`, the same `terminal.resize(current_size)` `enter_tui` uses after the editor and `Ctrl-Z`: ratatui then forgets its last frame and clears the screen, so the next draw sends every cell and overwrites whatever a background job, `wall` or the console printed behind it. Next it lets `Esc` disarm an armed critical step (`Critical step not checked`), then runs `dispatch_key`, then disarms a critical step the key didn't use (`disarm_unless_used`) and shows a pending in-place-save notice. `dispatch_key` tries, front to back:

1. `Ctrl-Z` (suspend, from anywhere, search input included).
2. `Ctrl-C` (`handle_ctrl_c`): raw mode delivers it as a key, not `SIGINT`. As in vim it cancels search, the picker, help, a confirmation and armed chords like `Esc`, and on a content screen shows the sticky `Type q to quit`. `Esc` never quits either, so neither can bypass the all-done reset offer.
3. **Unbound chords are rejected** (`is_bound_chord`): every handler below matches on `KeyCode` alone, so a key carrying `Ctrl`, `Alt` or another modifier besides `Shift` gets through only where the screen binds it — `Ctrl-E`/`Y`/`D`/`U`/`R`/`O` on `Checklist`, `Ctrl-R` on the completion screens, `Ctrl-E`/`Y`/`D`/`U` in help, `Ctrl-P`/`N`/`D`/`U`/`W` in the picker, `Ctrl-U`/`W` in search, nothing in the confirmations. Anything else cancels armed chords and shows `Ctrl-S is not bound (? lists every key)`. `Shift` passes, since capitals may arrive as `Char('L')` with `SHIFT` set. The table is a second copy of the handlers' `Ctrl` arms, so a test presses every `Ctrl-A`..`Ctrl-Z` on each screen through `dispatch_bound_key` (the dispatch past the gate) and requires the chords that act differently from both an unbound key and their plain letter to be exactly the ones the table lets through: a new arm missing from the table, or an entry left behind by a removed arm, fails it.
4. The digit prompts (`handle_pending_number_key`), before the global digit jump and `Esc`.
5. An open overlay takes every key: `handle_confirm_key`, `handle_help_key`, `handle_search_key`, `handle_picker_key`. So `q`, `j` or `?` typed into search are text.
6. `handle_checklist_scroll_key` (before `e`/`y`, so `Ctrl-E`/`Ctrl-Y` scroll), then `handle_global_key` (`Tab`, `Shift-H`/`L`, `1`–`9`, `e`, `R`, `u`/`Ctrl-R`, `T`, `Ctrl-O` on the checklist, `?`, `q`, `Esc`), then the view's own handler (`handle_checklist_key`, `handle_list_complete_key`, `handle_all_complete_key`).

`main.rs` passes the real `KeyModifiers`; `handle_key(code)` is the unmodified wrapper tests use.

**Chords.** The armed chord is one field, `pending_chord: PendingChord` (`None`, `G`, `OpenLink { typed }`, `CopyCode { typed }`), so two can never be armed at once and one assignment clears it. `gg` arms `PendingChord::G`, shown as `g… press g for the first task · any other key cancels`; any other key resets it. The digit prompts collect a number (below). Mouse input cancels every armed chord first (`clear_pending_chords`), so a click that changes the card can't leave a prompt armed against the new one.

**Navigating, not scrolling.** All four home-row motions navigate between items (the carousel slides left/right; the overview reads as a list going down), and card-body scrolling lives on vim's viewport keys. Card scroll resets on every navigation. `h`/`j`/`k`/`l` and the arrows at the very first or last task report a sticky `Already at the first task`/`Already at the last task`, with `· Tab finds unfinished ones` when another task is unfinished (`step_forward`/`step_backward` wrap `navigate_forward`/`navigate_backward`, which return whether the cursor moved; the wheel calls those directly and stays quiet). `Tab` reports a wrap (`Wrapped to the first unfinished task (<list>)`) and a sticky `No unfinished tasks`. `Shift-H`/`Shift-L` land on the target list's first unresolved task and never wrap (`No lists have unfinished tasks`, `Already at the last incomplete list`). `}`/`{` jump between sub-sections within the list, reporting the one landed on, or that there is no further one or none at all; they are in help, not on the legend.

**Help (`?`).** A modal card with one titled group per mode, from `HELP_SECTIONS` (`ui/cards.rs`): the checklist and global keys, the digit prompts, the completion screens, search, the picker, help itself and the reset dialogs. It takes the whole main area (the overview isn't drawn beside it, and its click rows are dropped), and a description too long for its column wraps onto rows indented under the description column (`HELP_KEY_COLUMN`, no narrower than `HELP_MIN_DESC_WIDTH`), so no row is cut mid-word and `help_max_scroll` counts the wrapped rows. It scrolls with the card keys, `j`/`k` and the arrows when it overflows; any other key closes it. `app/keys_tests.rs` presses, for each mode, every named key, printable character and bound chord on a fresh state, and requires every key that does something there to appear in that mode's rows, so a new binding without a help row fails the suite. The always-on legend stays trimmed to `hjkl:nav · space:toggle · /:search · y:copy · ?:help · q:quit`.

**Clicks.** `handle_left_click` tries, in order: the picker's rows (`picker_click`); on a content screen, the overview rows (`Item` → `focus_item` as navigation, `List` → `jump_to_list`, `Toggle` → `toggle_item` in place); on `Checklist`, the card's `layout.code_regions` (one per copy target, so a click copies exactly that command, even on a card `y` would ask about), then `layout.card_rect` (the whole card: behaves like `y`). Anything else is ignored. `body_layout` produces the copy targets in lockstep with the body lines, so a chip split over two rows copies its whole text from either fragment. Every row of one command holds the same shared (`Rc<str>`) copy of its text rather than its own, and the wrappers check whether a word fits by scanning at most one row's worth of it, so a long block or command costs time and memory in proportion to its size, not to its size times its row count (#245).

**Confirmations.** `ConfirmReset`: `y` all, `l` the list, `s` the sub-list; `s` outside a sub-list keeps the dialog open behind the error `No sub-list here: y resets everything`; any other key, `Enter` included, cancels. Its legend (`confirm_legend`) is built from the same conditions as the dialog's own key lines, so it offers `l` only with several lists and `s` only inside a sub-list. `ConfirmQuitReset`: `y` resets then quits (only when the reset reached disk; a refusal keeps markcheck open with the reason shown), `n` quits as is, anything else cancels.

---

## Incremental search (`/`)

`/` opens `Overlay::Search`, remembering the cursor in `search.origin`. Each edit re-runs `find_matches` and moves to the first match at or after the origin, wrapping (vim's forward `/`), and the status bar shows `/query  (N matches)` or an error-colored `/query  (no matches)`. A query that matches nothing (or is emptied) returns the cursor to the origin. `Ctrl-W` deletes the last word (`delete_last_word`), `Ctrl-U` the whole query, `Backspace` one character. Input during search leaves the status message alone (`clear_status_outside_search`), so a message that arrived mid-search is not discarded by the next key; it stays until the search closes, and `Enter` (and an unbound chord) run under `keeping_status`, which puts it in front of their own report, sticky and error-colored if either part is.

`Enter` (`commit_search`) reports `Match 7/12 · n/N to cycle`, stores the query in `search.last` and only then runs auto-copy, which is suppressed while typing. A committed query with no match skips auto-copy so the sticky `No matches` stays. An empty `Enter` repeats the last search, as in vim and `less`, or says `No previous search`. `Esc` restores the origin quietly.

`n`/`N` (`search_cycle`) step through `search.last` from the cursor, wrapping, with `Match i/N`, plus `· wrapped to top`/`· wrapped to bottom` when they wrap; with no previous search, `No previous search — press / to search`. Matching is **smart-case** substring containment (`smart_case_contains`: case-insensitive unless the query has a capital) over `Item::search_text`: the title, the body (with inline code) and the code blocks (fenced or indented), so a task is findable by its command.

---

## Go-to-task picker (`T`)

`Overlay::ListPicker` lists every card, task or note, with its state marker and a dim `— {list} › {sub-section}` suffix (the list only when there are several). `split_width` keeps at least half the row for the suffix, so same-named tasks stay distinguishable. Typed keys filter: `picker_matches` smart-case-matches `picker_match_text` (search text, the step number and the sub-section path, which stay out of `/`). The selection moves on the arrows, `Ctrl-N`/`Ctrl-P`, `Ctrl-D`/`Ctrl-U` (half a page of `layout.picker_viewport_height`) and the wheel, clamping, and resets on every filter edit; `Ctrl-W` deletes a word. `Enter` focuses the entry as navigation (auto-copy applies); `Esc` closes. A click highlights a row, a click on the highlighted row opens it (`layout.picker_rows_area` and `layout.picker_scroll_offset`, from the last frame). The title counts ` N items `, ` N matches ` or ` no matches ` (with a `No matching items` body).

---

## Editor (`e`)

`e` queues `Effect::OpenEditor { line }`. `open_in_editor` leaves the TUI (`leave_tui`: raw mode, mouse capture, bracketed paste and the alternate screen off, the same sequence `restore_terminal` and `suspend_to_shell` use; `take_terminal` is its inverse, shared by startup and `enter_tui`), runs the editor from `$VISUAL`, `$EDITOR` or `vi`, split with `shell-words` (`EDITOR="code --wait"` works; unbalanced quotes fall back to `vi` with a warning) with `SIGINT` held off markcheck (see Signals), and restores the terminal (`enter_tui`) with `terminal.resize(current_size)`, never `Terminal::clear()`, whose cursor-position query hangs on terminals that don't answer. `AppState::finish_editor` reloads first (`reload_if_changed`), queues a git-sync request (`request_external_edit_sync`) when the edit changed the file and the editor succeeded, and then reports a launch failure or non-zero exit, so the reload's status can't hide it.

`editor_file_args` opens the editor on the selected task's line, in the form that editor takes: `+N path` (vi family, nano, pico, emacs, emacsclient, joe, gedit, kak, micro), `path:N` (hx, helix, subl) or `--goto path:N` (code, codium), after any arguments from the variable. Anything else gets the bare path. A successful exit within `EDITOR_RETURNED_AT_ONCE` (1 s) that left the file unchanged is almost always a GUI editor handing off to a window, whose later save would arrive as an external edit and never be synced, so markcheck says so: `<editor> returned immediately — if it opened a window, make it wait: EDITOR="code --wait" (subl -w, gedit --wait)`.

---

## Suspend (`Ctrl-Z`)

Raw mode turns off `ISIG`, so `Ctrl-Z` arrives as a key. It cancels armed chords and queues `Effect::Suspend`; `suspend_to_shell` tears the terminal down as the editor does and sends itself `SIGTSTP` (`stop_self`: `kill(getpid(), SIGTSTP)`, delivered before the call returns), so it returns only after `fg` sends `SIGCONT`. The terminal is restored the same way and `finish_suspend` reports `Couldn't suspend: …`, the reload of an edit made meanwhile (never synced: it's an external change), or `Resumed`. A stopped process runs no git-sync worker, so with git sync on, `settle_git_sync_before_suspend` first `flush`es a request still in its quiet period and, while a sync runs, draws the sticky `Waiting for git sync before suspending…` (`wait_for_sync_before_suspend`) and waits up to `QUIT_WAIT` (`wait_for_outcomes`, each outcome applied as usual); `finish_waiting_to_suspend` then drops the notice unless the outcome replaced it (#206). Keys aren't read during that wait. A sync still running after it is left to `fg`: only its current git subprocess, in its own session, carries on, and `suspend_to_shell` prints `markcheck: git sync paused while suspended; fg to finish it` on stderr once the alternate screen is gone. Under a shell without job control the stop is discarded and markcheck repaints with `Resumed`.

---

## Reset (`R`; quit-reset)

`request_reset` opens `ConfirmReset`, or says `Nothing to reset: no tasks are done` when no task is done, started or skipped. The dialog says what changes (`This marks 1 done, 2 started and 1 skipped tasks (of 6) as not started`, the total counting skipped tasks too) and offers `y` (everything), `l` (the current list, only with several) and `s` (the sub-list the cursor is in, `sub_list_root`: the task itself when it has sub-tasks, else its parent; only inside one), each line with its count (`reset_count`). All go through `reset_scope(ResetScope)`: one `commit_write`, one undo point, and a git-sync description naming the scope (`Reset "Rollback" to not done`). A scope with nothing to reset keeps the dialog open behind `Nothing to reset there`. A failed write leaves the dialog open with the error and the document rolled back. The cursor stays.

`q` with every task resolved (done or skipped) and at least one of them opens `ConfirmQuitReset` instead of quitting; its `Reset all N tasks` counts skipped ones, and its heading says `done or skipped` when any are, so a finished checklist can be handed back ready for the next run.

---

## Undo / redo (`u` / `Ctrl-R`)

Every state change (`set_state`, the overview's `toggle_item`, `reset_scope`) captures a full `StateSnapshot` before it mutates; the same snapshot is what `commit_write` rolls back to on failure, and only a successful write pushes it as an undo point (`push_undo_point`: capped at `UNDO_HISTORY_CAP` = 100, clearing redo). A blocked or failed action never reaches the stacks.

`undo` pops a snapshot, pushes the current state for redo, and applies it (`apply_snapshot`, matched by line; it returns the lines that changed); `redo` mirrors it. `finish_history_apply` writes it back, leaves a completion screen, and reports: one change → focus that task and name its new state with `marked()`, `Undo: marked done` (or `started`, `not done`, `skipped`); several → `Undo: restored N tasks`. Its git-sync description (`history_change_desc`) names the change like `set_state`'s: one task → `Undo: Uncheck "Restart service"` (`commit_verb` of the restored state plus the label), several → `Undo: 3 tasks ("A", "B", "C")`, naming the first `HISTORY_NAMED_TASKS` (3) and eliding the rest with `…` (#175); nothing on a stack → sticky `Nothing to undo`/`Nothing to redo`. A failed write restores the document and puts the snapshot back on its stack, unless the failure reloaded the file: an external reload clears both stacks and bumps `history_epoch`, and the snapshot is re-pushed only when the epoch is unchanged, so a pre-reload snapshot is never replayed onto a different file. Our own writes never reload, so history survives them.

---

## Clipboard (`src/clipboard.rs`)

`y` copies code only. The candidates (`clipboard::candidates`) are the item's `code_blocks` (fenced or indented), else its inline `code_spans`. One → a `CopyKind::Command` copy; none → `Nothing to copy: item has no code`; several → `PendingChord::CopyCode` is armed and the status bar shows `press 1–N to copy · esc cancels`, matching the card's `[N]` markers. A number (collected as for links, below) copies candidate `[N]` through `copy_specific`, the same path as a click, or says `No command [n] on this card`.

**Routes (#133).** `--clipboard auto|system|osc52` (config `clipboard`, `ClipboardChoice`) resolves at startup into `AppState::copy_route`:

- `SystemThenOsc52` (`auto` locally): `arboard`, falling back to OSC 52 when it fails.
- `Both` (`auto` over SSH, from `cli::is_remote_session`: `SSH_CONNECTION` or `SSH_TTY` set): the system clipboard and always OSC 52 as well, `Copied (system clipboard + OSC 52)`. Over SSH arboard often "succeeds" against a display that isn't the user's (a tmux session reattached from another machine keeps the old `DISPLAY`).
- `SystemOnly` and `Osc52Only`: one backend, no fallback.

OSC 52 (`ESC ] 52 ; c ; <base64> BEL`, to stdout) reaches the local terminal through SSH, but is fire-and-forget, so it reports `Sent to clipboard (OSC 52)` rather than `Copied to clipboard`. Payloads are capped at 70 KB, under tmux's ~74 KB truncation point; tmux needs `set-clipboard on`, and VTE terminals don't support it at all. Both failing is reported. A payload over the cap under `Both` reaches the system clipboard alone: `CopiedSystemOnlyTooLarge`, reported sticky as `Copied to the system clipboard only — too large to send through the terminal (OSC 52)`, since that clipboard may be another machine's.

**One arboard instance per session.** `set_via_arboard` keeps one `arboard::Clipboard` in a `static Mutex<Option<Clipboard>>`. On X11 the owning instance serves the selection to whoever pastes and the last instance's `Drop` destroys it, so a per-copy instance made `Copied` a lie without a clipboard manager. A failed write drops the instance so the next copy reconnects; `release_system_clipboard()` on quit lets arboard hand over once.

**Hidden characters.** `copy_text`, under every copy, refuses text with a control character other than `\n`/`\t` or a bidi control (U+061C, U+200E/F, U+202A–U+202E, U+2066–U+2069; `printable::is_hidden`), an invisible format character (`is_invisible_format`: the zero-width `Cf` characters such as U+00AD, U+180E, U+200B–U+200F, U+2060–U+2064, U+FEFF and the tags U+E0001/U+E0020–U+E007F, but not the visible prepended concatenation marks), or a variation selector or U+034F with no base before it (`is_modifier`; at the start, after whitespace, a control or another such character) — ratatui can't draw them, so the card would show one command while the clipboard got another (an embedded `ESC [201~` ends a bracketed paste early). `CopyOutcome::HiddenCharacters` is reported sticky: `Not copied: command contains hidden control characters`.

**`--primary`** also sets the X11 PRIMARY selection (best effort) and emits a second OSC 52 sequence for `p`. **`--auto-copy`** runs `maybe_auto_copy` after each navigation: one candidate is copied and the success appended to the motion's message; no or several candidates stay silent; a hidden-characters refusal is always reported.

**Testing.** `copy_text_with` takes the system write and the OSC 52 stream as parameters; test builds route them to `clipboard::fake`, a per-thread recorder whose system clipboard is off unless a test turns it on, so no test touches the developer's clipboard or the harness's stdout. `set_via_arboard` isn't compiled into tests; the real path is checked by hand. `tests/pty.rs` removes `DISPLAY`/`WAYLAND_DISPLAY`, so its copies use OSC 52.

---

## Open link (`o`)

`open_current_link` reads `Item::link_urls()`:

- one link → `open_url`: a `#anchor` goes to `follow_anchor` (below), a safe scheme to `Effect::OpenLink`;
- one with another scheme → `Not opened: unsupported link type (<url>)`;
- none → `Nothing to open: card has no link`;
- several → `PendingChord::OpenLink`, no status message so the numbered list below the card stays readable, and `press 1–N to open · esc cancels` in the prompt slot.

**Number prompts** (`o` and `y`). `pick_number_key` collects digits into the chord's `typed`: a digit acts at once unless a longer valid number could still follow (`n * 10 <= count`), so up to nine choices act on one key and with twelve `1` waits for `0`–`2` or `Enter`. `Backspace` deletes a digit, `Esc` cancels, and any other key cancels and takes its normal action. A partial number shows as `open [1…] of 1–12 · next digit, or enter for [1] · esc cancels`. Out of range: `No link [n] on this card`.

**Safe schemes** are `http://`, `https://` and `mailto:` (`SAFE_LINK_SCHEMES`, case-insensitive). Everything else is refused, `file://` included (an opener may run a `.desktop` file or an executable it's handed), and since no allowed URL can start with `-`, a link can't inject an option into the opener's arguments (`xdg-open` has no `--` convention).

**Anchors and start targets (#114, #139, #186).** Every heading's GitHub-style anchor is in `Document::anchors`, quoted and in-item ones included so the `-1`, `-2` numbering matches GitHub's, each with the line its target is searched from: the line after the heading, or the card's own line for a heading inside an item (`parser::slug`: lowercased; everything but letters, digits, spaces, `-` and `_` dropped; spaces to `-`; repeats get `-1`, `-2`, …, skipping a suffix a literal heading such as `## Verify 1` already took; a set of taken anchors and the next suffix per slug keep this linear in the number of headings, #148). `anchor_target` percent-decodes the fragment (UTF-8, kept as written if it doesn't decode) and resolves it case-insensitively to the first task from that line, for both `follow_anchor` and `open_at`. `follow_anchor` pushes the cursor onto `jump_back` and moves (`Jumped to #rollback · Ctrl-O goes back`, or `No section named #x in this file`); `Ctrl-O` pops it; a reload clears it. On the command line, `cli::take_start_line` takes a vim-style `+N` before clap sees the arguments, and `cli::split_anchor` reads `file.md#anchor` only when the path as given doesn't exist but the part before its last `#` does; `open_at` moves there and adds `Opened at #rollback` as the startup message's first part, ahead of any warnings, or why not as a warning. `file.md:120` and a `--list` flag were left out, as ambiguous with real file names.

**Opening.** `open_link` resolves openers (`launch::resolve_opener`): `$BROWSER` as a colon-separated list, each entry shell-word-split, `%s` replaced by the URL (`%%` a literal `%`) or the URL appended; a bad entry is skipped with a warning; then the platform default (`xdg-open`, or `open` on macOS). Each is spawned in turn until one starts, with the URL as a plain argument (no shell), stdio to `/dev/null`, the TUI kept up, and the child reaped on a thread. The status says `Sent to <program>: <url>`, an attempt; when the opener exits non-zero or is killed within `OPENER_FAILURE_WINDOW` (10 s), the reaper sends `Couldn't open link (<program> exited with code N)` over a channel the main loop drains. **Headless sessions copy instead:** with no usable `$BROWSER` and, outside macOS, no `DISPLAY` or `WAYLAND_DISPLAY`, `copy_link_instead` queues a `CopyKind::LinkFallback` copy: `No browser here — link copied to clipboard`.

**Link panel.** URLs aren't drawn on the card; `render_link_panel_below` lists them under it (`→ <url>`, or `[1] <url>` …), painted into space left over after the card is placed, so the card doesn't move between cards with and without links. A URL wider than the area is clipped. Without room for the panel (`link_panel_fits`), the card's bottom border shows ` → link ` (the scroll indicator wins the slot).

---

## Terminal Setup (`src/main.rs`)

**Before the terminal.** Every way of failing to open the checklist names the path: `cannot resolve path: <file>`, `cannot read <file>` (with an `iconv` hint for a file that isn't UTF-8), and a file without tasks prints `No checklist items found in <file>.` with a hint about `- [ ] step` and exits 0. Then, before anything with side effects, a stdout that isn't a terminal is refused (`cannot open <file>: markcheck is interactive and needs a terminal`). `--new` checks this before creating the file (`cannot create <file>: markcheck is interactive and needs a terminal`), after the name check and unless the path already exists, so a refused run leaves nothing behind for the retry from a terminal to trip over. `--help` names the config file.

**Restoring it.** `TerminalGuard { mouse, title }` restores on drop, panics included: raw mode off, mouse capture released, bracketed paste off, the alternate screen left, the title popped. `install_panic_hook` restores the terminal on a main-thread panic *before* the previous hook prints, so the message lands on the normal screen, and adds a pointer to the issue tracker; other threads only run the previous hook (git-sync workers turn their panics into a `Failed` outcome while the TUI runs on). Debug builds read `MARKCHECK_TEST_PANIC` to panic after a draw, for `a_main_thread_panic_prints_its_message_on_the_restored_screen`.

**Signals (#5).** `StopSignals` (`signal-hook`) turns `SIGTERM`, `SIGHUP` and `SIGINT` into the same exit as `q`: the terminal is restored and a running sync is waited for, with `Waiting for git sync… press q again to abandon`. A second `SIGTERM` or `SIGINT` exits at once. While the editor runs, `StopSignals::ignoring_interrupts` swaps the `SIGINT` actions for one that drops it (#146): with raw mode off, the terminal sends its interrupt key (`Ctrl-C`, or `C-g` under Emacs) to the whole foreground process group, markcheck included, and it is meant for the editor. The signal stays caught rather than ignored so the editor starts with the default action; `SIGTERM` and `SIGHUP` keep working throughout. Abandoning a sync stops every git child's process group through the `Children` registry (`git_sync/process.rs`), which also refuses new children from then on.

**The loop.** It polls every 100 ms and **redraws only when something may have changed**: an input event other than key releases and pointer motion (`ui::event_needs_redraw`), a reload, a finished sync, an expired status (`expire_status`), a round trip to the editor, shell or a link, or the next change of a relative label (`ui::next_label_change`). Keys go to `handle_key_with_mods`, the wheel to `handle_scroll_up/down`, a left press to `handle_left_click`. Mouse capture is on unless `--no-mouse`. Bracketed paste is always on, so a paste arrives as one `Event::Paste` instead of keystrokes (a stray paste would otherwise toggle, reset and quit); `handle_paste` appends it as one line to the search or picker query and elsewhere says `Paste ignored`.

**Terminal title and exit line (#135).** With `terminal_title` (default on; `--terminal-title`/`--no-terminal-title` override it), startup pushes the current title (`CSI 22;2t`) and the guard pops it (`CSI 23;2t`); between them `launch::terminal_title` (`Deployment Runbook · 3/11 — markcheck`) is set after a draw when it changed (`TerminalTitle` in `main.rs`). `e` and `Ctrl-Z` pop the saved title before handing the terminal over and push the title the other program left on return, then forget the cached title so the next draw sets markcheck's again (#162). With `exit_summary` (default on; `--exit-summary`/`--no-exit-summary` override it), a successful run prints `launch::exit_summary` after the terminal is restored: done/total with started and skipped, and the card the cursor stopped on, or `all N done`.

**Text written outside ratatui (#145).** ratatui drops control characters from what it draws, but the terminal title, the exit line, the git-sync exit report and the startup error lines go straight to the terminal, and the checklist's H1, labels and file name (and git's own output) can carry raw `ESC`/`BEL` that pulldown-cmark passes through. Each of these sinks runs its text through `printable`: `printable_line` (title, exit line, file path in messages, commit subject and body lines) drops C0/C1 controls, DEL and bidi controls and turns line breaks and tabs into spaces; `printable` (`git_sync::exit_report`) keeps line breaks and tabs. Without it a heading could end the title's OSC sequence early and inject one of its own, such as an OSC 52 clipboard write.

**CLI** (`Cli` in `cli.rs`): `file` (or `--new PATH`), `+N`/`file.md#anchor` start targets, and paired flags for each boolean setting (`--nerd-font`/`--no-nerd-font`, `--mouse`/`--no-mouse`, `--primary`/`--no-primary`, `--auto-copy`/`--no-auto-copy`, `--terminal-title`/`--no-terminal-title`, `--exit-summary`/`--no-exit-summary`, `--git-sync`/`--no-git-sync`, each pair conflicting), `--color`, `--clipboard`, `--git-sync-delay`, `--config PATH`/`--no-config`, `--version` and the hidden `--generate-man`/`--generate-completions`.

**`--new PATH`** (`scaffold.rs`) requires a `.md` name with a non-empty stem, writes a template titled from the name (`meeting-notes.md` → `# Meeting Notes`, else `# Untitled`) with two blank tasks to a temp file, fsyncs it, and `hard_link`s it into place, which fails on an existing file rather than replacing it. On a filesystem without hard links any other link error falls back to `create_new` on the final path (`writer::write_new_file`), which keeps the no-overwrite guarantee. The temp name is removed and the directory fsynced. The new file then opens like any other, with `Created new checklist`.

**`--version`** prints `<CARGO_PKG_VERSION> (<git short sha>)`, e.g. `1.0.0 (da90ddd)`. `build.rs` embeds the SHA (`unknown` outside a checkout) and reruns on changes to `HEAD`, `packed-refs` and the directory holding the branch's loose ref (the next commit creates that file, which a watch on the file would miss); only existing paths are emitted.

**Platforms.** Linux and macOS. The code assumes Unix throughout (modes, `fchown`, sessions, `killpg`, `/proc` or `ps`); `main.rs` opens with `#[cfg(not(unix))] compile_error!` (#78).

---

## Config File (`src/config.rs`)

`$XDG_CONFIG_HOME/markcheck/config.toml`, else `$HOME/.config/markcheck/config.toml`; `--config PATH` reads another file (which must exist), `--no-config` reads none.

```rust
pub struct Config {
    pub nerd_font: Option<bool>,
    pub mouse: Option<bool>,
    pub primary: Option<bool>,
    pub auto_copy: Option<bool>,
    pub color: Option<ColorChoice>,         // "auto" | "truecolor" | "256" | "16"
    pub clipboard: Option<ClipboardChoice>, // "auto" | "system" | "osc52"
    pub git_sync_paths: Option<Vec<PathBuf>>,
    pub git_sync_delay: Option<u64>,        // seconds
    pub terminal_title: Option<bool>,
    pub exit_summary: Option<bool>,
}
```

**Precedence: a passed flag > the config value > the built-in default.** `Settings::resolve` owns the whole mapping, for every setting with both a flag and a config key (the six booleans, `color`, `clipboard` and `git_sync_delay`); `main.rs` reads only `Settings`, never `Config` or `Cli`, for these. Each boolean goes through `resolve_flag(cli_on, cli_off, config, default)` and every boolean has a flag in both directions, so any config value can be overridden for one run (#163). Its tests drive it through real argv (`Cli::parse_from`), since a test of `resolve_flag` alone passes with a hard-coded value at the call site.

**`git_sync_paths`** lists path prefixes that turn git sync on for files under them. `launch::git_sync_request` ORs a match with `--git-sync`; `--no-git-sync` beats both. A leading `~` is expanded (`expand_tilde`), each prefix is canonicalized before a component-wise `Path::starts_with`, and an entry that doesn't exist is reported (`git_sync_paths: <entry> does not exist — ignored`) rather than refused, since a shared config may name directories that exist only on some machines. **An entry covers its own repository, not ones nested in it (#124):** a repository unpacked into a configured tree brings its own `.git/config` and hooks, which git trusts by ownership alone. `launch::git_sync_scope` checks the repository root (from `GitSync::detect`, which has run only `git version` and `rev-parse`) against the covering entries before anything else runs git: a plain entry accepts the prefix itself or an ancestor as the root, an entry ending in `/**` or `--git-sync` accepts any. A refusal turns sync off with `Git sync off: the checklist's repository (...) is nested inside git_sync_paths entry ...; end the entry with /** ...`.

**Errors.** A missing default file is no error. A malformed one (bad TOML, a known key with the wrong type) aborts startup with the file named and `--no-config` suggested. An **unknown key** is a startup warning (#112), so a config shared with a newer release doesn't lock an older one out: `load_config` parses once as a table for the key names and once into `Config`, and reports keys not in `KNOWN_KEYS`, which two tests pin to `Config`'s fields.

`config_path(xdg_config_home, home)` takes the environment as arguments, so it's testable without touching the process environment; every process the integration tests spawn gets a fresh, empty `HOME` and `XDG_CONFIG_HOME` (`common::isolated_env`).

---

## Testing

- **Unit tests** live in `<module>_tests.rs` beside each module (`#[cfg(test)] #[path = …] mod tests;`), so coverage leaves them out. UI tests render to ratatui's `TestBackend` and assert on content, not style, unless the style is the state.
- **`tests/cli.rs`** runs the binary for the paths that exit before the terminal (bad path, unreadable file, no tasks, stdout not a terminal, `--help`).
- **`tests/pty.rs`** drives the binary under a pseudo-terminal (`portable-pty`) for `main.rs`'s wiring: the loop, editor and suspend round trips, signals, git sync end to end, the nested-repository refusal (a planted `uploadpack` and hook must not run). A `Session` waits for the alternate screen before typing; `Step::Key` paces keys, `Step::WaitForFile` and `Step::WaitForScreen` poll for a condition (the screen is replayed onto a grid, since ratatui redraws only changed cells). **No test synchronizes on a fixed sleep, and every wait has a deadline**, the child's exit included: every wait goes through one `poll_within` helper, and every condition (screen, file, hook marker, the remote's log) shares one generous `WAIT_TIMEOUT`, since a wait that succeeds returns at once. Each `Session` holds a file-wide lock, serializing the PTY tests (many at once starved git sync's thread on CI) while unit tests stay parallel.
- **git sync's tests** build real repositories (a bare remote and a clone) and run real `git`, hermetically: fixture commands and, under `cfg(test)`, markcheck's own go through `isolate_git`, which strips the redirecting variables and config parameters, points global and system config at nothing and `HOME` at an empty directory. The variable list comes from `src/git_test_support.rs`, which `include!`s the same `git_env.rs` as `git_command` and is shared with `tests/common` by `#[path]`, so a variable added there is stripped from every test's processes too. Most call `run_sync` directly; the few that exercise the thread write every file change before the first request and wait with `settle`, which uses `GitSync::wait_for_outcomes` (the same wait the quit path uses) and fails with what it saw if the worker is still busy. `git_sync/test_support.rs` builds every repository with one `init_repo(RepoFixture { .. })` and has the race harness that pauses a sync at a named point.
- **Property tests (#79).** `test_support::markdown::document()` generates Markdown from the constructs markcheck treats specially and the ones it must leave alone (headings, bullets and numbered items with every marker, space and tab indentation, quotes, fences, indented code, HTML, setext underlines; LF, CRLF or CR; with or without a BOM). `parsing_and_writing_back_unchanged_keeps_every_byte` and `one_toggle_changes_one_marker_and_nothing_else` run 256 cases each (`PROPTEST_CASES` raises it). Failure persistence is off: a shrunk counterexample becomes a named test.
- **Golden screens (#105).** `ui/golden_tests.rs` renders every `examples/` file at 50×24, 80×24 and 120×40, on the start card and after `j`, plus the picker and help, and compares the symbols with `insta` snapshots in `src/ui/snapshots/`. Unicode icons and a basic palette keep them machine-independent. Review a change with `cargo insta review`; on CI nothing is written, so an unreviewed change fails.
- **Mutation tests (#129):** see Verifying the guards.
- **Temp files** come from `unique_temp_path` in `src/temp_test_support.rs`, removed when the test's thread ends, pass or fail. It's one file compiled into both the unit tests (re-exported by `test_support`) and the integration tests (`tests/common` includes it with `#[path]`, since `tests/` can't see the crate's `cfg(test)` code), so it uses nothing but `std` (#143).
- **Prerequisites.** `git` and `sh` on `PATH`, and `cargo build` before the integration tests. A few tests skip with `skipped:` on stderr where the machine can't support them: running as root (permissions don't bite), no `ps` (slim images) and no `/proc`, no root-owned file.
- **Coverage** (`cargo llvm-cov`, see CLAUDE.md) measures production code only, gated at 97% in `check.yml`; it leaves out the test files and `main.rs`. That exclusion holds only while `main.rs` stays wiring: anything with branches (flag precedence, command resolution, message mapping, the post-editor reload) lives in a covered module.

---

## Verification Checklist

Manual checks on a real terminal, with an `examples/` file:

- `h`/`l`/`j`/`k` and the arrows navigate and cross list edges onto the first unresolved task; the card-scroll keys scroll an overflowing card and clamp
- `Space`/`Enter` toggles and the file changes at once (`grep -n '\[x\]' <file>`); identical task texts toggle only their own line; `s` and `-` write `[/]` and `[-]`
- A critical step takes a second press after a pause (holding `Space` or `-` doesn't get past it); any other key disarms it
- Completing a list shows the list-complete card; completing everything the all-complete summary with correct counts
- `y` copies a single command (check the paste, locally and over SSH); several ask for a number; none says so
- `o` opens a web link, follows a `#anchor` and `Ctrl-O` returns; over SSH without a browser the link is copied
- `e` opens the editor on the task's line and reloads on return; `Ctrl-Z` and `fg` repaint and pick up an edit
- Output written behind markcheck's back (`sleep 2; echo junk &` started before it, or a `wall` message) stays until `Ctrl-L`, which repaints the whole screen
- `Esc` and `Ctrl-C` show `Type q to quit`; `q` restores the terminal (cursor, no raw mode, mouse released, title restored) and prints the exit line
- `R` resets the chosen scope; `u` and `Ctrl-R` undo and redo
- An external edit reloads within two seconds with a change summary and the cursor kept; our own toggles never show `Reloaded`; emptying the file keeps the last document with `Reload skipped`
- A second markcheck on the same file is announced at startup
- Resizing reflows down to the single-card layout without a panic
- With git sync: a toggle shows `pending`, then `syncing…`, then `Synced`; the commit carries `Synced-by: markcheck`

---

## Design history

One line per decision, oldest first within each area, with the commit that made it (and its issue). `git show <sha>` has the reasoning; this list is the index.

### Parsing

- `[/]` markers come from pulldown-cmark's own task positions, not a line scan — `d816a6b` (#3); the pre-pass repeats until it finds nothing new, and looks past setext headings — `99b877a` (#79)
- A closing fence must be a bare fence line, as CommonMark says — `d31cb9e`
- One line model for `\r\n`, `\n` and `\r`, shared by parsing and write-back — `07233fd`
- A leading YAML or TOML front-matter block is skipped and kept byte for byte — `083afb6` (#43)
- A list whose only checkboxes are quoted is dropped — `afd6913` (#27)
- A heading inside an item or a quote isn't structural — `20b1968` (#92)
- A list's banner comes only from a list with tasks — `df0ca8c` (#103); a bold-only bullet carrying a command stays a card — `4b4e95e` (#115)
- Paragraphs and HTML-split words inside an item are separated by a space — `4521b53` (#72)
- Bare URLs in task text are links — `9eef786` (#81)
- Ordered-list step numbers show on cards and rows — `d03d9c0` (#82)
- A file without an H1 is titled from its name — `8e05f7f` (#86)
- An item's indented code block is shown and copyable — `57a52e2` (#130)
- Code in a card title is no copy candidate — `001f897` (#137)
- `parse_source` is a `ParseState` event walk — `f50e9f0` (#40)
- Prose under a heading shows above its first task; what can't be shown is reported — `84bc913` (#80)
- A skipped state, `[-]` — `198972c` (#54); critical steps, `**! Title**`, take a second press — `a98e483` (#95)
- Headings get GitHub-style anchors for `#links`, `+N` and `file.md#anchor` — `9670c45` (#114, #139)
- A tab-indented nested task gets its own line — `003b4c2` (#79)
- Property tests pin the round trip and the one-marker toggle — `61f8789` (#79); every example is pinned too — `b04f98a`

### Writing the file

- Write-back rolls memory back when it fails — `5509d83`; a missing final newline stays missing — `5b719a3`
- A write is refused when the file changed since it was loaded — `7d6602a`, made a compare-and-swap under a lock — `a814c32`, with SHA-256 as the fingerprint — `408d43f`
- Writes are refused after a reload markcheck declined — `1ef5efa`, and a failed reload doesn't advance the fingerprint — `eede861`
- A temp-file collision retries at a new name rather than deleting the file — `ed8160f`; names carry process-random entropy — `18642c0`
- The write lock is never deleted while another instance holds it — `2d2987a`; it is recovered only from a provably dead owner — `7775667`, `adbd3a5` (#6); created `0600` — `83e3008`; kept in a directory private to the user — `597497a` (#1)
- The checklist path is resolved before locking — `6b22320`
- A hard-linked checklist is warned about, not refused — `12701c3`
- A write-protected checklist is refused — `f3280bb` (#47)
- An untouched `[X]` keeps its casing — `fd7f3df` (#23)
- The group is kept on save and another owner flagged — `8988d46` (#106); setuid, setgid and sticky bits are cleared — `cd70b83` (#122)
- A symlink or special file swapped in for the checklist is refused — `73df2cd` (#123)
- A read-only folder falls back to an in-place save, and failures say why — `b491c12`, `12ae69a` (#46)
- One atomic temp-file writer serves save and `--new` — `bd712c8` (#19); `--new` falls back to a direct write without hard links — `aa4d64e` (#14)

### Reloading

- The watcher watches the parent directory; a 2 s recheck backs it up — `a5ee606` (#50)
- Search, the picker and help stay open across a reload — `758fb0d` (#24); armed chords are disarmed — `7ae1c62`
- The cursor stays on its task across a reload, and a reload never auto-copies — `def3585` (#60); same-named lists are told apart by rank — `d2c5fa7`
- A refused undo or redo that reloaded drops its stale entry — `aa5eb8d` (#91)
- A reload says which tasks it changed — `219fdaf` (#140)
- A second session on the same file is announced — `adfdc1c` (#101)

### Keys and feedback

- Unbound `Ctrl`/`Alt` chords are refused, not run as the plain key — `76bada0` (#25)
- `Ctrl-C` answers with how to quit — `f87d913` (#13); `Esc` no longer quits — `8370afa`, `611c5e6` (#51)
- Keys that do nothing say so — `6f6691c`, `bdcc1d6` (#52); a lone `g` shows its prompt — `afcc0e3` (#104)
- The reset dialog counts started tasks — `3992d74` (#26); the quit-reset stays open when it can't be saved — `87b4f10` (#7); `R` can reset one list or sub-list — `a9f8c12` (#118)
- `/` searches forward from the cursor — `ed5b296` (#61); a failed search puts the cursor and clipboard back — `3b49091` (#117); an empty `/` repeats the last search — `0b71837` (#131)
- `Tab`, `n` and `N` say when they wrap — `fd55457` (#87)
- `Space` advances to the next open task, never a done one — `79be1ff` (#84)
- Every mode's keys are in help, enforced by a test — `7736e9f` (#74)
- The wheel is routed per screen and picker rows are clickable — `b267844` (#65)
- Pasted text is ignored, not run as keys — `1c5454e` (#57)
- `Ctrl-Z` suspends to the shell — `375f59b` (#107)
- Toggles from keys and clicks share `set_state` — `9526138` (#19)

### Clipboard and links

- OSC 52 is reported as sent, not copied — `09286ac`; an oversized payload is named — `60d4d33`
- Commands with hidden control characters aren't copied — `970ede0` (#4)
- One arboard instance lives for the session, so X11 copies survive — `dff8215` (#58)
- A click copies the clicked command in full — `766b23e` (#59); `y` plus a digit picks one of several — `0c6bdc5` (#116), and `[10]` and up work for `o` and `y` — `9ac587e` (#100)
- Auto-copy runs on every navigation — `6f32862` (#30)
- `$BROWSER` follows the colon-list and `%s` convention — `a30e2c8` (#15)
- Headless sessions copy a link instead of opening it; failed openers are reported — `4a61362` (#63)
- Over SSH copies go to both clipboards; `--clipboard` chooses — `74ba3e8` (#133)

### Screen

- Text is measured in display cells — `290b5c8` (#12), `91b5629`
- A long status message wraps — `0cc4249` (#11)
- A completion claim is never made with work left — `46ddb3d`
- `NO_COLOR` is honoured and code has an explicit foreground — `10d8ed2` (#45); truecolor is detected from `TERM` too, with `--color` — `2600b88` (#134)
- The overview stays linear and the loop redraws only on change — `4632909` (#39)
- `Screen` split into a base `View` and an `Overlay` — `71be6ba` (#66)
- Side cards show the task title — `2953fde` (#75)
- Dialogs size to their content — `6da2f23` (#93), and never panic on a short terminal — `0e79194`
- Long banners and titles wrap; other rows are cut with `…` — `5e9e637` (#102)
- Tabs in commands render as indentation — `0d0cac7` (#85); wrapped command rows carry `↩` — `3fd86d6` (#136)
- One state-marker mapping draws every marker — `e50bb57` (#19); the progress ramp derives from the palette — `d287e74` (#19)
- Unicode icons are the default — `bbf566a` (#83)
- The terminal title and an exit line show progress — `d68b741` (#135)

### Git sync

- Commits are built from a captured snapshot through a temporary index, never the real one — `03203d9`, `b942ea9`, and the real index is kept in step — `e20d4cc`
- Every git subprocess is bounded and killed on timeout — `1f16c98`, with explicit stdio — `2c2c13c` and scratch files instead of pipes — `b884fed`
- The commit's scope is enforced after hooks run — `b16510c`; the commit made is verified and rolled back exactly — `31eb035`; a timed-out commit is reconciled against HEAD — `2081504`, `3c19d5b`
- Stale content is never committed — `c0707fc`; nothing is committed at startup — `8f63a2e`
- Unrelated unpushed commits block a sync — `9d89a8d`, `a1f2860`; commits are checked one by one — `cbce565`; unverifiable history refuses — `4f41c9d`
- A staged checklist is neither overwritten nor dropped — `39f6bd7`, `43205de`
- A toggle never publishes changes markcheck didn't make (#177)
- A commit markcheck didn't make is never rewound — `e1f7b99`
- The verified commit is pushed explicitly — `bb7b07e`, `b4f1c4d`; never an unresolved one — `b04d9e5`; never without an upstream — `7c199e5`; only where `git push` would push — `464f295` (#98); never into a local branch it tracks — `c5bf59f` (#71)
- The remote check and the push are one atomic operation — `17a13be`
- Retries are tracked by commit SHA — `901f062`, `dd3231c`, `1edb49e`, and an abandoned retry is cleared — `8ac542f`
- Trackedness comes from the index — `82a2d6a`; blobs are hashed as git hashes them — `fa99398`, and compared as SHAs — `f85b215` (#127)
- The ambient git environment is neutralised — `a3f1fe4`; the variables to strip live in one list — `b6af810` (#19)
- The pathspec is matched literally — `acea6aa` (#70); a non-UTF-8 file name refuses — `60cc8e7` (#8)
- An unknown repository state fails closed — `cdeb31d` (#9)
- Startup says why sync stays off — `a7bbc00` (#38) and reports an untracked checklist — `be0713d` (#28)
- Hooks get 5 minutes and the real error is reported — `e80a274` (#53)
- A failed sync stays in the title bar until a push lands — `3d3a136` (#64); the last one's failure is reported after quitting — `313d309` (#108)
- A stale `.git/index.lock` is never left behind — `c7283e8` (#119)
- An `e` edit is synced — `aea85ec`, but not after an editor failure — `2c7b0af` (#94); an external edit drops a queued request — `0ab2032`, also one waiting out the quiet period (#178)
- `--no-git-sync` overrides `git_sync_paths` — `ac665d4` (#111)
- Git runs with no controlling terminal — `3757ff9` (#2); quit and signals stop and clean up git — `b6deb70` (#5); every command runs from the repository root — `8f2432f` (#141)
- A `Synced-by: markcheck` trailer marks markcheck's commits — `1de27ce` (#62, #99)
- A `git_sync_paths` entry covers only its own repository unless it ends in `/**` — `3c227da` (#124)
- Changes are batched after a quiet period — `7b197bd` (#56)
- The module is split by concern into one dependency direction — `fd3877e` … `f366f23`
- A randomised operation-sequence test drives the invariants — `1db63de`

### Startup, config and packaging

- Every boolean setting has a flag in both directions — `9c0989f`
- The version is stamped from the checkout and restamped when it moves — `9525e7f`, `b26ac1b`
- Startup errors name the file and the fix — `8089d26` (#48); a main-thread panic shows on the restored screen — `7baac68` (#49)
- Logic moved out of `main.rs` into covered modules — `d427a6e` (#33)
- Editors open on the task's line — `cf535b6` (#110), with a hint when one returns at once — `470948b` (#109)
- Unknown config keys warn instead of failing; `--config`/`--no-config` — `372b004` (#112)
- AppState's outward actions go through one effect queue — `408afcb` (#138)
- Unix only, with a clear build error — `87c8e59` (#78)
- A static musl binary and a declared minimum Rust — `b8f26e2` (#44); license notices — `3916dff` (#132); a man page and completions — `ed4f632` (#90)
- The release workflow is hardened and reuses `check.yml` — `57272c5` (#16)

### Tests

- Test git and config are isolated from the machine — `e918f78` (#31)
- PTY waits are bounded and fixed sleeps dropped — `d381c88` (#32)
- Tests don't leak temp files or touch the clipboard — `f1b8788` (#29)
- Test code is outside the coverage gate — `f8d7f1e` (#125)
- Machine-dependent tests skip as root or without `ps` — `e8d0e1a` (#126)
- Write-back is asserted on the file's bytes — `a773656` (#76); auto-copy tests can fail — `29915f2` (#77)
- Golden screens for every example — `6095cbf` (#105)
- Load-sensitive waits widened — `7756f9f`, `5d4a7c3`, `b6deb70` (#144)
