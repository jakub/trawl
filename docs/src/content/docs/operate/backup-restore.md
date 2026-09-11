---
title: Back up and restore
description: Take a coordinated offline copy of the corpus and both databases, then rehearse recovery on an isolated host.
---

Back up the data filesystem and the dedicated Trawl database together. The
catalog's database identity must agree with `data/CATALOG`; saved report metadata
also refers to files in the data tree. A Parquet-only copy is an export, not a
complete server backup. The Fleet keystore and browser session material preserve
access, but contain sensitive data and need the same protection as credentials.

## Scope and prerequisites

This is a bounded offline procedure for a packaged host with:

- state at `/var/lib/trawl`, data at `/var/lib/trawl/data`, and WAL beneath that
  data root;
- configuration at `/etc/trawl` and environment files at `/etc/default/trawld`
  and `/etc/default/trawl-web`;
- a single-owner `trawl` database and a single-owner `fleet` database, without
  custom database ACLs that need separate restoration;
- the same Trawl version, PostgreSQL-compatible dump tools, service user IDs,
  and filesystem layout on the recovery host.

For an external WAL, external TLS/session-key files, additional config paths,
custom grants, or Kubernetes PVCs and Secrets, extend and rehearse the inventory
before using it. Do not silently omit them. Kubernetes storage snapshots need the
same stopped-writer boundary and separate database recovery points; a PVC snapshot
alone is insufficient.

Use a separate recovery host and separate PostgreSQL databases for the drill.
Do not restore over a live server. A shared Fleet keystore requires coordination
with every writer, including sibling applications, administrators, and migrations.
Restoring that keystore can resurrect revoked keys or undo role changes. Replacing
it in production requires an access-state reconciliation for every participating
app; this Trawl-only drill does not perform that cutover.

Provision libpq service names `trawl_backup` and `fleet_backup` for the selected
source databases and `trawl_restore` and `fleet_restore` for the isolated targets.
Store passwords in protected libpq credential files, not commands. Inspect service
host/database settings privately. A service name is not proof of the target.
Commands below run as the host administrator, with shell tracing disabled.

## Establish the recovery point

1. Record versions, configuration paths, database owners, filesystem mounts,
   service user IDs, and the selected database service names in a private manifest.
   Record the event time range and saved report IDs used for later validation.
2. Pause producers or route them to verified buffers. Stop external queries,
   scheduled administration, and all other Fleet writers for the backup window.
   UDP syslog senders need their own outage plan; they cannot rely on retries.
3. Inspect `schema repin-status`. Wait for a terminal job and correlate its ID.
   Do not begin this routine backup with an active repin or incomplete recovery.
4. Stop both Trawl services and verify they are inactive. Check no separately
   launched daemon, maintenance command, or sibling Fleet writer remains.

```bash
sudo systemctl stop trawl-web trawld
sudo systemctl is-active trawl-web trawld
```

`is-active` must report both inactive; its nonzero exit is expected here. Inspect
failure or unknown output instead of treating every nonzero exit as success.
Confirm no `data/REPIN`, `data.repin-next`, or `data.repin-aside` exists. If any
exists, resolve normal recovery before this procedure, rather than deleting it.
Do not run trawld merely to flush WAL after the stop: preserve the WAL as part of
the backup. A clean shutdown can still leave WAL awaiting compaction.

Both database dumps are taken while every relevant writer remains stopped, so
separate dump transactions describe the same unchanged application state. A
live `pg_dump` followed by a live filesystem copy does not provide this guarantee.

## Create the backup

Set `TRAWL_BACKUP_DIR` to a new directory on protected backup storage outside
`/var/lib/trawl`, with room for both database dumps and the full state tree.
Use encrypted backup storage according to the host's existing backup policy.
Do not share this directory as a public artifact.

```bash
(
  set -euo pipefail
  umask 077
  : "${TRAWL_BACKUP_DIR:?set a new absolute backup directory}"
  test "${TRAWL_BACKUP_DIR#/}" != "$TRAWL_BACKUP_DIR"
  case "$TRAWL_BACKUP_DIR" in /var/lib/trawl|/var/lib/trawl/*) exit 1 ;; esac
  test ! -e "$TRAWL_BACKUP_DIR"
  mkdir -m 0700 -- "$TRAWL_BACKUP_DIR"
  test ! -e /var/lib/trawl/data/REPIN
  test ! -e /var/lib/trawl/data.repin-next
  test ! -e /var/lib/trawl/data.repin-aside
  pg_dump --dbname=service=trawl_backup --format=custom --no-owner --no-acl \
    --file="$TRAWL_BACKUP_DIR/trawl.dump"
  pg_dump --dbname=service=fleet_backup --format=custom --no-owner --no-acl \
    --file="$TRAWL_BACKUP_DIR/fleet.dump"
  sudo tar --acls --xattrs --numeric-owner --exclude=var/lib/trawl/cores \
    -C / -cpf - \
    var/lib/trawl etc/trawl etc/default/trawld etc/default/trawl-web \
    > "$TRAWL_BACKUP_DIR/files.tar"
  pg_restore --list "$TRAWL_BACKUP_DIR/trawl.dump" > "$TRAWL_BACKUP_DIR/trawl.contents"
  pg_restore --list "$TRAWL_BACKUP_DIR/fleet.dump" > "$TRAWL_BACKUP_DIR/fleet.contents"
  (cd "$TRAWL_BACKUP_DIR" && sha256sum trawl.dump fleet.dump files.tar > SHA256SUMS)
)
```

If a listed config file is absent, identify whether the layout is actually in
scope before editing the inventory and rerunning into a new backup directory.
Do not label a partially written backup complete. `files.tar` preserves the
whole state tree, including WAL, scheduled results, `EPOCH`, `CATALOG`, TLS files,
session material, and any repin recovery state. Crash dumps are excluded because they
are a separate sensitive diagnostic store; archive them separately if needed.
The archive may still contain query-debug data or credentials, so keep it private.

Add the private manifest and matching artifact identifiers. Confirm all commands
succeeded and the backup filesystem's writes completed under its storage policy.
Checksums detect later alteration, not a coherent or readable application state.
Resume source services and producers only after the backup is complete; keep
Fleet administrative writers paused until the final dump has finished.

## Restore into an isolated target

Keep all target services stopped, producers blocked, and scheduled work disabled
until validation is complete. Install the recorded Trawl version without allowing
it to connect to production databases or accept production traffic. Preserve the
failed target separately if this is recovery from an incident.

Create empty target databases owned by the intended login roles. The restore
service names must connect as those owners. Use new protected credentials if
needed, then update the restored configuration to those credentials before boot.
The `--no-owner --no-acl` procedure restores objects as the connecting owner; it
is intentionally limited to the single-owner arrangement described above.

```bash
(
  set -euo pipefail
  umask 077
  : "${TRAWL_BACKUP_DIR:?select the verified backup}"
  : "${TRAWL_RESTORE_STAGE:?set a new private staging directory}"
  test ! -e "$TRAWL_RESTORE_STAGE"
  (cd "$TRAWL_BACKUP_DIR" && sha256sum --check SHA256SUMS)
  mkdir -m 0700 -- "$TRAWL_RESTORE_STAGE"
  sudo tar --same-owner --acls --xattrs --numeric-owner --keep-old-files \
    -C "$TRAWL_RESTORE_STAGE" -xpf "$TRAWL_BACKUP_DIR/files.tar"
  pg_restore --exit-on-error --single-transaction --no-owner --no-acl \
    --dbname=service=fleet_restore "$TRAWL_BACKUP_DIR/fleet.dump"
  pg_restore --exit-on-error --single-transaction --no-owner --no-acl \
    --dbname=service=trawl_restore "$TRAWL_BACKUP_DIR/trawl.dump"
)
```

If a restore fails, keep the target stopped and create fresh empty databases
before retrying. Do not retry by layering a partial restore over an unknown state.
Verify service user IDs match the manifest before installing the filesystem copy.
On this fresh target, `/var/lib/trawl/data` and all its repin siblings
must be absent. Replace only the packaged state/configuration paths listed below;
never overlay a pre-existing corpus. The package-created cookie is replaced by the
backed-up cookie so the drill tests the selected recovery point.

```bash
sudo cp -a "$TRAWL_RESTORE_STAGE/var/lib/trawl/." /var/lib/trawl/
sudo cp -a "$TRAWL_RESTORE_STAGE/etc/trawl/." /etc/trawl/
sudo cp -a "$TRAWL_RESTORE_STAGE/etc/default/trawld" /etc/default/trawld
sudo cp -a "$TRAWL_RESTORE_STAGE/etc/default/trawl-web" /etc/default/trawl-web
sudo systemd-tmpfiles --create trawl.conf
```

Privately update DSNs, addresses, TLS trust, browser origins, and any external
secret references to the isolated target. Keep the data path unchanged for this
drill. Set `[scheduler] enabled = false` before the first boot so recovery does not
immediately run saved schedules. Use network isolation to stop incoming ingest;
keep the intended ingest mode so normal catalog and WAL recovery runs.
Do not remove `EPOCH` or `CATALOG` to bypass a startup error.

## Validate before returning traffic

Before starting trawld, compare the restored `data/CATALOG` with:

```bash
psql service=trawl_restore -X -v ON_ERROR_STOP=1 -Atc \
  'SELECT catalog_id FROM catalog_state WHERE singleton'
```

If the backup contains a marker, it must equal the restored database value. A
missing marker is an unproven corpus, not permission to invent a matching file.
Investigate the original backup state and expect the documented conformance pass.

Start the isolated daemon, then validate all of the following:

1. Health checks, authenticated identity, and startup logs, including WAL replay,
   catalog conformance, and any retained recovery state.
2. Known historical queries with explicit time windows. Compare recorded counts
   and representative field values. Recent WAL-backed events should appear once.
   Health can pass before those events become queryable after restart. Allow
   eligible WAL files to compact, then require the expected records within a
   bounded wait. Do not resend events to fill a temporary query gap.
3. Catalog types and conflict evidence, including the expected catalog identity.
4. Saved-query definitions and existing report runs, including result-file access.
5. Browser login with the restored session configuration when the proxy is used.
6. A labeled test ingest and query, followed by another daemon restart and the same
   query, to check durability and absence of duplicate replay.

Only after this isolated drill succeeds should you plan a traffic cutover. Review
scheduled catch-up windows before re-enabling the scheduler. Reconcile key
revocations and role changes made since the backup before replacing a shared
Fleet keystore. Keep the backup and validation record until the recovered service
has passed its normal operating checks.
