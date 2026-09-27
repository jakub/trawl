# An acknowledged event is counted exactly once across compaction and restart

status: accepted (2026-09-23), prep record for the restart-visibility work

A search can read an acknowledged event from two places: the hot buffer and the parquet files. Two defects break that on disk today.

- **The consumed WAL outlives the publish.** Compaction renames its output into place and drains the hot buffer, then deletes the consumed WAL files. A crash between those steps leaves WAL batches whose rows are already in parquet, and so does a delete that fails, which today only logs a warning. The next compaction merges them again, and the duplicate is permanent.
- **A restart hides WAL-only events.** trawld starts with an empty hot buffer and never reloads the WAL. Events that were acknowledged but not yet compacted drop out of search until the first compaction after boot. Queries meanwhile answer 200 with those rows missing.

ADR-0026 excluded "WAL replay after a crash" from its guarantee. This record closes that exclusion.

## Decision

**The invariant.** An event whose ingest returned success is counted exactly once by every reader of the hot buffer and parquet. If that cannot hold, the read is refused with a 503 that names the reason. The guarantee holds across graceful stops, kills and power loss, from the moment the server reports ready.

It does not cover:

- WAL files quarantined as corrupt. That loss is counted.
- Retention expiry.
- A client resending an event.
- The SSE live tail, which stays live-only and never replays.

**Exactly-once publication (slice 1).** Every compaction publish is bracketed by a publication marker. The marker names the temporary output, the canonical file, the consumed WAL files, and the output's identity (size and digest). A publish runs in this order:

1. fsync the temporary output.
2. Write the marker durably.
3. Under the publication write guard: rename the output into place, fsync its directory, and drain the hot batches.
4. Retire the consumed WAL files. Delete them, or rename them aside if the delete fails. Then fsync the WAL directory.
5. Remove the marker.

Recovery runs at boot, before anything reads or writes the corpus, and again at the start of each compaction tick.

- **What the decision rests on.** Recovery decides whether the publish happened by checking that the canonical file carries the recorded identity. A missing temporary file never decides it, because recovery can itself crash after deleting that file.
- **Both branches can be interrupted.** Both lead to the same end state, so recovery can safely be interrupted and rerun.
- **What the marker blocks.** While a marker exists, no compaction, hydration, retention or rollup touches the files it names.
- **Directory fsync is part of the ack.** A WAL write is acknowledged only after its directory entry is durable. A failed directory fsync, including the first write into a new environment directory, fails the ingest request.

**Restart visibility (slice 2).** _Decided 2026-09-27 in the slice's prep._ Boot runs these steps in order, before any producer, compaction, the scheduler or the listener starts:

1. Recover publication markers (slice 1).
2. Recover rollup markers. Recovery renames or retires files and reads parquet footers. It never merges again. A failure does not stop the boot: reads stay refused as `rollup_pending`, and each compaction pass retries. Only a later complete scan clears a failed marker scan.
3. Run boot conformance. It defers a day directory with an unresolved rollup marker, as it already defers files that a publication marker claims.
4. Load the surviving WAL into the hot buffer under its existing batch identities. This reload is **hydration**.
5. Activate self-telemetry, start compaction and the other workers, then bind the listener.

"Ready" means the server answers HTTP. Every response therefore falls inside the guarantee.

Hydration:

- **Scope.** Hydration takes the WAL files that compaction would take. That includes environments outside the ingest allowlist, which governs new ingest, not stored data. It skips every environment and service that a publication marker blocks, and any scope it cannot list. It takes regular files only.
- **What it accepts.** A file is hydrated only if its bytes are exactly what the live writer produces: a writer file name, and lines that each parse to an object of scalar values with lowercase keys and serialize back to the same bytes. Float parsing must round-trip exactly. Hydration never canonicalizes an event again. Compaction's decoder handles every other file, as it does today.
- **Order and bound.** Before the listener binds, hydration examines at most `hot_buffer_max_events + 1` WAL files, oldest first by the time in the file name, and reads at most `hot_buffer_max_bytes` of them. It loads whole files. A file that does not fit is skipped, and later files are still tried. Listing the WAL costs what one compaction scan costs. Publication and rollup recovery are bounded by the interrupted operations, not by the caps.
- **Ledger.** Hydration charges the full caps, not the external share. It is not a producer: it never latches `Refusing`, counts no admission refusal and sends no pressure wake. Occupancy alone can move admission to `Pressure`.
- **Side effects.** Hydration never publishes to the event bus, never counts as ingested traffic and needs no Postgres. A hydrated batch takes its age from the time in its file name.
- **Unacknowledged files come back.** A `LeftVisible` file, or a withdrawn file whose directory fsync was lost, reappears at boot and is visible at once. A sender's retry can then duplicate its events. ADR-0043 accepts the same trade for compaction.

**Overhang.** WAL that existed at boot and did not become resident is the **overhang**. It did not fit, could not be read or recognized, sat in a blocked scope, or could not be listed.

- Overhang is a state of the publication gate, not a list of files. Nothing creates it after boot, because every later write is inserted before its producer releases the gate.
- Compaction clears it with a **coverage proof** after each pass. The proof lists the WAL without a guard, then re-checks each file that is not resident under the publication write guard. It clears overhang only if the listing was complete, no publication marker is pending, and every listed file is resident or gone. The list of files to re-check has a fixed bound, and exceeding it fails the proof.
- The first compaction pass runs at boot and starts no daily rollup. While overhang holds, passes take WAL of any age, as they do under admission pressure. The ADR-0043 cooldown still comes first. A file that marker recovery retires counts as progress, and a pass never waits for an insert while overhang holds.

**Reads while the corpus is unsettled.** Overhang and a pending rollup both mean a read cannot count every acknowledged event exactly once.

- Every read that takes the publication read guard answers **503 `corpus_recovering`**. That covers queries, exports, field values that are not cached, and repin admission. The cause kind is `restart_backlog` or `rollup_pending`. The rollup refusal moves from the generic `service_unavailable` to this code.
- The response carries no `Retry-After`. The next pass may run at once, and a standing fault never clears without an operator.
- These are unaffected: the SSE live tail, `| from saved`, health, metrics, cached field values, `/schema` and `/schema/services`. The schema routes count from parquet footers. Those counts are cold-only and approximate, and the invariant does not cover them.
- A hot snapshot that cannot be built refuses the read with 503 `service_unavailable` and a typed I/O cause kind. It never falls back to a cold-only answer, in steady state as well as after a restart.
- Health reports `checks.corpus` as `ok`, `rollup_pending` or `restart_backlog`. An unsettled corpus makes the status `degraded` at HTTP 200, so probes do not restart the server. An alert fires when the corpus stays unsettled for 10 minutes.

**Nets.** The scheduler skips its poll while the corpus is unsettled, before it claims a window, so no fire cursor moves. A manual run answers 503 `corpus_recovering` before it creates a run. A rollup marker that appears between that check and the query still fails the run, as it can today. Holding the read guard across the Postgres claim would close that gap, but a slow database would then stall all ingest behind the gate.

**Standing faults refuse search.** A contradictory publication marker, a WAL file that always fails, or a catalog outage with a backlog above the caps keeps search refused until an operator fixes the fault. That is the invariant applied as written. The `TrawlPublicationRecoveryBlocked` runbook states the consequence.

**Scope.** The guarantee covers servers with ingest enabled. A query-only node reads parquet only and never sees the WAL. It logs one warning at boot when it finds WAL files.

**WAL names are never reused while a file holds them.** The WAL writer never replaces an existing file. A name that collides, or that a pending publication marker claims, gets a fresh name. A failure after the final name exists either completes the acknowledgement, withdraws the file durably, or reports `LeftVisible`. A duplicate batch identity in the hot buffer keeps the resident batch and logs an error.

**Steady-state eviction is not solved here.** When the hot buffer fills during normal running, it evicts acknowledged, uncompacted rows. They are counted but not refused. That limit is documented until ingest backpressure replaces eviction, which is a cross-cutting change with its own issue.

_Amended 2026-09-24:_ [ADR-0043](0043-ingest-is-admitted-against-hot-buffer-capacity.md) replaces eviction with admission against the hot-buffer caps. Hydration charges that ledger.

**No epoch bump.** Parquet and WAL formats are unchanged, and the marker is an additional file that older binaries ignore. A WAL file without a marker is treated as unpublished, as it is today. Duplicates left by earlier crashes cannot be told apart and stay as they are.

## Considered options

**Compact the whole backlog before reporting ready**, rejected for slice 2:

- readiness would depend on Postgres and on the size of the backlog
- boots longer than the startup probe would need new probe endpoints
- a catalog outage would turn every restart into a read outage

Hydration gives the same guarantee for the usual backlog, and the overhang refusal covers the rest.

**Infer publication from the temporary file's absence**, rejected: a crash during recovery would make recovery delete WAL data that never reached parquet.

**A full transaction record with catalog bookkeeping**, rejected. A publish produces exactly one output file through one rename. Catalog bookkeeping is best-effort by design, and the output's identity is enough to decide the outcome.

**Refuse reads on steady-state eviction**, rejected in favour of backpressure. Refusing reads would turn a full buffer into an outage for every reader, when the pressure should fall on the sender, who can retry.

**Bump the epoch**, rejected: nothing about the stored format changes, and a bump would set aside every installation's archive.

**Hold the telemetry reserve back from hydration**, rejected. A backlog in the last sixteenth of the caps would become overhang, and every read would be refused. That includes the stall telemetry the reserve exists to keep searchable.

**Stop at the first file that does not fit, in directory order**, rejected. One large file early in directory order strands the smaller files behind it, and the hydrated subset changes from boot to boot.

**Parse the WAL leniently or through DuckDB at boot**, rejected. A second decoder that disagrees with the live writer shows different values hot than cold. Files the strict check rejects stay overhang until compaction's decoder handles them.

**Put a reason field in the error envelope and send `Retry-After`**, rejected. The field changes the error type for every client, and the health check and the message already name the reason. No interval is an honest retry hint for a fault that needs an operator.

**Hold the read guard from before the claim through the query**, rejected: Postgres waits would happen under the gate.

**Refuse reads on a query-only node that finds WAL**, rejected. On a shared data root the ingest node always has WAL, so the node would refuse every read.

## Consequences

When compaction writes a parquet file, it also writes and removes a marker file, and it adds up to three directory fsyncs. Ingest can now fail on a failed directory fsync where it used to warn. The architecture pages `data-flow.md` and `recovery.md` gain the restart guarantee and the boot steps. The shutdown comment in `main.rs` is corrected to say that compaction stops without a final pass, because the next boot recovers.

Slice 2 adds the `corpus_recovering` error code, the cause kinds `restart_backlog` and `rollup_pending`, a sixth health check and an alert. A client that handled the rollup refusal as `service_unavailable` now sees `corpus_recovering`. The WAL writer publishes a file name without replacing an existing one. Lowering the hot-buffer caps across a restart can turn a backlog that fitted into overhang once. `recovery.md`, `data-flow.md`, `health.md`, `api.md`, `operational-alerts.md` and `configuration.md` document the boot steps, the refusal and the alert.
