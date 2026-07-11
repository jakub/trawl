---
title: Architecture Overview
description: Design philosophy, component architecture, and comparisons to Splunk and Datadog.
---

trawl is a self-hosted log collection, storage, and search platform for homelabs and small-to-medium infrastructure. It combines Vector for log ingestion, Apache Parquet for columnar storage, DuckDB for analytical query execution, and a custom DSL inspired by Splunk's SPL.

The core thesis: modern storage formats and embedded analytical databases have made single-node log search viable at scales that previously demanded distributed clusters.

## Design principles

- **Single-node only.** No clustering, sharding, or multi-tenancy. One daemon, one machine.
- **Open formats.** Parquet files are queryable by any tool — DuckDB, pandas, Spark, or trawl's embedded mode.
- **Immediate visibility.** Events are queryable within milliseconds of ingest via the hot buffer, before WAL compaction to parquet.
- **Pipeline DSL.** Intuitive search syntax that compiles to optimized DuckDB SQL. No SQL knowledge required.
- **Zero licensing cost.** Everything is open source. No per-GB pricing, no seat licenses.

## Components

### Workspace structure

trawl is implemented as a Rust workspace with clean crate boundaries:

| Crate | Purpose |
|-------|---------|
| `trawl-core` | DSL parser, AST, SQL emitter (pure, no I/O) |
| `trawl-engine` | DuckDB integration, query execution |
| `trawl-auth` | API keys, roles, schedules (SQLite-backed) |
| `trawl-api` | Shared wire types (request/response structs) |
| `trawl-server` | Daemon (axum, HTTPS via tokio-rustls) |
| `trawl-client` | Typed async HTTP client library |
| `trawl-cli` | Unified CLI + TUI binary |
| `trawl-admin` | Admin CLI (TLS certs; keys live in `fleet-admin`) |

The parser and emitter in `trawl-core` are pure logic with no I/O dependencies — testable in isolation and potentially compilable to WASM.

### Server middleware stack

Outermost to innermost:

1. `CatchPanicLayer` — catch panics without crashing the server
2. `ConcurrencyLimitLayer(256)` — global connection limit
3. Security headers (HSTS, nosniff)
4. Optional CORS
5. Request ID (ULID)
6. `TraceLayer` — structured logging
7. `RequestBodyLimitLayer` — body size limits
8. Auth — token validation
9. Rate limiting — per-role limits
10. Handler

### Authentication

API keys use `flt_` prefix + 43 base64url characters. Hashed with argon2id (128 MiB memory, 3 iterations, 4 lanes). Prefix-based SQLite lookup with a 5-minute DashMap cache. Timing-safe dummy hash for invalid tokens prevents oracle attacks.

Four roles: admin, analyst, reader, ingest.

### TLS

Manual accept loop with rustls. Certificate hot-reload via content-based polling (default 300s). HTTP/1.1 and HTTP/2 via ALPN negotiation.

## Comparison to Splunk

### Similarities

- Pipeline-oriented DSL modeled on SPL, with SPL aliases (`head`=`limit`, `eval`=`let`, `rex`=`extract`, `fields`=`table`)
- Search-time field extraction via named capture groups and KV extraction
- Time-partitioned storage with hot/warm tiering (WAL → hourly parquet → daily parquet)
- Hot bucket equivalent: events immediately queryable before persistent write

### Divergences

| Aspect | Splunk | trawl |
|--------|--------|-------|
| Storage format | Proprietary tsidx + journal.gz | Open parquet (columnar, Snappy) |
| Query engine | Custom C++ engine | DuckDB (embedded, OLAP-optimized) |
| Indexing | Inverted index per field at ingest | No index — column scan via parquet stats |
| Schema | Index-time discovery + `props.conf` | Fully dynamic, `union_by_name=true` |
| Clustering | Search head + indexer cluster | Single node only |

The indexing tradeoff: Splunk is faster for high-cardinality exact-match lookups, trawl is simpler and avoids ingest-time overhead. DuckDB's vectorized execution and parquet statistics close much of the gap.

## Comparison to Datadog

Datadog is fundamentally different — multi-tenant SaaS with tag-based indexing, tiered storage (RAM → SSD → S3), pre-aggregation at ingest, and ML-based log clustering.

The key architectural difference: Datadog indexes everything at ingest time for O(1) reads. trawl indexes nothing and relies on DuckDB's columnar scan + parquet statistics. This is the correct tradeoff for trawl's scale target.

## Scaling roadmap

See the [data flow](/architecture/data-flow/) page for details on the ingestion pipeline and query execution path. The architecture supports several scaling extensions without breaking the single-node thesis:

1. **S3/MinIO cold storage** — extend retention from "however much disk you have" to effectively infinite via DuckDB's `httpfs` extension
2. **Read-only query replicas** — parquet files are immutable after write; any node with filesystem access can query them
3. **Bloom filter sidecars** — prune file lists for high-cardinality exact-match lookups without DuckDB changes
