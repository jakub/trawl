# The default request limit under a replayed load

Evidence for issue #293, acceptance criterion "The default of 32 refuses
nothing in normal use" (ADR-0054, "Range and default").

## Result

**Pass.** At the default `max_concurrent_requests` of 32, trawld refused no
request on either allowance.

| Measurement | Value |
| --- | --- |
| `trawl_http_requests_refused_total{allowance="regular"}` at the end | 0 |
| `trawl_http_requests_refused_total{allowance="control"}` at the end | 0 |
| Highest refused count in any sample, both allowances | 0 |
| `trawl_http_request_allowance`, regular / control | 32 / 4 |
| Sampled peak of `trawl_http_requests_in_progress{allowance="regular"}` | 8, at t = 12.0 s |
| Sampled peak of `trawl_http_requests_in_progress{allowance="control"}` | 1 |
| Events written to the senders / stored, distinct per sender | 148,740 / 148,740 for each of 3 |
| Duplicate events stored | 0 |
| `compaction_complete` events while the senders delivered | 311 (352 in the session) |
| `rollup_complete` events while the senders delivered | 35, from 02:36:39.32 to 02:36:39.96 UTC (t = 62.3 to 63.0 s) |
| Search queries / answered 200 | 89 / 89 |
| Failed `/metrics` scrapes | 0 |
| Cleanup: processes / Postgres container | removed / removed |

The peak is a sampled peak, not an exact high-water mark. The scraper read
`/metrics` every 0.25 s. The median gap between samples was 0.25 s and the
largest gap was 2.25 s, so a peak shorter than a gap can be missed. The
refusal counters are counters, so the pass condition does not depend on
sampling.

In-progress samples for the regular allowance (647 samples):

| In progress | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Samples | 179 | 167 | 127 | 70 | 46 | 24 | 14 | 17 | 3 |

## Hypothesis and threshold

Hypothesis: three hosts that ship through the shipped Vector sink, one
person searching, and trawld's own compaction and rollup do not fill 32
requests in progress.

Threshold, fixed before the run: `trawl_http_requests_refused_total` is 0
for both allowances at the end of the run and in every sample. The run also
requires that every written event is stored, that every scrape succeeds and
that cleanup succeeds. `run.py` exits 0 and writes `"status": "passed"` only
when all of these hold.

## What ran

- **Build:** debug profile, `cargo build --locked --no-default-features -p
  trawl-server -p fleet-admin`, at `02b6343a4be752486ef5f0a7bc0662aef40de949`.
  `run.py` refuses to start if `crates/`, the Cargo files or `bin/trawld-dev`
  differ from HEAD. The trawld binary's SHA-256 is in `output/summary.json`.
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
  last event at t = 181.0 s, and every event was stored at t = 192.7 s. With
  the build check, the database and the seed sessions, the script took about
  4.5 minutes.

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
accepted 189 requests from them. The decoded body had a median of
1,052,202 bytes and a maximum of 1,052,399 bytes, and 130 requests were at
least 900 KB. The gzip body had a median of 24,225 bytes. The replayed
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

It sent 89 queries. All answered 200, with a median of 796 ms and a maximum
of 2,977 ms.

### Compaction and rollup

No endpoint or CLI command starts a compaction or a rollup. Both run in the
compaction task, at the default interval of 10 s and on hot-buffer pressure.
That is all the run needs for compaction: trawld logged 311
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
   `rollup_complete` events, each with `hourly_files=1`, at t = 62.3 to 63.0 s.

Files move only while trawld is stopped. `output/summary.json` lists them
under `seededHistory`.

## What else the run showed

- **The hot buffer, not the request count, limited the backlog.** From
  t = 12.0 s to t = 127.75 s the hot buffer was mostly in the refusing state
  (299 of 647 samples in state 2). trawld answered 240 ingest requests with
  503 `hot_buffer_full`. Vector retried each one with backoff, and every
  event was stored once. Vector's adaptive concurrency reduces its
  concurrency after a 503, so a hot-buffer refusal also lowers the request
  count.
- **The sampled peak came while the hot buffer filled.** In-progress reached
  6 to 8 from t = 8.25 s to t = 12.75 s, while the hot buffer went from
  49,715 to 75,864 events. During the rollup window it was 0 to 6.
- Vector's own logs show no request timeouts and no errors. Vector logs a
  retry as `Service Unavailable` without the response body, and it
  suppresses repeats of that warning. The 503 count therefore comes from
  trawld's `http_failure` events (`httpFailuresFromTrawld`).

## Earlier runs

The script changed between runs. The earlier summaries are in
[`earlier-runs/`](earlier-runs/). All four runs refused nothing.

| Run | Head | Sampled peak (regular) | Refused | What differed |
| --- | --- | --- | --- | --- |
| 1 | `372e8d7a` | 8 | 0 | The page query matched `fixture_source="journald"`. Once more than 100,000 events matched, 18 of its 51 queries answered 400 `result_too_large`. |
| 2 | `02b6343a` | 14, at t = 202.5 s | 0 | Interrupted during the delivery wait, see below. |
| 3 | `02b6343a` | 10, at t = 11.25 s | 0 | The same script as the final run, without the batch-size and failure tallies. |
| Final | `02b6343a` | 8, at t = 12.0 s | 0 | `output/` |

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
    --work .flow-scratch/default-load \
    --vector /path/to/vector-x86_64-unknown-linux-gnu/bin/vector
```

`--work` must not exist and must not be under `/tmp`. It holds the database
password, the keys, the data and the raw logs. The script deletes it at the
end unless `--keep-work` is given. Only summaries reach `--out`, which
defaults to `output/`. `--help` lists the workload options.

## Files

| File | Contents |
| --- | --- |
| [`run.py`](run.py) | The run: build check, Postgres, keys, seed sessions, trawld, senders, query loop, scraper, summary, cleanup |
| [`trawld.toml`](trawld.toml) | trawld's configuration, with the data path filled in at run time |
| [`output/summary.json`](output/summary.json) | Every number above |
| [`output/metrics-samples.csv`](output/metrics-samples.csv) | Every `/metrics` sample: time, scrape latency, request counts and allowances, hot buffer, WAL and parquet file counts |
| [`output/metrics-final.prom`](output/metrics-final.prom) | The last scrape, request, hot-buffer, ingest, compaction and query series only |
| [`output/compaction-rollup-events.ndjson`](output/compaction-rollup-events.ndjson) | trawld's `compaction_complete` and `rollup_complete` events, with timestamps |
| [`output/queries.csv`](output/queries.csv) | Each search query: time, kind, status, latency |
| [`earlier-runs/`](earlier-runs/) | Summaries of runs 1 to 3, and run 2's samples |
