---
title: Test a change
description: Choose focused Rust, database, Wasm, and browser checks without using the persistent developer stack.
---

Start with the behavior you changed. A parser regression needs a parser input
and an assertion. A browser authentication change needs the real session proxy
and keystore. One kind of evidence does not replace the other.

## Choose the checks

| Change | Start here | What it proves |
| --- | --- | --- |
| Pure Rust behavior | `cargo nextest run -p CRATE TEST_FILTER` | The selected tests against this checkout |
| PostgreSQL behavior | The relevant SQLx suite against a disposable cluster | Migrations and database behavior with real PostgreSQL |
| Browser components | Wasm-target clippy, then the relevant Playwright scenarios | Compilation and UI behavior in the built browser application |
| Browser authentication or ingest | [Full-app experiments](/contribute/experiments/) | The selected scenario through real daemons and databases |
| Documentation | [Documentation checks](/contribute/documentation/) | Site build, routes, fragments, TOML syntax, and checked inventories |

Read [CI](https://github.com/jakub/trawl/blob/main/.github/workflows/ci.yml)
and [lefthook.yml](https://github.com/jakub/trawl/blob/main/lefthook.yml) for the
checks required by the changed feature. Use `cargo fmt --check` for Rust formatting.
Wasm components require the Wasm target; a native workspace check does not compile
the component tree that only exists under `wasm32`.

```bash
cargo nextest run -p trawl-core parser
cargo clippy -p trawl-web-ui --target wasm32-unknown-unknown -- -D warnings
```

## Isolate PostgreSQL tests

The interactive `bin/dev` stack keeps persistent data. Use
[docker-compose.dev.yml](https://github.com/jakub/trawl/blob/main/docker-compose.dev.yml)
for disposable test databases. Select the connection through `DATABASE_URL`.
SQLx creates test databases, so the login needs the rights the suite expects.
Never point the suite at an installation you want to keep.

Identical concurrent SQLx suites can choose the same test database names even
when their connection URLs name different databases. Use separate PostgreSQL
clusters. The pre-push configuration does this with `TRAWL_TEST_DATABASE_URL`
and `TRAWL_TEST_ND_DATABASE_URL`.

## Browser tests

```bash
env -u NO_COLOR cargo xtask e2e --grep 'scenario'
```

The task runner builds the SPA and runs Playwright against a stub API.
This checks browser routing, input, overlays, and response handling. It does not
prove real authentication, PostgreSQL behavior, or durable ingestion. Trunk can
reject an inherited `NO_COLOR=1`, so the example unsets it.

Parallel checkouts need distinct `E2E_PORT` values. Keep the suite's single
worker and `reuseExistingServer: false`. Run focused cases after a change;
run the required broader suite before delivery. Mutation checks need a passing
baseline, the intended failure with the mutation, and a passing control.

The [browser test runbook](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/e2e/README.md)
owns setup, browser requirements, and mutation commands.

## Generate adversarial test logs

Use a disposable data root and catalog. These inputs intentionally create new
field pins and conflicts. Keep the same seed and namespace between related phases.

```bash
cargo xtask ingest-fuzz --phase mutate --seed 42 --events 1000 > /tmp/trawl-fuzz.ndjson
cargo xtask ingest-fuzz --phase pin --seed 42 --namespace ci > /tmp/pin.ndjson
# Send pin.ndjson through the selected test server's HTTP ingest route.
# Wait for that batch to compact before sending conflicts.ndjson.
cargo xtask ingest-fuzz --phase conflicts --seed 42 --namespace ci > /tmp/conflicts.ndjson
```

The generator writes NDJSON to stdout and its row-count summary to stderr.
`mutate` covers scalar boundaries, nested values, derivation inputs, field-name
collisions, and repair paths. `rejects` produces valid JSON with invalid envelope
values so a test can assert per-event rejection.

`--profile syslog` and `--profile trawld` shape the generated payload for those
producers. Posting either file over HTTP still stamps `_producer=http`.
The [profile tests](https://github.com/jakub/trawl/blob/main/crates/trawl-server/tests/profile_fuzz.rs)
exercise each payload through its own producer entry point.

The pin phase covers BIGINT, DOUBLE, TIMESTAMP, BOOLEAN, and VARCHAR candidates,
all-null deferral, and the 9/10 and 8/10 BIGINT thresholds. If pin and conflict
events compact together, they test a different first-observation contract.

A Vector file source can decode the NDJSON before sending it to the normal HTTP sink:

```toml
[sources.trawl_fuzz]
type = "file"
include = ["/tmp/trawl-fuzz.ndjson"]
read_from = "beginning"
framing.method = "newline_delimited"
decoding.codec = "json"
```

Use the TLS and key handling in [Vector integration](/getting-started/vector-integration/)
for the selected test destination. Check accepted counts, then assert the stored
rows and catalog evidence. An HTTP success alone does not prove the conflict case.
