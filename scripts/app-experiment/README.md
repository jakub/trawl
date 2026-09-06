# Full-app experiment

`bin/app-experiment` runs a disposable Trawl instance and checks a seeded log
workload through the real HTTP API and browser. It builds this checkout's
`trawld`, `trawl-web`, and `fleet-admin`, builds the release SPA, and uses the
existing Playwright dependency in `crates/trawl-web-ui/e2e`.

## Quick start

Ask an agent to "run an app experiment" for the default 1000-event check, or
specify a seed, count, rate, and whether to keep the browser available. The
root `AGENTS.md` links to `CLAUDE.md`, which gives agents the default workflow.
This guide supplies the setup context without requiring source inspection.

Run commands from the worktree containing the code you want to exercise.
For tool-driven sessions, set the command's working directory explicitly.

```bash
# First run also prepares the build and browser dependencies.
bin/app-experiment --seed 42 --events 1000 --rate 200

# Repeat using exactly the same prepared checkout and artifacts.
bin/app-experiment --skip-build --seed 43 --events 2000 --rate 400

# Run the checks, then leave the verified instance open for ten minutes.
bin/app-experiment --skip-build --hold-seconds 600
```

The runner prints a fresh artifact directory and browser URL. During a hold,
log in with the key in the printed `private/browser-key` file. The hold starts
only after the complete scenario passes, including restart. Keep the runner
supervised while inspecting the app. Ctrl+C or SIGTERM requests cleanup.
The hold also ends automatically at its deadline.

An early Ctrl+C or SIGTERM deliberately produces exit code 1 and
`status: interrupted`, even after every scenario check passed. Report it as
an interrupted hold with completed checks only when the report includes
`browser-session-after-restart` and successful cleanup. Let the hold reach
its deadline for an exit-0, `passed` run.

The successful result is exit code 0 and `status: passed` in that run's
`report.json`, with all cleanup flags true. A printed URL means startup
succeeded, not that the experiment passed. Credentials, TLS keys, databases,
and data files disappear after cleanup; reports remain.

## Setup and build cache

Use Linux with these prerequisites:

| Requirement | Check or preparation |
| --- | --- |
| Git, Node, npm, Rust, Trunk | `command -v git node npm cargo rustup trunk` |
| Repository Rust toolchain | `rustup show active-toolchain` from the worktree |
| WebAssembly target | `rustup target add wasm32-unknown-unknown` from the worktree |
| Local Docker access | `docker --host unix:///var/run/docker.sock info` without sudo |
| Chromium system libraries | Must be installed on the host; inspect browser startup errors for missing libraries |
| Disk and network | Allow Rust/SPA builds and downloads from Cargo, npm, Playwright, and the Postgres image registry |

The runner uses the versions selected by the checkout's Rust toolchain and
lockfiles. It installs the existing Playwright dependency with `npm ci` and
installs Chromium. It does not install host packages, Trunk, or Chromium's
system libraries. On Arch-based hosts, resolve missing libraries with the
host package manager rather than assuming a Debian dependency installer.

No developer Fleet profile, Tailscale Serve mapping, existing Postgres,
manual migration, API key, or `fleet-dev` session is needed. The runner
creates its own Fleet and Trawl databases, separate reader and ingest roles,
short-lived browser key, ingest key, session material, TLS, and app configs.
It restricts app listeners and the database mapping to loopback. The ingest
key has only ingest permission.

By default Cargo builds in this worktree's `target`. On Jakub's current host,
select the cache prepared during development with:

```bash
export CARGO_TARGET_DIR=/home/jakub/code/trawl/target/app-experiment
bin/app-experiment --seed 42 --events 1000 --rate 200
```

Use the same value for subsequent runs and lifecycle tests. This absolute
path is a host convenience, not a requirement. On another host choose a
local disk-backed cache or omit it. Another checkout building into the same
cache can replace binaries and make `--skip-build` refuse the next run. Use
a separate cache for concurrent worktrees. The preparation manifest and run reports
still live under the selected worktree's `target`, even when Cargo artifacts
live elsewhere. The initial cache occupied about 8.4 GB on this host; allow
room for growth and avoid building on a small `/tmp` tmpfs.

Normal preparation invokes Cargo with `--locked --no-default-features` for
`trawl-server`, `trawl-web`, and `fleet-admin`, then Trunk's release build,
`npm ci`, and Playwright's Chromium installer. The daemon uses the downloaded
DuckDB build path through `bin/trawld-dev`. Build children receive a small
environment allowlist, without ambient application credentials.

## Options

| Option | Default | Accepted values and meaning |
| --- | --- | --- |
| `--events` | `1000` | 10 through 20000 total synthetic events |
| `--seed` | `42` | Integer 0 through 4294967295 |
| `--rate` | `200` | 1 through 100000 target events per second |
| `--batch-size` | `50` | 1 through 1000; strictly less than event count |
| `--hold-seconds` | `0` | 0 through 3600, after successful scenario checks |
| `--web-port` | Random 20000 through 59999 | Explicit port 1024 through 65535; collision fails startup |
| `--skip-build` | Off | Require the existing matching preparation manifest and build artifacts |
| `--help` | Off | Print usage without starting an instance |

The nominal `events / rate` duration must not exceed 600 seconds. For a small
run, lower the batch size too, for example `--events 20 --batch-size 10`.
These flags select one built-in scenario. There is no custom query, corpus
file, release-server, or keep-data flag.

## What a passing run proves

1. A new Postgres container starts with its own databases and loopback port.
   Fleet migrations and the real daemon's boot migrations complete.
2. Separate human and ingest keys authenticate through the real keystore.
   The sender's key has only ingest permission.
3. A first batch becomes searchable with the exact generated sequence IDs
   and status values.
4. Chromium logs in through the real session proxy and runs a query by typing
   into CodeMirror. The first page contains exactly the expected events, up to the SPA page size of 50. The rendered first row agrees with that response.
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

## Reading and reporting a result

Start with the exact report path printed by the command. The report includes
`build.commit`, application-source and binary hashes, `build.spaHash`, and
`runnerHash`. Compare runs only after identifying what changed. A run ID is
unique, so identical seeds produce the same event values except for the
run-specific `experiment_run` marker.

Check `ingest.sent`, `accepted`, `rejected`, and `ambiguousBatches`, then the
phase list. With defaults, expect 50 initial events, a 50-row browser page,
950 new live-tail events, 1000 events at the ingested/compacted/restarted
phases, and a 50-row browser page after restart. With a different batch size,
the initial count is the batch size and live tail receives the remainder.
For counts below 50, the expected browser page is the available event count.
Also require `browser-live-tail-closed`, which records closure on navigation.

`queryLatencyMs` contains raw HTTP query durations and their p50/p95/max.
They include authentication and transport. Queries also run between ingest
batches, so samples cover different corpus sizes. Do not treat this small,
mixed sample as a steady-state benchmark or equate requested rate with
achieved throughput. The report records actual paced ingestion duration.

Use phase `.prom` snapshots and `queries.ndjson` to investigate a slow query,
and phase memory samples for RSS, high-water mark, and thread count. Browser
traces and screenshots explain UI behavior. These observations do not yet
attribute CPU time to authentication, DuckDB, parsing, or serialization.

Return a concise result with the run path, commit, workload, passed or failed
phases, ingest accounting, relevant measurements, and cleanup outcome. If a
run fails, report its failure rather than substituting an earlier successful
run. Artifacts under `target` are local and can disappear during a clean;
copy evidence that must survive elsewhere before cleaning the build tree.

## Failure triage

| Symptom | Next action |
| --- | --- |
| Missing preparation manifest, or changed commit/source/cache/binaries/SPA | Run without `--skip-build`; never edit the stamp to force acceptance. Even a documentation-only commit changes the checked commit identity. |
| Cargo or SPA build failure | Read `build.log` or `spa-build.log`; check toolchain, WebAssembly target, disk, and dependency access. |
| npm or Chromium installation failure | Read `npm.log` or `browser-install.log`; check registry access and host libraries. |
| Docker unavailable | Check the explicit local socket and user access; the runner ignores remote Docker contexts. |
| Database startup or migration failure | Read `postgres-start.log`, `postgres.log`, and the report error. Readiness waits for TCP because the image's temporary initialization server accepts only Unix-socket connections. |
| Proxy port collision | Choose another `--web-port` or rerun with a random port. Do not stop the existing listener. |
| Browser assertion or JavaScript failure | Read browser response JSON, `browser-errors.json`, `failure.png` when present, and the trace; then compare daemon/proxy logs. |
| Missing, duplicated, changed, or truncated events | Compare `corpus.ndjson` and phase result JSON. Check ingest accounting and daemon logs. The sender does not retry ambiguous batches. |
| Compaction deadline | Inspect the last metrics snapshot, daemon log, and report; check hot-buffer drain and compaction errors. |
| Missing final report or cleanup failure | Treat cleanup as unverified and follow the ownership checks below. Do not assume that a killed runner removed its resources. |

Not every failure reaches every artifact-producing phase. Early argument
errors occur before a run directory is created. A build failure creates a
report but cannot have browser or Postgres evidence.

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

## Changing or extending an experiment

For a routine run, use the flags above. For a new workload or instrumentation
question, first run the existing scenario as a baseline and retain its report.
State the expected result before adding a scenario. Keep known sequence IDs
and independent count/value checks so a performance improvement cannot hide
lost events or a shorter browser page.

The built-in instance disables internal telemetry ingestion, syslog,
scheduling, daily rollup, and retention. It compacts every ten seconds and
allows two concurrent queries. Events have fixed January 2026 timestamps;
use the explicit absolute bounds in the recorded queries. A relative
`last=15m` search during an interactive hold will not find that corpus.

Compaction must preserve exact results while the workload runs. Keep the
independent ID and value oracle enabled, including for the 20,000-event
scenario. Do not add `DISTINCT`, raise the result limit to hide an overlap,
or retry an incorrect result until it happens to pass. Accepted identical
payloads remain separate events.

For a publication regression, run the focused real-WAL and DuckDB tests:

```bash
CARGO_TARGET_DIR=/home/jakub/code/trawl/target/app-experiment \
  cargo nextest run -p trawl-server --no-default-features \
  --test publication_consistency
```

These tests pause publication after the Parquet rename and before hot drain,
check query and export timeouts, and check that a timed-out query retains its
read guard until its blocking task finishes. They also check refusal after
restart with an unfinished rollup marker. The ordinary app scenario disables
daily rollup, so it does not replace the compactor's rollup recovery tests.

An unfinished `.rollup-*` marker makes corpus queries return 503 until rollup
recovery removes it. Inspect the daemon's `rollup_recovery` and `rollup_error`
logs. Fix the reported filesystem failure and let the owning daemon retry
before it compacts new WAL, even if daily rollup is disabled. Do not delete
the marker to make queries pass, because both daily and hourly copies may
still exist. Query-only instances
cannot perform that recovery. A failed startup marker scan also refuses
reads and requires a restart after the filesystem problem is fixed.

The publication lock coordinates one daemon. It does not promise exactly-once
ingestion across client retries or crash-time WAL replay, and it cannot
coordinate another process writing the same archive. See
[ADR-0023](../../docs/adr/0023-compaction-publication-consistency.md).

Read source only for the component being changed:

| File | Responsibility |
| --- | --- |
| `scripts/app-experiment/run.mjs` | Preparation, instance ownership, API/browser scenario, measurements, cleanup |
| `scripts/app-experiment/workload.mjs` | Deterministic corpus and independent result/page checks |
| `scripts/app-experiment/workload.test.mjs` | Loss, duplication, corruption, truncation, short pages, and workload variation |
| `scripts/app-experiment/lifecycle.test.mjs` | Real port-collision and SIGTERM cleanup checks |

After a runner change, run the focused Node tests and the full scenario.
After lifecycle changes, also run the real lifecycle tests with the same
Cargo target as preparation. Application changes require the relevant
application tests and a run without `--skip-build`. Update this guide and the
root agent quick start when commands, prerequisites, or the scenario contract
change.
