# Nested Checklist

Sub-lists under a task and under a plain note, at several depths. Each
sub-item is its own card and shows its parent chain as a breadcrumb.
On a conditional branch that doesn't apply this run, press `-` on its
parent to skip every sub-task at once, and `-` there again to reopen them.

## Rebuild

- [ ] Rebuild all packages
  - [ ] Fetch sources `fetch-sources --all`
  - [/] Compile
    - [ ] Compile the core
    - [ ] Compile the plugins
  - [ ] Run the test suite
- Preparation notes:
  - [ ] Free up disk space
  - [ ] Notify the team on `example.com`

## Release

- [ ] Tag the release
- [ ] Publish
  - [ ] Upload the artifact
  - [ ] Announce the release
- If publishing to the mirror as well:
  - [ ] Sync the mirror
  - [ ] Verify the mirror checksum
