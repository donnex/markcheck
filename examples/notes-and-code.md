# Notes, Banners, and Code

<!-- markdownlint-disable MD046 -- one task deliberately uses an indented code block, to show it renders like a fenced one -->

<!-- markdownlint-disable MD034 -- one task deliberately uses a bare URL, to show it is a link too -->

<!-- markdownlint-disable MD045 -- one task deliberately has an image without alt text, to show it still becomes a link -->

<!-- markdownlint-disable MD033 -- two tasks deliberately use inline HTML and an HTML block, to show they are dropped -->

<!-- markdownlint-disable MD010 -- one fenced block is deliberately tab-indented, to show tabs render as indentation -->

Non-task content and the ways code is handled.

## Reference

- **Read before proceeding: every step below assumes the service has already been drained from the load balancer**
- A note with a command but no checkbox — you can copy it with `y`, but it is
  not a task and cannot be checked off: `restart-service example-service`

- [ ] A task with a single inline command `apply-config --now`
- [ ] **Restart the primary application service and confirm that every health check passes** —
  a card title too long for one row wraps inside the card instead of being cut
- [ ] **Restart `example-service`** with `restart-service example-service` —
  code in the bold title is plain title text, so `y` copies only the body's
  command
- [ ] A task with a fenced block:

  ```sh
  run-migration --step 1
  run-migration --step 2
  ```

- [ ] A task with an indented code block, the older Markdown way to write a
  command — shown and copied just like a fenced one:

      restart-service example-service --graceful

- [ ] A task with a short one-line fenced block — its box still spans the full
  card width:

  ```sh
  sync-now
  ```

- [ ] A task with fenced lines too long for a narrow card — every row a line
  wraps onto ends in a dim `↩`, so one wrapped command can't be read as two:

  ```sh
  clear-cache --path /var/lib/example-app/cache/sessions --older-than 7d
  restart-service example-app --graceful --wait-for-health-checks
  ```

- [ ] A task with a tab-indented heredoc — the tabs show as indentation on
  the card, and `y` still copies the real tabs that `<<-` strips:

  ```sh
  cat <<-EOF > example.conf
  	listen 8080
  		workers 4
  	EOF
  ```

- [ ] A task with two commands `first` and `second` — each gets a `[1]`/`[2]`
  marker; `y` then `1` or `2` copies that one (or click it)
- [ ] A task with a command for each server — the boxes are numbered in their
  top borders for `y` then a digit:

  ```sh
  on-primary --promote
  ```

  ```sh
  on-replica --follow
  ```

- [ ] Fetch `curl -fsSL https://example.com/install.sh --output /tmp/install.sh`
  first — on a narrow card this long inline command wraps across rows, each
  continued row ending in a dim `↩`, and a click on any of them copies it whole
- [ ] A task with *emphasis*, mid-line **bold**, and ~~struck-through~~ text to
  show inline styling in the card body
- [ ] A task whose text runs on into a second paragraph

  Then carries on here — the two paragraphs read as one flowing body, with a
  space between them rather than the last and first words run together
- [ ] A task in wide (two-column) characters wraps by width instead of running
  off the card: 本番環境に適用する前に、ステージング環境で変更内容をすべて確認し、ログにエラーが出ていないことを確かめてください。
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
- [ ] [**Status board**](https://example.com/status-board) first — a link
  whose text is bold stays a link even at the start of a task, not the card
  title, so it keeps its `[1]` marker beside the [second link](https://example.com/help)
- [ ] See also every service dashboard: [api](https://example.com/d/api),
  [web](https://example.com/d/web), [db](https://example.com/d/db),
  [cache](https://example.com/d/cache), [queue](https://example.com/d/queue),
  [search](https://example.com/d/search), [auth](https://example.com/d/auth),
  [mail](https://example.com/d/mail), [cdn](https://example.com/d/cdn),
  [dns](https://example.com/d/dns), [logs](https://example.com/d/logs) and
  [metrics](https://example.com/d/metrics) — with twelve links, `o` `1` `2`
  opens `[12]`, `o` `1` then `Enter` opens `[1]`, and `o` `3` opens `[3]` at
  once since no longer number starts with 3
- [ ] A task linking to [a local file](file:///etc/hosts) — only `http://`,
  `https://` and `mailto:` links open, so `o` refuses this one and says so,
  even though it still renders like any other link
- [ ] Check the dashboard at https://example.com/status. A bare URL is a link
  too, as on GitHub, so `o` opens it; the trailing full stop is left out
- [ ] Check the mirror at www.example.com/mirror, a `www.` bare URL that `o`
  opens as `http://`, and the uptime page <https://example.com/uptime>, an
  autolink in angle brackets — both are links like any other
- [ ] Compare the dashboard with ![expected dashboard](https://example.com/dashboard.png)
  — an image shows as an `image: <alt text>` link, so `o` opens the picture;
  one without alt text, ![](https://example.com/screenshot.png), shows as
  `image`
- [ ] Press <kbd>Enter</kbd> at the prompt — inline HTML tags are dropped but
  the text between them is kept, and a dropped tag still counts as a space:
  line one<br>line two
- [ ] A task with an HTML block under it — the block is not shown on the card:

  <div class="note">
  This HTML block is dropped from the card.
  </div>

- [ ] Mail the [on-call owner](mailto:oncall@example.com) with `o` — `mailto:`
  is a safe scheme, so this one opens in your mail client

> - [ ] A quoted example from another runbook, shown for reference only — a
>   blockquoted checkbox never becomes a live task
> - [/] Neither does a quoted "started" marker — still just reference text

- ### [-] marks a skipped step

  A `[-]` after a heading's `#` is not a task marker, so this note card keeps
  it as written instead of showing an open checkbox

## Precondition with a command

- **Export the environment first:**

  ```sh
  export APP_ENV=staging
  ```

- [ ] Run the step that needs the exported environment

## Fence after a list

A fenced block placed directly after a list, at the left margin, belongs to
the list's last card.

- [ ] Confirm the release notes are published
- [ ] Run the smoke test — its command is the fenced block below the list

```sh
smoke-test --all
```

## Quoted callout

> [!WARNING]
>
> ### Before you deploy
>
> Take a snapshot of the volume first:
>
> ```sh
> snap-tool create example-volume
> ```
>
> - wait until the snapshot shows as complete
> - then start the deploy

- [ ] Deploy the new release

## Markers from other tools

Only `[ ]`, `[x]`, `[/]` and `[-]` start a task. The other two items open
with markers another task tool uses, so they show as notes and the startup
note counts them.

- [ ] Stop the worker queue
- [>] Run the data migration (deferred in another tool)
- [?] Check whether the cache needs warming

## Notes only (this section is dropped)

- This section has no checkboxes, so markcheck drops it entirely.
- Its bullets are counted in the startup note of what isn't shown.
- It appears in the file but not as a tab or in the overview.
