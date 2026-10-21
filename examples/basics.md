# Deployment Runbook

A short checklist showing markcheck's core Markdown handling: the four task
states, a card title (with or without a body), inline and fenced commands, and
a note card.

## Prepare

- **Do not run against production**
- [x] Confirm access to `example-host`
- [/] Warm the build cache
- [ ] Check out the release branch: `git checkout release`
- [ ] **Build the artifact** compile and package the release:

  ```sh
  build-tool package --profile release
  ```

- Keep this runbook open while you work.

## Verify

> [!WARNING]
> Run these only once the deploy has finished; a failed check means roll back.

- [ ] Check the health endpoint `curl https://example.com/health`
- [ ] **Run the smoke tests**
- [ ] **! Switch traffic to the new release** — a critical step: its title
  starts with `!`, so `Space` (or `-`) asks for a second, unhurried press
  before checking (or skipping) it
- [ ] Tail the logs and watch for errors; if they show a failure,
  [roll back](#roll-back) or go back to [Prepare](#prepare) (`o` follows a
  link, `Ctrl-O` comes back)
- [X] Record the start time — an uppercase `[X]` counts as done too
- [-] Restart the replica — skipped (`[-]`): there is no replica this time

- ## Roll back

  A heading inside a list item makes a titled note card, and a link to its
  anchor jumps straight to this card.

## Ordered steps

Task states work the same on an ordered (numbered) list as on a
plain bulleted one, and each card and overview row shows the step's number.

> [!IMPORTANT]
> Announce completion only after the change window has closed.

1. [x] Confirm the change window is open
2. [/] Notify the on-call channel
3. [ ] Announce completion

## Wrap-up

> [!CAUTION]
> Keep the old release running until every step below is done.

1) [ ] Remove the old release
2) [ ] Close the change window
