# Issue #304 kind evidence: AC1 before and after, and the AC19 mixed run

These runs took place on `fractal`, on the local kind cluster `trawl-i304`, on 2026-10-17 and 2026-10-18 (UTC). They cover two acceptance criteria of issue #304:

- **AC1.** At `b02b8acf`, a workload that holds every executor permit gets readiness 503 with `query_not_started kind=ping`, and a query that spills past 512Mi gets the pod evicted for the `tmp` emptyDir. On the PR head, the same workload keeps the pod Ready, answers 503 `query_resource_exhausted` to the spilling query, and evicts nothing.
- **AC19.** A mixed run of ingest, queries, compaction and repin at the new defaults, in the chart's 8Gi limit, finishes with no OOM kill, no eviction and no `query_resource_exhausted` for ordinary searches. The run records peak RSS and peak scratch bytes.

The before run shows both failures. The after run keeps the pod Ready and evicts nothing. The incident's spilling query (phase B) needs less spill than the head's 4 GiB budget, and it answered 200 in seven attempts of eight. A wider query that needs more spill than the budget (phase C) answered 503 `query_resource_exhausted` in all four attempts, and a small query then answered 200. The mixed run passed.

## Files

| File | Contents |
| --- | --- |
| `build-image.sh REPO SHA LABEL` | Builds `trawl-i304:LABEL-<sha12>` from one commit through the release image path, then loads it into the kind cluster. |
| `repro.sh IMAGE LABEL CHART_DIR` | The AC1 scenario, end to end, in the fresh namespace `i304-LABEL`: deploy and ingest, phase A (starvation), phase B (the incident's spill query) and phase C (a query over the spill budget, skipped when phase B has already evicted the pod). It writes `LABEL-transcript.txt`. |
| `values.yaml` | AC1 scenario values, the same for both runs: 2 permits, rate limits off, 4Gi and 1 CPU, no web sidecar. |
| `values-load.yaml` | Layered over `values.yaml` only while the AC1 corpus loads (16 CPU, 6Gi), then removed by a `helm upgrade`. |
| `gen-events.py` | The seeded AC1 corpus and its NDJSON ingest client. |
| `mixed.sh setup IMAGE CHART_DIR`, `mixed.sh run SECS`, `mixed.sh report` | The AC19 mixed run in the namespace `i304-mixed`. It writes `mixed-transcript.txt`. |
| `mixed-ingest.py` | Steady ingest for the mixed run, at a fixed rate, with `_time` set to the moment each batch is built. |
| `values-mixed.yaml` | The mixed run's only overrides: image pull policy, the two DSN Secrets, and no web sidecar. |
| `postgres.yaml` | A disposable in-cluster PostgreSQL for each namespace. |
| `before-transcript.txt` | The AC1 before run, image `trawl-i304:before-b02b8acfb8a2`, 2026-10-17. |
| `after-transcript.txt` | The AC1 after run, phases A to C, image `trawl-i304:after-7dc64b27f566`, 2026-10-18. It replaces an earlier after run with phases A and B only (see AC1 after). |
| `mixed-transcript.txt` | The AC19 mixed run on the same after image, 2026-10-18. |

## What ran

- Before: commit `b02b8acf`, image `trawl-i304:before-b02b8acfb8a2`, image ID `sha256:c5f182dd3515c40f216bae9e21dfeadea486c327410b98645ccd462df75aede6` (before transcript line 18).
- After and mixed: commit `7dc64b27f566bc3c1052865d4261eb6e1ee3997d`, image `trawl-i304:after-7dc64b27f566`, image ID `sha256:9dd5d5dd8c26e6a674adeb8f8621a96323784d6202ad03c32c258d90976ec067` (after transcript line 20, mixed transcript line 15). The chart came from the same `git archive` export as the image.
- kind v0.33.0, node Kubernetes v1.37.0, kubectl v1.36.4, Helm v4.2.2 (after transcript lines 3–13). The node has 16 CPUs (mixed transcript line 252).

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
   STOP_AFTER=b $D/repro.sh trawl-i304:after-7dc64b27f566 after $D/build/src-after/chart/trawl
   START_AT=c $D/repro.sh trawl-i304:after-7dc64b27f566 after $D/build/src-after/chart/trawl
   ```

   The after run takes about 9 minutes, and each agent command had a 10-minute limit, so it ran as two commands. `STOP_AFTER=b` stops after phase B. `START_AT=c` runs phase C against the same namespace and appends to the same transcript. Without either variable, one command runs every phase. At `b02b8acf`, phase B evicts the pod, so the script skips phase C and says so in the transcript. The committed before transcript was recorded before phase C existed.

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

The after run used the same `values.yaml`, 4Gi and 1 CPU, as the before run (line 209). The pod was not OOM-killed, so the 8Gi rerun was not needed. The rendered config keeps the head defaults: `query_spill_max_bytes = "4GiB"`, `query_memory_limit = "2GiB"` and `query_threads = 4` (lines 204–206).

This transcript replaces an earlier after run, recorded from 08:03 to 08:11 UTC on 2026-10-18 with the same image and values. That run had phases A and B only. Its spilling query fit the 4 GiB spill budget and answered 200 in 7 of 8 attempts, so it did not show the 503 clause. Phase C was added for that clause, and the whole after leg ran again in a fresh `i304-after` namespace. The earlier transcript is not committed.

The run took two commands on one pod (see Rerun): setup through phase B from 09:00:42 to 09:08:07 (lines 2–1022), then phase C from 09:08:11 to 09:09:53 (lines 1024–1352).

| AC1 clause | Verdict | Lines |
| --- | --- | --- |
| Every permit held | Yes. `pool_available` 0 with 3–4 active queries in every sample while the workers ran, 09:02:23 to 09:03:15. | 393–403 |
| Pod stays Ready throughout | Yes. The pod watcher shows `ready=True restarts=0` on one uid in all 36 samples of phase A, all 124 of phase B and all 42 of phase C. Health answered 200 with `"duckdb":"ok"` all 72 times in phase A and all 93 times in phase C. The kubelet logged no probe failure for `trawl-0` in any phase. All 87 STARVE_QUERY responses were 200. | 410–445, 764–887, 1131–1172, 315–388, 1176–1272, 455–488, 981–1014, 1310–1343, 311 |
| No `query_not_started kind=ping` | Yes. The count is 0, and all 88 outcomes are `success`. | 451, 453 |
| Phase B: the incident's spill query, under budget, completes with `/tmp` at 0 | Yes. Attempts 1 and 3–8 answered 200 in 9.2–12.4 s. Query scratch peaked at 2,838,630,950 bytes, under the 4 GiB budget. `/tmp` was 0 bytes in all 120 samples. Attempt 2 answered 503 `query_resource_exhausted` after 7.7 s, with scratch at 530,220,407 bytes in its last sample (see below). | 505–512, 640–760, 517–636, 506, 656, 961 |
| Phase C: the over-budget query answers 503 `query_resource_exhausted` | Yes, all 4 attempts, in 10.5–13.5 s, each with the fixed message. trawld logged each one as `http_failure status=503 cause_kind="duckdb_resource_exhausted"`. The stored `http_failure` events (`service=trawld`) carry the same status and cause for the same four request IDs. | 1033–1036, 1280, 1285, 1290, 1295, 1305–1308 |
| Phase C: scratch stays at or under the budget plus slack | Yes. The 2 s sampler's peak was 3,470,535,948 bytes. The budget plus DuckDB's block slack is 4,294,967,296 + 1,179,648 = 4,296,146,944 bytes. | 1084–1122, 1123 |
| Phase C: a small query then succeeds | Yes. `service=i304 \| stats count()` answered 200 in 0.11 s with 800,000. | 1037 |
| Evicts nothing | Yes. `/tmp` was 0 bytes in every sample of phases B and C, because spill goes to `/var/lib/trawl/data.query-scratch` on the PVC. The transcript has no `Evicted` event. The pod kept uid `df57fbca…` with restart count 0 and an empty `lastState`. The script printed `evicted=no` after phase B and again after phase C. | 517–636, 1042–1080, 1349, 1021, 1350 |

Phase B's health section prints only its first 60 samples (lines 891–950), all 200. The full sampler file held 255 samples, all 200 with `"duckdb":"ok"`. It is not committed. The pod watcher is the committed readiness evidence for phase B. Phase C prints its whole health log.

**Which budget stops each query.** The head logs only the safe message, even at debug (lines 960, 1279), so the transcript names no budget. The scratch samples tell them apart:

- Phase C: the 2 s sampler misses the last moments before each failure, because scratch grows by 300–500 MB a second. Before the run, in throwaway namespaces with the same image, values and corpus, SPILL_BIG_QUERY ran 10 times. All 10 answered 503 `query_resource_exhausted` in 9.6–14.0 s. In two of them, an in-pod sampler every 0.2 s saw scratch climb steadily to 4,294,968,827 bytes (1,531 bytes past the budget, inside the slack) and to 4,267,680,306 bytes in the last sample before the failure. In the four runs where it was sampled, trawld's anon memory peaked at 2.41–2.66 GB. So the 4 GiB spill budget stops this query, not the 2 GiB memory limit. The same calibration ran SPILL_QUERY once: it answered 200 in 11.2 s with a sampled scratch peak of 2,478,998,783 bytes.
- Phase B attempt 2 failed with 530 MB of scratch, far under the spill budget, so it most likely ran out of the 2 GiB memory limit. The run does not prove it.

After the run, the trawld cgroup read `memory.peak` 4294967296 (the 4Gi limit), `memory.events` `max 0`, `oom 0` and `oom_kill 0`, with restart count 0 and an empty `lastState`. Page cache from the scratch writes filled the cgroup up to its limit, and the kernel reclaimed it. These values were read with `kubectl exec` after the script ended, so they are not in the transcript.

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
- **`repro.sh` changed after the before run.** It gained a phase B sampler for the query scratch directory, which runs only where that directory exists, so it did not run at `b02b8acf`. It gained phase C, with its constants (`SPILL_BIG_QUERY`, `SPILL_BIG_ATTEMPTS=4`, `SPILL_BIG_PAUSE=5`, `SMALL_QUERY`) printed in the transcript header, and the `STOP_AFTER=b` and `START_AT=c` pair. Its header comments also changed: they describe step 0, the load and upgrade split, `STOP_AFTER`, phase C and the after outcome. Phases A and B, their values and their queries did not change. The after run used this `repro.sh`.
- **The incident's spilling query mostly fits the head's budgets, so phase C adds a wider one.** At the head defaults of 2 GiB memory and 4 GiB spill, SPILL_QUERY needs about 2.8–2.9 GB of scratch, and it completed in 7 of 8 attempts in both after runs. The budgets stay at the chart defaults; none was lowered. Instead, phase C's SPILL_BIG_QUERY widens the same `dedup` key from 32 to 64 copies of the event text, about 16 KB a row and a 12.8 GB sort, so it needs more spill than the budget. Like SPILL_QUERY, it is synthetic. The budget-exhaustion contract with a small configured budget is also covered by `cargo nextest run -p trawl-server --test query_resources spill_budget`.
- **The after run ran as two commands.** It takes about 9 minutes, and each agent command had a 10-minute limit. `STOP_AFTER=b` ended the first command after phase B at 09:08:07, and `START_AT=c` started phase C on the same pod at 09:08:11 (lines 1022–1024). Phase C reads phase B's result from the transcript (line 1027).
- **The after run's load hit one 503 `hot_buffer_full`.** The stored `http_failure` list shows one ingest 503 at 09:01:34 (line 1303), while the corpus loaded in the roomy load pod. `gen-events.py` retries a 503 after its `Retry-After`, and all 800,000 events landed (lines 143, 146).
- **The mixed run's first dry run failed.** In chunk 1, `mixed.sh` asked for a dry run of `seq`, a field the mixed corpus does not carry. trawld answered 400 `"seq" is not a pinned field` (line 333). The chunk 2 dry run of `status` succeeded. The script still names `seq` for odd chunks, because it is the script that produced this transcript.
- **The mixed run ran in chunks.** Each agent command had a 10-minute limit, so the 1009 s workload ran as two chunks on the same pod, with a 23 s gap. The pod, its data, its cgroup counters and its compaction state carried across the gap.
- **The mixed run reached trawld through a NodePort.** It did not use a port-forward, which drops every open connection when it restarts. The Service publishes NotReady addresses, so health samples would still reach a NotReady pod. TLS verifies trawld's pinned certificate as `localhost`.
- **The mixed workload does not cross an hour boundary.** Every compaction merged into the current hourly file (`merged=true`). The file grew from 800,000 rows to about 1.3 million.
- **The kubelet refreshes volume stats every 2 s, not 1m.** `repro.sh` step 0 sets `volumeStatsAggPeriod: 2s` on the kind node. It is a cluster setting, and it makes "nothing evicted" a stronger result.
