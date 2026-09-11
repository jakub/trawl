---
title: Query execution
description: SQL execution, hot/cold selection, stream parity, and work lifetime.
---

A server query takes one full catalog pin snapshot and carries that interpretation through execution. The query can read compacted Parquet, a recent hot snapshot, or both. Embedded `--data` queries have no server catalog and remain pin-blind.

## Parser and SQL emitter

```text
DSL → parser and AST → validation and pin scope → parameterized SQL
                                                    │
                                          admitted DuckDB work
                                                    │
                                      optional Rust extract-kv tail
                                                    │
                                               result rendering
```

The parser owns token boundaries and comments. The emitter builds CTEs, binds values, validates projection names, and limits generated expression expansion before DuckDB sees the query. A stage does not always become exactly one CTE; the emitter can accumulate compatible operations before flushing a stage.

`PinScope` follows fields through the pipeline. Rename and a bare field alias preserve the applicable pin; a computed replacement loses it. Aggregate outputs are new values while grouping keys retain their scope. `from saved` clears catalog typing because a materialized report's columns are not this corpus's current event fields.

Bare field-versus-literal comparisons use the shared rule table. Field-versus-field expressions, arithmetic, and other shapes retain their documented expression behavior. Search-stage `!=` includes missing fields, while pipeline `!=` follows strict SQL null propagation. The [DSL comparison reference](/reference/dsl/#pinned-comparison-semantics) defines the complete operator rules.

## Source computation

The planner narrows files by environment, service, and time over hourly and daily paths. Directory pruning is advisory: an hour directory may contain another service's file but no file for this query.

The executor resolves each list element once after the hot snapshot. Literal paths use filesystem metadata; patterns use DuckDB globbing. `read_parquet` rejects a list if even one element matches nothing, so sparse services need this resolution. The collected evidence also informs error handling. A file that matched and then disappeared must not be silently re-resolved away.

A publication read guard protects source selection through the last physical read. Compaction takes the write guard around file publication and hot drain. This prevents the same event appearing in both union branches during publication. See [ingest and publication](/architecture/data-flow/#hot-buffer).

## Hot buffer integration

A cached snapshot file represents the current hot generation. Snapshot generation ensures every observed field name appears within DuckDB's schema-detection prefix. The snapshot also carries current catalog pins intersected with its actual keys, avoiding references to absent hot columns.

The emitter conforms the hot branch through the same expression builder compaction uses. It does not recast already conformant cold files. The full catalog snapshot separately determines comparison rules, including on a cold start when no Parquet exists.

## Errors and fallback

Query and Parquet-export entry points, with and without hot data, use one outcome-policy table. Hot-only fallback is allowed only when it cannot hide cold data, such as a genuine cold start or the permitted missing-column case. Other failures with cold data present return an error.

A no-files read after source evidence established cold data becomes retryable `503 cold_data_unread`, not an empty success. A genuinely empty query source returns zero rows. A genuinely empty Parquet export reports its underlying error because there is no result file to write. Read-time type reconciliation is not a fallback: a type conflict outside the catalog contract remains an error.

## Executor pool and deadlines

The pool bounds concurrent physical DuckDB work. Query, export, schema-value sampling, and report execution use their applicable admission and ownership paths. Request lifetime and worker lifetime are distinct: DuckDB binding may continue after an interrupt or timeout.

A timed-out request therefore does not release a permit or publication guard while physical work still holds it. Running-query information can expose retained work. A cutover waits for actual exclusion, not merely for HTTP responses to finish. [ADR-0024](/contribute/decisions/#adr-0024) defines admission, deadlines, and retained permits; [ADR-0026](/contribute/decisions/#adr-0026) defines publication guards.

## Rust tail and display

`extract kv` can split batch execution into SQL and a Rust tail. That tail evaluates later expressions and must receive the pin scope at the split; it is not just formatting. Timestamp display offsets apply after the tail so evaluation reads UTC instants.

Severity columns render as tokens in human-readable views while machine formats retain numbers. The server stamps severity-column metadata using the snapshot under which the query executed. See [severity syntax](/reference/dsl/#severity-_severity).

## SSE streaming

A live stream filters event-bus batches in Rust rather than querying DuckDB for each event. Its search filter and supported pipeline stages share one pin snapshot for the stream's lifetime, so repin affects an existing stream only after reconnect.

The matcher follows shared comparison rules and SQL three-valued logic. Negated text containment has its own total rule described in the DSL reference. Not every SQL stage can stream; unsupported stages are refused. Sampling, eventstats, and saved-run sources are batch operations.

The event bus is bounded and lossy for slow consumers. A lag notification means events were missed; it does not replay a durable backlog. Stream admission is separately bounded.

## Source owners

`trawl-core/src/{compare,pin_scope,filter,stream}.rs`, `trawl-core/src/emitter/`, `trawl-engine/src/executor.rs`, and `trawl-server/src/pool.rs` own these mechanisms. See the [source map](/contribute/source-map/) for tests and related decisions.
