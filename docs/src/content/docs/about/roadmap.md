---
title: Roadmap
description: What's done, what's planned, and what's out of scope.
---

## What's done

trawl's core is production-quality. Here's what's shipped:

### Query language
Complete pipeline DSL with 14 pipe stages, 17 aggregation functions, 12 scalar functions, OR groups, time filters, and regex/glob/KV extraction. Parameterized SQL emission throughout — no injection vectors. `CompiledFilter` provides in-memory matching for SSE streaming, property-tested against DuckDB output for equivalence.

### Ingestion pipeline
ndjson/JSON ingest → WAL → hot buffer (visible within milliseconds) → hourly parquet compaction → daily rollup. Event bus with broadcast channel for real-time streaming. Age-based + disk-pressure retention policies.

### Server
axum with a full middleware stack: rate limiting per role, CORS, HSTS, concurrency limits, request IDs. TLS with auto-generated self-signed certs and hot-reload. Prometheus metrics. Internal telemetry that feeds server ops back into its own pipeline.

### Authentication
argon2id-hashed API keys with four roles (admin, analyst, reader, ingest). SQLite-backed with LRU-cached lookups and timing-safe dummy hashes.

### CLI
Query, validate, and embedded mode. Four output formats (table, JSON, CSV, parquet). Formula injection protection on CSV export.

### TUI
Multi-tab editor with syntax highlighting, schema browser with field profiling, query history, saved queries, live tail via SSE, vim-style result search, clipboard integration, and driver mode for programmatic control.

### Scheduled reports
Cron-style periodic query execution with zstd-compressed result storage, crash recovery, and atomic run tracking.

### Testing
~100 emitter snapshots, filter/SQL parity property test, ~30 engine integration tests against fixture parquet, ~40 HTTP integration tests against real server instances, hot buffer pipeline end-to-end test.

### Release infrastructure
Cross-compiled binaries (x86_64 + aarch64 Linux), `.deb` packages, APT repository, GitHub releases.

## What's next

### Documentation
You're reading the first pass. Getting-started guides, Vector integration cookbook, and configuration reference are in progress.

### Upgrade and migration story
Schema versioning for the auth SQLite database, embedded migration runner, and a documented breaking change policy.

### Graceful degradation
Explicit handling and recovery guidance for corrupted parquet files, truncated WAL files, and locked auth databases.

### Alerting and webhooks
The scheduled reports infrastructure is a natural foundation — the next step is "when this condition fires, POST to a webhook or send to Slack."

### S3/MinIO cold storage
DuckDB's `httpfs` extension makes this relatively cheap. Daily rollup parquets upload to S3 or a MinIO instance, extending retention from "however much disk you have" to effectively infinite.

### Timestamp-sorted compaction
Daily rollups already sort by timestamp, but hourly compaction writes events in ingest order. Sorting row groups by timestamp during hourly compaction enables parquet min/max pruning for `last=Xh` and range queries — a cheap win for the most common query shape.

### Parquet bloom filter sidecars
DuckDB can read parquet bloom filters natively. Emitting them on high-cardinality exact-match fields (`host`, `trace_id`, `level`) during compaction prunes file lists for point lookups without maintaining a separate inverted index. Closes much of the gap with Splunk's tsidx on exact-match queries while staying within the open-parquet thesis.

### Config reload on SIGHUP
TLS certs already hot-reload, but the rest of the config requires a restart.

### Cross-platform CI
CI currently runs on Ubuntu only. macOS compilation testing is planned.

## Out of scope

These are explicitly **not** on the roadmap. They fight the single-node design thesis.

- **Multi-node write sharding** — consistent hashing, coordination layer, rebalancing. If you need this, you've outgrown trawl.
- **Distributed query execution** — scatter-gather, partial aggregation, network partition tolerance. This is building a distributed database.
- **Custom storage format** — parquet is industry standard, well-optimized, and queryable by any tool. The complexity cost of a custom format is astronomical.
- **Consensus-based metadata store** — raft/paxos for cluster membership. Needed for exactly zero of trawl's requirements.
