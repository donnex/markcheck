# Notes, Banners, and Code

Non-task content and the ways code is handled.

## Reference

- **Read before proceeding**
- A note with a command but no checkbox — you can copy it with `y`, but it is
  not a task and cannot be checked off: `restart-service example-service`
> - [ ] A quoted example from another runbook, shown for reference only — a
>   blockquoted checkbox never becomes a live task
> - [/] Neither does a quoted "started" marker — still just reference text
- [ ] A task with a single inline command `apply-config --now`
- [ ] A task with a fenced block:
  ```
  run-migration --step 1
  run-migration --step 2
  ```
- [ ] A task with a short one-line fenced block — its box still spans the full
  card width:
  ```
  sync-now
  ```
- [ ] A task with two commands `first` and `second` — ambiguous for `y`, so
  click the specific command you want to copy
- [ ] A task with *emphasis*, mid-line **bold**, and ~~struck-through~~ text to
  show inline styling in the card body
- [ ] Open the [project runbook](https://example.com/runbook) with `o` — the
  link shows as underlined text on the card, its URL appears centered below the
  card, and `o` launches it in your browser
- [ ] A task with a very long link to the
  [deploy checklist](https://example.com/teams/platform/runbooks/deploy/checklist?step=rollback&ref=main)
  — the full URL used to wrap awkwardly across card rows; now it sits below the
  card, so the card stays clean no matter how narrow it is
- [ ] A task with two links — the [docs](https://example.com/docs) and the
  [wiki](https://example.com/wiki) — each link text is tagged with a `[1]`/`[2]`
  marker that matches the numbered URLs listed below the card; press `o` then
  the number (`o` `2`) to open a specific one
- [ ] A task linking to [a local file](file:///etc/hosts) — only `http://`,
  `https://` and `mailto:` links open, so `o` refuses this one and says so,
  even though it still renders like any other link
- [ ] Mail the [on-call owner](mailto:oncall@example.com) with `o` — `mailto:`
  is a safe scheme, so this one opens in your mail client

## Notes only (this section is dropped)

- This section has no checkboxes, so markcheck drops it entirely.
- It appears in the file but not as a tab or in the overview.
