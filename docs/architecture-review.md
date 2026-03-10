# trawl architecture review

comparative analysis against splunk/datadog, with scaling roadmap.

## data flow summary

```
vector (gzip json batch, 1MB/5s)
  → POST /ingest (rate limit: 1000/min, body: 16MB max)
    → auth middleware (argon2id, 5min DashMap cache)
    → spawn_blocking:
        decompress → parse json/ndjson → validate service name
        → WalWriter::write() (atomic tmp→rename, ndjson)
    → bus.publish(Arc<IngestBatch>)
        ├→ HotBuffer consumer (IndexMap, FIFO eviction, 100k events / 100MB)
        └→ SSE subscribers (CompiledFilter, aho-corasick SIMD matching)

  every 10s: compaction tick
    scan WAL for .ndjson older than 10s
    → group by service
    → mark_draining on hot buffer (TOCTOU prevention)
    → spawn_blocking: ephemeral DuckDB
        read_json_auto([wal files]) → CAST timestamp
        → UNION ALL BY NAME with existing parquet (if any)
        → COPY TO data/YYYY-MM-DD/HH/service.parquet (atomic rename)
    → drain hot buffer, delete WAL files

  daily: rollup
    hourly parquets → single daily parquet per service (sorted by timestamp)
    crash-safe via .rollup-{service} marker files

  query:
    DSL → chumsky parser → AST → CTE-chained SQL → DuckDB (parameterized)
    source = compute_source() narrows glob by time + service (filesystem stat)
    hot buffer snapshot() → UNION ALL BY NAME with parquet
    pool of N shared-DB DuckDB connections (semaphore-gated, num_cpus)
    timeout via DuckDB interrupt handle

  SSE streaming:
    completely separate path — no DuckDB
    CompiledFilter (in-memory matcher) against broadcast channel
    back-pressure via Lagged events, bounded by sse_semaphore (32)
```

## core components

### ingestion pipeline

- **ingest handler** (`trawl-server/src/ingest/handler.rs`): accepts JSON array or ndjson, auto-detected by first byte. all CPU-bound work (gzip, parse, WAL write) runs in `spawn_blocking`. timing captured per phase: `decompress_ms`, `parse_ms`, `wal_ms`.
- **WAL writer** (`trawl-server/src/ingest/wal.rs`): atomic write via tmp→rename. filename format: `{service}_{unix_millis}_{4_hex_random}.ndjson`. service name validated against `[a-zA-Z0-9\-_.]` to prevent path traversal.
- **event bus** (`trawl-server/src/bus.rs`): `tokio::sync::broadcast` channel (capacity 4096). publishes `Arc<IngestBatch>` — all subscribers share the same allocation. lossy by design: slow consumers get `RecvError::Lagged(n)`, never block the producer.
- **hot buffer** (`trawl-server/src/hot_buffer.rs`): `RwLock<IndexMap<Arc<str>, Arc<IngestBatch>>>`. FIFO eviction (100k events / 100MB). generation-based snapshot cache: concurrent queries share a single temp ndjson file via `Arc<NamedTempFile>`. TOCTOU-safe draining protocol: `mark_draining` → write parquet → `drain`.

### query execution

- **parser** (`trawl-core/src/parser/`): chumsky combinators. search stage parses OR-separated groups of AND-joined tokens. time filters hoisted globally. 65KB length guard.
- **emitter** (`trawl-core/src/emitter/`): transforms AST → CTE-chained DuckDB SQL. each pipe stage boundary creates a new CTE (`_s0`, `_s1`, ...). all user values parameterized via `?` placeholders except: (a) source paths (validated via allowlist), (b) PIVOT params (DuckDB limitation, inlined with quote-doubling).
- **executor** (`trawl-engine/src/executor.rs`): wraps `duckdb::Connection`. pool of N connections sharing one in-memory database via `try_clone()` (shared parquet metadata cache). cold-start fallback: if no parquet files exist, queries hot buffer only.
- **executor pool** (`trawl-server/src/pool.rs`): semaphore-gated (default: num_cpus). timeout via `DuckDB::InterruptHandle`. cancel via `DELETE /queries/{id}`.
- **source computation** (`trawl-server/src/source.rs`): narrows parquet glob by time filter + service filter using filesystem `stat()`. 1-hour padding for compaction lag. handles both hourly and daily layouts.

### SSE streaming

completely separate code path from SQL queries. `CompiledFilter` compiles the search stage into an in-memory matcher (aho-corasick for text search, regex for globs). filters against the broadcast channel, not DuckDB. bounded by `sse_semaphore` (default 32 concurrent streams).

### server infrastructure

- **middleware stack** (outermost→innermost): `CatchPanicLayer` → `ConcurrencyLimitLayer(256)` → HSTS/nosniff headers → optional CORS → request ID (ULID) → `TraceLayer` → `RequestBodyLimitLayer` → auth → rate limit → handler
- **auth** (`trawl-auth/`): `flt_` + 43 base64url chars. argon2id (128MiB/3 iter/4 lanes). prefix-based SQLite lookup. timing oracle prevention via `DUMMY_HASH`. 5-min TTL cache (DashMap).
- **TLS**: manual accept loop with `rustls`. cert hot-reload via content-based polling (default 300s). HTTP/1.1 + HTTP/2 via ALPN.
- **background tasks**: hot buffer consumer, compaction (10s), retention (1h), audit (30s), telemetry flush (1s), stats emitter (60s). ordered shutdown: telemetry → stats → hot buffer → compaction → retention → audit.

### storage

- **3-tier**: ndjson WAL → hourly parquet → daily parquet
- **partitioning**: `{data_dir}/{YYYY-MM-DD}/{HH}/{service}.parquet` (hourly), `{data_dir}/{YYYY-MM-DD}/{service}.parquet` (daily rollup)
- **schema**: fully dynamic. no predefined columns. `union_by_name=true` handles heterogeneity. timestamp cast to native `TIMESTAMP` during compaction for predicate pushdown.
- **retention**: age-based (default 90 days) + disk pressure (min 1 GiB free). unit of deletion: full date directory.
- **embedded mode** (`trawl query --data`): single ephemeral DuckDB connection, user-supplied glob, no hot buffer, no auth, no row limit.

## comparison to splunk

### similarities

- pipeline-oriented DSL modeled on SPL, with SPL aliases (`head`=`limit`, `eval`=`let`, `rex`=`extract`, `fields`=`table`)
- search-time field extraction via named capture groups and KV extraction
- time-partitioned storage with hot/warm tiering (WAL → hourly parquet → daily parquet ≈ hot/warm/cold buckets)
- hot bucket equivalent: events immediately queryable before persistent write

### divergences

| aspect | splunk | trawl | verdict |
|--------|--------|-------|---------|
| storage format | proprietary tsidx + journal.gz | open parquet (columnar, snappy) | trawl — parquet is queryable by anything |
| query engine | custom C++ engine | DuckDB (embedded, OLAP-optimized) | trawl — vectorized execution, predicate pushdown for free |
| indexing | inverted index per field at ingest time | no index — full column scan via parquet stats | tradeoff: splunk faster for high-cardinality lookups, trawl simpler |
| schema | index-time field discovery + `props.conf` | fully dynamic, `union_by_name=true` | trawl simpler but loses type safety |
| clustering | search head cluster + indexer cluster + deployment server | single node only | splunk's complexity is legendary and mostly unnecessary for trawl's target |

## comparison to datadog

datadog is fundamentally different — multi-tenant SaaS with:

- **tag-based indexing**: every metric/log indexed by tags at ingest → O(1) lookups. trawl does full scans.
- **tiered storage**: hot (RAM) → warm (SSD) → cold (S3) with automatic routing. trawl has WAL → parquet on local disk.
- **pre-aggregation**: metric rollups at ingest time (10s → 1m → 1h). trawl stores raw events only.
- **log patterns/clustering**: ML-based log grouping. trawl has nothing like this (and shouldn't — YAGNI).

key architectural difference: datadog indexes everything at ingest time for O(1) reads, trawl indexes nothing and relies on DuckDB's columnar scan + parquet statistics. correct tradeoff for trawl's scale target.

## scaling roadmap

### tier 1: high ROI, worth doing

#### 1. object storage tiering (S3/MinIO for cold data)

extends retention from "however much disk you have" to "effectively infinite."

- after daily rollup, upload to S3/MinIO
- delete local copy after upload confirmation
- queries with `last:>7d` (or whatever the local retention window) transparently query S3 via DuckDB's `httpfs` extension (`read_parquet('s3://bucket/path/*.parquet')`)
- `compute_source()` checks local disk first, falls back to S3 paths for older dates
- directory layout maps 1:1 to S3 key prefixes — natural fit

**complexity: medium. value: high for anyone with >30 days of data.**

#### 2. read-only query replicas

the single biggest scaling lever. the architecture already almost supports it:

- parquet files are immutable after write — any node with filesystem access can query them
- DuckDB connections are stateless — just read parquet via glob paths
- `compute_source()` is pure filesystem stat — no shared state needed
- the hot buffer is the only coupling: replicas accept ~10s staleness (the compaction interval)

concrete approach: a "query replica" that mounts the same `data/` directory (read-only NFS, or periodic rsync), runs its own DuckDB pool, has no hot buffer/WAL/ingest. trawl-client gets a list of query endpoints and round-robins. real-time stays on the primary via SSE.

**complexity: medium. value: high.**

#### 3. bloom filter sidecars

the biggest query performance gap vs. splunk: high-cardinality exact-match lookups. `service=nginx host=web-42` currently scans ALL parquet row groups even if `web-42` only appears in 3 of 10,000 files.

- write `.bloom` sidecar files during compaction for high-cardinality fields (`host`, `source`, `ip`)
- consult them in `compute_source()` to prune the file list before passing to DuckDB
- pure application-layer optimization, no DuckDB changes needed

**complexity: medium. value: medium-high for large deployments with many services/hosts.**

### tier 2: worth thinking about, not urgent

#### 4. WAL improvements

one WAL file per ingest batch per service → thousands of files at high ingest rates with many services. solutions: segment WAL by time window (1s per service), or single append-only WAL with index. not urgent — current approach works fine up to ~10k events/sec on modern SSDs.

#### 5. partitioning by more than service + time

adding `host` as a partition dimension would help `host:web-42` queries, but explodes file count (services × hosts × hours). parquet row group stats + bloom filters (tier 1 #3) get 80% of the benefit without the file explosion.

#### 6. pre-aggregation / materialized rollups

datadog-style metric rollups (10s → 1m → 1h) at ingest time. would speed up dashboard queries over long time ranges. but: trawl stores raw events not metrics, pre-aggregation requires knowing dimensions upfront, and DuckDB is surprisingly fast at scanning sorted parquet. measure first.

### tier 3: naval gazing (resist the urge)

#### 7. multi-node write sharding

consistent hashing by service name across multiple ingest nodes. needs coordination layer (etcd/consul), rebalancing is genuinely hard, and the current single node handles tens of thousands of events/sec. the target audience is "homelabs and small-to-medium infra" — if you need multi-node writes, you've outgrown trawl's design thesis. this is the splunk indexer cluster path. it took splunk years and thousands of engineers to get right.

#### 8. distributed query execution

scatter-gather across multiple nodes. requires query planning, partial aggregation, merge, network partition tolerance. this is literally building a distributed database. DuckDB's vectorized columnar execution is already fast — reimplementing worse versions of what DuckDB does well, and for trawl's scale a single beefy node with enough RAM is faster than network round-trips.

#### 9. custom storage format

replacing parquet with something "optimized for log data." parquet is industry standard, well-optimized (dictionary/RLE/delta encoding, snappy/zstd), and actively developed by much larger teams. the only benefit of a custom format would be embedded inverted indexes, but the complexity cost is astronomical and you lose the "query your data with any tool" benefit.

#### 10. consensus-based metadata store

raft/paxos for cluster membership, shard assignment, schema registry. needed for exactly zero of trawl's current or near-future requirements. if read replicas happen, a simple config file listing addresses is sufficient.

## summary

the architecture is well-designed for its stated purpose. the most impactful improvements are within or adjacent to the single-node model:

1. **S3 cold storage** — infinite retention, clean fit with existing partitioning
2. **read-only query replicas** — separate ingest from query load, ~10s staleness acceptable
3. **bloom filter sidecars** — targeted optimization for high-cardinality field lookups

everything beyond that is fighting the single-node design thesis that makes trawl good in the first place.
