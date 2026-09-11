---
title: Architecture overview
description: Processes, data ownership, and query boundaries.
---

Trawl runs log storage and query execution on one node. `trawld` accepts events, writes a durable ingest log, and answers queries over recent events and Parquet files. Server operation requires PostgreSQL. Embedded CLI queries over local Parquet do not require the daemon or PostgreSQL.

## Components

```text
Vector / HTTP sender ──bearer token──┐
Syslog devices ──TCP or UDP─────────┤
CLI / TUI ──bearer token────────────┤
                                  ▼
Browser ──session cookie──> trawl-web ──bearer token──> trawld
                            │                         │
                            └─ serves the SPA         ├─ Fleet PostgreSQL keystore
                                                      ├─ Trawl PostgreSQL app state
                                                      ├─ WAL and hot buffer
                                                      └─ hourly / daily Parquet
```

The browser proxy translates a session cookie into an API credential. Ingest clients connect to `trawld` directly; the browser proxy does not accept the ingest route. Syslog listeners are optional. The daemon's own telemetry enters the same event canonicalizer through a distinct producer profile.

The Fleet keystore stores keys, roles, and permissions. Trawl's database stores query history, saved queries, schedules, report-run metadata, and the field catalog. These are separate databases and may use the same PostgreSQL instance. Trawl holds an app-state advisory lock for its lifetime and migrates its own database at boot. Fleet migrations run through `fleet-admin`.

### Workspace structure

The [contributor source map](/contribute/source-map/) identifies crate boundaries and the modules that own shared contracts. A crate is not necessarily a deployed process: the shared Fleet crates are libraries and tools consumed by Trawl and Coastwatch.

## Design principles

- One node owns ingest and compaction. There is no distributed write coordination, sharding, or multi-tenancy.
- WAL durability precedes hot-buffer and event-bus publication. Recent events can be queried before Parquet compaction.
- Field names can appear dynamically, but the catalog pins their types. Conformance applies the same interpretation to hot and compacted values.
- Parquet is an open storage format. Embedded tools can read it, but files placed in Trawl's data tree do not automatically satisfy its catalog or ownership rules.
- The DSL compiles into parameterized DuckDB SQL. Live streams use a separate Rust evaluator with shared comparison rules and parity tests.

### Authentication

Roles are data, and permissions are code. A key can hold several roles; its effective permissions are their union. Handlers gate on recognized permissions, not names such as `admin`. A role with no recognized Trawl permissions does not grant Trawl access. See [the permission reference](/reference/api/) and [ADR-0006](/contribute/decisions/#adr-0006).

### Server middleware stack

Bearer authentication establishes key identity before per-key rate limiting. Trawl's policy layer rejects keys with no Trawl grant. Individual handlers then enforce their permissions. The router also applies request limits, security headers, tracing, and optional CORS. `/api/v1/health` and `/metrics` are outside the bearer-protected API routes. The ingest route requires authentication and ingest permission.

The browser has another boundary: the session proxy validates a present `Origin` against configured public origins for protected state-changing requests. Shared cookie material alone does not make every origin trusted.

### TLS

The daemon serves HTTPS with rustls and polls certificate content for reload. Browser deployment may add an HTTPS reverse proxy. Configure the browser's public origins to match the actual scheme, host, and port; see [configuration](/reference/configuration/).

### Internal telemetry

The daemon records its own admitted telemetry through WAL before making it visible. This does not capture the whole deployment or provide a transactional audit ledger. See [reports and telemetry](/architecture/reports-telemetry/#internal-telemetry).

## Follow the data

Start with [ingest and publication](/architecture/data-flow/), then read [catalog conformance](/architecture/catalog/), [query execution](/architecture/query-execution/), or [recovery](/architecture/recovery/) for the mechanism involved in your task.

## Comparison to Splunk

### Similarities

Trawl supports pipeline syntax and familiar spellings such as `head`, `eval`, `rex`, and `fields`.

### Divergences

That familiarity is not a compatibility or performance guarantee. Its own [DSL reference](/reference/dsl/) defines query behavior.

## Comparison to Datadog

Trawl is self-hosted and single-node. This documentation does not claim performance equivalence with a hosted log service. Evaluate your workload with known data and measured queries.

## Scaling roadmap

See [project direction](/about/roadmap/) for current capabilities and boundaries. Proposed storage extensions are not implemented features or release commitments.
