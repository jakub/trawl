---
title: Manage retention and disk pressure
description: Set age limits and diagnose suppressed retention without deleting recovery state.
---

Age retention and disk-pressure deletion are separate policies. An age limit
is not a guaranteed minimum: disk pressure can delete younger data, including
an environment whose age limit is zero. Select the data filesystem and the
server configuration before changing either setting.

## Set age limits

```toml
[retention]
max_age_days = 90
min_free_disk_bytes = "1G"
retention_interval_secs = 3600

[retention.env.prod]
max_age_days = 365

[retention.env.lab]
max_age_days = 7
```

Put global scalars before the per-env tables. Check the env names against the
actual archive and sender configuration, then restart trawld in the planned
maintenance window to load the change. Watch for `retention_env_without_dir`,
which can identify a misspelled override. Removing an env from `ingest.envs`
blocks new writes but does not remove its old files or retention policy.
See [configuration values and validation](/reference/configuration/#retention).

## How disk pressure ranks envs

Under pressure, trawl ranks date directories by expiry ratio. A date directory's
ratio is its age divided by its env's effective `max_age_days`. The highest
ratio goes first; equal ratios break on the older date, then on the path.
With `prod` at 365 days and `lab` at 7, a 300-day prod directory sits at 0.82
and a 6-day lab directory at 0.86, so the sweep takes the lab directory and the
prod directory remains.

An env at `max_age_days = 0` ranks after everything that expires, but it is
still a candidate. An age limit is a maximum, not a guaranteed minimum. Under sustained pressure trawl deletes
data younger than any limit you configured, keep-forever envs included. If
that matters, give the archive more room rather than a longer age.

## The `/schema` horizon

`GET /api/v1/schema` hides a field whose most recent observation predates the
retention horizon, and `trawl schema gc-pins` uses the same number as its
floor. The horizon is the longest effective age across the install: the global
`max_age_days` against every override, whichever is largest. A `0` anywhere in
that set means no window at all, and `?all=true` lifts whatever window applies. This keeps the observation
window at least as long as the longest configured retention. Pin reclamation
also requires proof that no standing Parquet file declares the column.

## Diagnose suppression

Both sweeps pause while `data/REPIN`, `data.repin-next/`, or
`data.repin-aside/` exists. A repin keeps its affected files in both generations
until cleanup and checks `min_free_disk_bytes` before starting. Retention cannot
safely remove files from the corpus during that rewrite. It resumes on the tick
after the job or its boot recovery completes.

If a permission or I/O failure leaves a staging root behind, trawld retains the
marker needed to authorize its cleanup. The next boot retries cleanup. Until it
succeeds, the server reports `repin_recovery_incomplete` and retention stays paused.

Monitor `trawl_retention_suppressed`. It is 1 when a retention tick stands down and
0 when the sweeps run. A prolonged value of 1 needs investigation even if
`trawl_catalog_repin_running` is 0: an abandoned staging root can suppress
retention without a running job.

Either epoch archive, `data.pre-schema-v2/` or `data.pre-epoch-3/`, also suppresses
disk-pressure deletion. Those archives sit outside the live data root, so deleting
live partitions cannot reclaim their space. Each tick under pressure logs
`retention_disk_pressure_suppressed` with `set_aside_path`. Inspect all retained
archives before reclaiming space. Age-based retention is unaffected by these
epoch archives.

## Recover space

1. Inspect filesystem free space, the configured data path, and the path named by
   `retention_disk_pressure_suppressed` or `repin_recovery_incomplete`.
2. Read the running/newest repin job and correlate its ID. If it is active, plan
   for its affected bytes to remain doubled. Do not delete its marker or staging roots.
3. If recovery is incomplete, resolve the reported filesystem permission or I/O
   failure. Plan a restart so normal boot recovery can finish. Preserve all job state.
4. Treat epoch set-asides as historical archives. Verify and back them up before
   an explicit decision to archive them elsewhere or delete them. They may be the
   only copy of old events. Both epoch suffixes can exist at once.
5. On the next retention tick, verify suppression ended and free space changed.
   Do not infer success from removal of one directory while another still blocks it.

The [upgrade guide](/operate/upgrades/) explains epoch cutovers. The
[backup procedure](/operate/backup-restore/) keeps corpus and catalog together.
