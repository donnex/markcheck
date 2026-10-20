# Example checklists

Sample Markdown files that exercise everything markcheck's parser and card
renderer support. Open one with:

```sh
markcheck examples/basics.md
```

- **`basics.md`** — prose under a heading shown above the first task's card
  (the intro paragraph under the `# H1` and the one under `## Ordered steps`),
  a `> [!WARNING]` alert in the warning color under `## Verify`, the three task
  states (`[ ]` / `[/]` / `[x]`) plus a skipped `[-]` task (dimmed and
  struck through, counted apart from done), a critical step (a bold title
  starting with `!`) that takes a second, unhurried `Space` to check, a list banner
  (leading bold-only bullet), a card title (a task's leading `**bold**`, also as a title-only task, which
  the side cards show by its title), inline
  and fenced commands, a plain-bullet note card, and the same three task states
  again on an ordered (numbered) list, whose cards and overview rows show
  each step's `1.` / `2.` / `3.` number, plus
  an uppercase `[X]` that counts as done.
- **`nested.md`** — nested / sub-lists under both a task and a plain note,
  at several depths, with the color-coded depth guides in the overview and the
  matching color-coded parent-chain breadcrumb shown on sub-item cards.
  The `Preparation notes:` info parent also shows the "children reflect on the
  parent" treatment: toggle its two sub-tasks and its note marker
  goes blue → yellow (one under way) → green (all done), on the card and in the
  overview, while staying a note.
- **`notes-and-code.md`** — banners (one long enough to wrap onto several
  rows above the card, and to end in `…` in the overview), a bold-only first
  bullet carrying a command that stays a copyable note card instead of a
  banner, a long card title
  that wraps inside the card, a card title containing inline code (plain title
  text, never a `y` candidate), a note that has a command but no checkbox
  (copyable, not a task), an indented (4-space) code block inside a task that
  renders and copies like a fenced one, a blockquoted `> - [ ]` and `> - [/]` example that
  both render as a non-interactive note rather than a live task, inline vs
  fenced code,
  multi-command tasks (inline and fenced) numbered `[1]`/`[2]` for `y` then a
  digit, a tab-indented fenced block whose tabs show as indentation (and
  copy as real tabs), fenced lines and a long
  mid-sentence command that wrap with a dim `↩` on every continued row (and
  still copy whole on click), inline
  styling (emphasis / mid-line bold / strikethrough), a task whose text
  continues in a second paragraph (joined with a space), a task in wide
  (East Asian) characters that wraps by display width rather than being cut
  off at the card edge, links whose URL shows
  centered below the card rather than on it — including a long URL that
  no longer wraps and a two-link task with `[1]`/`[2]` markers keyed to the
  numbered list below the card, a twelve-link task whose `[10]`–`[12]` are
  opened by typing both digits after `o` — an `https://` and a `mailto:` one that `o`
  opens, plus a `file://` one that `o` refuses as an unsupported scheme, a
  bare URL in the task text that `o` opens just the same,
  a fenced block placed directly after a list that attaches to the list's last
  card, and a no-checkbox section that gets dropped (its bullets are counted
  in the startup note of what isn't shown).
- **`sub-sections.md`** — `### H3`+ sub-sections *within* a list: items
  before the first sub-heading (directly under the H2), an `### H3` group, an
  `#### H4` nested under it, a second `### H3` that pops the H4 and starts a
  fresh group, an item-less `### H3` that shows nothing (empty sub-sections
  drop), and a following `## H2` that resets the sub-section context. Under
  `Rollout`, a heading inside a bullet (`- #### Watch the dashboards`) is just
  that card's title, and a quoted `> ### Caution` callout starts no group —
  neither one is a sub-section (and, sitting after the last task, the callout
  is one of the blocks markcheck reports at startup as not shown). The
  paragraph under `### Pre-flight checks` is that group's preamble, shown
  above its first task's card. Each group
  shows a `── divider` in the overview and a dim `Outer › Inner` breadcrumb
  above the card. The `Extended verification › Line-by-line checks` group is
  deliberately long: scroll into it in a short window to see the sticky overview
  header pin the full path, and use `}` / `{` to jump between groups.
- **`quick-tasks.md`** — items before any heading (the title-less default
  section) and the filename-derived title ("Quick Tasks") shown when there is
  no `# H1`.
- **`front-matter.md`** — a leading YAML front-matter block (as Obsidian,
  Hugo or Jekyll write it) that markcheck skips: no list is named after the
  metadata, and toggling a task leaves the block untouched in the file.
- **`phases.md`** — a runbook split into phases with `#` headings: the first
  `# H1` is the title (its tasks form the untitled first list), and each later
  `# Upgrade` / `# Rollback` starts a list of its own, the same as a `## H2`,
  so the rollback steps aren't counted as part of the upgrade. `# Verify` has
  only a `## Health` list under it, which is the list shown; the `# Rollback`
  list has a banner.
- **`no-headings.md`** — a titled document with **no `## H2` at all**: the whole
  checklist is the single default section (no tabs, no above-cards heading).

These double as manual-test fixtures. Keep them in sync when parsing or card
rendering changes — see `CLAUDE.md`.
