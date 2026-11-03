# Issue #333: what the input-column probe costs

`probe-cost-aea4af33.txt` is the stdout of
`scripts/evidence/issue-333/measure.py`, run once in release mode against a
`git archive` of aea4af33. The baseline is a `git archive` of 8b6d96ea, the
commit before the probe. The script's `sha256` in the transcript header
covers `measure.py` and `emit-sql/` as committed beside it.

Results on 256 parquet files of 1000 rows per corpus:

- The probe alone, a `DESCRIBE` of the cold reader, takes about 4 ms
  (median, p90 5 ms). DuckDB's CLI timer counts whole milliseconds.
- End to end through `trawl query --data`, the measured tree is 2.7 ms
  (`host=web1 f!=x`) and 3.8 ms (`host=web1 OR f=x`) slower by median than
  the baseline on the present-null control. On the absent corpus the
  baseline refuses both queries with `unknown field: f`, and the measured
  tree answers 64000 rows.
- Parquet filter pushdown is kept on the absent corpus for both queries.
  The scan projects only the columns the files carry. `host=web1 OR f=x`
  pushes no filter on the control, because DuckDB does not push an OR
  across two columns into the scan. On the absent corpus the missing
  operand folds to NULL, which leaves a filter on `host` alone that DuckDB
  does push.

The machine was shared with other builds during the run, so the transcript
records the load average at the start and the end.

To run it again:

```sh
python3 -I scripts/evidence/issue-333/measure.py --repo . [--sha <rev>] [--runs N]
```

The run writes its corpora, exports, target directories and binaries under
`.flow-scratch/issue-333-probe-<sha>/` and keeps them. It needs the `duckdb`
CLI and a checkout whose `target/duckdb-runtime-cache` holds the verified
DuckDB runtime, which any native build of the checkout leaves there.

`emit-sql/` prints the SQL the embedded lane emits by calling
`emitter::emit_with_pins` over the probe's answer, the same call
`ResolvedSource::emit_cold` makes. The script runs that SQL in DuckDB and
checks that its row count matches `trawl`'s answer.
