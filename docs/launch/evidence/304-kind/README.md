# Issue #304 kind evidence: AC1 before and after, and the AC19 mixed run

These runs took place on `fractal`, on the local kind cluster `trawl-i304`, on 2026-10-17 and 2026-10-18 (UTC). They cover two acceptance criteria of issue #304:

- **AC1.** At `b02b8acf`, a workload that holds every executor permit gets readiness 503 with `query_not_started kind=ping`, and a query that spills past 512Mi gets the pod evicted for the `tmp` emptyDir. On the PR head, the same workload keeps the pod Ready, answers 503 `query_resource_exhausted` to the spilling query, and evicts nothing.
- **AC19.** A mixed run of ingest, queries, compaction and repin at the new defaults, in the chart's 8Gi limit, finishes with no OOM kill, no eviction and no `query_resource_exhausted` for ordinary searches. The run records peak RSS and peak scratch bytes.

The before run shows both failures. The after run keeps the pod Ready and evicts nothing. Its spilling query answered 503 `query_resource_exhausted` in one attempt of eight. The other seven fit the head's 4 GiB spill budget and answered 200. The mixed run passed.

## Files

| File | Contents |
| --- | --- |
| `build-image.sh REPO SHA LABEL` | Builds `trawl-i304:LABEL-<sha12>` from one commit through the release image path, then loads it into the kind cluster. |
| `repro.sh IMAGE LABEL CHART_DIR` | The AC1 scenario, end to end, in the fresh namespace `i304-LABEL`. It writes `LABEL-transcript.txt`. |
| `values.yaml` | AC1 scenario values, the same for both runs: 2 permits, rate limits off, 4Gi and 1 CPU, no web sidecar. |
| `values-load.yaml` | Layered over `values.yaml` only while the AC1 corpus loads (16 CPU, 6Gi), then removed by a `helm upgrade`. |
| `gen-events.py` | The seeded AC1 corpus and its NDJSON ingest client. |
| `mixed.sh setup IMAGE CHART_DIR`, `mixed.sh run SECS`, `mixed.sh report` | The AC19 mixed run in the namespace `i304-mixed`. It writes `mixed-transcript.txt`. |
| `mixed-ingest.py` | Steady ingest for the mixed run, at a fixed rate, with `_time` set to the moment each batch is built. |
| `values-mixed.yaml` | The mixed run's only overrides: image pull policy, the two DSN Secrets, and no web sidecar. |
| `postgres.yaml` | A disposable in-cluster PostgreSQL for each namespace. |
| `before-transcript.txt` | The AC1 before run, image `trawl-i304:before-b02b8acfb8a2`, 2026-10-17. |
| `after-transcript.txt` | The AC1 after run, image `trawl-i304:after-7dc64b27f566`, 2026-10-18. |
| `mixed-transcript.txt` | The AC19 mixed run on the same after image, 2026-10-18. |

## What ran

- Before: commit `b02b8acf`, image `trawl-i304:before-b02b8acfb8a2`, image ID `sha256:c5f182dd3515c40f216bae9e21dfeadea486c327410b98645ccd462df75aede6` (before transcript line 18).
- After and mixed: commit `7dc64b27f566bc3c1052865d4261eb6e1ee3997d`, image `trawl-i304:after-7dc64b27f566`, image ID `sha256:9dd5d5dd8c26e6a674adeb8f8621a96323784d6202ad03c32c258d90976ec067` (after transcript line 18, mixed transcript line 15). The chart came from the same `git archive` export as the image.
- kind v0.33.0, node Kubernetes v1.37.0, kubectl v1.36.4, Helm v4.2.2 (after transcript lines 3–14). The node has 16 CPUs (mixed transcript line 252).

## Rerun

Set `D` to this directory's copy of the kit, which also holds the cluster's `kubeconfig`. Every script uses only `--kubeconfig $D/kubeconfig --context kind-trawl-i304`.

1. Create the cluster: `kind create cluster --name trawl-i304 --kubeconfig $D/kubeconfig`.
2. Build each image. curl on the build host failed certificate verification against github.com, so `DUCKDB_ZIP` points at a local copy of the pinned DuckDB archive. `distribution.py` checks its sha256 against `duckdb-runtime.json` before it extracts anything.

   ```bash
   DUCKDB_ZIP=<repo>/target/duckdb-runtime-cache/x86_64-unknown-linux-gnu/1fb8ce38…/libduckdb-linux-amd64.zip \
     $D/build-image.sh <repo> b02b8acf before
   DUCKDB_ZIP=... $D/build-image.sh <repo> 7dc64b27 after
   ```

3. Run AC1:

   ```bash
   $D/repro.sh trawl-i304:before-b02b8acfb8a2 before $D/build/src-before/chart/trawl
   $D/repro.sh trawl-i304:after-7dc64b27f566 after $D/build/src-after/chart/trawl
   ```

4. Run AC19. Each part is one command, and each command finished within 10 minutes. Run `run` until the chunks add up to at least 900 s.

   ```bash
   $D/mixed.sh setup trawl-i304:after-7dc64b27f566 $D/build/src-after/chart/trawl
   $D/mixed.sh run 500
   $D/mixed.sh run 500
   $D/mixed.sh report
   ```

Secrets go to `$D/private/<label>/`, which is mode 0700 with files at 0600: the PostgreSQL passwords, the API keys, the bearer-header files and the curl configs. The scripts never print them or put them on a command line.

## AC1 before (`before-transcript.txt`)

| AC1 clause | Verdict | Lines |
| --- | --- | --- |
| Every permit held | Yes. `pool_available` 0 with 3–4 active queries while the workers ran. | 516–530 |
| Readiness 503 | Yes. Health answered 503 with `"duckdb":"error"` 46 times and 200 17 times. The kubelet logged `Readiness probe failed … 503` 7 times and `Liveness probe failed … 503` twice. The pod was `ready=False` from 06:05:34 to 06:06:11. | 449, 510–512, 549–567, 671–672 |
| `query_not_started kind=ping` | Yes. `kind="ping" reason="expired"`, 55 times. | 574, 635 |
| Spill past 512Mi evicts the pod | Yes. `/tmp` held 587,600,160 bytes at 06:08:52.98. The kubelet logged `Evicted … Usage of EmptyDir volume "tmp" exceeds the limit "512Mi"`. The StatefulSet replaced pod `0bef6073…` with `ef08580e…`. The script printed `evicted=yes`. | 772, 1024, 873–874, 1029–1030, 1040 |

## AC1 after (`after-transcript.txt`)

The after run used the same `values.yaml`, 4Gi and 1 CPU, as the before run (line 207). The pod was not OOM-killed, so the 8Gi rerun was not needed.

| AC1 clause | Verdict | Lines |
| --- | --- | --- |
| Every permit held | Yes. `pool_available` 0 with 3–4 active queries in every sample while the workers ran, 08:05:46 to 08:06:38. | 390–400 |
| Pod stays Ready | Yes. Health answered 200 with `"duckdb":"ok"` all 72 times in phase A. The pod watcher shows `ready=True restarts=0` in every sample of phase A and all 123 samples of phase B, on one uid. The kubelet logged no probe failure for `trawl-0`. All 86 STARVE_QUERY responses were 200. | 308, 385, 407–441, 757–879, 451–497, 973–1011 |
| No `query_not_started kind=ping` | Yes. The count is 0, and all 87 outcomes are `success`. | 447, 449 |
| Spilling query answers 503 `query_resource_exhausted` | Partly. Attempt 7 answered 503 `query_resource_exhausted` with the fixed message, logged as `http_failure status=503 cause_kind="duckdb_resource_exhausted"`. Attempts 1–6 and 8 completed with 200 in 8.2–13.0 s. Their spill fit the 4 GiB `query_spill_max_bytes`: query scratch peaked at 2,944,867,460 bytes. | 501–508, 968, 634–753 |
| Evicts nothing | Yes. `/tmp` stayed at 0 bytes in every sample. Spill went to `/var/lib/trawl/data.query-scratch` on the PVC. No `Evicted` event. The script printed `evicted=no`. | 500, 512–631, 973–1011, 1013 |

Phase B's health section prints only its first 60 samples (lines 882–943), all 200. The full sampler file held 252 samples, all 200. It is not committed. The pod watcher above is the committed readiness evidence for phase B.

The transcript does not say which budget attempt 7 ran out of. The head logs only the safe message even at debug (line 967). Scratch held 1,394,182,438 bytes at the last sample before it failed (line 717), well under 4 GiB. So the memory limit is the likely cause, but the run does not prove it.

After the run, the trawld cgroup read `memory.peak` 4294967296 (the 4Gi limit), `memory.events` `max 4`, `oom 0` and `oom_kill 0`, with restart count 0 and an empty `lastState`. Page cache from the scratch writes filled the cgroup up to its limit, and the kernel reclaimed it. These values were read with `kubectl exec` after the script ended, so they are not in the transcript.

## AC19 mixed run (`mixed-transcript.txt`)

`mixed.sh` installed the head chart at its defaults. It overrode only the image, the pull policy, the two DSN Secrets and the web sidecar (`values-mixed.yaml`). The rendered config shows `query_spill_max_bytes = "4GiB"`, `query_memory_limit = "2GiB"`, `query_threads = 4`, `compaction_memory_limit = "1GiB"`, `min_free_disk_bytes = "6GiB"`, and the default rate limits of 100 and 1000 a minute (lines 87–100). The StatefulSet has an 8Gi limit, a 1Gi request, no CPU limit and a 60 s grace period (line 104). trawld sees 16 CPUs, so `max_concurrent_queries` defaults to 8 (line 252).

Setup preloaded 800,000 events into service `i304` and 5,000 events into `i304-repin`, which carries the integer field `code` (lines 255–268). Each of the two chunks then ran these for 500 s, all at once:

- steady ingest of 500 events a second, one batch every 2 s;
- 6 search clients, each with its own key, at most one search a second each, over 8 typical searches (lines 17–24): `stats count() by` small keys, `top 10 host`, `timechart`, time-ranged filters, a full-corpus count, and a full-text scan;
- one live tail, `service=i304 | stats count() by status`;
- a repin dry run at +60 s and a real repin of `code` at +200 s.

The gap between chunks was 23 s (line 670). The samplers covered 1009 s of workload (line 1084).

| Measure | Result | Lines |
| --- | --- | --- |
| Searches | 5321 responses, all 200, 0 `query_resource_exhausted`. Chunk 1 p95 was 1.31 s and max 2.91 s. Chunk 2 p95 was 2.26 s and max 4.57 s. | 300–314, 678–692, 1087–1089 |
| Ingest | 500 batches, all 200: 500,000 events, no `hot_buffer_full` retries. | 318, 696, 1091–1094 |
| Live tail | 200, 250 snapshots in each chunk. The stream ran until curl's 500 s limit. | 324–327, 702–705 |
| Compaction | 120 `compaction_complete` events, all `merged=true`. No `compaction_error`, no maintenance resource failure, no `catalog_invariant_violation`. | 629–641, 996–1010 |
| Repins | Real repin of `code` BIGINT→VARCHAR: succeeded. Dry run of `status` BIGINT→VARCHAR over 1,073,000 rows: succeeded. Real repin of `code` VARCHAR→BIGINT: succeeded. | 1103–1106, 330–337, 708–715 |
| Health and readiness | Health answered 200 497 times. The pod watcher shows `ready=True restarts=0` 195 times, on one uid. | 1096–1098, 566, 933 |
| OOM kills and evictions | None. cgroup `oom_kill 0`, `max 0`. Restart count 0, empty `lastState`. No `Evicted`, `OOMKilling` or `OOMKilled` event. | 1069–1077, 1085–1086 |
| Peak RSS | trawld `VmHWM` 2,989,160 KiB (2.85 GiB) since process start, preload included. Peak sampled anon 2,949,812,224 bytes. | 1080, 1109–1110 |
| Peak cgroup memory | `memory.peak` 4,737,486,848 bytes, page cache included. Peak sampled `memory.current` 4,456,235,008 bytes. | 1071, 1109–1110 |
| Peak query scratch | 43,797,268 bytes (`du -sb` every 2 s). | 1111 |
| Verdict | PASS | 1112 |

The executor pool reached all 8 permits briefly during each repin cutover. trawld logged `pool_pressure` 4 times in chunk 1 and 6 times in chunk 2 (lines 639, 1008). The waiting queries ran and answered 200.

## Deviations

- **The before transcript is relativized.** Every occurrence of the kit's absolute path was replaced with `$D` by one command, `sed "s#$KIT#\$D#g"`, where `KIT` was the kit's absolute path on the build host. Nothing else changed: the line count is 1042 and every line number above matches the original. The original file's sha256 is `59f66e9d0df0b2ebdeb2351aa1f238b4ed524222cd41fd8a149ce4b08afc1295`. The after and mixed transcripts were relativized with the same command, using their own kit path.
- **The after run uses the identical 4Gi values.** `values.yaml` sets the same values as in the before run. Its comments changed after the before run: the chart default is now 8Gi, not 4Gi, and the head logs only the safe query error text.
- **`repro.sh` changed after the before run.** It gained a phase B sampler for the query scratch directory, which runs only where that directory exists, so it did not run at `b02b8acf`. Its header comments also changed: they describe step 0, the load and upgrade split, `STOP_AFTER`, and now the after outcome. No value, query or other command changed. The after run used this `repro.sh`. The only later change is the comment that describes the after outcome.
- **The spilling query mostly fits the head's budgets.** AC1 says the head answers 503 to the spilling query. At the head defaults of 2 GiB memory and 4 GiB spill, SPILL_QUERY answered 503 `query_resource_exhausted` once in 8 attempts and completed the other 7. The workload is unchanged from the before run. The budget-exhaustion contract with a small configured budget is covered by `cargo nextest run -p trawl-server --test query_resources spill_budget`.
- **The mixed run's first dry run failed.** In chunk 1, `mixed.sh` asked for a dry run of `seq`, a field the mixed corpus does not carry. trawld answered 400 `"seq" is not a pinned field` (line 333). The chunk 2 dry run of `status` succeeded. The script still names `seq` for odd chunks, because it is the script that produced this transcript.
- **The mixed run ran in chunks.** Each agent command had a 10-minute limit, so the 1009 s workload ran as two chunks on the same pod, with a 23 s gap. The pod, its data, its cgroup counters and its compaction state carried across the gap.
- **The mixed run reached trawld through a NodePort.** It did not use a port-forward, which drops every open connection when it restarts. The Service publishes NotReady addresses, so health samples would still reach a NotReady pod. TLS verifies trawld's pinned certificate as `localhost`.
- **The mixed workload does not cross an hour boundary.** Every compaction merged into the current hourly file (`merged=true`). The file grew from 800,000 rows to about 1.3 million.
- **The kubelet refreshes volume stats every 2 s, not 1m.** `repro.sh` step 0 sets `volumeStatsAggPeriod: 2s` on the kind node. It is a cluster setting, and it makes "nothing evicted" a stronger result.
