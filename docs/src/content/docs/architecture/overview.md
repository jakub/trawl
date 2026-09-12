---
title: Architecture overview
description: Understand which process owns each part of Trawl and where the trust boundaries sit.
---

Why does one node run the whole system? Trawl targets homelab and small-installation log volumes, where one machine holds the corpus and answers every query. A single owner of ingest and compaction removes distributed write coordination, sharding, and multi-tenancy from the design. The trade-off is a ceiling. You grow Trawl by giving the node more disk and more cores, not by adding nodes.

## Components

```text
Vector or HTTP sender ──bearer token───┐
Syslog device ──TCP or UDP─────────────┤
CLI and TUI ──bearer token─────────────┤
Browser ──session cookie──> trawl-web ─┤
                                       ▼
                                     trawld
                                       │
                                       ├─ Fleet PostgreSQL keystore
                                       ├─ Trawl PostgreSQL app state
                                       ├─ WAL and hot buffer
                                       └─ hourly and daily Parquet
```

`trawld` accepts events, writes a durable ingest log, and answers queries over recent events and Parquet files. Ingest clients connect to it directly.

`trawl-web` serves the browser application and exchanges a session cookie for a bearer token on each forwarded request. It refuses `/api/v1/ingest`, so the browser can never ingest. Syslog listeners are optional. The daemon's own telemetry enters the same canonicalizer through the `trawld` producer profile.

Server operation requires PostgreSQL. Embedded CLI queries over local Parquet need neither the daemon nor PostgreSQL.

Two databases sit behind the daemon. The Fleet keystore holds keys, roles, and permissions. Trawl's app-state database holds query history, saved queries, schedules, report-run metadata, and the field catalog. Both can live in one PostgreSQL instance. `trawld` holds an app-state advisory lock for its lifetime and migrates its own database at boot. Fleet migrations run through `fleet-admin`.

A crate is not a process. The shared Fleet crates are libraries and tools that Trawl and another Fleet application consume. The [source map](/contribute/source-map/) names the crate boundaries.

## Design principles

- WAL durability comes before hot-buffer and event-bus publication, so recent events are queryable before compaction.
- Field names can appear at any time, and the catalog pins their types. Hot and compacted values get the same interpretation.
- Parquet is an open format that embedded tools can read. A file you drop into Trawl's data tree still does not satisfy the catalog and ownership rules.
- The DSL compiles into parameterized DuckDB SQL. Live streams use a separate Rust evaluator that shares the comparison rules and is covered by parity tests.
- The DSL borrows pipeline spellings such as `head`, `eval`, `rex`, and `fields`. That familiarity is not a Splunk compatibility guarantee, and the [DSL reference](/reference/dsl/) defines query behavior.

## Authentication

Roles are data and permissions are code. A key can hold several roles, and its effective permissions are the union of theirs. Handlers gate on recognized permission strings, not on a role name such as `admin`. A role that carries no recognized Trawl permission grants no Trawl access. See the [permission reference](/reference/api/#roles-and-permissions).

## Server middleware stack

The API router runs bearer authentication first, so key identity exists before anything else reads it. The Trawl grant check then rejects a key with no Trawl permission. Per-key rate limiting runs next, and each handler enforces its own permission last. The router also applies a body limit, a connection limit, security headers, and CORS when you configure origins. `/api/v1/health` and `/metrics` sit outside the authenticated routes. The ingest route uses the same authentication stack with its own body limit and its own rate-limit buckets.

The browser adds a second boundary. `trawl-web` validates a present `Origin` header against the configured public origins for state-changing requests. Shared cookie material alone does not make an origin trusted.

## TLS

The daemon serves HTTPS with rustls and polls its certificate files so it can reload them without a restart. A browser deployment may add an HTTPS reverse proxy. Set the browser's public origins to the scheme, host, and port that reach it. See [configuration](/reference/configuration/#web).

## Internal telemetry

The daemon records its own admitted telemetry through the WAL before making it visible. That covers only `trawld`, and it is not a transactional audit ledger for the deployment. See [reports and telemetry](/architecture/reports-telemetry/#internal-telemetry).

## Follow the data

Start with [ingest and publication](/architecture/data-flow/). Then read [catalog conformance](/architecture/catalog/), [query execution](/architecture/query-execution/), or [recovery](/architecture/recovery/) for the mechanism your task touches.
