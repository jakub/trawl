# The default request limit under a replayed load

Evidence for issue #293, acceptance criterion "The default of 32 refuses
nothing in normal use" (ADR-0054, "Range and default").

## Result

**Pass.** At the default `max_concurrent_requests` of 32, trawld refused no
request on either allowance. The run was on 2026-10-09 from 03:03 to 03:08
UTC (2026-10-08 local time).

| Measurement | Value |
| --- | --- |
| `trawl_http_requests_refused_total{allowance="regular"}` in the terminal snapshot | 0 |
| `trawl_http_requests_refused_total{allowance="control"}` in the terminal snapshot | 0 |
| Highest refused count in any sample, both allowances | 0 |
| Samples with a refusal series missing or unreadable, both allowances | 0 |
| `http_failure` events with `cause_kind=request_limit_reached` | 0 |
| `trawl_http_request_allowance`, regular / control | 32 / 4 |
| Sampled peak of `trawl_http_requests_in_progress{allowance="regular"}` | 7, at t = 8.0 s |
| Sampled peak of `trawl_http_requests_in_progress{allowance="control"}` | 1 |
| Events written to the senders / stored, distinct per sender | 148,740 / 148,740 for each of 3 |
| Duplicate events stored | 0 |
| `compaction_complete` events while the senders delivered | 282 (316 in the session) |
| `rollup_complete` events while the senders delivered | 35, from 03:05:43.18 to 03:05:52.91 UTC (t = 62.4 to 72.1 s) |
| Search queries / answered 200 | 83 / 83 |
| Failed `/metrics` scrapes | 0 |
| Cleanup: processes / Postgres container | removed / removed |

The terminal snapshot is one more `/metrics` scrape at t = 199.2 s. The
script takes it after the query loop and the three Vector processes have
stopped, and before it stops trawld. A refusal after the last sample is
therefore still counted.

The peak is a sampled peak, not an exact high-water mark. The scraper read
`/metrics` every 0.25 s. The median gap between samples was 0.25 s and the
largest gap was 2.0 s, so a peak shorter than a gap can be missed. The
refusal counters are counters, so the pass condition does not depend on
sampling.

In-progress samples for the regular allowance (631 samples):

| In progress | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Samples | 154 | 169 | 131 | 95 | 46 | 19 | 12 | 5 |

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
  trawl-server -p fleet-admin`, at `e451464ade6624004b0a838451f21deb48da7716`.
  `crates/` and the Cargo files at that commit are the same as at
  `02b6343a`, the head of the earlier runs. `run.py` refuses to start if
  `crates/`, the Cargo files or `bin/trawld-dev` differ from HEAD. The trawld binary's SHA-256 is in `output/summary.json`.
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
- **Duration:** the measured session ran 199 s. The senders received their
  last event at t = 181.0 s, every event was stored at t = 187.0 s, and the
  terminal snapshot was at t = 199.2 s. With the build check, the database
  and the seed sessions, the script took 4 minutes 24 seconds.

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
accepted 186 requests from them. The decoded body had a median of
1,052,192 bytes and a maximum of 1,052,399 bytes, and 132 requests were at
least 900 KB. The gzip body had a median of 24,259 bytes. The replayed
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

It sent 83 queries. All answered 200, with a median of 866 ms and a maximum
of 3,310 ms.

### Compaction and rollup

No endpoint or CLI command starts a compaction or a rollup. Both run in the
compaction task, at the default interval of 10 s and on hot-buffer pressure.
That is all the run needs for compaction: trawld logged 282
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
   `rollup_complete` events, each with `hourly_files=1`, at t = 62.4 to 72.1 s.

Files move only while trawld is stopped. `output/summary.json` lists them
under `seededHistory`.

## What else the run showed

- **The hot buffer, not the request count, limited the backlog.** From
  t = 9.75 s to t = 135.5 s the hot buffer was mostly in the refusing state
  (347 of 631 samples in state 2). trawld answered 153 ingest requests with
  503 `hot_buffer_full`. Vector retried each one with backoff, and every
  event was stored once. Vector's adaptive concurrency reduces its
  concurrency after a 503, so a hot-buffer refusal also lowers the request
  count.
- **The sampled peak came while the hot buffer filled.** In-progress reached
  6 to 7 from t = 8.0 s to t = 9.75 s, while the hot buffer went from 49,715
  to 78,756 events. It reached 6 again at t = 81 to 89 s. During the rollup
  window it was 0 to 4.
- Vector's own logs show no request timeouts and no errors. Vector logs a
  retry as `Service Unavailable` without the response body, and it
  suppresses repeats of that warning. The 503 count therefore comes from
  trawld's `http_failure` events (`httpFailuresFromTrawld`).

## What changed in the script

Review found two defects in the script of the earlier runs. Run 5 was the
first on the corrected script, and `output/` comes from it. A later review
found three more, fixed after run 5 without a new run (see below).

- **The script could delete a directory it did not create.** The old
  script refused an existing `--work`, but its cleanup still ran and
  deleted that path. `--work .` could delete the checkout. The script now
  creates `--work` with one `mkdir`. It refuses an existing path, a
  symlink, the filesystem root, `$HOME`, `/tmp`, the repository root and
  its ancestors before it creates anything, and a refused path is never
  created. See "The script no longer deletes `--work`" below for what
  became of the deletion itself.
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
- **A swapped work directory.** Between the inode check and `rmtree`,
  another process could replace `--work` with a different directory. A
  first fix refused a `--work` whose immediate parent another uid can
  write to. A later review showed that an ancestor higher up could still
  be swapped for a symlink, and that `rmtree` then deletes elsewhere. The
  next item removes the deletion.

- **The script no longer deletes `--work`.** Every fix to the deletion
  left a check-then-delete window, so the script now has no recursive
  delete at all. `Run.remove_work`, the device and inode bookkeeping, the
  parent-ownership rule, the `--keep-work` option and the `workDir`
  cleanup field are gone. The pass check no longer reads `workDir`, so
  an older `summary.json` that carries it still passes. The script
  creates `--work` fresh (mode 0700) and prints its path at the end.
  The operator removes it. It holds synthetic fixture data, the raw logs
  and trawld's throwaway self-signed loopback TLS key. It holds no
  database or API credential. The script still removes the Postgres
  container and its processes, and `--out` must not sit inside `--work`.

After PR #297's CodeQL scan:

- **The database password was written to disk.** The script put it in
  `private/postgres.env` for `docker run --env-file` and set the file to
  mode 0600 afterwards, so the file existed for a moment at the umask's
  permissions (CodeQL `py/clear-text-storage-sensitive-data`). The script
  now writes no credential to a file. `docker run` gets `--env
  POSTGRES_PASSWORD` with no value, and the value is set only in the
  environment of that one `docker` process, not on the command line, which
  any local user can read through `/proc`. The DSNs already reached
  trawld, and the keys Vector, through their environments, and the
  configs the script writes hold no secret. Those configs are still
  created with `os.open(O_CREAT | O_EXCL | O_NOFOLLOW, 0o600)`, so they
  are owner-only from the moment they exist and the script refuses an
  existing path or a symlink. This does not change what the script
  measures, so `output/` was not re-run.

Run 5's summary has every field the new check reads.
`output/predicate-check.txt` shows that it passes, so `output/` was not
re-run. Run 5 judged container cleanup by the old rule. After the fix, no
container with the script's `trawl.experiment` label existed on the
Docker host.

[`test_run.py`](test_run.py) runs the real `collect()` and `verdict()` on
in-memory scrapes and a written trawld log, with one case for each pass
condition. It runs `cleanup()` against stubbed Docker answers. It also
checks that the `--work` rules create nothing they refuse (an existing
path, a symlink, a dangerous path) and that `main` leaves `--work` in
place after a clean exit and after a failure. It checks that the `docker run` command line never carries the
password and that the private files are created 0600. It needs no build,
database or Vector.

## Threat model

`run.py` runs as the invoking user. It creates `--work` itself, and it
never deletes it. There is no check-then-delete window, and so no rule
about who owns or can write to the parent of `--work`. The script
deletes nothing recursively; it only removes its own Postgres container
and stops its own processes.

- **Writing.** `--work` must not exist, must not be a symlink or pass
  through one, and must not be the filesystem root, `$HOME`, `/tmp`, the
  repository root or an ancestor of it. `mkdir` is the claim, so a path
  that appears after the check is refused too. The configs are created
  with `O_CREAT | O_EXCL | O_NOFOLLOW` and mode 0600.
- **Processes running as the same user** are trusted. They can already
  read and delete anything the user can.
- **Cleanup** is the operator's: delete `--work` when done. It holds
  synthetic data and no credential the script wrote.

## Earlier runs

The script changed between runs. The earlier summaries are in
[`earlier-runs/`](earlier-runs/). Runs 1 to 4 used the old pass check from
[What changed in the script](#what-changed-in-the-script). Their end
counters were present and 0, but the old check would also have passed
with a refusal series missing. All five runs refused nothing.

| Run | Head | Sampled peak (regular) | Refused | What differed |
| --- | --- | --- | --- | --- |
| 1 | `372e8d7a` | 8 | 0 | The page query matched `fixture_source="journald"`. Once more than 100,000 events matched, 18 of its 51 queries answered 400 `result_too_large`. |
| 2 | `02b6343a` | 14, at t = 202.5 s | 0 | Interrupted during the delivery wait, see below. |
| 3 | `02b6343a` | 10, at t = 11.25 s | 0 | Without the batch-size and failure tallies. |
| 4 | `02b6343a` | 8, at t = 12.0 s | 0 | The old pass check. Its `output/` was replaced by run 5. |
| 5 | `e451464a` | 7, at t = 8.0 s | 0 | The corrected script. `output/` |

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

`--work` must not exist, must not be a symlink and must not be under
`/tmp`. A missing parent is created with mode 0700. `--work` holds the
data, the configs and the raw logs, and no database or API credential.
The script never deletes it. It prints the path at the end, and you
remove it when you are done:

```bash
rm -rf .flow-scratch/default-load
```

Only summaries reach `--out`, which defaults to `output/`. `--help` lists
the workload options.

This run used:

```bash
python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/run.py \
    --work .flow-scratch/w10-default-load \
    --vector ~/.cache/trawl-vector-0.57.0/vector-x86_64-unknown-linux-gnu/bin/vector
```

To check the pass predicate, the container cleanup and the `--work` rules
without a full run:

```bash
python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/test_run.py
```

To apply the pass check to the committed summary:

```bash
python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/run.py \
    --check-summary docs/launch/evidence/2026-10-08-issue-293/default-load/output/summary.json
```

## Files

| File | Contents |
| --- | --- |
| [`run.py`](run.py) | The run: build check, Postgres, keys, seed sessions, trawld, senders, query loop, scraper, terminal snapshot, summary, cleanup |
| [`test_run.py`](test_run.py) | Checks of the pass predicate, the container cleanup and the `--work` rules, without a full run |
| [`trawld.toml`](trawld.toml) | trawld's configuration, with the data path filled in at run time |
| [`output/summary.json`](output/summary.json) | Every number above |
| [`output/predicate-check.txt`](output/predicate-check.txt) | The current pass check applied to `output/summary.json`, with the command and the `run.py` blob SHA |
| [`output/metrics-samples.csv`](output/metrics-samples.csv) | Every `/metrics` sample: time, scrape latency, request counts and allowances, hot buffer, WAL and parquet file counts |
| [`output/metrics-final.prom`](output/metrics-final.prom) | The terminal snapshot, request, hot-buffer, ingest, compaction and query series only |
| [`output/compaction-rollup-events.ndjson`](output/compaction-rollup-events.ndjson) | trawld's `compaction_complete` and `rollup_complete` events, with timestamps |
| [`output/queries.csv`](output/queries.csv) | Each search query: time, kind, status, latency |
| [`earlier-runs/`](earlier-runs/) | Summaries of runs 1 to 3, and run 2's samples |
