---
title: Plan upgrades and cutovers
description: Check storage epochs, database migrations, session continuity, and rollback boundaries.
---

Start with the installed version, target version, artifact architecture, and a
verified backup. Do not run historical cutover steps merely because this is an
upgrade. Current package and chart configuration remains the starting point.

## Before changing the version

1. Record the daemon, proxy, chart, and database schema versions. Read release
   changes and migrations between the installed and requested versions.
2. Inspect the data root, configured WAL path, existing epoch archives, and the
   running/newest repin job. Finish or resolve an active job before maintenance.
3. Plan collector buffering or downtime. Native UDP syslog has no delivery
   acknowledgement; stopping the listener can lose incoming frames.
4. Take and rehearse a [coordinated offline backup](/operate/backup-restore/).
   Keep the matching binaries and configuration with its manifest.
5. Render the selected chart with complete values, or inspect package conffile
   differences and local systemd drop-ins. Preserve persistent session material.

## Storage epoch 3

The current storage epoch is 3. On an ingest-enabled node, a populated epoch-2
root is set aside as a sibling ending `.pre-epoch-3` and a new root is prepared.
A query-only node does not own that data and warns instead of renaming it.
An existing set-aside can prevent a second cutover; do not remove it to make a
startup error disappear without first checking its contents and backup.

Older markerless legacy layouts use a different archive suffix,
`.pre-schema-v2`. Both may coexist. They are outside ordinary live-data searches
and can suppress disk-pressure deletion. Age retention continues on live data.
See [retention recovery](/operate/retention/#recover-space).

The catalog identity has two halves: `data/CATALOG` and the app-state database's
`catalog_state.catalog_id`. Restore them together. Removing the file does not
repair a mismatched backup; it can re-arm an in-place conformance pass. A query-only
node refuses a marker that names a different catalog. Unreadable or unowned files
can leave a conformance pass incomplete, so inspect its logs before declaring
startup verified.

## Historical authentication changes

Use the [Fleet-auth cutover runbook](/reference/fleet-auth-cutover/) only when the
source deployment still uses the removed SQLite keystore/app-state path or the
older static grant schema. That transition does not import old SQLite state.
A normal upgrade of an already migrated Fleet deployment does not require
reminting every key or recreating every schedule.

Fleet migration runs through `fleet-admin`; app-state migration runs through
trawld. Both databases and the corpus need a coherent recovery point. A downgrade
of binaries alone is not a general rollback after schema migration or epoch cutover.

## Apply and validate

Follow the selected [deployment procedure](/operate/deployment/). Verify startup
and dependency checks, effective identity, a known historical query, recent ingest,
and browser login when applicable. Inspect saved schedules and their report runs;
a service being active is not proof that scheduled work resumed correctly.

If validation fails, keep collectors contained and preserve the failed state for
diagnosis. Choose forward repair or restore the complete coordinated backup with
its matching version into an empty, isolated target. Verify it before switching
clients back. Do not overlay old files onto a running or partially upgraded corpus.
