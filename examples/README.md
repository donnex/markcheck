# Example checklists

Sample Markdown files that exercise everything markcheck's parser and card
renderer support. Open one with:

```sh
markcheck examples/basics.md
```

- **`basics.md`** — the three task states (`[ ]` / `[/]` / `[x]`), a list banner
  (leading bold-only bullet), a card title (a task's leading `**bold**`), inline
  and fenced commands, a plain-bullet note card, and the same three task states
  again on an ordered (numbered) list to show they work identically there.
- **`nested.md`** — nested / sub-lists under both a task and a plain note,
  at several depths, with the color-coded depth guides in the overview and the
  matching color-coded parent-chain breadcrumb shown on sub-item cards.
  The `Preparation notes:` info parent also shows the "children reflect on the
  parent" treatment: toggle its two sub-tasks and its note marker
  goes blue → yellow (one under way) → green (all done), on the card and in the
  overview, while staying a note.
- **`notes-and-code.md`** — banners, a note that has a command but no checkbox
  (copyable, not a task), a blockquoted `> - [ ]` and `> - [/]` example that
  both render as a non-interactive note rather than a live task, inline vs
  fenced code,
  an ambiguous multi-command task (click to copy a specific one), inline
  styling (emphasis / mid-line bold / strikethrough), links whose URL shows
  centered below the card rather than on it — including a long URL that
  no longer wraps and a two-link task with `[1]`/`[2]` markers keyed to the
  numbered list below the card — an `https://` and a `mailto:` one that `o`
  opens, plus a `file://` one that `o` refuses as an unsupported scheme,
  and a no-checkbox section that gets dropped.
- **`sub-sections.md`** — `### H3`+ sub-sections *within* a list: items
  before the first sub-heading (directly under the H2), an `### H3` group, an
  `#### H4` nested under it, a second `### H3` that pops the H4 and starts a
  fresh group, an item-less `### H3` that shows nothing (empty sub-sections
  drop), and a following `## H2` that resets the sub-section context. Each group
  shows a `── divider` in the overview and a dim `Outer › Inner` breadcrumb
  above the card. The `Extended verification › Line-by-line checks` group is
  deliberately long: scroll into it in a short window to see the sticky overview
  header pin the full path, and use `}` / `{` to jump between groups.
- **`quick-tasks.md`** — items before any heading (the title-less default
  section) and the "Missing document title" placeholder when there is no `# H1`.
- **`no-headings.md`** — a titled document with **no `## H2` at all**: the whole
  checklist is the single default section (no tabs, no above-cards heading).

These double as manual-test fixtures. Keep them in sync when parsing or card
rendering changes — see `CLAUDE.md`.
