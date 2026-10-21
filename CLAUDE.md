# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## About this repo

A TUI application for pilot-style Markdown checklists. Written in Rust. See DESIGN.md for the full architecture and specification.

## Commands

```bash
cargo build                                                        # compile
cargo run -- <file.md>                                             # run (plain Unicode icons)
cargo run -- --nerd-font <file.md>                                 # run (Nerd Font icons)
cargo test                                                         # all tests
cargo test <test_name>                                             # single test
cargo test -- --nocapture                                          # with debug output
cargo fmt                                                          # format
cargo clippy                                                       # lint
cargo llvm-cov --summary-only --ignore-filename-regex 'main\.rs|_tests\.rs|test_support\.rs'  # coverage summary
cargo llvm-cov --html --ignore-filename-regex 'main\.rs|_tests\.rs|test_support\.rs'          # coverage HTML report
```

PTY integration tests (`tests/pty.rs`) require the binary to be built first — run `cargo build` before `cargo test`.

## Architecture

Data flows one way: `parser.rs` reads a Markdown file into `Document` (via `pulldown-cmark`), `app/` holds `AppState` which wraps the `Document` and owns all navigation/toggle logic (split by concern: navigation, search, write, reload, reset, history, copy, links, keys), `ui/` renders `AppState` each frame, and `writer.rs` atomically writes checkbox state back to the file on every toggle. `git_sync/` is a module directory split by concern in one dependency direction (process -> inspect -> guards -> commit/push -> sync); see DESIGN.md's Git Sync section.

Key non-obvious design points:

- **`[/]` started / `[-]` skipped pre-processing** (`parser.rs`): `pulldown-cmark` only recognises `[ ]`/`[x]`. `extract_extended_markers` parses with `pulldown-cmark` to find each `[/]` or `[-]` that opens a list item, rewrites only those to `[ ]` (same byte length, so offsets are preserved) and records their offsets and states, repeating until a parse finds no new one; the matching `TaskListMarker`s become `Started`/`Skipped` in the real parse. Never guess block structure from raw text there.
- **Items are a flat `Vec`, sorted by `line_number`** after parsing. Nesting depth is metadata on each `Item`, not a tree. Items are pushed at `End(Item)` — post-order — so a sort is needed to restore document order.
- **File watcher watches the *parent directory***, not the file path, because atomic saves (temp-then-rename, used by both `write_back` and most editors) silently lose a file-path watch. Self-writes are filtered via `AppState.file_mtime`.
- **Color capability** is sniffed from `COLORTERM`/`TERM` into a `Palette` at startup; all `ui/` code reads from `state.palette` rather than hardcoding `Color::*`.
- **Card height** is computed fresh each frame from `desired_card_height` (measures wrapped body at actual render width), not stored in state.
- **`tests/pty.rs`** drives the compiled binary under a pseudo-terminal (`portable-pty`) to cover `main.rs` terminal wiring that can't be reached headlessly.

## README disclaimer

- `README.md` must always open with a prominent disclaimer, at the very top (right after the title), stating clearly that the code was created by AI (Claude), comes with no guarantees (correctness, security, fitness for any purpose), and may not have been reviewed by a human. If the README is rewritten or the disclaimer is removed, restore it. Keep it visually obvious (e.g. a bold blockquote with a warning marker), not a buried footnote.

## README Markdown reference

- `README.md` must contain a section documenting **all special Markdown handling** — every piece of syntax the app treats specially, how it renders, and the support level (e.g. the first `# H1` as document title and a later `# H1` as a list, `## H2` list/section, a leading bold-only first item = list banner, a task's leading bold = card title, `[ ]`/`[/]`/`[-]`/`[x]` task states, inline/fenced code and the trailing-command layout, plain-bullet note cards, and what is *not* supported or degrades, such as a list with no checkboxes being skipped). It's the user-facing counterpart to `DESIGN.md`.
- Whenever you change how Markdown is **parsed** or how items/cards are **rendered** (new or changed special syntax, a new card element, changed display of an existing one), update this README section in the **same** piece of work — the same rule as keeping `DESIGN.md` in sync. Don't let it drift from the actual behavior.

## Example files

- The `examples/` directory holds sample Markdown checklists that demonstrate **every supported piece of Markdown handling** and double as manual-test fixtures. `examples/README.md` indexes them and says what each one shows.
- Whenever you change how Markdown is **parsed** or how items/cards are **rendered** (new or changed syntax, a new card element, a changed display), update the `examples/` files in the **same** piece of work so they still cover the feature — the same in-sync rule as `DESIGN.md` and the README Markdown section. Add a new example (or extend an existing one) for genuinely new syntax; keep the content generic (per Test Data & Privacy).
- When you need a checklist to **test, verify, or reproduce** something, reach for an `examples/` file first instead of generating a throwaway file on the fly. If none of them fits the case, that usually means the examples are missing something — extend them (per the sync rule above) rather than making a one-off. It's fine to copy an example into the scratchpad and modify the copy for a specific test.

## Changelog

A changelog is for a reader who will not read the commit history. Git already records every change, so `CHANGELOG.md` is not a second copy of it: **`CHANGELOG.md` is for users, not contributors** — a summary of the changes worth knowing about, written for someone deciding whether anything affects them.

### Format

- `CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The file opens by saying so; keep that header.
- **`## [X.Y.Z] - YYYY-MM-DD` sections, newest first**, each broken into `Added` / `Changed` / `Deprecated` / `Removed` / `Fixed` / `Security` subsections **as needed** — not every heading in every release.
- **`## [Unreleased]` at the top**, always, for work already on `dev` but not yet released. It stays in the file even when empty, so there is always somewhere for the next entry to go.
- `CHANGELOG.md` is tracked and committed like any other documentation. **Never add it to `.gitignore`** — the opposite of `REVIEW.md`; don't reason from one to the other.
- Every release repeats headings like `### Added`, which markdownlint's MD024 (no-duplicate-heading) would reject. `.markdownlint.json` settles this with `"MD024": { "siblings_only": true }` beside the `MD013` line — keep it. Don't make headings artificially unique (`### Added (2026-09-12)`) instead.

### When it is updated

- **In the same commit as the change it describes**, under `## [Unreleased]` — the same sync-in-the-same-work discipline as `DESIGN.md`/the README Markdown reference/`examples/`. This is not optional and not a follow-up: a changelog updated later is written from memory, and a change that shipped without its entry is invisible in the file for ever.
- A commit that only fixes a not-yet-merged commit is squashed into it per the history-rewriting rules below, so its entry is folded in the same way rather than added as a second one.
- An entry is only filed under a version once it has been released — see "Releasing" below.

### What goes in, and what does not

- Include a change **a user would notice**: a new feature, a behavior change, a bug fix they would have seen, and anything that changes their configuration, their command-line flags, their keybindings, or what they have to install.
- **Purely internal changes get no entry** — refactors, test-only additions, CI and tooling, and contributor-facing documentation like this file or `DESIGN.md`. Also left out: dependency bumps, unless one changes behavior or adds something the reader has to satisfy; and review bookkeeping and other process artefacts.
- When a stretch of history consists of little but the above, say so in one line rather than listing it.
- The test for an entry is whether someone who did not make the change would want to know about it — not whether it was hard, and not how many files it touched.

### How an entry is written

- **Write it for a user reading release notes**, not as a copy of the commit title: describe the effect, not the implementation. An entry naming files or functions is a commit message in the wrong place — the reader does not have the code open.
- Lead with the change itself. The reason belongs in the sentence after, where it can be skipped.
- Where a change replaced something that was wrong, **say what was wrong**, rather than "improved X".
- End with the issue number in parentheses where the work is tracked, matching the `(#N)` suffix on commit titles, so the entry, the commits and the issue all point at each other.
- Where a change genuinely needs more than a few sentences, put the detail in a second paragraph under the same bullet rather than splitting it into several entries.
- Within a section, lead with the largest change. A section is scanned, not read through.
- This repo is public: the Test Data & Privacy rules below apply to the changelog like any other file, and an entry has to make sense to someone who has never run markcheck — name what it is talking about the first time it comes up in a release.

### Releasing

Every merge to `main` is a release; the version bump, the renaming of `## [Unreleased]` and the release tag are covered under "Releasing" at the end of this file.

## Keybindings

- The TUI's keyboard shortcuts are **vim-like**, and should stay that way. When adding or changing a binding, match vim conventions wherever it makes sense (`h`/`j`/`k`/`l` motion, `gg`/`G` for first/last, `y` to yank/copy, capitals for the bigger jumps, `?` for help), and when proposing options to the user, **lead with the vim-idiomatic choice**.
- Keep the always-on status-bar legend trimmed to the essentials; the `?` help overlay is the full reference and must list every binding. Update both (and the DESIGN.md / README keybinding tables) in the same work whenever a binding changes.

## UI Feedback

- **Always think about and implement good user feedback for every action.** When adding or changing any user-facing behavior, ask "how does the user know what just happened?" and build the answer in as part of the same work — never leave an action silent.
- A good action-feedback pass covers: **confirmation** that the action happened (e.g. "Copied to clipboard"), the **current state / input** where relevant (e.g. the live search query), **quantitative feedback** where it helps (counts, positions, progress — e.g. "5 matches", "Match 2/5"), and an explicit, visually distinct **empty / nothing-happened / failure** state (e.g. "No matches", "Nothing to copy") rather than a silent no-op that looks broken.
- Match the existing feedback machinery: the status bar (`src/ui/statusbar.rs`), ephemeral vs. sticky status messages (`set_status` for passive confirmations that auto-expire, `set_error` for failures or "nothing happened" feedback that must not vanish before being read), and error-colored text for problems. Prefer reusing these over inventing new channels.

## Workflow

- **Commit to `dev`, or to a topic/issue branch, the moment the current piece of work is complete — do not ask first, and do not wait to be told to commit.** This is not a judgment call and not something to soften into a question ("should I commit this?", "want me to commit?", "ready to commit whenever you are"). The instant a change reaches a natural, self-contained stopping point (per the git conventions' one-commit-per-concern rule), commit it, then move on. Treat "confirm before committing" as something that applies only to the specific, separate actions the git conventions call out by name — merging to `main`, force-pushing, and the like — never to an ordinary commit on `dev` or a topic/issue branch. If a task produces several distinct commits back to back, make all of them without pausing in between to ask; only pause once the whole piece of work is done. A missing commit at the end of a task is a bug in following this rule, not a safe default.
- After the commit completes, show a summary of what changed and why — this is a report of what already happened, not a request for permission. If the work produces multiple commits, commit each as its own concern per the git conventions below, then show one summary covering the full set once they are all done.
- Before committing any `.md` file, run `markdownlint <file>` and fix all findings. The file must pass with no errors before it is committed.
- Disable MD013 (line-length) in `.markdownlint.json` — prose and instruction files can't reasonably be wrapped at 80 characters. Once set, don't remove it or attempt to reformat files to satisfy it.
- If the project generates a review report (e.g. a code-review agent instruction that writes `REVIEW.md`), leave that file intentionally untracked (not gitignored) in the repo root. Its presence signals that a review has been generated and needs to be processed. Never add it to `.gitignore`.
- Before running a prompt that generates `REVIEW.md` (e.g. a deep-code-review or an instructions-audit prompt), check whether an untracked `REVIEW.md` already exists in the repo root. If it does, ask whether it has already been processed — its presence signals unfinished review work per the rule above. If the user confirms it's been processed, proceed (the new prompt will overwrite it); otherwise stop and let the user decide how to handle the existing report before running the new prompt.
- When a large task wraps up, or context usage is running high (roughly 20% or more used), remind the user to run `/clear` to start the next task with a fresh context. Output the reminder as a `> [!WARNING]` markdown callout so it stands out from surrounding prose. This is a reminder only — `/clear` is a built-in CLI command, not something invocable through a tool, so it can never be run on the user's behalf.

### Issue drafts: `new-issues.md`

A `new-issues.md` file in the repo root holds issue drafts the user wants filed in the project's issue tracker. Like `REVIEW.md`, it is a signal file: leave it untracked, and never add it to `.gitignore`.

Look for it at the start of a session, before starting each new task or step, and after each commit; long work checks between its steps too. When it exists, run the `new-issues` skill, which holds the procedure: reword the drafts, ask once whether to file them, wait one minute at most, then carry on with the work whether or not an answer came. The pause never stops, rolls back or skips work, and an unanswered question is not a no: ask again at the top of every later reply, in this session or the next, until the user answers. Without the skill, follow that outline.

### Shortcuts: `issues`, `ideas`, `changes`

A message consisting of only one of these words (ignoring surrounding whitespace) runs the skill of the same name, which holds the procedure; inside a longer message the word is ordinary text.

- **`issues`**: review the open issues, the ones waiting on a decision as short decision cards.
- **`ideas`**: review the open idea issues the same way.
- **`changes`**, optionally followed by a period (`changes since yesterday`): summarize what changed as a shareable page.

When the skill isn't available, say so rather than improvising the procedure.

### Naming an issue

**This must always be followed, with no exceptions.** Whenever a reply mentions an issue by number, write the number and the issue's title together in bold, followed by a very short summary of what it is about: one or two sentences. For example: **#12 [IDEA] Cache the API responses**: the client refetches every page on each run; a small on-disk cache would make reruns instant.

- This applies everywhere an issue is named: prose, lists, summaries, questions, commit reports and tables. A table of issues gets title and summary columns, never numbers alone.
- Give the full form at least the first time an issue appears in a reply. A later mention in the same reply may repeat just the bold number and title.
- Never write a bare number, and never guess a title or summary from memory: look it up.

**Cache issue data when lookups would be slow.** When a reply names many issues, or the tracker is slow to query, fetch the open issues once into a cache file in a temporary working directory (e.g. a JSON array of `{number, title, state, summary}`, under the session's scratch directory or `$TMPDIR`), and look issues up there instead of querying the tracker for each one. The cache is a convenience, not a record:

- Refresh it at the start of a session and after creating, closing, retitling or editing any issue, and refetch any issue missing from it.
- Never commit it, and keep it out of the repository's working tree.

### Deciding on anything

**This must always be followed, with no exceptions: every time the user is asked to decide or choose something, a short explanation comes first, shown in plain text before the question.** It applies to every decision and every question, not only to issues: a choice between approaches, a yes/no on an action, a clarifying question, a question mid-task, while planning, after a review, or one that comes up in passing. It is never optional, never skipped because the topic seems familiar or was discussed earlier, and never left for the prompt to carry. A question that names only a number, a title or a bare option label asks the user to decide from memory.

Keep it brief. The user wants to decide, not read a page: a few lines, not a document.

The explanation, written out before the prompt appears:

- **What it is**: one to three plain sentences on the problem or request, where it applies and what is still open, checked against the current code. For an issue, lead with its identifier and title in bold. A technical subject may take a little more, but only as much as the decision needs.
- **The choice being asked**, with what each option would mean.
- **Pros, cons and cost**: a few short bullets each, always, even when the choice looks small. The cost is the rough effort (small, medium or large) and what it would touch; for a choice between several options, give each option its own pros, cons and cost.

When one prompt asks about several things at once, explain each of them before it. A prompt's own option labels and descriptions don't count as the explanation: they are too short, and some interfaces hide them until an option is selected.

## CI

If a project has no CI workflow configured yet, suggest setting one up — don't wait to be asked. This is a suggestion, not something to set up unprompted: propose it and let the user decide.

- Check `git remote -v` to see which host(s) the project actually uses. Suggest a workflow for each one present: GitHub Actions (`.github/workflows/`) for a `github.com` remote, Gitea Actions (`.gitea/workflows/`) for a self-hosted Gitea remote. If the project is pushed to both, suggest both — the two are close enough in syntax that a workflow's content is largely shareable between them as-is.
- The workflow must run every check the project actually enforces before a commit or change is considered clean — derive this from this file, not a generic template. For example: linters, formatters (run in check mode, never autofix mode, in CI), type checkers, test suites, and any build step. If the project has no automated checks at all yet, there's nothing to wire up yet either — say so instead of inventing checks that don't exist.
- If the user declines CI setup (not now, not wanted, whatever the reason), note that decision — and the date — in this file, then drop it for the rest of the conversation. In a later session, once this file carries that note, treat it as a standing prompt to ask again: whether real time has clearly passed, or the project has grown enough that the case for CI is stronger than when it was declined.
- A CI workflow is a second copy of "what needs to pass before this is clean" — the same failure mode as any other duplicated logic applies: whenever the project's tools or checks change (a new linter, a new required check, a build step added or removed), the workflow file(s) must change with it, in the same change that changed the tooling. Don't let this slip to a follow-up — treat an out-of-date workflow as a bug, the same way an unsynced snippet or a stale doc would be.

`.github/workflows/release.yml` builds and packages tagged releases for four targets, each natively on a runner of its own architecture in a `build` matrix: static `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` binaries, smoke-testing their X11 and OSC 52 clipboard paths first, and `aarch64-apple-darwin` and `x86_64-apple-darwin`, checked only to start; the archives reach the `release` job as workflow artifacts, so a failed target publishes nothing. `.github/workflows/check.yml` runs `cargo fmt --check`/`clippy`/`cargo test`, then `cargo llvm-cov` to enforce the 97% coverage threshold, on every push and pull request, plus `cargo deny check licenses` and `cargo deny check advisories` (both against `deny.toml`; the advisories check fails on any RustSec advisory or yanked version in `Cargo.lock`, and an advisory accepted on purpose goes in `deny.toml`'s `ignore` list with its reason) a `macos` job that runs `clippy`/`cargo build`/`cargo test` on macOS, and an `msrv` job that reads the `rust-version` from `Cargo.toml` and builds with exactly that toolchain. `release.yml` also generates each archive's `THIRD-PARTY-LICENSES` with `cargo-about` from `about.toml` and `about.hbs`, for that archive's target (keep the target lists in `about.toml` and `deny.toml`'s `[graph]` covering every release target); `check.yml` fails when `about.toml`'s `accepted` list and `deny.toml`'s `allow` list differ, so a license added to one goes into the other in the same change. `release.yml` calls `check.yml` as a reusable workflow (`workflow_call`) for its release gate rather than repeating its steps, and adds a check that the tag matches the `Cargo.toml` version. `.github/workflows/mutants.yml` runs `cargo mutants` over the safety guards (scope and exclusions in `.cargo/mutants.toml`) weekly and on demand rather than on every push, since it reruns the unit tests once per mutant; a mutant it reports as missed or timed out is a guard no test pins, fixed by a test or, when no test can tell it apart, an exclusion with its reason. `.github/workflows/advisories.yml` reruns `cargo deny check advisories` weekly and on demand, because a new advisory against an unchanged `Cargo.lock` appears without any push to trigger `check.yml`. Third-party actions are pinned to full commit SHAs and npm tools to exact versions; bump a pin by updating the SHA and its version comment together. Every `actions/checkout` step sets `persist-credentials: false`, so the job's token isn't left in the git config for a dependency's build script to read; a step that genuinely needs git credentials after checkout gets them explicitly instead. Keep both in sync with this file per the rule above.

- The PTY tests serialize themselves: each `Session` in `tests/pty.rs` holds a file-wide lock while its binary runs, because many concurrent PTY-spawning tests starved git-sync's background thread on CI runners. Keep that serialization inside the test file rather than as a `RUST_TEST_THREADS` setting in CI, so it applies to every invocation that runs them (`cargo test`, `cargo llvm-cov`, a local run) while the unit tests stay parallel. The harness must never synchronize on a fixed sleep or wait without a deadline: poll for an observable condition (screen text, file content, the child's exit) up to a timeout that fails the test.

## Rust

- Always target the latest stable Rust release, and verify the toolchain is current before committing. Check with `rustup check`.
- Only use a nightly toolchain, or a pre-release dependency version, if it is strictly necessary — confirm with the user first.
- Use the latest stable release versions of dependencies.
- For a program a user runs directly, use color and clear presentation — structured formatting, tables, progress/status indicators, etc. — for its terminal output when it makes sense: status messages that should stand out (success/warning/error), progress, or tabular data. Skip it for something too small to warrant it, e.g. a program whose entire output is a single line.
- Default to no comments in code — clear naming and control flow should carry the intent by themselves. Doc comments (`///`, `//!`), where the project requires them (e.g. via `#![warn(missing_docs)]`), are a separate, always-required matter, not part of this rule. Add a regular `//` comment only when the code is genuinely hard to follow without one: a non-obvious constraint, a workaround for a specific bug or quirk, or logic that isn't self-evident from reading it — in that case, add the comment rather than leaving a future reader to puzzle it out.
- Use `cargo fmt` for formatting and `cargo clippy` for linting. Both must pass with no errors before committing.
- Aim for high test coverage on every new feature or fix. This project's agreed threshold is **97%** region/line coverage of **production code** (excluding `main.rs` and all test code, per the exclusions below) — keep coverage at or above it as code is added, rather than letting it slip. It's fine for genuinely untestable paths (e.g. something that can't run headless) to stay uncovered — note why with a comment at the untested site (e.g. "requires a live X11/Wayland session, not reachable headlessly") rather than forcing a brittle test or filing an issue for something with no real resolution.
- Measure coverage with `cargo llvm-cov` (requires the `cargo-llvm-cov` subcommand and the `llvm-tools-preview` rustup component):
  - `cargo llvm-cov --summary-only --ignore-filename-regex 'main\.rs|_tests\.rs|test_support\.rs'` — summary
  - `cargo llvm-cov --html --ignore-filename-regex 'main\.rs|_tests\.rs|test_support\.rs'` — line-by-line HTML report
  - Exclude `main.rs` (or an equivalent thin entry-point file) from coverage accounting if it's mostly wiring that's better exercised by integration tests than unit tests.
  - Keep test code out of the measurement: test code is executed by definition, so counting it inflates the figure. Each module's tests live in a sibling `<module>_tests.rs` file (`#[cfg(test)] #[path = "<module>_tests.rs"] mod tests;`, not an inline `mod tests { ... }` block), and shared fixtures in `test_support.rs`, whose own tests may stay inline since the file is excluded whole; the ignore regex above excludes both. The regex excludes files by name, so any other `#[cfg(test)]` item (a fake, a test hook, a test-only wrapper method) also goes in a `test_support.rs` or `<module>_test_support.rs` file, never inline in a production file, where it would count as covered production code; an inherent `impl` block or a `use` re-export lets the production module keep the same names. A single `#[cfg(test)]` statement inside a production function is the only exception.
- UI/rendering tests should assert on rendered output content, not styling or presentation details, so tests survive cosmetic tweaks. Exception: when color or style *is* the feature under test — a state encoded in color rather than text (e.g. done/started/error rendering in a specific hue) — asserting on that styling is asserting on the actual behavior, not incidental presentation, so it's fine.
- After making changes, run `cargo fmt` and `cargo clippy` and fix all findings before committing.
- Run the full test suite (`cargo test`) before committing.
- Integration tests that drive a compiled binary (e.g. under a pseudo-terminal) require the binary to be built first — run `cargo build` before running them.
- markcheck-specific, stays here regardless: when making an architectural or behavioral change (new module, new data flow, changed data types, new dependency, new UI element), update `DESIGN.md` as part of the same piece of work. Don't let it drift out of sync with the actual implementation.

## Issues

Issues are tracked on GitHub, managed with the `gh` CLI rather than the web UI for routine operations.

**GitHub issues are frequently public, and even a private repo's issues can be read by anyone with access to it — treat every issue title, body, comment, and label as content that could leak beyond the intended audience.** Never write any of the following into an issue: private or confidential data, real system information (hostnames, IP addresses, internal file paths, internal URLs, infrastructure details), credentials, tokens or secrets of any kind, real personal names, or any other identifying or sensitive detail. Use a placeholder or generic description instead — the same discipline as writing any other file in the project.

**If it is ever unclear whether something counts as sensitive, stop and ask the user for explicit confirmation before creating or posting anything** — do not guess, and do not proceed on the assumption that a repo or issue is private enough to relax this.

Before actually running `gh issue create` or `gh issue comment`, re-read the fully drafted title/body a second time as a distinct check, looking specifically for anything that violates the rule above. This second pass happens after the content is written and before the command runs — never skip it, even for a small or seemingly obvious issue. Only submit once that second read confirms it's clean.

- Install `gh` via the OS's package manager (e.g. `apt install gh`, `brew install gh`) or from GitHub's own release page.
- Authenticate once per machine: `gh auth login`, following its interactive prompts (browser or token; add `--hostname <host>` for a GitHub Enterprise Server host). The resulting credentials live only in `gh`'s own local config (`~/.config/gh/hosts.yml`) on the machine running it — never commit a token or write one into the repo.
- Run `gh` from inside the project's repo; it auto-detects the repository from the git remote, so `--repo <owner>/<name>` isn't needed unless operating on a different repo than the current checkout.
- List issues: `gh issue list`. **This stops at 30 issues by default**, with no sign that anything was cut off. To count or sweep the whole backlog, raise the limit well past the backlog's size (`gh issue list --limit 1000`) and check the result count is below it — a listing that comes back exactly at its limit has been truncated.
- View a single issue: `gh issue view <number>`
- Create an issue: `gh issue create --title "..." --body "..."`. For a body of any real length prefer `--body-file <file>` (or `--body-file -` for stdin), which also keeps a long value out of the process's argument list.
- Comment on an issue: `gh issue comment <number> --body "..."` (or `--body-file`).
- Close an issue via `gh issue close <number>`, once it's actually fixed and the fix is committed (and pushed, if that's part of the workflow in play) — never with a bare state change; see below.
- Label an issue: `gh issue edit <number> --add-label "..."`
- Unlike some other issue-tracker CLIs, `gh`'s issue subcommands cover the full set of routine operations natively (list, view, create, comment, close, label) — there's no need to fall back to the API or the web UI for any of these.
- Before starting substantial work, check open issues (`gh issue list`) so nothing gets duplicated or missed.

### Attribution and closing

- Every issue, comment, or other content posted to the tracker must end with a trailer identifying the active model, in the same `Co-Authored-By: <model name> <noreply@anthropic.com>` format used in git commit messages, e.g. `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`. Append it as the last line of the body or comment text. Never omit it.
- Never close an issue with a bare state change. First post a comment with a short description of what changed to resolve it, and the commit(s) that fixed it (short SHA plus subject line, e.g. `abc1234 scope: Fix the thing`), then close it — a plain closed state with no comment loses the traceability back to what actually resolved it and how. `gh issue close <number> --comment "..."` does both in one step.
- Alongside each commit SHA and subject line, also include a link to that commit (e.g. `https://<host>/<owner>/<repo>/commit/<full-sha>`), so the closing comment lets a reviewer jump straight to the actual diff instead of having to look the commit up themselves.

### Remind about the open issue count

Open issues accumulate silently otherwise — nothing surfaces them again once filed, so they're easy to forget for months. If this project already has a periodic review-timer mechanism for other recurring obligations (docs review, security audit, etc.), add an issue-triage timer to it: periodically list the whole backlog (with a limit well past its size, per the listing note above — a default listing stops at 30 and under-reports the very number this check exists to surface), spot-check a sample of open issues against the current code (not necessarily every one, for a large backlog), and report the total open count plus anything found stale to the user — offering to close the confirmed-stale ones per the closing rule above. If the project has no such mechanism, do the same check-and-report at the start of a session or before ending one, whichever is more natural given how the mechanism (or its absence) works here, rather than letting the count grow unremarked indefinitely.

### Filing deep code review findings

When a deep code review (or similar audit prompt) says to file its findings with the project's issue tracker, and this project's is GitHub, file one issue per finding instead of writing a local report — per that prompt's own instructions on where findings go.

- Title: `[<severity>] <short description>`, e.g. `[HIGH] SQL injection in the search endpoint`.
- Body: the same content the review would otherwise put in a finding's entry in a local report — file and line number(s), why it's a problem, and the suggested fix — plus the required trailer from above.
- Before creating one, check the open issues (the whole backlog, per the listing note above) for an existing one covering the same finding, so re-running the review doesn't file a duplicate.
- Create with `gh issue create --title "..." --body-file <file>`, same as any other issue.
- Filing the issue is the full extent of this step — do not apply the fix or open a pull request as part of it. The review reports findings before anything gets fixed, the same as the local-report fallback would.

## Test Data & Privacy

Applies to every file in the repository — not just test fixtures, but source code, comments, commit messages, and the repo's own documentation (`CLAUDE.md`, `README.md`, any design doc, etc).

Real content must never be committed, full stop — no confirmation step, no "just this once," and no exception beyond the single narrow one called out below for non-secret public URLs. Always use placeholders or made-up content instead otherwise. This is the strict counterpart to a confirm-first policy (one that allows a real value once the user has explicitly signed off on that specific use) — here, real data leaking into the repository at all is the failure mode to guard against, not just an unconfirmed one.

- Never write a real person's name anywhere in the repository. Use a generic placeholder instead (e.g. "the user", "example-user").
- Never write a real hostname, URL, IP address, command, or any other identifying detail. Use a made-up placeholder instead (e.g. `example-host`, `example.com`, or a documentation-range address like `203.0.113.10` — never a real one). **Exception:** a real URL to a public website is fine to write as-is when nothing about it looks secret or private — no token, session ID, API key, or other credential embedded in the path or query string, and no private-looking host (an internal hostname, a `.local`/`.internal`-style domain, or a private/loopback IP literal). This exception covers a plain link to genuinely public web content (official documentation, a package registry page, a public repository, an RFC, etc) — it does not extend to a hostname, IP, or command used as a configuration value or infrastructure reference, even one that happens to be publicly reachable, nor does it relax any other bullet in this list.
- This applies even when writing a rule *about* real data: cite a placeholder, never an actual value, even as an illustrative example.
- If the user supplies a real, non-generic example (a real hostname, a real command, a real name) to illustrate a request, generalize it into a placeholder before writing it down anywhere in the repository — never commit it verbatim, no matter who provided it or why.
- There is no confirm-and-proceed path here: if something might be real data, treat it as real data and replace it with a placeholder. When in doubt, default to a placeholder rather than asking whether the real value is fine to use.

### Posting outside the project: nothing private, ever

**THIS RULE CAN NEVER BE BROKEN. NO EXCEPTIONS, EVER.** Not for a small comment, not for a detail that seems harmless, not because the user seemed to imply it was fine, not to save time, not "just this once". It overrides every other content rule, including any confirm-first policy: permission to use a real name in the project's own files, docs or commits never covers posting it anywhere outside the project.

**Where it applies:** anything written where people outside the project can read it. That means an issue, comment, pull request, review, discussion, commit message or release note in another project's repository or tracker (GitHub, GitLab, any forge, a mailing list, a forum, a chat), and the user's own public projects too.

**What may never appear there. Nothing from the project, at all:**

- the names of the project, its repositories, folders, files, scripts, functions, branches or settings;
- its issue or pull-request numbers, titles or links, and its commit hashes;
- hostnames, machine names, domains, IP addresses, usernames, paths, accounts, services or internal URLs;
- code, config, logs, error output or screenshots copied from it, even a single line;
- a description of the project's setup, tools or purpose specific enough to identify it.

**Write it generically instead.** Describe the problem as it would happen to anyone ("a wrapper script that opens the tool in a terminal window", "a laptop with an external monitor"). Build every example, reproduction and log from scratch with placeholder names (`example.org`, `host-a`, `my-script`). A minimal reproduction made for the report is fine; a copy of anything from the project is not.

**Check before posting, every time.** Draft the whole text first, then reread the title, body, labels and any attachment line by line against the list above, as a separate final step just before submitting.

**When unsure, ask. When even a little unsure, ask.** Whether a detail counts as private, whether a place counts as outside the project, whether a reproduction is generic enough: any doubt at all means asking the user before posting, never deciding alone.

**Not for the project's own private tracker.** In an issue tracker only the project's own people can read, naming the project's files, issues and hosts is normal, and this section doesn't apply. If it's unclear whether a tracker is private, treat it as public and ask.

**If something private was posted anyway**, tell the user at once, so it can be removed or edited; never leave it to be noticed later.

**markcheck's own tracker is the exception for markcheck's own content.** This repository and its GitHub issues are public, and markcheck itself is meant to be seen: in markcheck's own issues, comments, commits and docs, naming markcheck, its files, functions, settings, issue numbers and commit hashes, and quoting its code, Markdown and output, is normal and allowed. The strict Test Data & Privacy rules above still apply there in full (no real hostnames, names, paths or credentials).

**Nothing from any other repository, ever.** Nothing from the user's private repositories, or anything they link to, may appear anywhere in markcheck: not in its files, commits, issues, comments or release notes. That covers their names, paths, file and script names, issue numbers, commit hashes, content and setup, even when a convention or instruction here came from one of them. Describe such an origin generically ("the project's conventions") or leave it out. The same holds in the other direction: posting markcheck's content in another project's tracker falls under the rule above.

## Git

General git conventions for this project: branching, history rewriting, merging to `main`, and commit formatting.

### Branching

- Default to the `dev` branch for all work, unless a different branch has already been checked out or explicitly set for the current task — in that case, keep working on that branch instead of switching to `dev`.
- Never commit, amend, rebase onto, or push directly to `main`, and never push `dev` to trigger anything main-facing on your own initiative. `main` only moves via an explicit user-requested merge (see "Merging to main" below).
- Before starting work on a tracked issue or a larger/multi-commit piece of work, ask whether to create and switch to a new topic branch for it, rather than committing straight to `dev`.
- When the work is tied to a tracked GitHub issue, name the topic branch with that issue's identifier: `issue-<number>-<short-kebab-case-description>` (e.g. `issue-123-fix-broken-parser`). This mirrors the `(#N)` suffix used in commit titles, so the branch, its commits, and the issue are all traceable to each other.
- Keep `dev` linear on top of `main`: rebase `dev` onto `main` (never merge `main` into `dev`), so `dev` stays a fast-forwardable descendant of `main`.

#### History rewriting

- History rewriting — amending, rebasing, force-pushing — is allowed on `dev` (and topic branches based on it), but only for commits not yet merged into `main`, and only as long as `dev` stays linear on top of `main` per the rule above. This overrides the general "always create new commits, never amend/force-push" default, but *only* for `dev`/topic branches — never rewrite `main` history.
- Aim to keep `dev`'s commit count low. Whenever a later commit would touch the same change as one already on `dev` and not yet merged to `main` — most often a fix or refinement found via testing or review of something just committed — rewrite the earlier commit(s) instead of stacking a new one on top: `git commit --amend` for the tip commit, or a soft-reset-and-recommit for an earlier one (Claude Code's tooling disallows interactive git flags, so `git rebase -i` isn't an option). Do not carry multiple commits into a merge to `main` that are really just successive fixes or changes to the same not-yet-merged work — combine them into the commit(s) they fix before merging. This is about the same change accruing fixes over time, not about splitting distinct work: the one-commit-per-concern rule under Commits still applies to genuinely separate concerns. Keep each commit correct and self-contained, since it hasn't shipped yet; re-run lint and push with `--force-with-lease` afterward per the rule below. Once a commit is merged to `main`, this no longer applies — fix it forward with a new commit as usual.
- A topic branch gets the same treatment relative to `dev` that `dev` gets relative to `main`: rewrite its commits — amend, or soft-reset and recommit — to fold in fixes and refinements found while working on it, rather than stacking new fix-up commits on top. Clean it up before merging into `dev`, the same way `dev` gets cleaned up before merging into `main`, so what lands on `dev` is already tidy rather than a commit plus a trail of its own fixes. This is not optional — see "Mandatory pre-merge history check" below, which applies to this merge exactly as much as it applies to merging `dev` into `main`.
- Always push `dev` after committing or rewriting its history, so the remote never lags local. The same applies to a topic branch: push it after every commit made on it, not only once it's ready to merge, so it's never sitting local-only. When history was rewritten (amend/rebase), push with `--force-with-lease` (never a bare `--force`), so the push fails safely instead of clobbering anything unexpectedly added to the remote branch since the last fetch.

#### Mandatory pre-merge history check

**Before merging `dev` into `main`, or a topic branch into `dev`, the commit history being merged MUST be checked and rewritten if needed. This is not optional cleanup, not a judgment call, and not something to skip because the commits "look fine" — it is a required gate, every single time, with no exceptions. This has been missed before, and a missed check means every messy in-between commit ships permanently, since the destination branch's history is never rewritten after the fact.**

The check: walk every commit being merged (`git log --oneline main..dev`, or `main..<topic-branch>` for a topic branch) and compare each one against the History rewriting rules above. Ask, for every commit: *is this really just a fix, refinement, typo correction, or follow-up to an earlier not-yet-merged commit in the same range?* If so, it must not survive as its own commit — squash it into the commit it fixes (`git commit --amend` for the tip, or a soft-reset-and-recommit for an earlier one) before doing anything else. The goal is that what lands on the destination branch reads as if it had been written correctly the first time, not as a live recording of the back-and-forth it took to get there. This is about collapsing accrued fixes to the *same* change, not about squashing genuinely separate concerns into one commit — the one-commit-per-concern rule under Commits still applies.

Do this check — and any resulting rewrite plus `--force-with-lease` push — *before* presenting a merge summary to the user, not after. A merge summary should already describe the clean, final history, not a history that's about to be rewritten out from under it.

#### Merging to main

Merging to `main` happens only on explicit request. Every merge request, typed out or as `mm` (a message consisting of only `mm`, ignoring surrounding whitespace; inside a longer message it is ordinary text), runs the `mm` skill, which holds the full sequence.

**The hard gate holds with or without the skill, with no exceptions.** Immediately before running `git merge --ff-only` or pushing to `main` for any reason, verify: was the mandatory pre-merge history check above done; was the user's *most recent* merge request answered with a summary (`git log --oneline main..dev` and a nutshell of what changed) shown after that request; and did the user explicitly confirm that specific summary? If any answer is no, the merge does not happen yet, regardless of size, urgency, or how much of it was discussed earlier. `mm` never approves a merge: sent right after a summary, it is a new merge request that gets a fresh summary. Merge only as a fast-forward of `main` to `dev`; when that isn't possible, stop and ask rather than falling back to a merge commit or a force-push.

Remind the user to merge when it seems due: when a large feature/fix on `dev` looks finished, or `dev` has accumulated a lot of commits ahead of `main`, say so and suggest merging. This is a reminder only — never merge to `main` automatically or without explicit confirmation, no matter how done the work looks or how many commits have piled up.

#### Branch cleanup

Once a topic branch has been merged into `dev`, delete it — both the local branch and its remote counterpart (`git branch -d <branch>`, then `git push <remote> --delete <branch>`) — no need to ask first. A merged branch is fully redundant the moment its commits live on `dev`; its only purpose was getting them there. `dev` and `main` themselves are never deleted — this applies only to topic branches.

### Commits

- Every commit Claude Code creates must end with a `Co-Authored-By:` trailer identifying the active model, e.g. `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`. Never omit this — if a commit is later found to be missing it and hasn't been merged to `main` yet, fix it via the history-rewriting rules above rather than leaving it out.
- One commit = one concern, not necessarily one file. Bundle files that share the same concern into a single commit — a source change routinely spans several files, and this applies to documentation too: if a single change is documented across more than one file (e.g. a design doc and `README.md` both describing the same change), commit them together rather than splitting per file. Keep genuinely separate concerns in separate commits even when they land together for the same task — don't bundle work together just because it happened at the same time if the concerns themselves are distinct.
- Prefix commit titles with the **scope** of the change, followed by a colon and an uppercase description. The scope names the **one area the change is about** — a single path, never a list of files or directories. Match the prefix casing to the actual file or directory name. One file → its exact path (`fetch-data:`, `CLAUDE.md:`). Several files → the directory that holds the ones the change is about (`snippets:`, `src/ui:`). Files that change only because the change carries them along don't count toward the scope: the docs, changelog, examples, tests and snapshots that the sync rules require in the same commit. A parser change that also updates `README.md`, `DESIGN.md`, `CHANGELOG.md` and `examples/` is `src/parser.rs: Show mid-list prose (#187)`, not `src, examples, README.md, DESIGN.md, CHANGELOG.md: Show mid-list prose (#187)`. When the change itself spans several areas, name the one at its center (the module most of it is about) or, when there is none, their nearest shared directory (`src:`). Never list paths comma-separated, however few: name every file in the body's per-file bullets instead. End the title with the issue number when the work is tied to a tracked issue: `(#12)`.
- Keep the title under 80 characters. Only exceed this if absolutely unavoidable.
- Put detailed descriptions in the commit body, not the title. For a commit spanning several files, use a one- or two-line prose summary followed by per-file bullets (`- bin/fetch-data: ...`) saying what each file's part does.
- Examples: `fetch-data: Add retry prompt`, `CLAUDE.md: Add indentation rule`
- When adding a new file with no meaningful description, use `filename: Add file`. If there is a reason worth stating, describe it instead: `fetch-data: Add script to fetch remote data on a schedule`
- Never modify `.gitignore` files without explicit confirmation from the user.
- Before every commit, all lint/format/type checks required by this file must pass with zero errors. Never commit with outstanding failures — fix them, or ask the user how to handle a check that genuinely should be skipped.

## Releasing

Every merge to `main` is a release. Whenever merging to `main` (see "Merging to main" above), always bring both of these up as part of it — don't wait to be asked, and don't let a merge land without them:

- **Version bump + changelog:** before presenting the merge summary, look at what's landed on `dev` since the last release and recommend a `Cargo.toml` `version` bump (semver: patch for fixes/internal work, minor for a backward-compatible feature, major for a breaking change) — then ask for confirmation before applying it. Apply it together with finalizing `CHANGELOG.md` (see "Changelog" above): rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` with the new version and today's date, and add a fresh empty `## [Unreleased]` above it. **This version-bump-and-changelog commit must be the last commit on `dev` before the fast-forward merge to `main`** — nothing else lands on `dev` after it (`cargo build` picks up `Cargo.lock` automatically, so it's included in the same commit).
- **Release tag:** once `main` is updated, create and push an annotated tag matching the new version, `vX.Y.Z` (matching the existing `v1.0.0`/`v1.1.0` convention), so `release.yml` picks it up.

**Do not** update the illustrative `--version` example in `DESIGN.md`/`main.rs` (`e.g. 1.0.0 (da90ddd)`) when bumping the version. It demonstrates the output *format*, not a value that needs to stay current — the same as its git-sha half, which nobody keeps in sync with the real SHA either. Only touch it if the format itself changes.
