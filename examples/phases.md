<!-- markdownlint-disable-file MD025 -->

# Service Upgrade

A runbook split into phases with `#` headings, as a word processor's
"Heading 1" exports. The first `# H1` is the title; each later one starts a
list of its own, the same as a `## H2`.

- [ ] Announce the maintenance window
- [ ] Take a snapshot: `snapshot-tool create example-host`

# Upgrade

- [ ] Stop the service: `service-ctl stop example-service`
- [ ] Install the new release
- [ ] Start the service: `service-ctl start example-service`

# Verify

## Health

- [ ] Check the health endpoint
- [ ] Watch the error rate for ten minutes

# Rollback

- **Only if the checks above failed**
- [ ] Restore the snapshot
- [ ] Start the old release
