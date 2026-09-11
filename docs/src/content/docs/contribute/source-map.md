---
title: Source map
description: Find the owner and the relevant verification path without loading an implementation inventory.
---

Start with the behavior being changed, then read the owning module and nearby tests. This map points to shared rules; it does not duplicate their implementations. Paths below are relative to the repository root.

## Query semantics

| Concern | Owner | Verification starting point |
| --- | --- | --- |
| Grammar, names, comments | `crates/trawl-core/src/parser/` | Parser tests and formatter round trips |
| Declared fields and type vocabulary | `trawl-core/src/schema.rs` | Schema tests and API parity tests |
| Guarded casts | `trawl-core/src/conform.rs` | `trawl-engine/tests/duckdb_probe.rs` |
| Pin-aware comparison and scope | `trawl-core/src/compare.rs`, `pin_scope.rs` | `trawl-core/tests/filter_parity.rs`, `pin_stage_parity.rs` |
| SQL construction and admission | `trawl-core/src/emitter/` | Emitter snapshots and complexity-admission tests |
| Live evaluation | `trawl-core/src/filter.rs`, `stream.rs`, `eval.rs` | SQL/Rust parity tests |
| DuckDB execution and failure policy | `trawl-engine/src/executor.rs` | Engine integration tests and `cold_action` matrix |
| Query work lifetime | `trawl-server/src/pool.rs` | Deadline, cancellation, and retained-work tests |

Paths in the tables that begin with a crate name are beneath `crates/`. The module documentation explains why one expression or policy table is shared by multiple consumers. Read those consumers before changing a shared rule.

## Ingest, storage, and recovery

| Concern | Owner |
| --- | --- |
| Event repair and profile derivation | `trawl-server/src/ingest/envelope.rs`, `producer.rs` |
| WAL and publication | `trawl-server/src/ingest/`, publication-consistency integration tests |
| Catalog authority and evidence | `trawl-server/src/catalog/`, `src/store/catalog.rs` |
| Storage epoch | `trawl-server/src/epoch.rs` |
| Repin lifecycle and cutover | `trawl-server/src/repin/` |
| Retention | `trawl-server/src/retention.rs` |
| Report windows | `trawl-server/src/scheduler.rs`, app-state store |
| Internal telemetry | `trawl-server/src/telemetry.rs` |

A change to file publication can affect query selection, export, field-value sampling, compaction, and rollup. A change to conformance can affect hot queries, stored values, boot recovery, and repin. Follow references to the named shared owner rather than adding a local variant.

## Clients and shared Fleet code

`trawl-cli/src/lib.rs` owns the CLI command declarations; `src/tui/` owns terminal behavior. `trawl-client` provides the typed HTTP client, and `trawl-api` defines shared wire values.

`trawl-web` serves the browser and translates sessions into bearer credentials. `trawl-web-ui` owns browser requests, routes, and app-specific state. `fleet-ui` owns reusable components, focus behavior, and visual tokens. `fleet-auth` owns the shared keystore and session functions; Trawl's `policy.rs` owns Trawl permissions. `fleet-admin` manages the keystore, while `trawl-admin` handles certificate tools.

Fleet crates also serve Coastwatch through sibling dependencies. A shared change needs both consumer checks where applicable. The [Fleet UI glossary](https://github.com/jakub/trawl/blob/main/crates/fleet-ui/context.md) and [SPA glossary](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/context.md) define their local terms. Shared vocabulary lives in [context.md](https://github.com/jakub/trawl/blob/main/context.md).

## Choose verification

[Local development](/getting-started/development/) describes the persistent interactive stack. Agent tests need disposable infrastructure. README and `lefthook.yml` describe focused test commands and broader automated gates; Cargo manifests own feature and target selection.

The [full-app experiment](https://github.com/jakub/trawl/blob/main/scripts/app-experiment/README.md) uses real daemons, PostgreSQL, ingest, and Chromium. The [browser E2E suite](https://github.com/jakub/trawl/blob/main/crates/trawl-web-ui/e2e/README.md) uses a stub API. Neither is a substitute for the other's claims, and neither demonstrates Firefox or Safari behavior unless separately tested.

## Decisions

Use the [ADR index](/contribute/decisions/) to find rationale and amendments. Current source and tests establish what is implemented; an accepted proposal or an old test report alone does not establish current runtime behavior.
