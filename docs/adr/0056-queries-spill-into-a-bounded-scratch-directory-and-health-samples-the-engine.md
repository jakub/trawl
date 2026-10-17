# Queries spill into a bounded scratch directory, and health samples the engine

status: accepted (2026-10-17), prep record for the homelab eviction of 2026-10-17; amended (2026-10-17): sizing for an 8 GiB process ceiling, see the Amendment

On 2026-10-17 one search from the web UI took the homelab's only trawld pod out of service twice in one minute. The chart gave trawld a 512Mi `/tmp` emptyDir, and the query engine spilled into it. DuckDB sizes its spill cap from `statvfs` on the temp directory, and inside a disk-backed emptyDir `statvfs` reports the node's disk, not the `sizeLimit`. Nothing in trawld set `max_temp_directory_size`, `memory_limit` or `threads` on the query database. The kubelet evicted the pod for exceeding the emptyDir limit 30 seconds after the query timed out.

The readiness probe had failed first. The `duckdb` health check pinged through the executor pool, on the same permits as user queries, with a 250 ms budget. The node had 4 CPUs, so the pool had 4 permits. At the probe, those permits were held by a timed-out query that DuckDB had not yet stopped and by concurrent searches. Live-tail streams take an SSE slot, not an executor permit. The ping expired, health answered 503, and the Service lost its only endpoint. A busy pool made a working engine look dead.

The same spill has no bound on the other channels. Debian spills into the unit's `PrivateTmp`, and the container image and `trawl trial` spill into the container's writable layer. The hot-buffer query snapshots and the Parquet export staging file also write into `TMPDIR`, outside the private spill directory.

## Decision

**Queries spill into the query scratch directory.** `[server] query_scratch_dir` names it. The default is the sibling of the data root, `<[data] path>.query-scratch`, which puts it on the data volume on every channel: the Helm PVC, `/var/lib/trawl` on Debian, and the trawld volume in the trial. The directory holds three things: each query database's private spill directory (the random-named 0700 directory of ADR-0052), the hot-buffer query snapshots, and the Parquet export staging files. trawld writes nothing large to `TMPDIR` after this decision. The boot checks prove the configured location at startup: trawld opens the final component without following a symlink, compares the directory with the storage roots by device and inode, and takes the lock. The directories above the scratch directory are operator-trusted storage, as ADR-0052 rules for the roots, so trawld does not defend against that part of the namespace being renamed or retargeted while it runs.

The scratch directory is not the data root, the WAL directory or a repin staging root, and it is not inside or above any of them. A `spill` directory under the data root was rejected because `spill` is a legal environment name. trawld makes the directory 0700 and opens its final component no-follow, as ADR-0052 rules for the roots. It then takes an exclusive kernel lock on it for the life of the process. A second trawld that finds the lock held refuses to start. Under the lock, boot removes the entries an earlier trawld left behind, such as after a SIGKILL or an eviction. It removes only the names trawld makes, never follows a link and never crosses a mount. Any other entry refuses the start. The Postgres sole-writer lock does not authorize this cleanup: it guards one app-state database, not a directory, and a lost lock can go unseen for seconds. A query-only node whose data root is read-only sets `query_scratch_dir` to a writable directory.

**The query database has explicit budgets.** trawld applies three database-global settings once, before it clones the pool's connections, and reads each one back from a clone at startup:

| key | DuckDB setting | default |
|---|---|---|
| `[server] query_spill_max_bytes` | `max_temp_directory_size` | `512M` |
| `[server] query_memory_limit` | `memory_limit` | `256M` |
| `[server] query_threads` | `threads` | `2` |

*Amended 2026-10-17: the defaults are now 4 GiB, 2 GiB and 4. See the amendment.*

Every query, export and field sample that runs on the query database shares these budgets. DuckDB enforces the spill cap on the bytes it accounts. On disk it can hold a little more: when a temp file shrinks, DuckDB keeps one block of that file's size class past its count, so the directory can exceed the cap by one block per size class, 1,179,648 bytes on DuckDB 1.5.5. Startup refuses a zero, an overflow or a setting DuckDB rejects. It does not compare `query_spill_max_bytes` with the free or total space of the filesystem: a maximum is not a reservation, and free space changes after the check.

`[ingest] compaction_memory_limit` drops from `2GB` to `256MB`. Compaction, conform and repin each open their own database with that limit, and they can run at the same time. The chart's trawld container moves to a 4Gi memory limit with a 1Gi request. The 2Gi limit could not hold the documented ceilings: 1 GiB of request bodies and 512 MiB of argon2 before any DuckDB database. DuckDB limits bound DuckDB-managed memory only, not the process RSS, so the change carries a committed mixed run of ingest, queries, compaction and repin at these defaults. *Amended 2026-10-17: the limit is now `1GiB`, at most two of these databases run at once, and the container limit is 8Gi. See the amendment.*

Snapshots need no budget of their own. Every query that takes a snapshot holds an executor permit, and the cache keeps one file per generation, so at most `max_concurrent_queries + 1` snapshot files exist, each no larger than the serialized hot buffer. Export staging files are bounded in count by the permits, not in size.

**A query over budget gets 503 `query_resource_exhausted`.** Every connection of the query database turns on DuckDB's `errors_as_json`, and trawld classifies a DuckDB error by its top-level `exception_type`. `Out of Memory` covers both the spill cap and the memory limit, because DuckDB reports both with that type. trawld answers 503 with code `query_resource_exhausted`, no `Retry-After`, and the message `query could not complete with the available query resources; narrow the time range or simplify the query`. The class is `service_unavailable` and the cause is `duckdb_resource_exhausted`. It is not 500, because nothing is broken, and it is not 4xx, because the budgets are shared and the same query can finish on an idle server. The response names no path, size or DuckDB text. The existing no-files and binder-column matchers move to the structured type in the same change. A diagnostic that does not parse, or names another type, stays the generic 500 `execution_error`, and every other response keeps its status, code and message. The rule covers bind, execute, row fetch and export. A 504 already sent for a timeout is never replaced.

**Health reads an engine sample instead of taking a permit.** trawld clones one diagnostic connection from the query database. It sits outside the executor pool: it takes no permit and never appears in the query listing. A dedicated thread, not tokio's blocking pool, runs `SELECT 1` on it every 10 seconds with a 250 ms budget. When the budget runs out, trawld interrupts the probe, and it starts no new probe until that one returns. `/api/v1/health` reads the latest sample and never waits for one. The `duckdb` check reads:

- `ok`: the latest sample succeeded within its budget;
- `slow`: the latest sample ran past its budget or failed with `Out of Memory`, and a sample succeeded in the last 60 seconds. The diagnostic connection shares the query database's budgets, so queries that fill them can starve the sample too, and a starved sample is not a success;
- `error`: the latest sample returned any other DuckDB error, no sample has succeeded in the last 60 seconds, or none has succeeded since boot.

`slow` makes the status `degraded` at HTTP 200. `error` makes it `unavailable` at 503, as before. A wedged engine therefore still fails readiness and, after the liveness threshold, gets restarted, and acknowledged ingest survives that in the WAL (ADR-0041). Pool saturation, retained permits, live streams and exhausted budgets never change the status. The human ruled this on 2026-10-17: a long queue never closes the shop, but a broken till does. `/metrics` reports the permits in use, the pool size and the sample's age, so saturation stays visible. The startup, readiness and liveness probes keep calling `/api/v1/health`, and the probe and control allowances of ADR-0054 do not change.

**Retention reads real free space.** Scratch uses ordinary headroom on the data filesystem, as compaction spill does, and pressure deletion keeps comparing actual available bytes with the floor. A large query can therefore start pressure deletion early. The human ruled this on 2026-10-17: counting scratch as free would let real free space sink below the floor and leave the WAL less room.

## Considered options

- **Raise or expose the emptyDir `sizeLimit`.** It postpones the eviction, and DuckDB still cannot see the limit. Rejected.
- **A memory-backed emptyDir.** Spilled pages count against the container's memory, so it trades eviction for an OOM kill. Rejected.
- **Spill under `<data>/spill`.** `spill` is a legal environment name, so repin would walk and swap it as an environment. Rejected for the sibling directory.
- **Count scratch bytes as free for pressure deletion.** Rejected by the human: durable data loses a day early, but the WAL never loses its room.
- **Health never answers 503 after startup.** A wedged engine would stay routed until a person noticed. Rejected by the human.
- **Any failed or stale sample answers 503.** A host starved of CPU fails the 250 ms budget while the engine works, which brings back a restart under load. Rejected by the human in favour of `slow`.
- **A ping on demand with a reserved connection.** Every unauthenticated health request could start DuckDB work, and a wedged ping still holds a blocking thread. Rejected for the sampled thread.
- **A separate small DuckDB database for the probe.** It proves that some database runs, not the one queries use. Rejected.
- **Classify DuckDB errors by message text.** DuckDB does not promise its text across versions. Rejected for `exception_type`.
- **A byte ledger for snapshots.** The permits already bound the files, and a ledger adds a new refusal under normal load. Rejected.

## Consequences

- The timeout tail stays unbounded. DuckDB's interrupt is cooperative, and only a separate worker process could stop it at a deadline. The budgets bound what the tail can consume: spill, memory and the probe.
- Export staging files have no byte cap. A large export can still fill the scratch directory's filesystem. `max_export_rows` limits it indirectly.
- Compaction and conform still spill into the data root without a private directory. This decision does not change that.
- Live-tail streams take an SSE slot and no executor permit, so this decision leaves their capacity alone.
- The 512Mi `/tmp` emptyDir stays, for the read-only root filesystem, and holds no query data.

## Amendment: sizing for an 8 GiB process ceiling, 2026-10-17

The budgets above were sized to fit a 4Gi container. They are now sized for an 8 GiB process ceiling, which the chart and the Debian unit both enforce, and every byte value is written in GiB.

| key | default |
|---|---|
| `[server] query_spill_max_bytes` | 4 GiB |
| `[server] query_memory_limit` | 2 GiB |
| `[server] query_threads` | 4 |
| `[server] max_concurrent_queries` | the CPU count, at most 8; 4 when the count cannot be read |
| `[ingest] compaction_memory_limit` | `"1GiB"` |
| `[retention] min_free_disk_bytes` | 6 GiB |

trawl's byte sizes are 1024-based. `compaction_memory_limit` is a raw DuckDB string, and DuckDB reads `GB` as 10^9, so its default is spelled `1GiB`. The permit default stops at 8 because the chart sets no CPU limit: on a 64-core node every one of 64 permits would share the same 2 GiB. The shared CPU-count helper is not capped; only the permit default is.

**Memory arithmetic.** At the defaults the components that can peak together are:

| component | ceiling |
|---|---|
| query database, `query_memory_limit` | 2 GiB |
| maintenance databases at `compaction_memory_limit`, at most two at once | 2 × 1 GiB |
| in-flight ingest bodies, 32 requests × (sent + decoded) × 16 MiB | 1 GiB |
| argon2, 4 checks × 128 MiB | 512 MiB |
| hot buffer, self-telemetry buffer and RSS DuckDB does not account | the rest |

At most two maintenance databases are open at once: the ingest pipeline's compaction and rollup pass, and a repin. The conformance pass runs at boot, before either starts. The sum is about 6.1 to 6.6 GiB of the 8 GiB ceiling. That is a sizing, not a measured bound. The chart's trawld container gets an 8Gi limit and keeps its 1Gi request. The trawl-web sidecar gets a 512Mi limit. On Debian, `trawld.service` sets `MemoryMax=8G` and `trawl-web.service` sets `MemoryMax=512M`. `MemoryMax` is a ceiling, not a reservation.

**Disk arithmetic.** The 6 GiB pressure floor covers the 4 GiB spill budget, `max_concurrent_queries + 1` hot-buffer snapshots (9 × 100 MiB at 8 permits), and export staging. This is documented, not validated. The decision above already rules that startup does not compare the spill budget with free space, and the same holds for the floor: a floor below the spill budget, including `0`, still starts. The two keys are independent, and the docs tell the operator to raise them together.

**Maintenance out of memory.** Compaction, rollup, conform and repin turn on `errors_as_json` and classify a statement failure by the same `exception_type` the query database uses. An `Out of Memory` there is a resource failure, not bad input. trawld does not quarantine or isolate the WAL file, does not mark a day unreadable, and does not skip a file as foreign. It keeps the inputs, logs `maintenance_resource_exhausted`, counts the statement in `trawl_maintenance_resource_exhausted_total`, and the next pass retries. Before this change, an `Out of Memory` while compaction read a sound WAL file went to isolation, the probe ran out too, and the file was quarantined as data loss.

**Termination grace.** The chart derives `terminationGracePeriodSeconds` as `shutdown_drain_secs + 30`. After the drain, trawld gives the runtime 10 seconds to shut down and the telemetry flush 5 seconds; 30 seconds covers both with margin. It is not a proven exit bound. DuckDB's interrupt is cooperative, so a statement in flight can outlast it, and the kubelet then sends SIGKILL. Acknowledged ingest survives that in the WAL (ADR-0041). The startup probe allows five minutes (`periodSeconds: 5`, `failureThreshold: 60`, `timeoutSeconds: 5`) for boot recovery, hydration and the conformance pass.

**Descriptor limit.** Each query, maintenance pass and ingest connection holds descriptors, and DuckDB opens one per Parquet file it scans. At startup trawld raises its soft `RLIMIT_NOFILE` to the hard limit and logs both values once. It does not follow an unlimited hard limit, because Linux refuses a descriptor limit above `fs.nr_open`. If the raise fails, trawld warns and runs on the inherited limits. `trawld.service` sets `LimitNOFILE=65536`, which sets both limits.
