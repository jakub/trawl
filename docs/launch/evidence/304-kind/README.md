# Issue #304 kind evidence: AC1 before and after, and the AC19 mixed run

These runs took place on `fractal`, on the local kind cluster `trawl-i304`, on 2026-10-17 and 2026-10-18 (UTC). They cover two acceptance criteria of issue #304:

- **AC1.** At `b02b8acf`, a workload that holds every executor permit gets readiness 503 with `query_not_started kind=ping`, and a query that spills past 512Mi gets the pod evicted for the `tmp` emptyDir. On the PR head, the same workload keeps the pod Ready, answers 503 `query_resource_exhausted` to the spilling query, and evicts nothing.
- **AC19.** A mixed run of ingest, queries, compaction and repin at the new defaults, in the chart's 8Gi limit, finishes with no OOM kill, no eviction and no `query_resource_exhausted` for ordinary searches. The run records peak RSS and peak scratch bytes.

The before run shows both failures. The after run keeps the pod Ready and evicts nothing. The incident's spilling query (phase B) needs less spill than the head's 4 GiB budget, and it answered 200 in seven attempts of eight. A wider query built to need more spill than the budget (phase C) answered 503 `query_resource_exhausted` in all four attempts, and a small query then answered 200. The public answer does not say which budget stopped it. A supplementary calibration, committed beside the after run, shows the spill budget doing so. The mixed run passed.

## Files

| File | Contents |
| --- | --- |
| `build-image.sh REPO SHA LABEL` | Builds `trawl-i304:LABEL-<sha12>` from one commit through the release image path, then loads it into the kind cluster. |
| `repro.sh IMAGE LABEL CHART_DIR` | The AC1 scenario, end to end, in the fresh namespace `i304-LABEL`: deploy and ingest, phase A (starvation), phase B (the incident's spill query) and phase C (a query over the spill budget, skipped when phase B has already evicted the pod). It writes `LABEL-transcript.txt`. |
| `values.yaml` | AC1 scenario values, the same for both runs: 2 permits, rate limits off, 4Gi and 1 CPU, no web sidecar. |
| `values-load.yaml` | Layered over `values.yaml` only while the AC1 corpus loads (16 CPU, 6Gi), then removed by a `helm upgrade`. |
| `gen-events.py` | The seeded AC1 corpus and its NDJSON ingest client. |
| `mixed.sh setup IMAGE CHART_DIR`, `mixed.sh run SECS`, `mixed.sh report` | The AC19 mixed run in the namespace `i304-mixed`. It writes `mixed-transcript.txt`. `report` prints the verdict checks, and exits 1 on FAIL. |
| `mixed.sh window CHUNK_DIR...` | The report's per-chunk window check alone, over saved chunk directories. It needs no cluster and exits 1 when any chunk has a problem. |
| `mixed-ingest.py` | Steady ingest for the mixed run, at a fixed rate, with `_time` set to the moment each batch is built. It exits 1 when it sent nothing, dropped a batch, or had fewer events accepted than sent. |
| `values-mixed.yaml` | The mixed run's only overrides: image pull policy, the two DSN Secrets, and no web sidecar. |
| `postgres.yaml` | A disposable in-cluster PostgreSQL for each namespace. |
| `calib-fast.sh LABEL PAUSE NAME...` | Supplementary calibration for phase C. It runs the named `repro.sh` queries once each in a namespace that `STOP_AFTER=ingest repro.sh` set up, while a loop in the trawld container samples query scratch bytes and cgroup memory every 0.2 s. It writes `LABEL-fast-transcript.txt`. |
| `before-transcript.txt` | The AC1 before run, image `trawl-i304:before-b02b8acfb8a2`, 2026-10-17. |
| `after-transcript.txt` | The AC1 after run, phases A to C, image `trawl-i304:after-7dc64b27f566`, 2026-10-18. It replaces an earlier after run with phases A and B only (see AC1 after). |
| `spill-calib-transcript.txt` | The calibration setup, `STOP_AFTER=ingest` in the throwaway namespace `i304-spill-calib`, image `trawl-i304:after-7dc64b27f566`, 2026-10-18. |
| `spill-calib-fast-transcript.txt` | The calibration runs in that namespace: SPILL_BIG_QUERY three times, then SPILL_QUERY once. |
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

   The supplementary calibration for phase C runs in a throwaway namespace, which you delete afterwards:

   ```bash
   STOP_AFTER=ingest $D/repro.sh trawl-i304:after-7dc64b27f566 spill-calib $D/build/src-after/chart/trawl
   $D/calib-fast.sh spill-calib 5 SPILL_BIG_QUERY SPILL_BIG_QUERY SPILL_BIG_QUERY SPILL_QUERY
   kubectl --kubeconfig $D/kubeconfig --context kind-trawl-i304 delete namespace i304-spill-calib
   ```

   Each `calib-fast.sh` run names its in-pod stop file with a run id, removes it after the sampler ends, and stops with a message when a query's sample log holds no sample. The first version reused `/tmp/calib-fast-stop-N` and left it behind, so a second invocation on the same prepared pod sampled nothing and then aborted at the peak pipeline. The committed calibration transcript came from the first version, in a single invocation on a fresh pod, so the fault did not touch it. Both versions were run against a stub `kubectl` that executes the in-pod commands locally (not committed): the first version failed its second invocation, and this version passed two in a row, left no marker, and stopped with the message when the sampler produced no sample.

   The after run takes about 9 minutes, and each agent command had a 10-minute limit, so it ran as two commands. `STOP_AFTER=b` stops after phase B. `START_AT=c` runs phase C against the same namespace and appends to the same transcript. Without either variable, one command runs every phase. At `b02b8acf`, phase B evicts the pod, so the script skips phase C and says so in the transcript. The committed before transcript was recorded before phase C existed.

4. Run AC19. Each part is one command, and each command finished within 10 minutes. Run `run` until the chunks add up to at least 900 s. `report` exits 0 only on PASS.

   ```bash
   $D/mixed.sh setup trawl-i304:after-7dc64b27f566 $D/build/src-after/chart/trawl
   $D/mixed.sh run 500
   $D/mixed.sh run 500
   $D/mixed.sh report
   ```

   `run` writes each chunk's `SECS` to `private/mixed/work/cN/secs`. `report` needs it to check the chunk's window. `$D/mixed.sh window $D/private/mixed/work/c*` repeats that check on saved chunks without a cluster.

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
| Phase C: the over-budget query answers 503 `query_resource_exhausted` | Yes, all 4 attempts, in 10.5–13.5 s, each with the fixed message. This shows that a query resource budget stopped each attempt, with nothing evicted and the pod Ready. It does not show which budget (see below). trawld logged each one as `http_failure status=503 cause_kind="duckdb_resource_exhausted"`. The stored `http_failure` events (`service=trawld`) carry the same status and cause for the same four request IDs. | 1033–1036, 1280, 1285, 1290, 1295, 1305–1308 |
| Phase C: scratch stays at or under the budget plus slack | Yes, in every 2 s sample. The largest was 3,470,535,948 bytes. The budget plus DuckDB's block slack is 4,294,967,296 + 1,179,648 = 4,296,146,944 bytes. | 1084–1122, 1123 |
| Phase C: a small query then succeeds | Yes. `service=i304 \| stats count()` answered 200 in 0.11 s with 800,000. | 1037 |
| Evicts nothing | Yes. `/tmp` was 0 bytes in every sample of phases B and C, because spill goes to `/var/lib/trawl/data.query-scratch` on the PVC. The transcript has no `Evicted` event. The pod kept uid `df57fbca…` with restart count 0 and an empty `lastState`. The script printed `evicted=no` after phase B and again after phase C. | 517–636, 1042–1080, 1349, 1021, 1350 |

Phase B's health section prints only its first 60 samples (lines 891–950), all 200. The full sampler file held 255 samples, all 200 with `"duckdb":"ok"`. It is not committed. The pod watcher is the committed readiness evidence for phase B. Phase C prints its whole health log.

**Which budget stops each query.** ADR-0056 gives memory exhaustion and spill exhaustion the same public answer, and the head logs only the safe message, even at debug (lines 960, 1279). So the after transcript establishes the AC1 clause, 503 `query_resource_exhausted` with nothing evicted and the pod Ready, but it does not name the budget. A query that ran out of the 2 GiB memory limit first would leave the same transcript. In phase C the 2 s sampler's largest sample was 3,470,535,948 bytes (lines 1094, 1123), under the 4 GiB spill budget. Near the end, scratch grows by 100–200 MB every 0.2 s, so a 2 s sampler misses the last seconds before a failure.

- Phase C: a supplementary calibration supports the attribution to the spill budget. `STOP_AFTER=ingest repro.sh` set up the throwaway namespace `i304-spill-calib` with the after image, the same `values.yaml` (4Gi, 1 CPU, `query_spill_max_bytes = "4GiB"`, `query_memory_limit = "2GiB"`) and the same 800,000-event corpus, and stopped before phase A (`spill-calib-transcript.txt` lines 19–20, 203–208, 211–212). `calib-fast.sh` then ran SPILL_BIG_QUERY three times and SPILL_QUERY once, sampling every 0.2 s inside the trawld container (`spill-calib-fast-transcript.txt`):
  - All three SPILL_BIG_QUERY runs answered 503 `query_resource_exhausted`, in 8.7–9.7 s (lines 22, 82, 138).
  - In each run, scratch climbed steadily to 4,277,062,003, 4,278,756,507 and 4,237,671,326 bytes in the last sample before DuckDB freed it. That is 16–57 MB short of the 4,294,967,296-byte budget, less than the 100–200 MB that scratch grew between samples (lines 66–71, 125–128, 181–184, and the peak lines 78, 134, 191).
  - The cgroup's anon memory peaked at 2.46–2.57 GB in those runs (lines 79, 135, 192). SPILL_QUERY answered 200 in 16.1 s with a scratch peak of 2,398,535,696 bytes and an anon peak of 2,833,387,520 bytes, more anon memory than any failing run used (lines 195, 281–282).
  - The pod kept its uid, restart count 0 and `oom_kill 0` (lines 286, 291–292).

  So each failing run stopped with scratch at the spill budget, and with less anon memory than a run that succeeded. This points to the 4 GiB spill budget, not the 2 GiB memory limit. It is still an inference, for two reasons. Anon memory counts more than DuckDB's buffer pool. The calibration ran on a fresh pod, not after phases A and B.
- Phase B attempt 2 failed with 530 MB of scratch, far under the spill budget, so it most likely ran out of the 2 GiB memory limit. The run does not prove it.

After the run, the trawld cgroup read `memory.peak` 4294967296 (the 4Gi limit), `memory.events` `max 0`, `oom 0` and `oom_kill 0`, with restart count 0 and an empty `lastState`. Page cache from the scratch writes filled the cgroup up to its limit, and the kernel reclaimed it. These values were read with `kubectl exec` after the script ended, so they are not in the transcript.

## AC19 mixed run (`mixed-transcript.txt`)

`mixed.sh` installed the head chart at its defaults. It overrode only the image, the pull policy, the two DSN Secrets and the web sidecar (`values-mixed.yaml`). The rendered config shows `query_spill_max_bytes = "4GiB"`, `query_memory_limit = "2GiB"`, `query_threads = 4`, `compaction_memory_limit = "1GiB"`, `min_free_disk_bytes = "6GiB"`, and the default rate limits of 100 and 1000 a minute (lines 87–100). The StatefulSet has an 8Gi limit, a 1Gi request, no CPU limit and a 60 s grace period (line 104). trawld sees 16 CPUs, so `max_concurrent_queries` defaults to 8 (line 252).

Setup preloaded 800,000 events into service `i304` and 5,000 events into `i304-repin`, which carries the integer field `code`, with no batch dropped (lines 255–268). Each of the two chunks then ran these for 500 s, all at once:

- steady ingest of 500 events a second, one batch every 2 s;
- 6 search clients, each with its own key, at most one search a second each, over 8 typical searches (lines 17–24): `stats count() by` small keys, `top 10 host`, `timechart`, time-ranged filters, a full-corpus count, and a full-text scan;
- one live tail, `service=i304 | stats count() by status`;
- a dry run of pinning `status` as VARCHAR at +60 s, and a real repin of `code` at +200 s.

The gaps before the chunks were 3 s and 5 s (lines 292, 681). The samplers spanned 1011 s (line 1122).

**Verdict.** `mixed.sh report` prints PASS, and exits 0, only when every check below holds. Otherwise it prints FAIL with the names of the failed checks, and exits 1. Each worker also exits nonzero when its own work failed, and each chunk records every worker's exit status. The checks follow AC19: the run must exercise ingest, queries, compaction and repin, each of them must succeed, and none of them may end in an OOM kill, an eviction or `query_resource_exhausted`.

| Check | Threshold |
| --- | --- |
| `workload_seconds`, `chunk_windows` | A chunk's window is the `SECS` it was asked to run. It counts only when the workers overlapped for all of it, as `chunk_window` checks from the chunk's logs. The ingest worker's `secs` is at least `SECS` and at most `SECS` + 5 s. It sent the full steady rate, 500 events a second times `SECS`. It logged no `503 retry hot_buffer_full` more than 1 s after its deadline, which is its `done` line's time minus its `secs` plus `SECS`. Each of the 6 search clients logged requests, its first one started within 15 s of the chunk's start, and its last one started within 15 s of the deadline. The windows of passing chunks add up to at least 900 s, and no chunk may have a problem. The samplers' span is printed but does not count, because they run until the last worker exits. |
| `ingest_hot_buffer_full_retries` | The report counts every `503 retry hot_buffer_full` and fails the run on one after a deadline. |
| `oom_kill`, `restarts`, `same_pod`, `evict_or_oom_events` | cgroup `oom_kill` 0, trawld restart count 0, the pod uid from setup, and no `Evicted`, `OOMKilling` or `OOMKilled` event. |
| `search_responses`, `searches_not_200_with_rows`, `searches_answered`, `query_resource_exhausted` | At least one search. Every response is 200 with at least one row. Each of the 8 searches answered. No `query_resource_exhausted`. |
| `ingest_ran_to_end`, `ingest_dropped_batches`, `ingest_accepted` | In every chunk, the ingest worker ran to its end. No batch was dropped. The events accepted equal the events sent, and are more than 0. A 503 `hot_buffer_full` carries `Retry-After` and is retried, so a retry inside the deadline does not fail the run by itself. |
| `live_tail` | In every chunk: HTTP 200, at least one snapshot, and the stream open until curl's time limit (curl exit 28). |
| `compaction`, `trawld_error_lines` | At least one `compaction_complete` in every chunk. No trawld `ERROR` line, and no `resource_exhausted`, `compaction_error`, `catalog_invariant_violation`, `rollup_unreadable` or `catalog_conform_skip` line. |
| `repin` | Every repin the driver sent ended `succeeded`, with at least one dry run and one real repin among them. |
| `workers` | Every worker of every chunk exited 0. |

**Retries.** AC19 asks for ingest at the new defaults with no failure. The report fails the run on a `hot_buffer_full` retry after the deadline, and only on that. The 503 with `Retry-After` is the server's documented backpressure, and `mixed-ingest.py` retries it without losing the batch, so a retry inside the window is not a failed ingest: `ingest_dropped_batches` and `ingest_accepted` still require every event to land. A retry inside the window that cost the worker its rate fails the full-rate test in `chunk_windows`. A retry after the deadline means a batch was still stalled when the workload should have ended, so the window was not concurrent work. If AC19 is read as allowing no 503 at all, change the `hb_late` check in `report` to `hb_retries`. The committed run had none, so it passes either way.

**Which version produced the transcript.** `mixed-transcript.txt` was produced by the previous version of `mixed.sh`, before the window check above. That version is the commit `eb4a1789`, `docs(evidence): make the #304 mixed verdict require a successful workload and rerun it`, the last commit that changed `mixed.sh` before this fix. It took `workload_seconds` from the first and last cgroup samples of each chunk (1011 s, line 1122), and it printed 17 checks. The new version prints 19.

That derivation could have counted a stall. The samplers run until every worker exits, and `mixed-ingest.py` retries a `hot_buffer_full` 503 without limit, so a chunk that stalled for 500 s past its deadline would have counted about 1000 s and could have passed. This run had no such stall:

- Each ingest worker finished in 500 s with every event accepted: `done sent=250000 accepted=250000 dropped=0 secs=500` (`mixed-transcript.txt:331` and `:720`).
- The transcript has 0 `retry hot_buffer_full` lines. The `grep -vE ' 200 [0-9]+ '` over each chunk's ingest log printed only the `done` line (lines 330–331 and 719–720).
- Run on the saved chunk logs, with `secs` set to 500 for each, `mixed.sh window` reports `window_secs=500 retries=0 late_retries=0 sent=250000 problems=none` for both chunks. Driven through `report` with a stub `kubectl` (not committed), the new version prints `workload verified for 1000s over 2 chunks`, `chunk_windows: 0 of 2`, `ingest_hot_buffer_full_retries: 0 retried, 0 after a deadline`, and PASS. The new predicate passes this run, and the run needs no rerun.

Before this run, the checks were tried against the failure the review described. In a short throwaway run, with the search, tail and ingest keys revoked between two chunks, the report printed `verdict: FAIL (workload_seconds searches_not_200_with_rows ingest_dropped_batches ingest_accepted live_tail workers)` and exited 1. That run is not committed.

| Measure | Result | Lines |
| --- | --- | --- |
| Verdict | PASS. All 17 checks of that version hold (see "Which version produced the transcript"). | 1179–1197 |
| Searches | 5577 responses, all 200 with at least one row, each of the 8 searches about 350 times per chunk. 0 `query_resource_exhausted`. Chunk 1 p95 was 1.42 s and max 3.41 s. Chunk 2 p95 was 1.50 s and max 3.26 s. | 308–324, 697–713, 1125–1127 |
| Ingest | 500 batches, all 200: 500,000 events sent and accepted, no batch dropped, no `hot_buffer_full` retries (0 `retry hot_buffer_full` lines in the transcript). Every ingest worker exited 0. | 326–331, 715–720, 1128–1135, 1157–1174 |
| Live tail | HTTP 200 and 250 snapshots in each chunk. The stream stayed open until curl's 500 s limit (exit 28). | 333–339, 722–728 |
| Compaction | 143 `compaction_complete` events, 64 in chunk 1 and 79 in chunk 2. 141 merged into an existing hourly file. 2 started the new hour's files at 10:00:01 and 10:00:13 UTC, one for trawld's own telemetry and one for `i304`. No `ERROR`, `compaction_error`, maintenance resource failure or `catalog_invariant_violation` line. | 637–652, 1031–1048, 1140–1143 |
| Repins | Dry run of `status` BIGINT→VARCHAR: succeeded in both chunks. Real repin of `code` BIGINT→VARCHAR, then VARCHAR→BIGINT: both succeeded. | 341–349, 730–738, 1144–1155 |
| Health and readiness | Health answered 200 498 times. The pod watcher shows `ready=True restarts=0` 195 times, on one uid. | 1136–1139, 576, 970 |
| OOM kills and evictions | None. cgroup `oom_kill 0`, `max 0`. Restart count 0, same uid. No `Evicted`, `OOMKilling` or `OOMKilled` event. | 1107, 1113–1115, 1123–1124 |
| Peak RSS | trawld `VmHWM` 2,870,012 KiB (2.74 GiB) since process start, preload included. Peak sampled anon 2,786,488,320 bytes. | 1118, 1175–1176 |
| Peak cgroup memory | `memory.peak` 4,413,730,816 bytes, page cache included, under the 8Gi `memory.max` of 8,589,934,592. Peak sampled `memory.current` 4,287,647,744 bytes. | 1109–1110, 1175–1176 |
| Peak query scratch | 32,662,780 bytes (`du -sb` every 2 s). | 1177 |

trawld logged `pool_pressure` (WARN) 5 times in chunk 1 and once in chunk 2, each within a second of a real repin starting (lines 650, 1046; repins at lines 346–347 and 735–736). Every search still answered 200.

This transcript replaces an earlier mixed run on the same image and values. That run's report checked only OOM kills, restarts, evictions, the pod uid and `query_resource_exhausted`, so it could print PASS for a run whose workload failed. Its first dry run asked for `seq`, a field the mixed corpus does not carry, and answered 400. The earlier transcript is not committed.

## Deviations

- **The before transcript is relativized.** Every occurrence of the kit's absolute path was replaced with `$D` by one command, `sed "s#$KIT#\$D#g"`, where `KIT` was the kit's absolute path on the build host. Nothing else changed: the line count is 1042 and every line number above matches the original. The original file's sha256 is `59f66e9d0df0b2ebdeb2351aa1f238b4ed524222cd41fd8a149ce4b08afc1295`. The after and mixed transcripts were relativized with the same command, using their own kit path.
- **The after run uses the identical 4Gi values.** `values.yaml` sets the same values as in the before run. Its comments changed after the before run: the chart default is now 8Gi, not 4Gi, and the head logs only the safe query error text.
- **`repro.sh` changed after the before run.** It gained a phase B sampler for the query scratch directory, which runs only where that directory exists, so it did not run at `b02b8acf`. It gained phase C, with its constants (`SPILL_BIG_QUERY`, `SPILL_BIG_ATTEMPTS=4`, `SPILL_BIG_PAUSE=5`, `SMALL_QUERY`) printed in the transcript header, and the `STOP_AFTER=b` and `START_AT=c` pair. Its header comments also changed: they describe step 0, the load and upgrade split, `STOP_AFTER`, phase C and the after outcome. Phases A and B, their values and their queries did not change. The after run used this `repro.sh`, except for one later change to cleanup, described in the next item.
- **`pf_stop` changed after the after run.** It used to finish with a host-wide `pkill -f "port-forward pod/trawl-0 $PORT:5514"`, which also matched port-forwards that other kubeconfigs had started. Now `pf_start` runs its restart loop under `setsid`, so the loop leads a process group of its own, and `pf_stop` signals that group only. Cleanup writes nothing to the transcript, so the before and after transcripts are unchanged. The supplementary calibration (see AC1 after) ran with the new `pf_stop` beside a decoy process whose command line matched the old pattern. The decoy survived both commands, and no port-forward was left behind.
- **The incident's spilling query mostly fits the head's budgets, so phase C adds a wider one.** At the head defaults of 2 GiB memory and 4 GiB spill, SPILL_QUERY needs about 2.8–2.9 GB of scratch, and it completed in 7 of 8 attempts in both after runs. The budgets stay at the chart defaults; none was lowered. Instead, phase C's SPILL_BIG_QUERY widens the same `dedup` key from 32 to 64 copies of the event text, about 16 KB a row and a 12.8 GB sort, so it needs more spill than the budget. Like SPILL_QUERY, it is synthetic. The budget-exhaustion contract with a small configured budget is also covered by `cargo nextest run -p trawl-server --test query_resources spill_budget`.
- **The after run ran as two commands.** It takes about 9 minutes, and each agent command had a 10-minute limit. `STOP_AFTER=b` ended the first command after phase B at 09:08:07, and `START_AT=c` started phase C on the same pod at 09:08:11 (lines 1022–1024). Phase C reads phase B's result from the transcript (line 1027).
- **The after run's load hit one 503 `hot_buffer_full`.** The stored `http_failure` list shows one ingest 503 at 09:01:34 (line 1303), while the corpus loaded in the roomy load pod. `gen-events.py` retries a 503 after its `Retry-After`, and all 800,000 events landed (lines 143, 146).
- **The mixed run ran in chunks.** Each agent command had a 10-minute limit, so the 1011 s workload ran as two chunks on the same pod, with a 5 s gap. The pod, its data, its cgroup counters and its compaction state carried across the gap.
- **The mixed run reached trawld through a NodePort.** It did not use a port-forward, which drops every open connection when it restarts. The Service publishes NotReady addresses, so health samples would still reach a NotReady pod. TLS verifies trawld's pinned certificate as `localhost`.
- **The mixed workload crossed one hour boundary.** It ran from 09:50 to 10:07 UTC. Compaction merged into the 09:00 files until 10:00. It then started one 10:00 file for `i304` and one for trawld's own telemetry (`merged=false`), and merged into those after that.
- **The kubelet refreshes volume stats every 2 s, not 1m.** `repro.sh` step 0 sets `volumeStatsAggPeriod: 2s` on the kind node. It is a cluster setting, and it makes "nothing evicted" a stronger result.
