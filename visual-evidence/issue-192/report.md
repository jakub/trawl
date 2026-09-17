# History cost results

The recorded measurements used `measure.py` at commit
`71dd9661a71d71575fa141bce9372d1248979a8b`. A later input-validation fix
rejects malformed URLs without a traceback. SQL and measurement behavior
are unchanged, and these measurements were not rerun. `provenance.json`
records the measured runner revision/hash separately from current artifact hashes.

The 200,000-row fixture completed all 16 cases and 80 measured transactions.
With default planning, median statement sums were 8.873–9.070 ms unfiltered,
47.237–48.355 ms for common matches, 106.622–166.259 ms for sparse matches,
and 168.472–169.685 ms for no matches. These are sequential warm SQL
measurements, not application latency or exact connection-hold times.

## Environment and reproduction

The production SQL is from revision
`064a6929c3c137496b4d79aacfeec88644ad28ed`. The infrastructure owner ran the
measurement during the 2026-09-17 implementation session against database
`trawl_issue_192_cost`, role `fleet`, on `127.0.0.1:65433`. See the
[runner instructions](README.md), [SQL](queries.sql), and [seed](seed.sql).
The invocation was the following, with test credentials supplied privately
in the environment:

```bash
python3 visual-evidence/issue-192/measure.py --output visual-evidence/issue-192/results
```

The runner was corrected before the successful measurement to pass parsed
URL fields as explicit libpq environment variables (`PGHOST`, `PGPORT`,
`PGUSER`, `PGDATABASE`, and `PGPASSWORD`). The first attempt failed before
connecting. The [provenance manifest](provenance.json) records the corrected runner and
all retained artifact hashes. The failed pre-connection attempt produced no
measurement and is excluded from the results.

[Database metadata](results/identity.log) records PostgreSQL 18.6, Debian
18.6-1.pgdg13+2, x86-64; UTF-8 encoding; libc locale provider; `en_US.utf8`
collation and character classification; collation version 2.41. The default
isolation was `read committed`. Every case log confirms active
`repeatable read` and read-only `on` on its measurement connection.

The owner identified the database container as
`codex-trawl-six-20260917-default-1`, image
`sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280`,
with port 65433 forwarded to 5432. Its PostgreSQL data directory was on
**tmpfs**, with `fsync`, `synchronous_commit`, and `full_page_writes` off.
These are disposable test settings, not durable-installation settings.
The companion container on port 65434 was not the measurement target.

[Runner hardware](results/runner-hardware.json) records Linux
7.2.4-arch1-2, glibc 2.44, AMD Ryzen 7 7800X3D 8-Core Processor,
16 logical CPUs, and `MemTotal: 32560504 kB` (about 31.1 GiB).
The database ran on that same host. Docker imposed no explicit CPU quota,
CPU set, or memory limit. The data tmpfs capacity was 16,670,978,048 bytes;
`/dev/shm` was 67,108,864 bytes. The [container limits](container-limits.json)
retain the selected Docker fields. No disk performance can be inferred.
Coastwatch baseline Cargo checks and Trawl browser work ran concurrently on
the host. The owner serialized database measurement under its database lock;
this was not an otherwise idle-host experiment.

Planner settings were `shared_buffers=128MB`, `work_mem=4MB`,
`effective_cache_size=4GB`, `random_page_cost=4`, `seq_page_cost=1`,
`max_parallel_workers_per_gather=2`, and `jit=on`.

[Distribution](results/distribution.log) confirms 100,000 rows per key,
284–326 bytes per query, 50,000 common matches per key, 100 sparse matches
per key, and zero absent-marker matches. Rows are interleaved by key, with
tied timestamps and deterministic varied text. The heap occupies 78,020,608
bytes; indexes occupy 18,997,248 bytes. Only the primary key and existing
`query_history_key_executed_idx (key_id, executed_at DESC, id DESC)` were
present. The seed ran `ANALYZE`; it did not add a vacuum phase or a new index.

## Warm statement timings

Each combination had a fresh connection and prepared statement pair, six
warm-up transactions, and five measured transactions. All used limit 50 and
one read-only repeatable-read transaction for count plus page. Offset 50 is
the second page, not a deep-offset workload. Cases ran in the table's order,
without randomization: all default cases, then all forced-generic cases.

Each cell below is **median (minimum–maximum)** in milliseconds across five
samples. The [CSV](results/timings.csv) retains all five individual samples
per case, including BEGIN, SET, count, page, and COMMIT. Each case link contains
both raw plans and prepared-statement counters. Statement sum is the sum of
those five psql durations, calculated per sample before taking its median.
It excludes gaps between statements and is not an exact connection-hold time.

| Mode / filter / offset (raw log) | Count ms | Page ms | Statement sum ms |
| --- | --- | --- | --- |
| [auto / unfiltered / 0](results/auto-unfiltered-offset-0.log) | 8.611 (8.591–8.732) | 0.149 (0.129–0.152) | 8.873 (8.855–8.978) |
| [auto / unfiltered / 50](results/auto-unfiltered-offset-50.log) | 8.818 (8.760–9.053) | 0.140 (0.127–0.146) | 9.070 (9.003–9.309) |
| [auto / no-match / 0](results/auto-no-match-offset-0.log) | 50.136 (48.699–54.107) | 120.810 (118.905–124.227) | 169.685 (168.397–178.055) |
| [auto / no-match / 50](results/auto-no-match-offset-50.log) | 49.317 (48.023–50.118) | 119.460 (118.569–121.264) | 168.472 (167.580–171.543) |
| [auto / sparse / 0](results/auto-sparse-offset-0.log) | 48.540 (48.035–49.444) | 57.997 (57.882–58.404) | 106.622 (106.216–108.016) |
| [auto / sparse / 50](results/auto-sparse-offset-50.log) | 47.748 (47.651–48.381) | 118.340 (117.398–118.867) | 166.259 (165.193–167.408) |
| [auto / common / 0](results/auto-common-offset-0.log) | 46.799 (46.380–46.998) | 0.297 (0.270–0.320) | 47.237 (46.783–47.412) |
| [auto / common / 50](results/auto-common-offset-50.log) | 47.830 (46.991–49.377) | 0.435 (0.394–0.485) | 48.355 (47.549–49.944) |
| [force_generic_plan / unfiltered / 0](results/force_generic_plan-unfiltered-offset-0.log) | 7.867 (7.763–8.034) | 0.099 (0.099–0.128) | 8.066 (7.990–8.237) |
| [force_generic_plan / unfiltered / 50](results/force_generic_plan-unfiltered-offset-50.log) | 7.858 (7.827–8.218) | 0.127 (0.105–0.165) | 8.075 (8.052–8.468) |
| [force_generic_plan / no-match / 0](results/force_generic_plan-no-match-offset-0.log) | 54.031 (53.780–56.411) | 133.740 (133.505–138.666) | 188.587 (187.456–195.265) |
| [force_generic_plan / no-match / 50](results/force_generic_plan-no-match-offset-50.log) | 54.697 (54.262–55.916) | 134.985 (132.941–139.974) | 189.516 (187.778–196.060) |
| [force_generic_plan / sparse / 0](results/force_generic_plan-sparse-offset-0.log) | 54.232 (53.806–54.927) | 66.022 (65.351–66.169) | 120.424 (119.487–120.964) |
| [force_generic_plan / sparse / 50](results/force_generic_plan-sparse-offset-50.log) | 54.506 (53.600–56.840) | 132.993 (131.997–134.047) | 188.032 (185.757–190.749) |
| [force_generic_plan / common / 0](results/force_generic_plan-common-offset-0.log) | 54.419 (52.907–55.754) | 0.277 (0.269–0.473) | 54.822 (53.301–56.298) |
| [force_generic_plan / common / 50](results/force_generic_plan-common-offset-50.log) | 53.975 (52.661–55.837) | 0.405 (0.389–0.585) | 54.516 (53.190–56.499) |

## Plans and prepared-statement behavior

All eight `auto` cases recorded **0 generic / 11 custom** executions for
both count and page before EXPLAIN, then **0 / 12** afterward. All eight
`force_generic_plan` cases recorded **11 generic / 0 custom**, then
**12 / 0**. Default planning did not switch to generic in this workload.
These counters describe fresh, homogeneous per-case sessions. They do not
establish what a long-lived SQLx connection with mixed keys, filters, and
pages will choose.

All 16 count plans used Finalize Aggregate → Gather → Partial Aggregate →
Parallel Seq Scan, launching two workers (three scan loops with the leader).
Each scanned the complete 200,000-row table and reported 9,524 shared buffer
hits. Counts selected 100,000 / 0 / 100 / 50,000 rows for unfiltered /
no-match / sparse / common respectively. EXPLAIN reports per-loop averages
for parallel rows removed: 33,333 / 66,667 / 66,633 / 50,000, rounded. Those
figures correspond to rejecting 100,000 / 200,000 / 199,900 / 150,000 rows
over the complete scan; they must not be read as whole-query totals.

All page plans used Limit → Index Scan on the existing history index,
with the key as the index condition. There was no Sort node. Substring
matching remained a residual filter, so the index supplied key scope and
order but did not locate substring matches. Page work below was identical
in both plan modes; rows below Limit include offset rows that are discarded.

| Filter | Offset | Matching index rows produced | Rows removed by filter | Returned rows | Shared buffer hits |
| --- | --- | --- | --- | --- | --- |
| unfiltered | 0 | 50 | 0 | 50 | 8 |
| unfiltered | 50 | 100 | 0 | 50 | 13 |
| no-match | 0 | 0 | 100000 | 0 | 10404 |
| no-match | 50 | 0 | 100000 | 0 | 10404 |
| sparse | 0 | 50 | 48951 | 50 | 5099 |
| sparse | 50 | 100 | 98901 | 50 | 10300 |
| common | 0 | 50 | 49 | 50 | 13 |
| common | 50 | 100 | 99 | 50 | 24 |

All captured execution buffer activity was shared hits, with no reported
shared reads or temporary spill. These are warm-cache plans. The separate
EXPLAIN execution times below include instrumentation and were collected
after the five timings; they are not additional samples in the timing table.

| Mode / filter / offset | EXPLAIN count execution ms | EXPLAIN page execution ms |
| --- | --- | --- |
| auto / unfiltered / 0 | 11.096 | 0.020 |
| auto / unfiltered / 50 | 10.412 | 0.022 |
| auto / no-match / 0 | 49.248 | 124.562 |
| auto / no-match / 50 | 48.816 | 124.197 |
| auto / sparse / 0 | 48.332 | 61.723 |
| auto / sparse / 50 | 47.601 | 122.627 |
| auto / common / 0 | 47.166 | 0.175 |
| auto / common / 50 | 47.997 | 0.297 |
| force_generic_plan / unfiltered / 0 | 9.403 | 0.018 |
| force_generic_plan / unfiltered / 50 | 10.619 | 0.034 |
| force_generic_plan / no-match / 0 | 54.716 | 140.674 |
| force_generic_plan / no-match / 50 | 54.291 | 143.674 |
| force_generic_plan / sparse / 0 | 53.988 | 68.283 |
| force_generic_plan / sparse / 50 | 54.147 | 140.859 |
| force_generic_plan / common / 0 | 55.105 | 0.221 |
| force_generic_plan / common / 50 | 55.349 | 0.299 |

Custom plans folded the supplied filter into a constant, and removed the
optional substring predicate entirely when the filter was NULL. Generic
plans retained `$1`, `$2 IS NULL`, and `lower($2)`. Their page Limit estimate
was 3,367 rows at both offsets, despite the actual bound of 50; custom plans
estimated 50 at Limit. Both modes nevertheless chose the same scan shapes.

Forced-generic filtered statement-sum medians were higher in every paired
case: about 188–190 ms for no matches, 120–188 ms for sparse matches, and
54.5–54.8 ms for common matches. Default medians were about 168–170 ms,
107–166 ms, and 47.2–48.4 ms respectively. The remaining runtime predicate
is a plausible contributor, but this fixed-order run with concurrent host
work does not isolate its cost. Unfiltered forced-generic medians were
slightly lower (about 8.1 ms versus 8.9–9.1 ms), which also rules out a
blanket claim that generic planning was slower in every case.

## Runtime implications and limits

The page limit bounds returned rows, not count work or the work required
to find those rows. No-match pages evaluated all 100,000 caller candidates
after the count had scanned the whole table. The second sparse page visited
99,001 caller candidates to produce 100 matching rows and return the last
50. In contrast, common and unfiltered pages stopped near the start of the
ordered index, leaving the count as their dominant statement cost.

The application acquires one connection from its shared pool of eight and
holds it through count, page, row decoding, and commit. The read-only
repeatable-read transaction gives one response a consistent snapshot; it
does not release the connection between scans. The observed expensive cases
therefore demonstrate work that occupies a pool slot even when zero rows
are returned. Multiple such reads can compete with other users of that pool
and for CPU; count also launched parallel workers in this fixture. This run
did not measure that contention and cannot establish throughput, queueing
time, or a safe concurrency threshold from eight times a reciprocal timing.

Psql statement sums include statement request/response cost but exclude
pool wait, inter-statement gaps, application decoding, and HTTP/browser work.
They are neither application latency nor an exact measure of application
connection occupancy. The fixture measures warm sequential prepared SQL
on 200,000 synthetic rows, with a RAM-backed database, disabled durability
settings, and concurrent non-database host work. It does not measure cold
cache, durable storage, deeper offsets, mixed prepared-statement histories,
concurrency, or larger retained volumes. It establishes no installation-wide
latency SLA and makes no claim about unmeasured scale.
