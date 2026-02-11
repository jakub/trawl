# fleet

self-hosted log collection, storage, and search platform for homelabs and small-to-medium infra. splunk-like DSL, zero licensing cost, single-node by design.

## stack

- **core**: rust workspace — parser, SQL emitter, DuckDB executor, daemon, CLI, TUI
- **ingestion**: vector → parquet (columnar, compressed, partitioned by hour)
- **query engine**: custom DSL → AST → DuckDB SQL (parameterized)
- **web ui**: rails 8 (thin proxy over fleetd HTTP API)
- **agent** (v2 scope): signed-template execution on managed endpoints, mTLS, ed25519 signing

## workspace layout

```
crates/
  fleet-core/     # DSL parser, AST, SQL emitter (pure, no I/O)
  fleet-engine/   # DuckDB integration, query execution
  fleet-auth/     # API keys, roles, SQLite-backed
  fleet-server/   # daemon (axum, unix socket, TCP+TLS)
  fleet-client/   # shared client library
  fleet-cli/      # CLI tool
  fleet-admin/    # admin CLI (key mgmt, templates, enrollment)
```

## key design decisions

- single-node only. no clustering, sharding, or multi-tenancy.
- pipeline-oriented DSL: `service:nginx level:error last:2h | stats count() by host | where count > 10`
- SQL injection prevention via parameterized queries + field allowlists
- agent tasks are cryptographically signed offline — compromised server can't create novel execution authority

## tooling

- **edition 2024**, resolver 3, rust-version 1.85 MSRV
- **clippy pedantic** + `unsafe_code = "forbid"` at workspace level
- **lefthook** pre-commit: `cargo fmt --check` + `cargo clippy -- -D warnings`
- **bacon** for continuous clippy-on-save (`bacon` or `bacon test`)
- **cargo-nextest** for testing, **cargo-insta** for snapshot tests
- **cargo-deny** for license/vulnerability auditing

## docs

- `docs/overview.md` — project overview and architecture thesis
- `docs/initial_plan.md` — 15-phase implementation plan with detailed per-phase breakdowns
