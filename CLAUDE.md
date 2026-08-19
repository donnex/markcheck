# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## About this repo

A TUI application for pilot-style Markdown checklists. Written in Rust. See DESIGN.md for the full architecture and specification.

## Commands

```bash
cargo build                                                        # compile
cargo run -- <file.md>                                             # run (Nerd Font icons)
cargo run -- --no-nerd-font <file.md>                              # run (plain Unicode icons)
cargo test                                                         # all tests
cargo test <test_name>                                             # single test
cargo test -- --nocapture                                          # with debug output
cargo fmt                                                          # format
cargo clippy                                                       # lint
cargo llvm-cov --summary-only --ignore-filename-regex 'main\.rs'  # coverage summary
cargo llvm-cov --html --ignore-filename-regex 'main\.rs'          # coverage HTML report
```

PTY integration tests (`tests/pty.rs`) require the binary to be built first — run `cargo build` before `cargo test`.

## Architecture

Data flows one way: `parser.rs` reads a Markdown file into `Document` (via `pulldown-cmark`), `app.rs` holds `AppState` which wraps the `Document` and owns all navigation/toggle logic, `ui/` renders `AppState` each frame, and `writer.rs` atomically writes checkbox state back to the file on every toggle.

Key non-obvious design points:

- **`[/]` started-state pre-processing** (`parser.rs`): `pulldown-cmark` only recognises `[ ]`/`[x]`. Before parsing, `extract_started_markers` rewrites `[/]` → `[ ]` (same byte length, so offsets are preserved) and records which lines had `[/]`; those items are promoted to `Started` in `End(Item)`.
- **Items are a flat `Vec`, sorted by `line_number`** after parsing. Nesting depth is metadata on each `Item`, not a tree. Items are pushed at `End(Item)` — post-order — so a sort is needed to restore document order.
- **File watcher watches the *parent directory***, not the file path, because atomic saves (temp-then-rename, used by both `write_back` and most editors) silently lose a file-path watch. Self-writes are filtered via `AppState.file_mtime`.
- **Color capability** is sniffed from `COLORTERM`/`TERM` into a `Palette` at startup; all `ui/` code reads from `state.palette` rather than hardcoding `Color::*`.
- **Card height** is computed fresh each frame from `desired_card_height` (measures wrapped body at actual render width), not stored in state.
- **`tests/pty.rs`** drives the compiled binary under a pseudo-terminal (`portable-pty`) to cover `main.rs` terminal wiring that can't be reached headlessly.

## README disclaimer

- `README.md` must always open with a prominent disclaimer, at the very top (right after the title), stating clearly that the code was created by AI (Claude), comes with no guarantees (correctness, security, fitness for any purpose), and may not have been reviewed by a human. If the README is rewritten or the disclaimer is removed, restore it. Keep it visually obvious (e.g. a bold blockquote with a warning marker), not a buried footnote.

## README Markdown reference

- `README.md` must contain a section documenting **all special Markdown handling** — every piece of syntax the app treats specially, how it renders, and the support level (e.g. `# H1` document title, `## H2` list/section, a leading bold-only first item = list banner, a task's leading bold = card title, `[ ]`/`[/]`/`[x]` task states, inline/fenced code and the trailing-command layout, plain-bullet note cards, and what is *not* supported such as flattened nested lists). It's the user-facing counterpart to `DESIGN.md`.
- Whenever you change how Markdown is **parsed** or how items/cards are **rendered** (new or changed special syntax, a new card element, changed display of an existing one), update this README section in the **same** piece of work — the same rule as keeping `DESIGN.md` in sync. Don't let it drift from the actual behavior.

## Example files

- The `examples/` directory holds sample Markdown checklists that demonstrate **every supported piece of Markdown handling** and double as manual-test fixtures. `examples/README.md` indexes them and says what each one shows.
- Whenever you change how Markdown is **parsed** or how items/cards are **rendered** (new or changed syntax, a new card element, a changed display), update the `examples/` files in the **same** piece of work so they still cover the feature — the same in-sync rule as `DESIGN.md` and the README Markdown section. Add a new example (or extend an existing one) for genuinely new syntax; keep the content generic (per Test Data & Privacy).
- When you need a checklist to **test, verify, or reproduce** something, reach for an `examples/` file first instead of generating a throwaway file on the fly. If none of them fits the case, that usually means the examples are missing something — extend them (per the sync rule above) rather than making a one-off. It's fine to copy an example into the scratchpad and modify the copy for a specific test.

## Keybindings

- The TUI's keyboard shortcuts are **vim-like**, and should stay that way. When adding or changing a binding, match vim conventions wherever it makes sense (`h`/`j`/`k`/`l` motion, `gg`/`G` for first/last, `y` to yank/copy, capitals for the bigger jumps, `?` for help), and when proposing options to the user, **lead with the vim-idiomatic choice**.
- Keep the always-on status-bar legend trimmed to the essentials; the `?` help overlay is the full reference and must list every binding. Update both (and the DESIGN.md / README keybinding tables) in the same work whenever a binding changes.

## UI Feedback

- **Always think about and implement good user feedback for every action.** When adding or changing any user-facing behavior, ask "how does the user know what just happened?" and build the answer in as part of the same work — never leave an action silent.
- A good action-feedback pass covers: **confirmation** that the action happened (e.g. "Copied to clipboard"), the **current state / input** where relevant (e.g. the live search query), **quantitative feedback** where it helps (counts, positions, progress — e.g. "5 matches", "Match 2/5"), and an explicit, visually distinct **empty / nothing-happened / failure** state (e.g. "No matches", "Nothing to copy") rather than a silent no-op that looks broken.
- Match the existing feedback machinery: the status bar (`src/ui/statusbar.rs`), ephemeral vs. sticky status messages (`set_status` for passive confirmations that auto-expire, `set_error` for failures or "nothing happened" feedback that must not vanish before being read), and error-colored text for problems. Prefer reusing these over inventing new channels.

## Workflow

- Commits to `dev` or any other in-progress branch do not need confirmation before committing — proceed directly once the change is ready. After the commit completes, show a summary of what changed and why. If the work produces multiple commits, commit each as its own concern per the git conventions below, then show one summary covering the full set once they are all done.
- Before committing any `.md` file, run `markdownlint <file>` and fix all findings. The file must pass with no errors before it is committed.
- Disable MD013 (line-length) in `.markdownlint.json` — prose and instruction files can't reasonably be wrapped at 80 characters. Once set, don't remove it or attempt to reformat files to satisfy it.
- If the project generates a review report (e.g. a code-review agent instruction that writes `REVIEW.md`), leave that file intentionally untracked (not gitignored) in the repo root. Its presence signals that a review has been generated and needs to be processed. Never add it to `.gitignore`.
- Before running a prompt that generates `REVIEW.md` (e.g. a deep-code-review or an agent-instructions audit prompt), check whether an untracked `REVIEW.md` already exists in the repo root. If it does, ask whether it has already been processed — its presence signals unfinished review work per the rule above. If the user confirms it's been processed, proceed (the new prompt will overwrite it); otherwise stop and let the user decide how to handle the existing report before running the new prompt.
- When a large task wraps up, or context usage is running high (roughly 20% or more used), remind the user to run `/clear` to start the next task with a fresh context. Output the reminder as a `> [!WARNING]` markdown callout so it stands out from surrounding prose. This is a reminder only — `/clear` is a built-in CLI command, not something invocable through a tool, so it can never be run on the user's behalf.

## CI

If a project has no CI workflow configured yet, suggest setting one up — don't wait to be asked. This is a suggestion, not something to set up unprompted: propose it and let the user decide.

- Check `git remote -v` to see which host(s) the project actually uses. Suggest a workflow for each one present: GitHub Actions (`.github/workflows/`) for a `github.com` remote, Gitea Actions (`.gitea/workflows/`) for a self-hosted Gitea remote. If the project is pushed to both, suggest both — the two are close enough in syntax that a workflow's content is largely shareable between them as-is.
- The workflow must run every check the project actually enforces before a commit or change is considered clean — derive this from this file, not a generic template. For example: linters, formatters (run in check mode, never autofix mode, in CI), type checkers, test suites, and any build step. If the project has no automated checks at all yet, there's nothing to wire up yet either — say so instead of inventing checks that don't exist.
- If the user declines CI setup (not now, not wanted, whatever the reason), note that decision — and the date — in this file, then drop it for the rest of the conversation. In a later session, once this file carries that note, treat it as a standing prompt to ask again: whether real time has clearly passed, or the project has grown enough that the case for CI is stronger than when it was declined.
- A CI workflow is a second copy of "what needs to pass before this is clean" — the same failure mode as any other duplicated logic applies: whenever the project's tools or checks change (a new linter, a new required check, a build step added or removed), the workflow file(s) must change with it, in the same change that changed the tooling. Don't let this slip to a follow-up — treat an out-of-date workflow as a bug, the same way an unsynced snippet or a stale doc would be.

`.github/workflows/release.yml` builds and packages tagged releases; `.github/workflows/check.yml` runs `cargo fmt --check`/`clippy`/`cargo test`, then `cargo llvm-cov` to enforce the 97% coverage threshold, on every push and pull request. Keep both in sync with this file per the rule above.

## Rust

- Always target the latest stable Rust release, and verify the toolchain is current before committing. Check with `rustup check`.
- Only use a nightly toolchain, or a pre-release dependency version, if it is strictly necessary — confirm with the user first.
- Use the latest stable release versions of dependencies.
- For a program a user runs directly, use color and clear presentation — structured formatting, tables, progress/status indicators, etc. — for its terminal output when it makes sense: status messages that should stand out (success/warning/error), progress, or tabular data. Skip it for something too small to warrant it, e.g. a program whose entire output is a single line.
- Default to no comments in code — clear naming and control flow should carry the intent by themselves. Doc comments (`///`, `//!`), where the project requires them (e.g. via `#![warn(missing_docs)]`), are a separate, always-required matter, not part of this rule. Add a regular `//` comment only when the code is genuinely hard to follow without one: a non-obvious constraint, a workaround for a specific bug or quirk, or logic that isn't self-evident from reading it — in that case, add the comment rather than leaving a future reader to puzzle it out.
- Use `cargo fmt` for formatting and `cargo clippy` for linting. Both must pass with no errors before committing.
- Aim for high test coverage on every new feature or fix. This project's agreed threshold is **97%** region/line coverage (excluding `main.rs`, per the exclusion below) — keep coverage at or above it as code is added, rather than letting it slip. It's fine for genuinely untestable paths (e.g. something that can't run headless) to stay uncovered — note why in the project's issue tracker rather than forcing a brittle test.
- Measure coverage with `cargo llvm-cov` (requires the `cargo-llvm-cov` subcommand and the `llvm-tools-preview` rustup component):
  - `cargo llvm-cov --summary-only --ignore-filename-regex 'main\.rs'` — summary
  - `cargo llvm-cov --html --ignore-filename-regex 'main\.rs'` — line-by-line HTML report
  - Exclude `main.rs` (or an equivalent thin entry-point file) from coverage accounting if it's mostly wiring that's better exercised by integration tests than unit tests.
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
- List issues: `gh issue list`
- View a single issue: `gh issue view <number>`
- Create an issue: `gh issue create --title "..." --body "..."`
- Comment on an issue: `gh issue comment <number> --body "..."`
- Close an issue via `gh issue close <number>`, once it's actually fixed and the fix is committed (and pushed, if that's part of the workflow in play).
- Label an issue: `gh issue edit <number> --add-label "..."`
- Unlike some other issue-tracker CLIs, `gh`'s issue subcommands cover the full set of routine operations natively (list, view, create, comment, close, label) — there's no need to fall back to the API or the web UI for any of these.
- Before starting substantial work, check open issues (`gh issue list`) so nothing gets duplicated or missed.

## Test Data & Privacy

Applies to every file in the repository — not just test fixtures, but source code, comments, commit messages, and the repo's own documentation (`CLAUDE.md`, `README.md`, any design doc, etc).

Real content must never be committed, full stop — no confirmation step, no exception, no "just this once." Always use placeholders or made-up content instead. This is the strict counterpart to a confirm-first policy (one that allows a real value once the user has explicitly signed off on that specific use) — here, real data leaking into the repository at all is the failure mode to guard against, not just an unconfirmed one.

- Never write a real person's name anywhere in the repository. Use a generic placeholder instead (e.g. "the user", "example-user").
- Never write a real hostname, URL, IP address, command, or any other identifying detail. Use a made-up placeholder instead (e.g. `example-host`, `example.com`, or a documentation-range address like `203.0.113.10` — never a real one).
- This applies even when writing a rule *about* real data: cite a placeholder, never an actual value, even as an illustrative example.
- If the user supplies a real, non-generic example (a real hostname, a real command, a real name) to illustrate a request, generalize it into a placeholder before writing it down anywhere in the repository — never commit it verbatim, no matter who provided it or why.
- There is no confirm-and-proceed path here: if something might be real data, treat it as real data and replace it with a placeholder. When in doubt, default to a placeholder rather than asking whether the real value is fine to use.

## Git

General git conventions for this project: branching, history rewriting, merging to `master`, and commit formatting.

### Branching

- Default to the `dev` branch for all work, unless a different branch has already been checked out or explicitly set for the current task — in that case, keep working on that branch instead of switching to `dev`.
- Never commit, amend, rebase onto, or push directly to `master`, and never push `dev` to trigger anything master-facing on your own initiative. `master` only moves via an explicit user-requested merge (see "Merging to master" below).
- Before starting work on a tracked issue or a larger/multi-commit piece of work, ask whether to create and switch to a new topic branch for it, rather than committing straight to `dev`.
- When the work is tied to a tracked GitHub issue, name the topic branch with that issue's identifier: `issue-<number>-<short-kebab-case-description>` (e.g. `issue-123-fix-broken-parser`). This mirrors the `(#N)` suffix used in commit titles, so the branch, its commits, and the issue are all traceable to each other.
- Keep `dev` linear on top of `master`: rebase `dev` onto `master` (never merge `master` into `dev`), so `dev` stays a fast-forwardable descendant of `master`.

#### History rewriting

- History rewriting — amending, rebasing, force-pushing — is allowed on `dev` (and topic branches based on it), but only for commits not yet merged into `master`, and only as long as `dev` stays linear on top of `master` per the rule above. This overrides the general "always create new commits, never amend/force-push" default, but *only* for `dev`/topic branches — never rewrite `master` history.
- Aim to keep `dev`'s commit count low. Whenever a later commit would touch the same change as one already on `dev` and not yet merged to `master` — most often a fix or refinement found via testing or review of something just committed — rewrite the earlier commit(s) instead of stacking a new one on top: `git commit --amend` for the tip commit, or a soft-reset-and-recommit for an earlier one (Claude Code's tooling disallows interactive git flags, so `git rebase -i` isn't an option). Do not carry multiple commits into a merge to `master` that are really just successive fixes or changes to the same not-yet-merged work — combine them into the commit(s) they fix before merging. This is about the same change accruing fixes over time, not about splitting distinct work: the one-commit-per-concern rule under Commits still applies to genuinely separate concerns. Keep each commit correct and self-contained, since it hasn't shipped yet; re-run lint and push with `--force-with-lease` afterward per the rule below. Once a commit is merged to `master`, this no longer applies — fix it forward with a new commit as usual.
- A topic branch gets the same treatment relative to `dev` that `dev` gets relative to `master`: rewrite its commits — amend, or soft-reset and recommit — to fold in fixes and refinements found while working on it, rather than stacking new fix-up commits on top. Clean it up before merging into `dev`, the same way `dev` gets cleaned up before merging into `master`, so what lands on `dev` is already tidy rather than a commit plus a trail of its own fixes. This is not optional — see "Mandatory pre-merge history check" below, which applies to this merge exactly as much as it applies to merging `dev` into `master`.
- Always push `dev` after committing or rewriting its history, so the remote never lags local. The same applies to a topic branch: push it after every commit made on it, not only once it's ready to merge, so it's never sitting local-only. When history was rewritten (amend/rebase), push with `--force-with-lease` (never a bare `--force`), so the push fails safely instead of clobbering anything unexpectedly added to the remote branch since the last fetch.

#### Mandatory pre-merge history check

**Before merging `dev` into `master`, or a topic branch into `dev`, the commit history being merged MUST be checked and rewritten if needed. This is not optional cleanup, not a judgment call, and not something to skip because the commits "look fine" — it is a required gate, every single time, with no exceptions. This has been missed before, and a missed check means every messy in-between commit ships permanently, since the destination branch's history is never rewritten after the fact.**

The check: walk every commit being merged (`git log --oneline master..dev`, or `master..<topic-branch>` for a topic branch) and compare each one against the History rewriting rules above. Ask, for every commit: *is this really just a fix, refinement, typo correction, or follow-up to an earlier not-yet-merged commit in the same range?* If so, it must not survive as its own commit — squash it into the commit it fixes (`git commit --amend` for the tip, or a soft-reset-and-recommit for an earlier one) before doing anything else. The goal is that what lands on the destination branch reads as if it had been written correctly the first time, not as a live recording of the back-and-forth it took to get there. This is about collapsing accrued fixes to the *same* change, not about squashing genuinely separate concerns into one commit — the one-commit-per-concern rule under Commits still applies.

Do this check — and any resulting rewrite plus `--force-with-lease` push — *before* presenting a merge summary to the user, not after. A merge summary should already describe the clean, final history, not a history that's about to be rewritten out from under it.

#### Merging to master

Merging to `master` happens only on explicit request, as a sequence:

1. Perform the mandatory pre-merge history check above. Do not proceed to the next step until `dev`'s history is already clean.
2. Present a summary of what's about to land — the commit range (`git log --oneline master..dev`) and a nutshell of what changed.
3. Get explicit confirmation on that summary. Asking for the merge and confirming its contents are two separate steps; don't collapse them just because the user already said "merge."
4. Once confirmed, fast-forward `master` to `dev`: `git merge --ff-only dev` — no merge commit, no squash, since `dev`'s history is already linear and clean. If there's no local `master` checkout to merge into, push `dev`'s tip directly to `master` on the remote instead.
5. If a true fast-forward isn't possible (something moved `master` independently), stop and ask rather than falling back to a merge commit or force-push.

Remind the user to merge when it seems due: when a large feature/fix on `dev` looks finished, or `dev` has accumulated a lot of commits ahead of `master`, say so and suggest merging. This is a reminder only — never merge to `master` automatically or without explicit confirmation, no matter how done the work looks or how many commits have piled up.

#### Branch cleanup

Once a topic branch has been merged into `dev`, delete it — both the local branch and its remote counterpart (`git branch -d <branch>`, then `git push <remote> --delete <branch>`) — no need to ask first. A merged branch is fully redundant the moment its commits live on `dev`; its only purpose was getting them there. `dev` and `master` themselves are never deleted — this applies only to topic branches.

### Commits

- Every commit Claude Code creates must end with a `Co-Authored-By:` trailer identifying the active model, e.g. `Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>`. Never omit this — if a commit is later found to be missing it and hasn't been merged to `master` yet, fix it via the history-rewriting rules above rather than leaving it out.
- One commit = one concern, not necessarily one file. Bundle files that share the same concern into a single commit — a source change routinely spans several files, and this applies to documentation too: if a single change is documented across more than one file (e.g. a design doc and `README.md` both describing the same change), commit them together rather than splitting per file. Keep genuinely separate concerns in separate commits even when they land together for the same task — don't bundle work together just because it happened at the same time if the concerns themselves are distinct.
- Prefix commit titles with the **scope** of the change, followed by a colon and an uppercase description. Match the prefix casing to the actual file or directory name. One file → its exact path (`fetch-data:`, `CLAUDE.md:`). Several files sharing one concern → the directory that contains them (`snippets:`, `src/ui:`). End the title with the issue number when the work is tied to a tracked issue: `(#12)`.
- Keep the title under 80 characters. Only exceed this if absolutely unavoidable.
- Put detailed descriptions in the commit body, not the title. For a commit spanning several files, use a one- or two-line prose summary followed by per-file bullets (`- bin/fetch-data: ...`) saying what each file's part does.
- Examples: `fetch-data: Add retry prompt`, `CLAUDE.md: Add indentation rule`
- When adding a new file with no meaningful description, use `filename: Add file`. If there is a reason worth stating, describe it instead: `fetch-data: Add script to fetch remote data on a schedule`
- Never modify `.gitignore` files without explicit confirmation from the user.
- Before every commit, all lint/format/type checks required by this file must pass with zero errors. Never commit with outstanding failures — fix them, or ask the user how to handle a check that genuinely should be skipped.
