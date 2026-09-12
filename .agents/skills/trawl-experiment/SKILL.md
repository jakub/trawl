---
name: trawl-experiment
description: Exercise Trawl with a seeded workload against disposable real daemons and Chromium, including bounded browser inspection and performance experiments.
---

# Full-app experiments

Use `bin/app-experiment` in the candidate's isolated worktree. It owns Postgres,
keys, TLS, `trawld`, `trawl-web`, Chromium, and synthetic HTTP events. It does
not need `fleet-dev`, a saved profile, or access to an existing server.

Run `bin/app-experiment --help`. The [experiment runbook](../../../scripts/app-experiment/README.md)
owns prerequisites, options, result interpretation, and cleanup. Read the
relevant section when preparing or diagnosing a run; routine execution does
not require reading the runner's implementation.

```bash
# Default scenario, including build preparation.
bin/app-experiment --seed 42 --events 1000 --rate 200
# Reuse only the exact prepared checkout and artifacts.
bin/app-experiment --skip-build --seed 43 --events 2000 --rate 400
# Keep the verified instance open after its checks.
bin/app-experiment --skip-build --hold-seconds 600
```

Use a disk-backed build cache. If setting `CARGO_TARGET_DIR`, keep it consistent
through preparation, execution, and lifecycle tests. Concurrent builds need
separate caches. A stale preparation refusal means rebuild without
`--skip-build`, not editing the manifest.

For a custom experiment, state the hypothesis, workload, expected result, and
measurement threshold. Keep the existing event-ID and value oracle under load.
The default scenario does not establish Vector, syslog, retention, reports,
or repin behavior; those need their own scenarios. Debug-daemon timings are
not release benchmarks.

Keep a hold supervised. Use its printed URL and private browser-key file;
credentials are removed at teardown and do not belong in evidence.
Read that run's `report.json` after exit. Success requires exit 0, `status:
passed`, the expected phases, and successful cleanup. A URL or accepted ingest
count alone is insufficient. An early stopped hold is `interrupted`, even if
the scenario passed; report completed checks and cleanup separately.

Preserve cited evidence before removing the worktree. After a forced stop,
follow the runbook's live ownership checks. Do not kill recorded PIDs or prune
Docker resources from a stale report.
