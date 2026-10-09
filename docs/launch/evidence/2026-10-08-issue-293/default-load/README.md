# The default request limit under a replayed load

Evidence for issue #293, acceptance criterion "The default of 32 refuses
nothing in normal use" (ADR-0054, "Range and default").

## Result

**Pass.** At the default `max_concurrent_requests` of 32, trawld refused no
request on either allowance. The run was on 2026-10-09 from 04:33 to 04:38
UTC (2026-10-08 local time), on the final script.

| Measurement | Value |
| --- | --- |
| `trawl_http_requests_refused_total{allowance="regular"}` in the terminal snapshot | 0 |
| `trawl_http_requests_refused_total{allowance="control"}` in the terminal snapshot | 0 |
| Highest refused count in any sample, both allowances | 0 |
| Samples with a refusal series missing or unreadable, both allowances | 0 |
| `http_failure` events with `cause_kind=request_limit_reached` | 0 |
| `trawl_http_request_allowance`, regular / control | 32 / 4 |
| Sampled peak of `trawl_http_requests_in_progress{allowance="regular"}` | 9, at t = 12.5 s |
| Sampled peak of `trawl_http_requests_in_progress{allowance="control"}` | 1 |
| Events written to the senders / stored, distinct per sender | 148,740 / 148,740 for each of 3 |
| Duplicate events stored | 0 |
| `compaction_complete` events while the senders delivered | 280 (316 in the session) |
| `rollup_complete` events while the senders delivered | 35, from 04:36:07.52 to 04:36:10.58 UTC (t = 67.9 to 71.0 s) |
| Search queries / answered 200 | 82 / 82 |
| Failed `/metrics` scrapes | 0 |
| Cleanup: processes / Postgres container | removed / removed |

The terminal snapshot is one more `/metrics` scrape at t = 204.8 s. The
script takes it after the query loop and the three Vector processes have
stopped, and before it stops trawld. A refusal after the last sample is
therefore still counted.

The peak is a sampled peak, not an exact high-water mark. The scraper read
`/metrics` every 0.25 s. The median gap between samples was 0.25 s and the
largest gap was 2.25 s, so a peak shorter than a gap can be missed. The
refusal counters are counters, so the pass condition does not depend on
sampling.

In-progress samples for the regular allowance (613 samples):

| In progress | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Samples | 142 | 172 | 126 | 85 | 56 | 8 | 10 | 12 | 1 | 1 |

## Hypothesis and threshold

Hypothesis: three hosts that ship through the shipped Vector sink, one
person searching, and trawld's own compaction and rollup do not fill 32
requests in progress.

Threshold, fixed before the run: `trawl_http_requests_refused_total` is 0
for both allowances in every sample and in the terminal snapshot. Each
series must be present and must read as a whole number in every one of
these scrapes. A missing series or an unreadable value fails the run. It is
never counted as 0. The run also fails if:

- trawld logged any `http_failure` with `cause_kind=request_limit_reached`,
  or a Vector log mentions `request_limit_reached`;
- `trawl_http_request_allowance` in the terminal snapshot is missing or is
  not 32 for `regular` and 4 for `control`;
- no sample read a regular `trawl_http_requests_in_progress` of at least 1;
- the senders are not exactly `vector-1`, `vector-2` and `vector-3`, a
  sender wrote no events, a sender stopped reading its input, or a written
  event was not stored;
- no search query ran, or any query answered something other than 200.
  A 400, a 5xx or a client-side failure each fail the run;
- no `compaction_complete` or no `rollup_complete` event falls between the
  start of the measured session and the moment every event was stored;
- a scrape failed, or the terminal snapshot is missing;
- the run stopped with an error, or collecting the evidence raised one;
- the query loop or the scraper was still running at the terminal snapshot;
- cleanup of the processes or the container failed. The container
  counts as removed only after `docker rm` succeeds, or when `docker
  inspect` reports no such container. Any other Docker error, such as an
  unreachable daemon, fails the run.

`run.py` exits 0 and writes `"status": "passed"` only when none of these
happened. Otherwise `summary.json` lists each reason under
`failureReasons`. If a run refuses requests at 32, that is a finding to
report. The run does not change any setting to make the run pass.

`--check-summary` applies the same check to an existing `summary.json` and
runs nothing. [`output/predicate-check.txt`](output/predicate-check.txt)
is its result on the committed summary: passed. It records the command,
the git blob SHA of the `run.py` that ran the check, and the SHA-256 of the
summary.

## What ran

- **Build:** debug profile, `cargo build --locked --no-default-features -p
  trawl-server -p fleet-admin`, at `b38305242b361ec022252e5372869b018d2d2801`,
  the commit that holds the final `run.py`. Its `crates/` differs from
  `e451464a`, the head of run 5, only in the test file
  `crates/trawl-server/tests/request_limit/transport.rs`. `run.py` refuses
  to start if `crates/`, the Cargo files or `bin/trawld-dev` differ from
  HEAD. The trawld binary's SHA-256 is in `output/summary.json`.
  A debug build parses and serializes more slowly than a release build, so
  each request stays in progress longer.
- **Host:** Linux, 16 logical CPUs, 32 GB RAM. Everything ran on loopback.
- **trawld:** [`trawld.toml`](trawld.toml). It leaves every setting the run
  measures at its default: `max_concurrent_requests` (32),
  `max_concurrent_queries` (CPU count), `compaction_interval_secs` (10),
  `daily_rollup` (true), `internal_telemetry` (true), and the ingest body and
  hot-buffer limits. It sets only the listener address, the data path and
  `[retention] max_age_days = 0`.
- **Databases:** a private `postgres:18` container on a random loopback port,
  with its own Fleet and Trawl databases. `run.py` creates and removes it.
- **Keys:** one service key per sender with `trawl:ingest`, and one human key
  with `trawl:query`, `trawl:schema_read` and `trawl:validate` for the query
  loop. Both roles have `--rate-rpm 100000`, so the per-key rate limiter never
  answers 429 and hides load from the request limit.
- **Duration:** the measured session ran 205 s. The senders received their
  last event at t = 181.1 s, every event was stored at t = 192.7 s, and the
  terminal snapshot was at t = 204.8 s. With the build check, the database
  and the seed sessions, the script took 4 minutes 26 seconds.

## Workload

### Senders

Three Vector 0.57.0 processes are the senders. The Vector binary is the
release that CI pins (`.github/workflows/ci.yml`). Its SHA-256 is in
`output/summary.json`.

Each Vector runs the shipped sink. `run.py` reads `[sinks.trawld]` from
`config/vector/debian/base.toml` at run time and changes two keys:

- `inputs` is a `stdin` source with the JSON codec. The fixture is the output
  of the shipped transforms, so the shipped sources and transforms are not
  used.
- `tls.ca_file` is trawld's generated certificate, as base.toml line 29 says
  to set for a private CA.

The senders also get `VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true`, as
base.toml lines 22 to 24 require for Vector 0.57.

The sink values the run used, from `config/vector/debian/base.toml` and the
table in `docs/src/content/docs/getting-started/vector-integration.md`
(lines 471 to 474):

| Setting | Value | base.toml line |
| --- | --- | --- |
| Encoding | JSON array | 378 |
| Compression | gzip | 385 |
| Batch size | `max_bytes = 1_048_576` (1 MB, before compression) | 388 |
| Batch timeout | 5 s | 389 |
| Buffer | disk, 1 GB | 392 to 393 |
| Request timeout | 30 s | 398 |
| Retry backoff | 1 s to 30 s | 396 to 397 |
| Concurrency | `adaptive` | 399 |

trawld's `ingest_complete` events show the batches the senders sent. trawld
accepted 188 requests from them. The decoded body had a median of
1,052,220 bytes and a maximum of 1,052,399 bytes, and 134 requests were at
least 900 KB. The gzip body had a median of 24,227 bytes. The replayed
fixture repeats 67 events, so it compresses far better than real logs do.

### Events and pacing

The fixture is `crates/trawl-server/tests/fixtures/vector-capture/debian.ndjson`:
67 events, all 67 accepted. Each replayed event is a fixture event with three
added fields: `load_run`, `load_sender` and `load_seq`. `load_seq` counts
from 0 for each sender, so a distinct count shows whether an event was lost.

Each sender has two phases:

1. **Backlog:** 1,500 fixture cycles (100,500 events) written to Vector's
   stdin as fast as Vector reads them, as on a first start over a long
   journal. Vector holds them in its disk buffer and sends 1 MB batches at
   its adaptive concurrency.
2. **Steady:** 4 fixture cycles (268 events) each second for 180 s (48,240
   events).

That is 148,740 events per sender and 446,220 in total.

### Search loop

One loop stands in for the SPA. It sends `POST /api/v1/query` straight to
trawld and waits 1 s between queries. The loop alternates two queries:

- A page of 50 rows: `load_run="<run>" fixture_id="journal-pid1"` in a fixed
  window. This matches one event in 67, which stays under
  `max_result_rows` (100,000).
- An aggregation: `load_run="<run>" | stats count() by load_sender` in the
  same window.

It sent 82 queries. All answered 200, with a median of 1,370 ms and a
maximum of 3,279 ms.

### Compaction and rollup

No endpoint or CLI command starts a compaction or a rollup. Both run in the
compaction task, at the default interval of 10 s and on hot-buffer pressure.
That is all the run needs for compaction: trawld logged 280
`compaction_complete` events while the senders delivered.

A rollup needs more. trawld rolls up only date directories before today, and
it files events under the UTC date and hour they arrive, not under `_time`.
`run.py` therefore seeds a day of history before the measured session:

1. Seed session 1 starts trawld, sends 30 fixture cycles and stops trawld
   after compaction has written them to parquet. Its hour directory moves to
   `prod/2026-10-08/22`.
2. Seed session 2 does the same. Its first normal pass rolls hour 22 into
   daily files. Its own hour directory then moves to `prod/2026-10-08/23`.
3. The measured session starts with the senders. Its first normal pass
   with a rollup merged hour 23 into the 35 daily files: 35
   `rollup_complete` events, each with `hourly_files=1`, at t = 67.9 to 71.0 s.

Files move only while trawld is stopped. `output/summary.json` lists them
under `seededHistory`.

## What else the run showed

- **The hot buffer, not the request count, limited the backlog.** From
  t = 12.75 s to t = 147.5 s the hot buffer was mostly in the refusing state
  (341 of 613 samples in state 2). trawld answered 198 ingest requests with
  503 `hot_buffer_full`. Vector retried each one with backoff, and every
  event was stored once. Vector's adaptive concurrency reduces its
  concurrency after a 503, so a hot-buffer refusal also lowers the request
  count.
- **The sampled peak came while the hot buffer filled.** In-progress held
  at 6 to 7 from t = 8.25 s to t = 12.25 s, while the hot buffer went from
  49,715 to 74,894 events. It read 9 at t = 12.5 s, with 87,222 events in
  the hot buffer, one sample before the buffer started refusing. It read 8
  once more at t = 22.25 s. During the rollup window it was 0 to 3.
- Vector's own logs show no request timeouts and no errors. Vector logs a
  retry as `Service Unavailable` without the response body, and it
  suppresses repeats of that warning. The 503 count therefore comes from
  trawld's `http_failure` events (`httpFailuresFromTrawld`).

## What changed in the script

Review found two defects in the script of the earlier runs. Run 5 was the
first run on the corrected pass check. Later reviews found more, fixed
after run 5. Run 6 is the first run on the final script, and `output/`
comes from it.

- **The script could delete a directory it did not create.** The old
  script refused an existing `--work`, but its cleanup still ran and
  deleted that path. `--work .` could delete the checkout.
- **The pass check could pass without refusal evidence.** The old check
  read a missing refusal series as 0. It ignored collection errors and
  `request_limit_reached` failure events. It stopped sampling while the
  senders still ran, so a late refusal could be missed. The new check is
  in [Hypothesis and threshold](#hypothesis-and-threshold).

After run 5:

- **The pass check did not require the workload it measures.** It passed
  with an allowance other than 32, with no in-progress reading, with
  failed or missing search queries, and with no compaction or rollup
  during ingest. It now requires each of these, and three senders that
  each delivered.
- **A Docker error read as a removed container.** Any failed `docker
  inspect` counted as proof that the container was gone, so an unreachable
  daemon passed cleanup. Only a successful `docker rm`, or Docker reporting
  no such container, now counts.
- **The script no longer deletes its work directory.** Every fix to the
  deletion left a check-then-delete window, so the script has no recursive
  delete at all. It removes only its own Postgres container and stops its
  own processes. The pass check no longer reads the `workDir` cleanup
  field.
- **The database password was written to disk** (CodeQL
  `py/clear-text-storage-sensitive-data` on PR #297). The script put it in
  a file for `docker run --env-file`. It now writes no credential to a
  file. `docker run` gets `--env POSTGRES_PASSWORD` with no value, and the
  value is set only in the environment of that one `docker` process, not
  on the command line, which any local user can read through `/proc`. The
  configs the script writes hold no secret, and they are created with
  `O_CREAT | O_EXCL | O_NOFOLLOW` and mode 0600.
- **The script could not finish a run.** The CodeQL fix created
  `private/trawld.toml` with `O_EXCL` every time trawld started, so the
  second seed session failed with `FileExistsError`. `test_run.py` started
  trawld only once and stayed green. The script now writes the config once,
  before the seed sessions, and `test_run.py` starts trawld for all three
  sessions.
- **A run could overwrite the committed evidence.** `--out` defaulted to
  `output/`, so a failed reproduction truncated the committed summaries.
  `--work` took any path, and each review found another way to misuse one.
  Both options are gone. Each run writes only inside a fresh
  `.flow-scratch/<run id>/` in the checkout, and promoting a run into
  `output/` is a manual copy (see [Reproduce](#reproduce)).

[`test_run.py`](test_run.py) runs the real `collect()` and `verdict()` on
in-memory scrapes and a written trawld log, with one case for each pass
condition. It runs `cleanup()` against stubbed Docker answers. It checks
that a run claims a fresh 0700 directory, refuses an existing one, writes
only inside it and never deletes it, and that trawld starts three times on
one config. It checks that the `docker run` command line never carries the
password and that the private files are created 0600. It needs no build,
database or Vector.

## Threat model

`run.py` writes only inside `.flow-scratch/<run id>/` in your checkout, a
directory it creates with mode 0700 and never deletes. Anyone who can write
the checkout or its ancestors, and any process running as the same user,
can already change the code the script builds and runs, so findings that
need either are out of scope.

## Earlier runs

The script changed between runs. The earlier summaries are in
[`earlier-runs/`](earlier-runs/). Runs 1 to 4 used the old pass check from
[What changed in the script](#what-changed-in-the-script). Their end
counters were present and 0, but the old check would also have passed
with a refusal series missing. All six runs refused nothing.

| Run | Head | Sampled peak (regular) | Refused | What differed |
| --- | --- | --- | --- | --- |
| 1 | `372e8d7a` | 8 | 0 | The page query matched `fixture_source="journald"`. Once more than 100,000 events matched, 18 of its 51 queries answered 400 `result_too_large`. |
| 2 | `02b6343a` | 14, at t = 202.5 s | 0 | Interrupted during the delivery wait, see below. |
| 3 | `02b6343a` | 10, at t = 11.25 s | 0 | Without the batch-size and failure tallies. |
| 4 | `02b6343a` | 8, at t = 12.0 s | 0 | The old pass check. Its `output/` was replaced by run 5. |
| 5 | `e451464a` | 7, at t = 8.0 s | 0 | The corrected pass check, before the fixes listed after run 5. |
| 6 | `b3830524` | 9, at t = 12.5 s | 0 | The final script. `output/` |

In run 2, one compaction pass took 33,969 ms
(`compact_service=trawl-check-…`, 32 WAL files, 11,528 rows), from about
t = 172 s to t = 206 s. Three ingest requests waited behind it, with `wal_ms`
from 33,081 to 33,709 ms. The sampled peak of 14 came near the end of that
pass. One query timed out at 30 s and answered 504. Vector's 30 s request
timeout fired on requests that trawld then stored, and Vector sent them again,
so trawld stored some events twice (Vector delivers at least once). The
exact-count delivery check of that version could never match, so the run was
stopped. The final version counts distinct `load_seq` values instead. Other
builds and tests were running on the host during run 2. That they caused the
slow pass is a guess, not a measurement. Its samples are in
`earlier-runs/run-2-metrics-samples.csv`.

Run 2 is the closest any run came to the limit: 14 of 32 during a 34 s stall
of the publication path.

## Limits of this evidence

- A debug build on one host, over loopback. These are not release timings.
- The senders read stdin, not journald or files. The stdin source does not
  support acknowledgements, and Vector warns about this at startup. That does
  not change what the sink sends.
- The query loop talks to trawld directly, not through trawl-web or a browser.
- The fixture compresses about 43 times, so gzip bodies are far smaller than
  real ones. The decoded batch size, which drives parse work, matches the
  shipped 1 MB.
- One measured session per run, with one rollup window.

## Reproduce

From the repository root, with Docker and the Vector 0.57.0 binary from CI:

```bash
python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/run.py \
    --vector /path/to/vector-x86_64-unknown-linux-gnu/bin/vector
```

The script prints its run directory, `.flow-scratch/<run id>/`, at the end.
`work/` holds the data, the configs and the raw logs, and no database or
API credential. `output/` holds the summaries. The script never deletes the
run directory, so remove it when you are done. In a clone without a local
exclude for `.flow-scratch/`, git shows it as untracked. `--help` lists the
workload options. Run 6 used `--vector
~/.cache/trawl-vector-0.57.0/vector-x86_64-unknown-linux-gnu/bin/vector`.

To compare your run with the committed one, apply the pass check to it and
read the same fields from both summaries:

```bash
E=docs/launch/evidence/2026-10-08-issue-293/default-load
python3 -I $E/run.py --check-summary <run dir>/output/summary.json
jq '{status, refused: .metrics.refusedTotalAtEnd, peak: .metrics.sampledPeakInProgress,
     delivered: .delivery.complete}' <run dir>/output/summary.json $E/output/summary.json
```

To promote a run into the evidence, copy its summaries over the committed
ones and record the pass check against them:

```bash
cp <run dir>/output/* $E/output/
python3 -I $E/run.py --check-summary $E/output/summary.json > $E/output/predicate-check.txt
```

To check the pass predicate, the container cleanup and the run directory
without a full run:

```bash
python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/test_run.py
```

## Files

| File | Contents |
| --- | --- |
| [`run.py`](run.py) | The run: build check, Postgres, keys, seed sessions, trawld, senders, query loop, scraper, terminal snapshot, summary, cleanup |
| [`test_run.py`](test_run.py) | Checks of the pass predicate, the container cleanup, the run directory and trawld restarts, without a full run |
| [`trawld.toml`](trawld.toml) | trawld's configuration, with the data path filled in at run time |
| [`output/summary.json`](output/summary.json) | Every number above |
| [`output/predicate-check.txt`](output/predicate-check.txt) | The current pass check applied to `output/summary.json`, with the command and the `run.py` blob SHA |
| [`output/metrics-samples.csv`](output/metrics-samples.csv) | Every `/metrics` sample: time, scrape latency, request counts and allowances, hot buffer, WAL and parquet file counts |
| [`output/metrics-final.prom`](output/metrics-final.prom) | The terminal snapshot, request, hot-buffer, ingest, compaction and query series only |
| [`output/compaction-rollup-events.ndjson`](output/compaction-rollup-events.ndjson) | trawld's `compaction_complete` and `rollup_complete` events, with timestamps |
| [`output/queries.csv`](output/queries.csv) | Each search query: time, kind, status, latency |
| [`earlier-runs/`](earlier-runs/) | Summaries of runs 1, 2, 3 and 5, and run 2's samples |
