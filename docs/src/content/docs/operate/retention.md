---
title: Manage retention
description: Set age limits per environment, respond to disk pressure, read headroom and pressure-deletion evidence, and clear suppressed retention.
---

trawld deletes date directories by age, per environment, and separately under
disk pressure. Disk pressure ignores age limits and can delete data from any
environment, including one set to keep forever. Today's directory is never
deleted. The [`[retention]` reference](/reference/configuration/#retention)
states the rules and defaults.

## Set age limits

1. Edit `[retention]` in `/etc/trawl/trawld.toml`. Keep the three scalars
   before the per-environment tables. In TOML, a scalar written after
   `[retention.env.prod]` belongs to that table:

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

   `max_age_days = 0` keeps that environment's data until disk pressure. An
   environment without a table uses the global value. On Helm, set
   `config.retention.maxAgeDays`, `config.retention.minFreeDiskBytes`,
   `config.retention.retentionIntervalSecs`, and `config.retention.envs`, a
   map of environment name to days.

2. Restart trawld:

   ```bash
   sudo systemctl restart trawld
   ```

   An unknown key in `[retention]` or in a per-environment table stops the
   start.

3. Read the retention lines in the journal:

   ```bash
   journalctl -u trawld -g retention
   ```

   `retention_start` echoes the loaded values. `retention_env_without_dir`
   names an entry whose environment has no directory under the data path,
   usually a misspelled name.

Removing an environment from `[ingest] envs` stops new events for it but
leaves its directories on disk. Keep its retention entry until they are gone.

## Free disk space

Every `retention_interval_secs` seconds, trawld compares free space on the
data filesystem with `min_free_disk_bytes`. Below it, trawld deletes the date
directory closest to its environment's age limit, measures again, and repeats
until free space is above the threshold. To keep young data, add space rather
than a longer age limit.

If free space stays below the threshold:

1. Read `trawl_retention_suppressed` on `/metrics`. A value of 1 means a repin
   marker or staging directory is pausing retention, or the publication
   markers under the WAL root cannot be read. Follow
   [Clear suppressed retention](#clear-suppressed-retention).
2. Compare the threshold with the filesystem: `df -h /var/lib/trawl`.
   Files outside the active data root consume space but are not retention
   candidates. Inspect those files separately if deleting expired partitions
   does not restore enough free space.

To see how many days of each policy the disk is projected to hold, read
[retention reach](/operate/health/#read-retention-reach).

## Read headroom

Headroom is the free space on each filesystem that trawld writes to. Read it
in the **Disk and retention** section of **Health**, in `capacity.headroom` of
the [dashboard snapshot](/reference/api/#capacity), or in
`trawl_disk_available_bytes` on [`/metrics`](/reference/api/#prometheus-metrics).
Health and the dashboard snapshot need a key with `server_manage`.

trawld measures the filesystem under each of three roles:

- `data`: the data root. Repin staging and compaction spill share its
  filesystem.
- `wal`: the live WAL directory, `ingest.wal_dir`. The role exists only when
  ingest is enabled.
- `spill`: the query engine's temporary directory, `TMPDIR` or `/tmp`.

Roles on the same device share one row, and the row lists each role it holds.
A `wal_dir` or temporary directory on a different device gets its own row.
trawld never adds available bytes across devices, because two directories on
one device draw on the same free space.

The row that holds `data` also reports the deletion floor,
`min_free_disk_bytes`:

- The floor and a deficit. The deficit is `floor − available` while available
  bytes are strictly below the floor, and 0 otherwise. At exactly the floor,
  the deficit is 0 and pressure deletion does not start.
- With `min_free_disk_bytes = 0`, the row says that pressure deletion is off,
  in place of a floor.

A `wal` or `spill` row that is on its own device shows total and available
bytes only. Retention deletes nothing there, so those rows have no floor.

One measurement reads every role, so all rows share its status and age. The
statuses are the ones the [dashboard reference](/reference/api/#dashboard-snapshot)
defines for storage:

- Before the first complete measurement, there are no rows. Until then trawld
  does not know which roles share a device.
- A failed measurement keeps the last complete rows, and their age keeps
  growing. If any one role cannot be read, the whole measurement fails.
- A measured 0 available bytes is a reading, not a missing value.

## Read pressure-deletion evidence

Retention records what it deleted as facts. Read the evidence in the **Disk
and retention** section of **Health**, in `capacity.pressure` of the dashboard
snapshot, or in the retention counters on `/metrics`.

- **Removals by trigger.** Each counts date directories that retention
  removed, with the removal confirmed by a successful delete. `age` counts
  directories older than their environment's `max_age_days`. `disk_pressure`
  counts directories removed because free space was below the floor. On
  `/metrics` the series is `trawl_retention_deletions_total{trigger}`.
- **Pressure attempts.** Each counts one sweep whose first free-space check
  found the data filesystem below the floor. A sweep counts once, however
  many directories it then deletes. A sweep that stands down before that
  check counts nothing. On `/metrics` the series is
  `trawl_retention_pressure_attempts_total`.
- **The last sweep.** Its outcome and how long ago it finished.
- **The oldest date per environment**, beside the environment's
  `max_age_days`.

The last sweep has one of four outcomes. When more than one applies, the
first one in this table wins:

| Outcome | Meaning | What to do |
| --- | --- | --- |
| `failed` | The sweep returned an error, its task panicked, or at least one date directory failed to delete. One failed deletion makes the whole sweep `failed`, even when every other deletion succeeded. | Search the journal for `retention_error`, which names the path and the error. Fix the permission or storage error. |
| `suppressed` | A repin marker or staging directory, or publication markers that cannot be read, stopped the sweep before it started or while it ran. | Follow [Clear suppressed retention](#clear-suppressed-retention). |
| `exhausted_below_floor` | Pressure deletion deleted every date directory it was allowed to delete, and free space is still below the floor. | Follow the steps in [Free disk space](#free-disk-space). Something other than deletable date directories holds the space. |
| `completed` | Every step ran, and every deletion that the sweep tried succeeded. A sweep that deleted nothing is also `completed`. | Nothing. |

Pressure deletion never deletes today's directory or a directory that a
pending publication marker claims. Those directories can remain after an
`exhausted_below_floor` sweep.

The counters and the last sweep start at process start. A restart sets the
counters to 0 and clears the last sweep. The journal keeps each removal as a
`retention_delete` line, with its `trigger`:

```bash
journalctl -u trawld -g retention_delete
```

The oldest date is the oldest date directory of the environment that holds
Parquet. A date directory with no Parquet in it does not count. Unlike the
counters, the oldest date survives a restart. trawld does not infer a cause
from it. An oldest date that is newer than `max_age_days` allows can come from
pressure deletion, but it can also come from a new install or a new
environment. Compare it with the `disk_pressure` count and the journal.

No bytes-freed figure is published. Before each deletion, retention adds up
the logical length of the files it can read and skips the rest. That sum
cannot prove how much space the filesystem got back. The `bytes_freed` field
on a `retention_delete` line is that sum, not released space. Read released
space from headroom.

## Expect a burst to cross the floor between sweeps

Retention checks free space once per sweep, every `retention_interval_secs`
(3600 seconds by default). Nothing checks the floor between two sweeps. The
floor does not reserve space and does not stop writes. If a source writes more
than the headroom above the floor within one interval, free space falls below
the floor, possibly to zero, before the next sweep starts to delete.

To leave room for a burst, set `min_free_disk_bytes` to more than the largest
amount of data you expect in one interval, or shorten
`retention_interval_secs`.

## Clear suppressed retention

Both sweeps pause while `data/REPIN`, `data.repin-next/`, or
`data.repin-aside/` exists. A repin keeps the affected files in two
generations until cleanup, and retention must not delete files under it.
Retention resumes on the tick after the job or its boot recovery finishes.

1. Check for a running repin:

   ```bash
   trawl -p prod schema repin-status
   ```

   If a job is running, wait for it. Its files use double the space until it
   completes.

2. If no job is running and the paths remain, search the journal for
   `repin_recovery_incomplete`. It names the permission or I/O failure that
   stopped cleanup. Fix the cause and restart trawld. Boot recovery retries
   the cleanup. Do not delete the marker or the staging directories yourself.
   The marker is what authorizes the cleanup.

3. After the next tick, confirm that `trawl_retention_suppressed` is 0 and
   that free space changed.

`trawl_catalog_repin_running` can be 0 while `trawl_retention_suppressed` is
1. An abandoned staging directory suppresses retention without a running job.
So does a WAL root that trawld cannot read: search the journal for
`retention_publication_claims_unreadable`, then correct the reported
permission or storage error.

## Dates kept for a pending publish

Compaction writes a publication marker before it publishes a parquet file
and removes it when the publish is complete. Retention keeps a date
directory while a marker names a file in it, and logs
`retention_publication_claimed` on each tick. The directory becomes a
candidate again once recovery resolves the marker. If it stays, follow
[Publication recovery blocked](/operate/operational-alerts/#publication-recovery-blocked).

[Back up and restore](/operate/backup-restore/) keeps the data directory and
the catalog together.
