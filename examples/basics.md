# Deployment Runbook

A short checklist showing markcheck's core Markdown handling: the three task
states, a card title, inline and fenced commands, and a note card.

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

- [ ] Check the health endpoint `curl https://example.com/health`
- [ ] Tail the logs and watch for errors

## Ordered steps

The three task states work the same on an ordered (numbered) list as on a
plain bulleted one.

1. [x] Confirm the change window is open
2. [/] Notify the on-call channel
3. [ ] Announce completion
