---
title: Run a full-app experiment
description: Exercise real Trawl daemons, authentication, ingestion, and Chromium in disposable infrastructure.
---

`bin/app-experiment` prepares an isolated Trawl installation, sends seeded
synthetic events, and checks the application through Chromium and its API.
It owns its PostgreSQL container, keys, TLS files, daemons, and browser.
It does not need an existing Trawl server or your interactive development data.

## Run the default scenario

Use an isolated worktree containing the exact change you want to check. Read
`bin/app-experiment --help` and the
[experiment runbook](https://github.com/jakub/trawl/blob/main/scripts/app-experiment/README.md)
for prerequisites and command options.

```bash
bin/app-experiment --seed 42 --events 1000 --rate 200
```

The runner builds the checkout, prepares the disposable stack, and checks
login, queries, HTTP ingest, live results, compaction, and restart behavior.
The fixture's event IDs and values give it an exact result to compare.

Use a disk-backed build cache. If you set `CARGO_TARGET_DIR`, keep it consistent
across preparation and execution. Concurrent builds need separate caches.
Only reuse a prepared checkout with `--skip-build` when its preparation manifest
still matches; rebuild after a stale-preparation refusal.

## Keep the browser open

```bash
bin/app-experiment --seed 42 --events 1000 --rate 200 --hold-seconds 600
```

After the checks, the runner keeps its instance open for ten minutes. Use the
printed URL and that run's private browser-key file. Keep the runner supervised
until the hold ends. Its credentials are temporary and must stay out of screenshots,
reports, and published evidence.

## Read the result

Read the exact `report.json` path printed by the run. A successful result requires
exit zero, `status: passed`, the expected phases, and successful cleanup.
If you stop the hold early, the run reports `interrupted`; report the completed
scenario checks and cleanup separately.

The default scenario does not establish Vector, native syslog, retention,
report scheduling, repin, or backup/restore correctness. Add a scenario with
explicit expected results for the behavior you changed. Debug binary timings
are not release performance measurements.

After a forced stop, use the runbook's ownership checks before removing resources.
A PID saved in an old report may now belong to another process. Preserve cited
evidence before deleting the worktree.
