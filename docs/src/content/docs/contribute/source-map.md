---
title: Source map
description: Find the crate or module that owns the behavior you are about to change.
---

Start from the behavior you are changing, then read the owning module and the
tests beside it. This map points at owners. It does not repeat what they do.
Paths are relative to the repository root.

## Crates in the workspace

| Crate | Owns |
| --- | --- |
| `crates/trawl-core` | DSL parser, AST, comparison rules, and the SQL emitter |
| `crates/trawl-engine` | DuckDB execution and failure policy |
| `crates/trawl-server` | `trawld`: ingest, catalog, storage, scheduler, and the HTTP API |
| `crates/trawl-api` | Shared wire types for the HTTP API |
| `crates/trawl-client` | Typed HTTP client for `trawld` |
| `crates/trawl-config` | Shared config types and filesystem helpers |
| `crates/trawl-cli` | `trawl` command declarations and the TUI |
| `crates/trawl-dashboard` | Shared ratatui dashboard rendering |
| `crates/trawl-admin` | Local TLS certificate tooling |
| `crates/trawl-crashdump` | Crash-dump capture |
| `crates/trawl-web` | Browser session proxy that serves the SPA and adds bearer tokens |
| `crates/trawl-web-ui` | Leptos SPA, its routes, and app-specific state |
| `crates/fleet-auth` | Shared keystore, API keys, sessions, and origin checks |
| `crates/fleet-admin` | Keystore CLI: migrations, session keys, key lifecycle |
| `crates/fleet-ui` | Shared Leptos components, focus behavior, and visual tokens |
| `crates/fleet-dev` | Local development controller behind `bin/dev` |
| `xtask` | Workspace task runner: `build-web`, `e2e`, `ingest-fuzz`, and more |

## Query semantics

| Concern | Owner | Start verification at |
| --- | --- | --- |
| Grammar, names, comments | `crates/trawl-core/src/parser/` | Parser tests and formatter round trips |
| Declared fields and type vocabulary | `crates/trawl-core/src/schema.rs` | Schema tests and API parity tests |
| Guarded casts | `crates/trawl-core/src/conform.rs` | `crates/trawl-engine/tests/duckdb_probe.rs` |
| Pin-aware comparison and scope | `crates/trawl-core/src/compare.rs`, `pin_scope.rs` | `crates/trawl-core/tests/filter_parity.rs`, `pin_stage_parity.rs` |
| SQL construction and admission | `crates/trawl-core/src/emitter/` | Emitter snapshots and admission tests |
| Live evaluation | `crates/trawl-core/src/filter.rs`, `stream.rs`, `eval.rs` | SQL and Rust parity tests |
| DuckDB execution and failure policy | `crates/trawl-engine/src/executor.rs` | Engine integration tests |
| Query work lifetime | `crates/trawl-server/src/pool.rs` | Deadline, cancellation, and retained-work tests |

## Ingest, storage, and recovery

| Concern | Owner |
| --- | --- |
| Event repair and profile derivation | `crates/trawl-server/src/ingest/envelope.rs`, `producer.rs` |
| WAL and publication | `crates/trawl-server/src/ingest/`, `publication.rs` |
| Catalog authority and evidence | `crates/trawl-server/src/catalog/`, `src/store/catalog.rs` |
| Storage format marker | `crates/trawl-server/src/epoch.rs` |
| Repin lifecycle and cutover | `crates/trawl-server/src/repin/` |
| Retention | `crates/trawl-server/src/retention.rs` |
| Report windows | `crates/trawl-server/src/scheduler.rs`, `report_window.rs` |
| Internal telemetry | `crates/trawl-server/src/telemetry.rs` |
| Trawl permissions | `crates/trawl-server/src/policy.rs` |

A change to file publication can reach query selection, export, field-value
sampling, compaction, and rollup. A change to conformance can reach hot queries,
stored values, boot recovery, and repin. Follow the reference to the named owner
instead of adding a local variant.

## Shared Fleet code

The Fleet crates also serve another Fleet application through sibling path
dependencies, so a change to a shared contract needs both consumers checked.
The [Fleet UI glossary](https://github.com/jakub/trawl/blob/main/crates/fleet-ui/context.md)
and the [SPA glossary](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/context.md)
define their local terms. Shared vocabulary lives in
[context.md](https://github.com/jakub/trawl/blob/main/context.md).

## Choose verification

[Local development](/getting-started/development/) runs the interactive stack.
[Test a change](/contribute/testing/) picks the check for your change.
The [full-app experiment](/contribute/experiments/) uses real daemons,
PostgreSQL, ingest, and Chromium, while the
[browser suite](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/e2e/README.md)
uses a stub API. Neither substitutes for the other, and neither says anything
about Firefox or Safari.
