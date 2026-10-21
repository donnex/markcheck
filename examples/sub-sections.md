# Release checklist

## Deploy

- [ ] `refresh-cache`
- [ ] `build-tool sync --profile default`

### Pre-flight checks

Run these on the staging host before touching production.

- [ ] confirm the staging host is idle
- [/] run `verify-output` and wait for it to finish

#### Data safety

- [ ] take a snapshot: `snapshot-tool create`
- [ ] confirm the snapshot is listed

### Rollout

<!-- markdownlint-disable MD022 -->
- [x] `restart-service`
- [ ] `check-status example-host`
- #### Watch the dashboards
- [ ] confirm the error rate is flat
<!-- markdownlint-enable MD022 -->

> ### Caution
>
> A quoted heading is a callout, not a sub-section.

- [ ] announce the rollout in the team channel

### Skipped when empty

### Post-rollout

- [ ] tail the logs for one minute
- [ ] `verify-output --final`

### Extended verification

#### Line-by-line checks

- [ ] verify service A responds on its health endpoint
- [ ] verify service B responds on its health endpoint
- [ ] verify service C responds on its health endpoint
- [ ] confirm queue depth is below the threshold
- [ ] confirm cache hit rate is nominal
- [ ] confirm error rate is within budget
- [ ] check disk usage on the primary node
- [ ] check disk usage on the replica node
- [ ] check memory headroom on each worker
- [ ] confirm scheduled jobs are enabled
- [ ] confirm overnight backups completed
- [ ] confirm alerting is armed
- [ ] review the latest deploy log for warnings
- [ ] spot-check a sample request end to end
- [ ] confirm metrics dashboards are populating
- [ ] sign off with the on-call engineer

## Second workspace

- [ ] `refresh-cache`
- [ ] notes page `workspace-notes.md`

## Handover

> Hand over to the next shift only once every group above is signed off.

```sh
for group in pre-flight verification; do
    handover-tool status "$group"   --all
done
```

<!-- markdownlint-disable MD022 -->
- ## Hand over in writing, never only by voice
- [ ] write the handover note
- ## Share the note
- [ ] post the note in the team channel
<!-- markdownlint-enable MD022 -->
