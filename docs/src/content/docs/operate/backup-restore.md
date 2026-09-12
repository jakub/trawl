---
title: Back up and restore
description: Take a consistent backup of the two databases and the state directory, restore it on another host, and verify the result.
---

A backup has three parts: the `trawl` database, the `fleet` database, and the
state directory. Take all three while `trawld` is stopped. The catalog in the
`trawl` database and the `data/CATALOG` marker must match, and report runs
refer to files under the state directory. A copy of the Parquet files alone is
an export, not a backup.

This page covers a Debian host with the packaged layout. On Kubernetes the
order is the same: scale the StatefulSet to zero, snapshot the PVC, and dump
both databases before you scale back up. A PVC snapshot alone is not a backup.

## What to back up

| Part | Contents | Tool |
| --- | --- | --- |
| `trawl` database | Field catalog, query history, saved queries, schedules, report run records | `pg_dump` |
| `fleet` database | API keys and roles, shared with other Fleet applications | `pg_dump` |
| `/var/lib/trawl` | `data/` with the Parquet files, `wal/`, `scheduled/`, `EPOCH`, `CATALOG`, and any repin state. `web.cookie`. `tls/` when trawld generated the certificate | `tar` |
| `/etc/trawl`, `/etc/default/trawld`, `/etc/default/trawl-web` | Configuration, DSNs, and environment | `tar` |

Leave out `/var/lib/trawl/cores`. A crash dump is a copy of process memory.
The `fleet` dump and `web.cookie` are credentials, so encrypt the backup and
keep it private.

## Create a backup

You need `pg_dump` at least as new as the PostgreSQL server, the two DSNs in
`TRAWL_DATABASE_URL` and `FLEET_DATABASE_URL`, and a backup directory outside
`/var/lib/trawl` with room for both dumps and the state directory.

1. Record a reference count for a fixed window. You compare against it after
   a restore:

   ```bash
   umask 077
   mkdir -m 0700 "/backup/trawl-$(date -I)" && cd "/backup/trawl-$(date -I)"
   trawl -p prod query 'earliest="2026-09-10T00:00:00Z" latest="2026-09-11T00:00:00Z" | stats count() by service' > reference.txt
   ```

2. Confirm that no repin is running. `trawl -p prod schema repin-status`
   reports no active job, and none of `data/REPIN`, `data.repin-next`, or
   `data.repin-aside` exists under `/var/lib/trawl`. If one does, finish
   [Clear suppressed retention](/operate/retention/#clear-suppressed-retention)
   first.

3. Stop both services and confirm they are inactive:

   ```bash
   sudo systemctl stop trawl-web trawld
   systemctl is-active trawl-web trawld
   ```

   Both lines read `inactive`. Ingest stops for the whole window: Vector
   buffers to disk, and UDP syslog senders lose events. Do not start trawld
   to flush the WAL. The WAL is part of the backup. Do not run `fleet-admin`
   or another Fleet application's migrations during the window either.

4. Dump both databases:

   ```bash
   pg_dump --dbname="$TRAWL_DATABASE_URL" --format=custom --no-owner --no-acl --file=trawl.dump
   pg_dump --dbname="$FLEET_DATABASE_URL" --format=custom --no-owner --no-acl --file=fleet.dump
   ```

5. Archive the state directory and the configuration, then record checksums:

   ```bash
   sudo tar --acls --xattrs --exclude=var/lib/trawl/cores -C / -cf files.tar \
     var/lib/trawl etc/trawl etc/default/trawld etc/default/trawl-web
   sha256sum trawl.dump fleet.dump files.tar > SHA256SUMS
   ```

6. Start the services:

   ```bash
   sudo systemctl start trawld trawl-web
   ```

## Restore on another host

You need the same Trawl package installed with both services stopped, empty
`trawl` and `fleet` databases owned by their login roles as in
[Provision the databases](/operate/deployment/#provision-the-databases), their
DSNs in `TRAWL_DATABASE_URL` and `FLEET_DATABASE_URL`, and no
`/var/lib/trawl/data` directory. Keep the senders away from the host until
the restore is verified.

Restoring the `fleet` dump into a database that other Fleet applications use
reverts every key and role change made since the backup. For a shared
database, restore the `trawl` dump only and reconcile keys by hand.

1. Verify the files:

   ```bash
   cd /backup/trawl-2026-09-11 && sha256sum --check SHA256SUMS
   ```

2. Restore both databases:

   ```bash
   pg_restore --exit-on-error --single-transaction --no-owner --no-acl \
     --dbname="$FLEET_DATABASE_URL" fleet.dump
   pg_restore --exit-on-error --single-transaction --no-owner --no-acl \
     --dbname="$TRAWL_DATABASE_URL" trawl.dump
   ```

   `--single-transaction` rolls a failed restore back. If one fails, fix the
   cause and run it again against the still-empty database.

3. Extract the files and reapply the packaged modes:

   ```bash
   sudo systemctl stop trawl-web trawld
   sudo tar --acls --xattrs -C / -xpf files.tar
   sudo systemd-tmpfiles --create trawl.conf
   ```

   The archive replaces the package-generated `web.cookie` with the backed-up
   key, so browser sessions and shared Fleet sessions keep working.

4. Edit the restored configuration for this host: the DSNs in
   `/etc/default/trawld`, `http_addr`, the TLS paths, and `public_origins`.
   Keep `[data] path` unchanged. Set `[scheduler] enabled = false` so restored
   schedules do not run before you have checked the result.

5. Compare the catalog marker with the database:

   ```bash
   sudo cat /var/lib/trawl/data/CATALOG
   psql "$TRAWL_DATABASE_URL" -Atc 'SELECT catalog_id FROM catalog_state'
   ```

   The two UUIDs must be equal. If they differ, the dump and the archive come
   from different backups. Do not edit the marker.

6. Start trawld, read its startup lines, then start trawl-web:

   ```bash
   sudo systemctl start trawld
   journalctl -u trawld -n 100
   sudo systemctl start trawl-web
   ```

## Verify the restore

1. Check health. Every check reads `ok`:

   ```bash
   curl --fail-with-body https://restore.example.com:5514/api/v1/health
   ```

2. Run the reference query from the backup directory and compare its output
   with `reference.txt`. Events that were still in the WAL become visible
   after the first compaction, within `[ingest] compaction_interval_secs`,
   10 seconds by default.

3. List the saved queries and report runs with `GET /api/v1/saved` and
   `GET /api/v1/runs`, or open them in the browser UI after you log in with a
   human key.

4. Send one test event as in [Send events over HTTP](/operate/ingestion/#send-events-over-http),
   restart trawld, and query it again. Exactly one row comes back.

5. Set `[scheduler] enabled = true`, restart trawld, and reconnect the
   senders. A schedule with missed runs catches up on at most
   `[scheduler] max_catchup_intervals` intervals.

Keep the backup until the restored server has passed its normal checks.
