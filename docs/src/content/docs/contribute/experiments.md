---
title: Run a full-app experiment
description: Exercise real Trawl daemons, authentication, ingestion, and Chromium in disposable infrastructure.
---

`bin/app-experiment` builds this checkout, prepares an isolated Trawl
installation, sends seeded synthetic events, and checks the application through
Chromium and the HTTP API. It owns its PostgreSQL container, keys, TLS files,
daemons, and browser. It needs no existing Trawl server and no development data.

## Run the default scenario

Run it from the worktree that holds the change you want to check:

```bash
bin/app-experiment --seed 42 --events 1000 --rate 200
```

The runner prints an artifact directory and a browser URL, then checks login,
queries, HTTP ingest, live results, compaction, and restart behavior. A
successful run exits `0` and writes `status: passed` to that run's
`report.json`, with every cleanup flag true. A printed URL only means startup
worked.

Read `bin/app-experiment --help` and the
[experiment runbook](https://github.com/jakub/trawl/blob/main/scripts/app-experiment/README.md)
for the prerequisites, the remaining options, and the ownership checks.

Use a disk-backed build cache. If you set `CARGO_TARGET_DIR`, use the same value
for preparation and for later runs, and give concurrent worktrees separate
caches. `--skip-build` reuses a prepared checkout only while its preparation
manifest still matches, and it refuses a stale one.

## Keep the browser open

```bash
bin/app-experiment --seed 42 --events 1000 --rate 200 --hold-seconds 600
```

The hold starts only after the whole scenario passes, including the restart
check, and it ends at its deadline. Open the printed URL and log in with the key
in that run's `private/browser-key` file. Keep the runner supervised until the
hold ends.

Ctrl+C or SIGTERM during the hold requests cleanup, exits `1`, and records
`status: interrupted`, even when every check had passed. Let the hold reach its
deadline if you want an exit-`0`, `passed` run. The run's credentials are
temporary, so keep them out of screenshots and published evidence.

## Extend the scenario

The default scenario does not cover Vector, native syslog, retention, report
scheduling, repin, or backup and restore. Add a scenario with explicit expected
results for the behavior you changed. Debug-build timings are not release
performance measurements.

After a forced stop, follow the runbook's ownership checks before you remove
anything. A PID in an old report may now belong to another process. Copy any
evidence you cite out of the worktree before you delete it.
