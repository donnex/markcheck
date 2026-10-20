# Deployment Runbook

A short checklist showing markcheck's core Markdown handling: the three task
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
  starts with `!`, so `Space` asks for a second press before checking it
- [ ] Tail the logs and watch for errors; if they show a failure, go back to
  [Prepare](#prepare) (`o` follows the link, `Ctrl-O` comes back)
- [X] Record the start time — an uppercase `[X]` counts as done too
- [-] Restart the replica — skipped (`[-]`): there is no replica this time

## Ordered steps

The three task states work the same on an ordered (numbered) list as on a
plain bulleted one, and each card and overview row shows the step's number.

1. [x] Confirm the change window is open
2. [/] Notify the on-call channel
3. [ ] Announce completion
