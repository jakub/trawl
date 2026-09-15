# Full-app experiment

`bin/app-experiment` runs a disposable Trawl instance and checks a seeded log
workload through the real HTTP API and browser. It builds this checkout's
`trawld`, `trawl-web`, and `fleet-admin`, builds the release SPA, and uses the
existing Playwright dependency in `crates/trawl-web-ui/e2e`.

## Quick start

Ask an agent to "run an app experiment" for the default 1000-event check, or
specify a seed, count, rate, and whether to keep the browser available. The
project [trawl-experiment skill](../../.agents/skills/trawl-experiment/SKILL.md)
gives agents the default workflow; root `AGENTS.md` links to the task skills.
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

## Create an experiment

Before running, write a short brief outside the disposable worktree. Record:

- Hypothesis and decision, for example whether a query change lowers p95 at
  the same event count without changing any returned ID or status.
- Baseline commit, candidate change, seed, event count, batch size, target
  rate, and number of repeats. Keep these fixed for comparisons.
- Oracle, meaning the expected result computed independently of the app.
  The built-in oracle checks every sequence ID and status, rejects duplicate
  or truncated results, and checks the expected browser page size.
- Measurements and acceptance criteria chosen before running. Name the
  metric, phase, threshold, and allowed variation. Include correctness and
  cleanup in the decision even for a performance experiment.
- Evidence destination and known exclusions. State whether the built-in
  valid HTTP workload answers the question or needs a custom scenario.

For a new experiment, start from the current local `main`. These Bash
commands create a separate branch and worktree. Choose a fresh slug once
and retain these variables for the later evidence and removal steps:

```bash
experiment_repo=/home/jakub/code/trawl
experiment_slug=query-latency-20260906
experiment_wt="$experiment_repo/.worktrees/$experiment_slug"
experiment_evidence="$HOME/trawl-experiment-evidence/$experiment_slug"
git -C "$experiment_repo" status --short
git -C "$experiment_repo" log -1 --format='%H %s' main
git -C "$experiment_repo" worktree add -b "chore/$experiment_slug" "$experiment_wt" main
install -d -m 700 "$experiment_evidence"
git -C "$experiment_wt" rev-parse HEAD > "$experiment_evidence/base-commit.txt"
```

This does not fetch or update `main`. If the question requires the latest
remote code, update it through the repository's normal Git workflow before
creating the worktree. If the question names an existing candidate branch,
use its assigned worktree and record that commit instead. Do not reset or
stash unrelated changes. A worktree missing `bin/app-experiment` needs the
runner change integrated before this guide applies.

Write the brief to `$experiment_evidence/notes.md`. Set subsequent tool
commands' working directory to `$experiment_wt`, or use a subshell such as
`(cd "$experiment_wt" && bin/app-experiment --help)`. Do not run the experiment
from the persistent developer checkout. Complete the prerequisites below,
then run the baseline without `--skip-build` and retain its exact report path.

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

When pushing an experiment branch, the normal pre-push hook runs two Rust
suites against separate Postgres clusters. Set `TRAWL_TEST_DATABASE_URL` and
`TRAWL_TEST_ND_DATABASE_URL` to existing databases on two owned test clusters.
Each URL must authenticate a role with `CREATEDB`, since the suites create
per-test databases. Keep the prepared `CARGO_TARGET_DIR` on the push command
to reuse its build cache. The hook's builds can replace prepared binaries;
run the experiment without `--skip-build` after pushing. The hook ignores ambient
`DATABASE_URL`; without these overrides it uses the shared development
clusters on ports 5433 and 5434. Do not run both suites against one cluster.

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

### Verify teardown and recover a forced stop

For normal completion, let the foreground runner exit and read its exact
`report.json`. Require `cleanup.processes`, `cleanup.container`, and
`cleanup.secrets` to be true. For SIGINT or SIGTERM, wait for that same final
report before treating cleanup as complete. Keep the exit status with the
report; an interrupted hold is not an exit-0 pass.

The commands below use Python 3 to read only ownership metadata. Set the run
path to the one printed by this execution, not the newest directory found
by a glob. These are inspection commands and do not stop anything:

```bash
experiment_run="$experiment_wt/target/app-experiments/REPLACE_WITH_PRINTED_RUN_ID"
experiment_run_id=$(basename -- "$experiment_run")
python3 - "$experiment_run" <<'PY_CHECK'
import json, pathlib, sys
run = pathlib.Path(sys.argv[1])
for name in ('report.json', 'instance.json'):
    path = run / name
    if path.exists():
        value = json.loads(path.read_text())
        keys = ('runId', 'status', 'cleanup', 'processes') if name == 'report.json' else ('runId', 'container', 'pids')
        print(name, {key: value.get(key) for key in keys})
print('private directory exists:', (run / 'private').exists())
PY_CHECK
docker --host unix:///var/run/docker.sock ps --all \
  --filter "label=trawl.experiment=$experiment_run_id" \
  --format '{{.ID}} {{.Names}} {{.Status}}'
```

An unavailable Docker daemon leaves container cleanup unknown. An empty
successful inventory query confirms no container with this run's label.
After SIGKILL or host loss, the report may be absent or stale. `instance.json`
is written after startup and again after restart, so its PIDs may be stale
and early failures may have no instance file at all.

To remove an orphaned container, first establish the exact name and full ID
from that run's instance file and live label-filtered inventory. If the
instance file is missing, use the exact run ID and inspect the matching
container's name, creation time, and label before selecting it. Never select
a container by a name prefix alone. Then remove by its immutable full ID so
a concurrent name replacement cannot redirect the removal:

```bash
experiment_container=REPLACE_WITH_VERIFIED_EXACT_NAME
experiment_container_id=$(docker --host unix:///var/run/docker.sock inspect \
  --format '{{.Id}}' "$experiment_container")
experiment_owner=$(docker --host unix:///var/run/docker.sock inspect \
  --format '{{index .Config.Labels "trawl.experiment"}}' "$experiment_container_id")
if [ -n "$experiment_container_id" ] && [ "$experiment_owner" = "$experiment_run_id" ]; then
  docker --host unix:///var/run/docker.sock rm --force "$experiment_container_id"
else
  printf '%s\n' 'Ownership not established; no container removed.' >&2
fi
```

Repeat the inventory check afterward. Do not run Docker prune or remove
shared volumes, databases, or developer instances.

Never signal a PID or process group copied from `instance.json` or a report.
For possible orphan processes, inspect live `/proc/<pid>/exe`, command line,
working directory, start time, and parent/group membership. The command line
must reference this exact run's private config, or a browser must have proven
ancestry from this run. Do not dump process environments, which contain
credentials. A matching name or PID is insufficient. The runner has no
orphan-process cleanup command. If manual termination is needed, use a
supervisor handle or Linux pidfd tied to the verified process and recheck its
identity after acquiring the handle. If ownership cannot be established,
leave it running and report cleanup as unknown instead of using `pkill` or
killing a possibly reused PID.

Only after every owned process and container is confirmed gone, remove any
remaining `private` directory under this exact run path. Verify the resolved
path first and refuse a symlink or a path outside the selected run. Preserve
diagnostic artifacts but do not copy the residual private directory to the
evidence archive. Do not overwrite the original report to turn manual
recovery into a passing run; record recovery separately in `notes.md`.

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

These start real disposable instances. They check port collisions, SIGTERM
during a verified hold, compaction polling with a temporarily missing directory,
a non-retryable filesystem error, and failure to remove the private directory.
The tests check the final report and Docker's live container inventory. Docker
inventory checks have a ten-second deadline. A timeout fails verification.

Fault injection uses a Node preload in the runner process. The runner does not
pass that preload to app processes. The removal-failure test proves that the
report records `cleanup.secrets: false` and `cleanup.secretsError`, then removes
its own residue after verifying that the processes and container stopped.
For a real removal failure, follow the ownership checks in the teardown section
before removing the remaining private directory.

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
Confirmed missing paths, including dangling directory links, do not count
as scan failures. An unreadable existing directory still does. A failed
startup scan also stops WAL compaction and repin admission. Ingestion can
continue writing durable WAL, so repair the filesystem problem and restart
the daemon before accumulated WAL consumes the available disk space.

Repin also refuses to scan or build while a rollup needs recovery. Let
compaction finish recovery before retrying the repin experiment. The
admission check and rollup pause are taken together, so an in-flight rollup
cannot leave an incomplete publication between the check and the pause.

The publication lock coordinates one daemon. It does not promise exactly-once
ingestion across client retries or crash-time WAL replay, and it cannot
coordinate another process writing the same archive. See
[ADR-0026](../../docs/adr/0026-compaction-publication-consistency.md).

For a custom scenario, follow this sequence:

1. Save a passing baseline report before editing the generator or runner.
2. Change `corpus()` for the new deterministic input and compute expected
   values from those inputs. Extend the oracle when testing new columns or
   aggregates. Do not derive the expected count from the response under test.
3. Add generator and oracle tests that deliberately lose, duplicate, corrupt,
   or truncate the new results. Run the focused Node tests below before a
   full build.
4. Add only the required configuration, query, browser action, or measurement
   phase in `run.mjs`. Retain isolation, deadlines, existing correctness
   phases, and cleanup. New transport or configuration coverage needs its own
   assertions, not merely a changed setting.
5. Run the custom scenario without `--skip-build`, repeat with the planned
   seeds, and run lifecycle tests after runner changes. Keep baseline and
   candidate commands identical except for the variable under test.
6. Preserve the custom source diff and any new files with the evidence. A
   custom experiment is not reproducible from its CLI flags alone.

Read source only for the component being changed:

| File | Responsibility |
| --- | --- |
| `scripts/app-experiment/run.mjs` | Preparation, instance ownership, API/browser scenario, measurements, cleanup |
| `scripts/app-experiment/workload.mjs` | Deterministic corpus and independent result/page checks |
| `scripts/app-experiment/workload.test.mjs` | Loss, duplication, corruption, truncation, short pages, and workload variation |
| `scripts/app-experiment/lifecycle.test.mjs` | Real lifecycle and filesystem-failure checks |

After a runner change, run the focused Node tests and the full scenario.
After lifecycle changes, also run the real lifecycle tests with the same
Cargo target as preparation. Application changes require the relevant
application tests and a run without `--skip-build`. Update this guide and the
root agent quick start when commands, prerequisites, or the scenario contract
change.

## Preserve evidence and remove the worktree

Finish teardown before this step. Copy each exact run directory you used,
including failed baselines, into the private evidence destination. The
following refuses to copy a run whose private directory still exists:

```bash
if [ ! -e "$experiment_run/private" ] && [ ! -L "$experiment_run/private" ]; then
  cp -a -- "$experiment_run" "$experiment_evidence/"
else
  printf '%s\n' 'Private data remains; complete teardown before copying evidence.' >&2
fi
git -C "$experiment_wt" rev-parse HEAD > "$experiment_evidence/tested-commit.txt"
git -C "$experiment_wt" diff --binary HEAD > "$experiment_evidence/candidate.patch"
git -C "$experiment_wt" status --short > "$experiment_evidence/worktree-status.txt"
```

The patch captures tracked changes only. Preserve any intended untracked
scenario files separately, or commit the experiment changes through the
normal repository workflow. Save exact commands, exit codes, environment
choices such as `CARGO_TARGET_DIR`, and the brief beside the copied reports.
Verify the copied `report.json`, corpus, and any artifacts cited by your
conclusion exist before removing the worktree. Raw traces remain private;
inspect and redact evidence before sharing it. PR evidence must use committed
SHA-pinned files or the private artifact publisher with `--keep`.

From outside the worktree, inspect its state and use ordinary Git removal:

```bash
git -C "$experiment_wt" status --short
git -C "$experiment_repo" worktree remove "$experiment_wt"
git -C "$experiment_repo" worktree list
```

If Git refuses removal, inspect and preserve the remaining changes. Do not
add `--force`. Keep the experiment branch until its code and evidence have
been reviewed; remove it later through the normal Git workflow. An external
Cargo cache is independent of the worktree and may still serve other runs.

## Result template and agent prompts

Use this structure in the saved notes and final handoff:

```text
Hypothesis and acceptance criteria:
Baseline and candidate commits, source changes, runner/build hashes:
Host, toolchain, Cargo target, preparation command:
Commands, seed, count, batch size, target rate, repeats:
Exact run paths and exit statuses:
Oracle and phase results, including expected/observed counts:
Ingest sent/accepted/rejected/ambiguous batches:
Measurements by run and phase, units, sample counts, variation:
Decision against the original criteria:
Cleanup flags, live ownership checks, any manual recovery:
Evidence location and retained custom scenario files:
Limitations and checks not run:
```

For a routine check, a sufficient agent request is:

> Create an isolated experiment worktree from main. Follow the AGENTS quickstart
> and app-experiment runbook. Run seed 42 with 1000 events at rate 200, then
> seed 43 with 2000 events at rate 400. Require every built-in oracle and
> cleanup check. Preserve both reports outside the worktree, report the
> results and limitations, and remove the clean worktree after teardown.
> Do not change application code.

For a custom experiment:

> Create an isolated experiment worktree from main. Test whether the proposed
> query change lowers HTTP query p95 by at least 10 percent for seed 44,
> 10000 events, batch size 1000, and target rate 2000. Run three baselines and
> three candidates on the same host. Preserve every ID and status check.
> Define any additional measurement phase and expected results before coding
> it, add oracle failure tests, and run the real scenario and lifecycle tests.
> Treat mixed-corpus latency samples as exploratory unless a fixed-corpus
> measurement phase is added. Retain commands, changes, and evidence outside
> the worktree. Report a failed hypothesis if the evidence does not support it.

## Issue 188 real-app evidence

The committed `issue188-evidence.mjs` scenario checks Search execution facts
against the daemon's lifecycle records and global Runs ordering against
individual stored receipts. Start a fresh experiment in the candidate
worktree, with at least 11 events. Keep the default runner supervised.

```bash
bin/app-experiment --seed 42 --events 1000 --rate 200 --hold-seconds 900
```

Wait for `Verified instance held` after restart. In another foreground
session rooted in the same worktree, pass the exact printed artifact directory.
Replace `run-TIMESTAMP-ID` with that run's name.

```bash
node scripts/app-experiment/issue188-evidence.mjs \
  --run "$PWD/target/app-experiments/run-TIMESTAMP-ID"
```

The custom scenario has a ten-minute deadline. It uses only the selected
loopback origin and checks its process/config identity before reading the
private browser credential into Node memory. It logs in with a same-origin
browser request. It creates its own browser context, closes it on success or
failure, and leaves infrastructure teardown to the supervised default runner.
Do not interrupt the default hold. Let its deadline expire.

The scenario requires no existing saved runs and refuses to overwrite an
existing `issue188-evidence` directory. Use a fresh experiment for a rerun.
Its assertions cover:

- Nonempty and zero-row Search requests through CodeMirror, with explicit
  January 1, 2026 bounds. The visible count, duration and full UTC start must
  match the accepted response.
- A unique newly appended `query_start` and its `query_complete`, joined by
  the daemon's internal query ID after matching user, query length, page
  bounds and browser wall-clock bounds. Duration and returned count must
  match exactly. Start time must fall within browser send/receive and daemon
  lifecycle timestamps, allowing two milliseconds for timestamp precision.
  The script polls appended logs for up to five seconds to allow the runner
  to flush daemon output. Multiple matching starts or completions fail.
  No expected start time is calculated by subtracting duration.
- Three mixed-case net names with independently known counts of 2, 11 and 5.
  Twenty-one manual runs must succeed, and every individual stored result
  must contain exactly the expected corpus sequence IDs.
- All ten global sort/direction combinations across offsets 0 and 20, using
  individual receipts as the ordering oracle. Every page must match exact
  identities and receipt fields, with total 21 and no omissions or duplicates.
- The real Runs UI for Net and Rows, both directions and both pages, checking
  row identity, name, status, duration, count and active `aria-sort`.

Only `issue188-evidence/report.json` and its PNG captures are custom evidence
intended for publication. The report includes the source commit, source/build
hashes, corpus hash, safe lifecycle fields, receipts, expected/observed orders
and UI cells. Its command uses a worktree-relative run path. Search report
fields contain only the parsed visible count, duration and UTC start, without
filter-chip text. Before login, the script requires its current bytes to match
`git show HEAD:scripts/app-experiment/issue188-evidence.mjs` and records their
`scenarioSHA256`. Commit scenario edits before collecting evidence. The script
also recomputes the runner's source fingerprint and requires it to match
preparation. Binary and SPA hashes are explicitly
labelled as manifest identity; their verification belongs to the default
runner. It omits credentials, cookies, headers, raw query text, private
paths from receipts and raw logs. Do not publish `private/`, daemon logs,
`queries.ndjson`, or the complete run directory. A failed report names the
phase and assertion source line, with numeric or boolean actual/expected
values where available. It never copies exception text that might contain
request data.

Success requires both commands to exit zero, the custom report to say
`status: passed` with `cleanup.browser: true`, and the default `report.json`
to say `status: passed` with every cleanup flag true. Compare their run IDs
and build identities. A custom pass alone does not establish default-scenario
completion or cleanup. Null values, different terminal statuses and deliberate
timestamp ties remain covered by disposable PostgreSQL tests; this scenario
uses successful immutable receipts from real queries. Debug build timings
are correctness evidence, not a release performance benchmark.
