# Service Runbook

Code blocks labelled as expected output or as a console transcript.

## Check

- [ ] **Check the service**

  ```sh
  systemctl status example-service
  ```

  ```text
  Active: active (running)
  ```

- [ ] **Apply the migration**

  ```console
  $ example-cli migrate --apply
  Applied 3 migrations.
  $ example-cli migrate --status
  Up to date.
  ```

- [ ] Confirm the version with `example-cli --version`

  ```output
  example-cli 2.4.1
  ```

- [ ] **Restart the workers**

  ```shell-session
  # example-cli workers restart
  Restarted 4 workers.
  ```

  ```sh
  example-cli workers list
  ```
