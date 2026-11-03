# Issue #333: what the input-column probe costs

`probe-cost-2178b8ba.txt` is the stdout of
`scripts/evidence/issue-333/measure.py`, run once in release mode against a
`git archive` of 2178b8ba. That commit holds every product fix of the run;
it changes only the script after f1febec0. The baseline is a `git archive`
of 8b6d96ea, the commit before the probe. The script's `sha256` in the
transcript header covers `measure.py` and `emit-sql/` as committed beside
it.

## Method

The script builds `trawl` from both trees and the `emit-sql` harness from
the measured tree. It then writes two corpora of 256 parquet files of 1000
rows each. In `control`, half the files carry `f` as an all-null VARCHAR
column. In `absent`, no file carries `f`. It times the probe alone, a
`DESCRIBE` of the cold reader in a fresh DuckDB CLI process, 31 times per
corpus. It then times `trawl query --data` end to end for both builds, 31
runs per query and corpus.

Each run times one measured/baseline pair. Even runs time the measured
build first and odd runs the baseline first. The transcript lists every
run's order beside its timings. It reports the median difference over all
runs and separately for each order, so you can see whether the order moved
the result.

Last, the script EXPLAINs each emitted query on each corpus. It prints the
parquet scan's pushed filters and projected columns, and checks the SQL's
row count against `trawl`'s answer.

No other seat ran builds or tests during the measurement. Builds
from an earlier invocation were reused, so no compile overlapped the
timings. The transcript records the load average at the start and the end.

## Results

On 256 parquet files of 1000 rows per corpus:

- The probe alone takes 4 ms (median and p90) on both corpora. DuckDB's
  CLI timer counts whole milliseconds.
- On the present-null control, the measured tree is slower by median than
  the baseline. For `host=web1 f!=x` it is 4.39 ms slower over all runs:
  3.63 ms when the measured build ran first and 5.27 ms when it ran
  second. For `host=web1 OR f=x` it is 3.36 ms slower over all runs: 2.27
  ms when it ran first and 3.78 ms when it ran second. Both orders give the
  same sign and the same size as the probe, so the order did not cause the
  difference.
- On the absent corpus the baseline refuses both queries with
  `unknown field: f`, and the measured tree answers 64000 rows. The
  harness's SQL returns the same 64000 rows in DuckDB for every query and
  corpus.
- Parquet filter pushdown is kept on the absent corpus for both queries.
  The scan projects only the columns the files carry. `host=web1 OR f=x`
  pushes no filter on the control, because DuckDB does not push an OR
  across two columns into the scan. On the absent corpus the missing
  operand folds to NULL, which leaves a filter on `host` alone that DuckDB
  does push.

## Changes since the first measurement

The first transcript, at aea4af33, had two faults:

- Every run timed the measured build before the baseline. Page cache,
  thermal and frequency effects then all pushed one way, and the deltas
  were the same size as those effects. The order now alternates with the
  run index, as described above.
- The harness builds with its committed `emit-sql/Cargo.lock` against the
  trawl-core of whichever `--sha` you give. An older revision with a
  different trawl-core dependency set would fail with an unclear `--locked`
  error. The script now runs `cargo metadata --locked --offline` on the
  harness first. If the lockfile does not fit, it stops before any timing,
  names the revision, and says that the harness lockfile supports only
  revisions whose trawl-core dependency set matches it. Supported revisions
  still build with `--locked`.

That run also shared the machine with other builds, with a load average
near 20.

## Run it again

```sh
python3 -I scripts/evidence/issue-333/measure.py --repo . [--sha <rev>] [--runs N]
```

The run writes its corpora, exports, target directories and binaries under
`.flow-scratch/issue-333-probe-<sha>/` and keeps them. A second run at the
same SHA reuses the builds. To keep compiles out of the timings, run it
once with a small corpus (`--runs 2 --files 4 --rows 10`), wait for the
load to settle, then run it in full. It needs the `duckdb` CLI and a
checkout whose `target/duckdb-runtime-cache` holds the verified DuckDB
runtime, which any native build of the checkout leaves there.

`emit-sql/` prints the SQL the embedded lane emits by calling
`emitter::emit_with_pins` over the probe's answer, the same call
`ResolvedSource::emit_cold` makes. The script runs that SQL in DuckDB and
checks that its row count matches `trawl`'s answer.
