---
title: Test a change
description: Pick the check that matches your change, and run it without the persistent development stack.
---

Start from the behavior you changed. A parser regression needs a parser input
and an assertion. A browser authentication change needs the real session proxy
and keystore. One kind of evidence does not replace the other.

## Pick the check

| Change | Run this | What it proves |
| --- | --- | --- |
| Rust behavior in one crate | `cargo nextest run -p CRATE FILTER` | The selected tests against this checkout |
| Rust formatting | `cargo fmt --check` | The pre-commit formatting gate |
| Any Rust source | `cargo clippy --workspace --all-targets -- -D warnings` | The pre-commit lint gate |
| Browser components | `cargo clippy -p trawl-web-ui --target wasm32-unknown-unknown -- -D warnings` | The component tree compiles for `wasm32` |
| Shared UI components | `cargo clippy -p fleet-ui --target wasm32-unknown-unknown -- -D warnings` | The same gate for the shared crate |
| PostgreSQL behavior | `cargo nextest run --workspace` with `DATABASE_URL` set | Migrations and queries against real PostgreSQL |
| Feature-gated code | `cargo nextest run -p trawl-server --no-default-features` | The build without default features |
| Browser routing and UI | `cargo xtask e2e --grep 'scenario'` | Browser behavior against a stub API |
| Documentation | `npm run check` in `docs/` | Site build, links, fragments, TOML, DSL stage headings |

The pre-commit hook runs the formatting and lint rows. It adds the `fleet-ui`
row a second time with `--features atmosphere`. The pre-push hook runs the
workspace suite and the no-default-features suite in parallel, each against its
own PostgreSQL cluster. Read
[lefthook.yml](https://github.com/jakub/trawl/blob/main/lefthook.yml) for the
exact commands and the globs that trigger them.

A native workspace check never compiles the component tree that exists only
under `wasm32`, so run the Wasm rows when you touch `crates/trawl-web-ui/` or
`crates/fleet-ui/`.

## Isolate PostgreSQL tests

The `bin/dev` stack keeps persistent data. Use
[docker-compose.dev.yml](https://github.com/jakub/trawl/blob/main/docker-compose.dev.yml)
for disposable test clusters:

```bash
docker compose -f docker-compose.dev.yml up -d
DATABASE_URL=postgres://fleet:fleet@localhost:5433/fleet_test cargo nextest run --workspace
```

The compose file starts two clusters, `5433` and `5434`. SQLx creates a database
per test, so the login needs create rights. Never point a suite at an
installation you want to keep.

Two concurrent SQLx suites pick the same test database names even when their
`DATABASE_URL` values name different logical databases, because SQLx derives the
name from the test path. Give each suite its own cluster. The pre-push hook does
this with `TRAWL_TEST_DATABASE_URL` and `TRAWL_TEST_ND_DATABASE_URL`.

## Write timing-safe tests

A test that measures time passes on a quiet machine and fails on a busy one.
The pre-push hook runs two suites at once, so a busy machine is the normal
case. Follow these rules in any test that waits on a thread, a socket, a timer,
or a background task. `trawl_server::test_timing` holds the helpers:
`HANG_GUARD`, `until`, and `until_polling`.

1. **Order steps by events.** Establish "A happened before I check B" with
   something the code publishes: a seam arrival, a test-only record, a task
   join, a channel message, a job generation, or a committed row. Elapsed time
   never establishes order.
2. **Use a real-time bound only as a hang guard.** Use `HANG_GUARD` (120
   seconds) for every wait, so a stuck build fails instead of blocking the run.
   A bound is legal only if a buggy build cannot meet the condition before the
   bound runs out. If a bug would meet the condition later in time, as a lost
   wake does when the next timer tick drains the work, tell the paths apart by
   which pass ran, not by when it ran.
3. **Do not assert throughput.** Never write "at least N within T" or
   "finished before T". Run a counted number of iterations and assert the
   count.
4. **Sleep for a minimum time only on pinned state.** "Let at least T pass" is
   safe only while a held seam keeps the state fixed, because oversleeping then
   cannot change the result.
5. **Anchor negative checks.** Check "X did not happen" only after an event that
   follows the last chance for X to happen. A correct timer can falsify a
   negative check in real time. Rewrite that check under controlled time or
   prove it another way.
6. **Make holds wait for their release.** A test-support hold waits for its
   release signal and never for a wall clock. Give the test a guard that
   releases the hold on drop, so a failing test still frees the job.
7. **Keep paused tokio time fixed.** Paused time is fine on a `current_thread`
   runtime if the test keeps the runtime runnable. Wait with `until` or
   `until_polling`, which yield instead of sleeping. An idle tokio sleep or
   timeout in a paused test lets the clock jump ahead on its own. This also
   applies to your own polling helpers.
8. **Do not mask a flaky test.** Do not add retries, a known-flakes entry, or
   a test-group throttle. Fix the wait.

This example waits for a worker thread two ways:

```rust
// Fails on a busy machine: the worker may need longer than 300 ms.
tokio::time::sleep(Duration::from_millis(300)).await;
assert!(worker.is_finished());

// Waits as long as the worker needs and fails only on a hang.
until("the worker finished", || worker.is_finished()).await;
```

`until(what, cond)` yields until `cond` holds and panics with `what` after
`HANG_GUARD`. When the test owns a future it must keep polling while it waits,
such as a connection driver or an HTTP request, use `until_polling(what, fut,
cond)`. It returns `Some(output)` if the future completes first and `None` once
`cond` holds while the future is still pending. Put a comment on each wait that
names the event it waits for.

## Run the browser suite

```bash
env -u NO_COLOR cargo xtask e2e --grep 'scenario'
```

The task runner builds the SPA with Trunk, installs the suite's own
dependencies, and runs Playwright against the stub server in
`crates/trawl-web-ui/e2e/harness/`. It checks routing, input, overlays, and
response handling. It does not prove real authentication, PostgreSQL behavior,
or durable ingestion. Trunk rejects an inherited `NO_COLOR=1`, so the example
unsets it.

Parallel checkouts need distinct `E2E_PORT` values. The default is `8123`. Keep
the suite's single worker and `reuseExistingServer: false`. A mutation check
needs a passing baseline, the intended failure with the mutation, and a passing
control afterwards. The
[browser test runbook](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/e2e/README.md)
owns setup, browser requirements, and the mutation commands.

## Generate adversarial test logs

Use a disposable data root and catalog. These inputs create new field pins and
conflicts by design. Keep the same seed and namespace across related phases.

```bash
cargo xtask ingest-fuzz --phase mutate --seed 42 --events 1000 > /tmp/trawl-fuzz.ndjson
cargo xtask ingest-fuzz --phase pin --seed 42 --namespace ci > /tmp/pin.ndjson
cargo xtask ingest-fuzz --phase conflicts --seed 42 --namespace ci > /tmp/conflicts.ndjson
```

Send `pin.ndjson` through the test server's HTTP ingest route and wait for that
batch to compact before you send `conflicts.ndjson`. If the two compact
together, they exercise a different first-observation contract.

The generator writes NDJSON to stdout and its row-count summary to stderr.
`mutate` covers scalar boundaries, nested values, derivation inputs, field-name
collisions, and repair paths. `rejects` produces valid JSON with invalid
envelope values, so a test can assert per-event rejection. The `pin` phase
covers `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, and `VARCHAR` candidates,
all-null deferral, and the ladder thresholds.

`--profile syslog` and `--profile trawld` shape the `mutate` payload for those
producers, and no other phase takes a profile. Posting either file over HTTP
still stamps `_producer=http`. The
[profile tests](https://github.com/jakub/trawl/blob/main/crates/trawl-server/tests/profile_fuzz.rs)
drive each payload through its own producer entry point.

A Vector file source can read the NDJSON and forward it to the normal HTTP sink:

```toml
[sources.trawl_fuzz]
type = "file"
include = ["/tmp/trawl-fuzz.ndjson"]
read_from = "beginning"
framing.method = "newline_delimited"
decoding.codec = "json"
```

Use the TLS and key handling in
[Vector integration](/getting-started/vector-integration/) for the test
destination. Check the accepted counts, then assert the stored rows and the
catalog evidence. An HTTP success alone does not prove the conflict case.
