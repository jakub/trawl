# Bind-time explosion repro (issue #98)

Chained lateral aliases in ONE `| let` stage, measured through trawl's own
embedded CLI against a fixture parquet on this workspace's DuckDB 1.5.5
(AMD Ryzen 7 7800X3D), wall clock, 30s hard cap per depth:

| let-chain depth | DSL bytes | wall clock | outcome |
|---:|---:|---:|:--|
| 8  | 135 | 0.314s  | ok |
| 10 | 164 | 0.765s  | ok |
| 12 | 198 | 2.918s  | ok |
| 14 | 232 | 12.681s | ok |
| 16 | 266 | >30s    | wedged, still binding |
| 17 | 283 | >30s    | wedged, still binding |
| 18 | 300 | >30s    | wedged, still binding |

DSL shape: `* | let a0 = 1, a1 = a0 + a0, a2 = a1 + a1, ... | head 1`

Wall clock roughly doubles per added assignment. 266 bytes of DSL is enough
to wedge a bind past 30s. The whole thing runs inside a single ExecutorPool
permit that the client-facing timeout cannot release, because DuckDB does not
honour interrupt() during prepare()/bind.
