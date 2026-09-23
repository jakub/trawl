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

**Restart visibility (slice 2, prepared separately).** Before any producer, the scheduler or the listener starts, boot does two things:

1. Recover the rollup and publication markers synchronously.
2. Load the surviving WAL files into the hot buffer, using their existing batch identities. This reload is **hydration**.

Hydration details:

- **It never publishes to the live bus** and needs no Postgres.
- **Boot cost is bounded by the hot-buffer caps.** If the backlog exceeds the caps, the part that doesn't fit is the **overhang**, and corpus reads are refused with a named 503 until compaction drains it.
- **Timing:** the first compaction tick runs immediately. Scheduled nets claim a window only after the server is ready.

**Steady-state eviction is not solved here.** When the hot buffer fills during normal running, it evicts acknowledged, uncompacted rows. They are counted but not refused. That limit is documented until ingest backpressure replaces eviction, which is a cross-cutting change with its own issue.

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

## Consequences

When compaction writes a parquet file, it also writes and removes a marker file, and it adds up to three directory fsyncs. Ingest can now fail on a failed directory fsync where it used to warn. The architecture pages `data-flow.md` and `recovery.md` gain the restart guarantee and the boot steps. The shutdown comment in `main.rs` is corrected to say that compaction stops without a final pass, because the next boot recovers.
