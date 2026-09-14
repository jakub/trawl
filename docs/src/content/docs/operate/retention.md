---
title: Manage retention
description: Set age limits per environment, respond to disk pressure, and clear suppressed retention.
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
   marker or staging directory is pausing retention. Follow
   [Clear suppressed retention](#clear-suppressed-retention).
2. Compare the threshold with the filesystem: `df -h /var/lib/trawl`.
   Files outside the active data root consume space but are not retention
   candidates. Inspect those files separately if deleting expired partitions
   does not restore enough free space.

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

[Back up and restore](/operate/backup-restore/) keeps the data directory and
the catalog together.
