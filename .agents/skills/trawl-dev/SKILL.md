---
name: trawl-dev
description: Run Trawl's local development stack or select build and test commands for a repository change.
---

# Local development and verification

Run from the selected checkout. Read [README](../../../README.md) for
prerequisites and [local development](../../../docs/src/content/docs/getting-started/development.md)
for the Fleet controller's profiles, state, and recovery.

## Choose the environment

For a requested interactive dev session, `bin/fleet-dev plan trawl --format human`
shows the selected stack without resolving secrets. `bin/fleet-dev doctor trawl`
checks prerequisites. `bin/dev` starts the attached stack; `--release-spa` uses
an optimized SPA. These wrappers may build the controller through Cargo.

The controller can select a machine profile, including a remote database.
Inspect the plan before starting. Its Docker volume persists after exit.
Tests use disposable databases, never this interactive data. Use
[trawl-experiment](../trawl-experiment/SKILL.md) for isolated real-app checks.

## Choose the evidence

| Change | Starting point |
| --- | --- |
| Rust behavior | Focused `cargo nextest run -p CRATE` with the relevant test filter; `lefthook.yml` and CI describe broader gates |
| PostgreSQL behavior | Disposable cluster selected through `DATABASE_URL`; consult `docker-compose.dev.yml` and `lefthook.yml` before parallel suites |
| Wasm components | Targeted `cargo clippy -p CRATE --target wasm32-unknown-unknown -- -D warnings`; native checks do not compile the Wasm component tree |
| Embedded browser distribution | `env -u NO_COLOR cargo xtask build-web --release` builds and precompresses the SPA before embedding it in `trawl-web` |
| Browser interactions | `env -u NO_COLOR cargo xtask e2e --grep 'scenario'`; see the [E2E guide](../../../crates/trawl-web-ui/e2e/README.md) |
| Full API, auth, ingest, compaction, and restart | [trawl-experiment](../trawl-experiment/SKILL.md) |

Cargo defaults use the downloaded DuckDB shared runtime, including ICU, JSON,
and Parquet. Cargo provides its loader path for `cargo run` and tests; an
already-built development daemon uses `bin/trawld-dev`. For relocatable
artifacts, use the documented distribution build helper instead of copying a
Cargo executable alone.

Unset `NO_COLOR` for Trunk because the installed version rejects its inherited
value of `1`. The Playwright suite uses a stub API. It does not prove real
authentication or database behavior. Parallel checkouts need distinct `E2E_PORT` values; keep
`reuseExistingServer: false` and the suite's single worker.

SQLx test databases can collide across identical concurrent suites even when
their connection URLs name different databases. Use separate disposable
Postgres clusters, as the two pre-push suites do. Their override variables are
`TRAWL_TEST_DATABASE_URL` and `TRAWL_TEST_ND_DATABASE_URL`.

Use `cargo fmt --check` for Rust formatting. Follow the changed feature's checks
in [CI](../../../.github/workflows/ci.yml), including Fleet UI's atmosphere
feature when relevant. Stop broadening verification once the required checks
pass unless a change or failure introduces a new concern.
