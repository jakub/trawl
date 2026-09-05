# Full-app experiment

`bin/app-experiment` runs a disposable Trawl instance and checks a seeded log
workload through the real HTTP API and browser. It builds this checkout's
`trawld`, `trawl-web`, and `fleet-admin`, builds the release SPA, and uses the
existing Playwright dependency in `crates/trawl-web-ui/e2e`.

Run from a worktree on Linux with Docker, Node, Rust, Trunk, and the system
libraries Chromium needs:

```bash
bin/app-experiment
bin/app-experiment --skip-build --seed 43 --events 2000 --rate 400
bin/app-experiment --skip-build --hold-seconds 600
```

The runner prints its browser URL and private artifact directory. During a
hold, the login key is in the printed `private/browser-key` file. The hold has
a deadline. Ctrl+C also stops the experiment and runs cleanup. Credentials,
TLS keys, databases, and data files disappear after cleanup; reports remain.

`CARGO_TARGET_DIR` can select an on-disk build cache when the worktree is on a
small tmpfs. The application never reads the developer's Fleet profile or
Trawl config. Build children receive a small environment allowlist, without
application credentials.

## What a passing run proves

1. A new Postgres container starts with its own databases and loopback port.
   Fleet migrations and the real daemon's boot migrations complete.
2. Separate human and ingest keys authenticate through the real keystore.
   The sender's key has only ingest permission.
3. A first batch becomes searchable with the exact generated sequence IDs
   and status values.
4. Chromium logs in through the real session proxy and runs a query by typing
   into CodeMirror. The returned page agrees with the expected events.
5. The browser opens live tail. While the sender delivers the remaining
   batches, API queries check the growing corpus. The browser's actual
   EventSource receives every new sequence ID once, with no lag notification.
   Navigating to History closes the stream.
6. The hot buffer drains and at least one parquet file exists. The full
   query still returns the same IDs and status values.
7. The proxy and daemon stop and restart against the same disposable data and
   databases. The full query still agrees with the generator. The browser's
   existing cookie still works and the rendered query response agrees too.
8. Cleanup removes the owned container, processes, and private files.

The browser checks real application behavior. An init script observes its
EventSource instances without replacing the network or response data. The
stub-backed `cargo xtask e2e` suite remains separate.

## Workload

`--seed` controls the service, host, status, severity, and latency distribution.
Every event has `experiment_run` and a consecutive `experiment_seq`. The
run-specific ID separates experiments; `corpus.ndjson` preserves the exact
bytes for diagnosis. Timestamps are fixed at `2026-01-01T12:00:00Z`; the query
uses explicit absolute bounds and age retention is disabled in this instance.
This avoids wall-clock-dependent expected results.

`--events` accepts 10 to 20000. `--batch-size` defaults to 50 and must be
smaller than the event count. `--rate` is a target events-per-second rate,
paced in batches. Sending and query checks share one loop, so a slow server
can reduce the achieved rate. The report records the actual paced duration.
The workload must fit within 600 seconds at the requested rate.

The sender never retries a batch. A lost response leaves its acceptance
unknown and fails the run, with an ambiguous-batch count. HTTP acceptance,
query correctness, live delivery, and persistence are separate checks.

The existing `cargo xtask ingest-fuzz` remains the generator for malformed
inputs and pin/conflict boundary cases. This first full-app workload uses
valid synthetic traffic. It exercises HTTP ingest directly; it does not claim
Vector, syslog transport, repin, scheduled reports, or retention coverage.

## Evidence and build identity

Each run writes a private directory under `target/app-experiments/`:

- `report.json` records phase results, ingest accounting, raw query latency samples and process memory, with timings summarized as p50/p95/max, build identity, and cleanup status.
- `instance.json` identifies the temporary endpoints, child PIDs, and
  container. Those endpoints are valid only while that run is alive.
- `corpus.ndjson` and phase result files preserve inputs and checked outputs.
- Phase `.prom` files preserve server metrics.
- Daemon, Postgres, and build logs explain startup and execution failures.
- Browser screenshots, errors, and `browser-trace.zip` show the exercised UI.
  Tracing begins after login so the API key is not recorded by the login action.

Treat these artifacts as private. A browser trace can contain session cookies
and synthetic query results. Do not publish a raw trace while its instance is
still alive.

`--skip-build` requires the same application-source fingerprint, worktree path, commit, Cargo target, binary
hashes, and SPA hash as the preparation manifest. A mismatch fails with an
instruction to rebuild. Normal runs always invoke Cargo and Trunk before
starting an instance. The server build uses the existing downloaded DuckDB
path and debug profile; these timings establish a working measurement loop,
not release performance claims.

## Ownership and failure

Docker commands explicitly use the local `/var/run/docker.sock`, regardless of
the user's selected Docker context. Postgres uses a random container name, an ownership label, tmpfs storage, and
an OS-assigned loopback port. The daemon binds port zero and reports the port
it actually owns. The proxy binds a randomly selected port, or `--web-port`.
A collision fails startup; the runner does not probe and release a port or
reuse someone else's server.

The runner owns process groups and imposes command, startup, request, and
shutdown deadlines. Cleanup reconciles an uncertain Docker start by checking
its exact container name and ownership label. It never prunes volumes, sweeps
databases, or kills a PID read from a stale file.

SIGKILL or host loss cannot run cleanup. If a run is interrupted that way,
inspect its `instance.json` and the container's `trawl.experiment` label before
removing that exact container. A normal failure, SIGINT, or SIGTERM follows
the cleanup path and reports its outcome.

Focused generator and result-oracle tests:

```bash
node --test scripts/app-experiment/workload.test.mjs
```

The result oracle's tests deliberately remove, duplicate, corrupt, and truncate
results to prove those failures are detected. Full-stack evidence comes from
running the experiment itself, including repeated seeds and failure cleanup.

Real lifecycle tests, after preparing the build:

```bash
node scripts/app-experiment/lifecycle.test.mjs
```

These start real disposable instances. One holds a port open and proves that
startup refuses to reuse it without disturbing its owner. The other sends
SIGTERM during a verified interactive hold. Both check the final report,
private-directory removal, and Docker's live container inventory.
