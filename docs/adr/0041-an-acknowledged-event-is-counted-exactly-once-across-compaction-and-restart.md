# An acknowledged event is counted exactly once across compaction and restart

status: accepted (2026-09-23), prep record for the restart-visibility work; amended (2026-11-08): rollup marker discovery ignores the parent's name, and marker hours are 00 to 23, see the Amendment

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

_Amended 2026-09-27:_ the human decided that the writer fails closed on a marker whose claims it cannot establish. The slice's prep let an unreadable or invalid marker claim nothing. That decision is superseded. Now a missing marker claims nothing, but a present marker refuses the WAL write for its environment and service when it cannot be read, when it is invalid, or when it is over the marker size bound. A marker that is not a regular file is invalid. The refusal is retryable and publishes nothing. Every write for that environment and service is refused until the marker is resolved. The reason is that such a marker may still list the new file's name. Once an operator repairs the marker, recovery would retire that file and lose an acknowledged batch. The size bound is derived from `MAX_COMPACTION_CHUNK_SIZE` = 4096: at most 4096 WAL names of at most 255 bytes each, plus a 1 KiB frame, about 1 MiB. Configuration caps `compaction_chunk_size` at 4096, so compaction never writes a marker the reader refuses.

**Steady-state eviction is not solved here.** When the hot buffer fills during normal running, it evicts acknowledged, uncompacted rows. They are counted but not refused. That limit is documented until ingest backpressure replaces eviction, which is a cross-cutting change with its own issue.

_Amended 2026-09-24:_ [ADR-0043](0043-ingest-is-admitted-against-hot-buffer-capacity.md) replaces eviction with admission against the hot-buffer caps. Hydration charges that ledger.

_Amended 2026-09-27:_ the human decided the threat model for the files that recovery and hydration read. The data root and the WAL directory are trusted storage, owned by the trawld user. A party who can write to them is inside trawld's trust boundary, because that party can already delete or alter the corpus. trawld opens these files without following a symlink and without waiting on a FIFO, and it reads each marker only up to a size bound. Those guards stop accidental damage, such as crash residue, a truncated file, or garbage, from hanging the boot or wedging recovery. They are not a defence against planted FIFOs, symlinks, or floods of files. A flood of WAL files is the example this excludes. Listing a flooded WAL at boot costs what compaction's own scan of it costs, which the hydration bound above already accepts, and the admission ledger of ADR-0043 bounds WAL growth in normal running. Boot rollup recovery follows the same rule. It opens a `.rollup-*` marker and the staged `.parquet.tmp` output as regular files only, and it reads a marker only up to 100 hourly paths. A marker or an output that fails those checks fails its recovery, so the marker stays pending and reads refuse `rollup_pending`.

**No epoch bump.** Parquet and WAL formats are unchanged, and the marker is an additional file that older binaries ignore. A WAL file without a marker is treated as unpublished, as it is today. Duplicates left by earlier crashes cannot be told apart and stay as they are.

_Amended 2026-11-06:_ a daily rollup publishes in a durable order, like a compaction publish (#357). Before this amendment, the rollup never fsynced its output or the day directory, and it published its marker with a best-effort directory fsync. Recovery retired every listed hourly file as soon as any canonical daily file existed, without reading that file. After a crash, recovery could retire the only copy of an hour's rows under a daily file that never held them. In this record a **service-day** is one service's files in one day directory. A **rollup marker** now records, within a size bound:

- the identity of the replacement daily file: its size and BLAKE3 digest;
- each input hourly file, by its name relative to the day directory, with its identity;
- the identity of the daily file that existed before the merge, if there was one.

A rollup runs in this order:

1. Write the merged output to `{service}.parquet.tmp`.
2. Open the tmp as a regular file, without following a symlink. On that one descriptor, parse the Parquet footer with the pure-Rust reader, fsync the file, and hash it. If the footer does not parse, the rollup fails before any marker exists, and the inputs stay as they are. The footer check guards only this path. Recovery decides by identity, not by footer.
3. fsync the day directory and each ancestor up to the data root. Retention can delete a day directory and a publish can recreate it, so no step assumes an earlier sync still holds.
4. Under the publication write guard, register the marker and write it with a fatal directory fsync.
5. Rename the tmp onto the canonical daily file, then fsync the day directory. A failed fsync is fatal.
6. Retire exactly the listed hourly files, then fsync each distinct hour directory. The fsync runs even when the inputs are already gone. If an hour directory is gone, its parent is synced instead.
7. Unlink the marker and fsync the day directory, under the removal-sync obligation below.
8. Finish the marker's registration with the publication gate.

The rollup marker's format changed, so a release older than this amendment cannot read a marker this one writes. Its size bound holds 100 entries with their identities.

_Amended 2026-11-06:_ rollup recovery decides from the recorded identities and never guesses (#357). It runs under the publication write guard, before a compaction pass compacts new WAL. It reads the marker within its bound and checks that every name is relative and stays inside the day directory. Then it applies the first case that holds:

- **a. The canonical daily file carries the replacement identity.** This holds whatever the tmp holds: the rename happened. Recovery fsyncs the day directory, removes any leftover tmp, then runs steps 6 and 7.
- **b. The tmp carries the replacement identity.** The rename did not happen. Recovery makes the marker and the directory chain durable again, renames the tmp, fsyncs the day directory, then runs steps 6 and 7.
- **c. Provable abort.** Every listed hourly file is present with its recorded identity. Also, either no canonical daily file exists and none was recorded, or the canonical carries the recorded prior identity. Nothing was lost. Recovery deletes the tmp, removes the marker under the removal-sync obligation, and logs at WARN. The next rollup merges again.
- **d. Fail closed.** In every other state, recovery keeps the hourly files, the marker, the canonical daily file and the tmp. It logs `rollup_recovery_unresolved` at ERROR, naming the service and day but no path. The marker stays pending, so reads keep refusing as `rollup_pending`, and the existing recovery failure counter records it.

A marker that a release older than this amendment wrote holds no identities, so it always reaches case d. Recovery checks the canonical file first, whatever the tmp holds, because a recovery that crashed after its rename leaves no tmp. Recovery reads no Parquet footer. The slice 2 text above says it does. It never did: it checked the `PAR1` magic bytes.

_Amended 2026-11-06:_ the human ruled that rollup recovery fails closed when it cannot prove the state (#357). The question was "When recovery cannot prove the state, what should trawl do?", and the answer was "Stop and wait for me". A canonical daily file that does not carry an expected identity does not prove that rows were lost. Two alternatives were rejected:

- **Quarantine the canonical as counted loss.** A marker can come back after a later file replaced the daily. A removal whose directory fsync was lost leaves the marker on disk, and a repin or conformance rewrite can then replace the daily with a correct newer one. After a power loss the marker reappears, and quarantine would destroy the only copy of the data.
- **Quarantine only a canonical whose footer does not parse.** The human chose one rule.

Case d is a standing fault, like those under "Standing faults refuse search". Reads refuse with 503 `corpus_recovering`. Each compaction pass stops at rollup recovery, so no WAL is published anywhere in the process, and ingest is refused once the hot buffer is full, because only compaction drains it (ADR-0043). Health reports `checks.corpus` as `rollup_pending`, and `TrawlCorpusUnsettled` fires after 10 minutes. `trawld --doctor` reports each unresolved marker with its service, day and the proof that failed. `recovery.md` gives the operator steps: back up the data root, inspect, then move the candidate file aside or remove the marker. Recovery still clears a marker on its own whenever case a, b or c holds.

_Amended 2026-11-06:_ a removed rollup marker stays tracked until its removal is durable (#357). The publication gate forgets a marker whose path is absent. An unlink that succeeds, followed by a day-directory fsync that fails, leaves the path absent while the entry can still come back after a power loss. Each unlink of a rollup marker therefore creates a removal-sync obligation for its service-day. The obligation ends only when a later fsync of that day directory succeeds, and each compaction pass retries the fsync. `evaluate()`, `pending_rollup_markers()` and a registration's `finish()` never end it because the path is absent. While it holds, no rollup of that service-day runs, repin admission refuses, and conformance does not rewrite that service-day's files. Reads are not refused: the files on disk are consistent, and only the durability of the unlink is unproven.

_Amended 2026-11-06:_ the limits of the rollup guarantee (#357). After an fsync fails with EIO, Linux can clear the error, so a later fsync can succeed while the earlier writes never reached the disk. A retry that succeeds after a failure therefore proves less than a first success. Trawl still ends a removal-sync obligation on that success, because it has no stronger signal, and the alternative is a block that only a restart clears. The order of writes across a power loss is not proven. The tests kill the process, which leaves the page cache intact, and they record what each file-system call returned. No test cuts power. The guarantee assumes a local file system whose fsync is honest. Storage that reports an fsync it did not perform, and an operator who edits the data root while trawld runs, are outside it.

_Amended 2026-11-07:_ the implementation of #357 refines the rollup amendments above in seven ways.

- **Present inputs carry their identity in a and b.** Before either case retires anything, each listed hourly file still at its path must carry its recorded identity. An absent one counts as already retired. A present one that differs, or that cannot be read, sends the marker to case d, because retiring it could delete rows the replacement does not hold.
- **Case b proves that the rename did not happen.** The tmp must carry the replacement identity. Also, either the canonical daily file is absent and no prior was recorded, or the canonical carries the recorded prior identity. Any other canonical sends the marker to case d.
- **An empty rollup does not run.** When every hourly input was quarantined as invalid, the rollup returns before it merges, so a marker never lists zero inputs.
- **The marker bound shrinks.** Inputs are named relative to the day directory as `HH/{service}.parquet`, not by absolute path. The bound is derived from the encoding, at 100 inputs and the longest service name, and comes to 26 KiB, down from about 1.2 MB.
- **The removal-sync obligation is held in memory.** It does not survive a restart, and no boot step rebuilds it. A marker that comes back after a restart and a power loss is classified again from the files. The identity checks above mean it never retires or deletes a file it cannot prove, so it finishes in case a, b or c, or fails closed in case d.
- **A removal whose day directory is gone syncs the env directory.** Retention can delete a day directory while its marker's removal still owes the fsync. A retry then finds no day directory to sync. It fsyncs the env directory instead, which makes the day's removal durable, marker entry and all, and that success ends the obligation. A missing env directory is still a failure.
- **`QuarantineKind::RollupTemporary` is retired.** Its only producer was recovery's quarantine of an invalid tmp. Case c now deletes the tmp, so the `rollup_temporary` label of `trawl_files_quarantined_total` is gone.

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

## Amendment (2026-11-08): rollup marker discovery ignores the parent's name, and marker hours are 00 to 23

Prep record for the untangle prep of 2026-11-08 (defect D13). ADR-0009's
amendment of the same date gives the data-root layout one owner and one
strict grammar.

**Discovery stays fail-closed.** `publication::scan_markers` skips a child of
an env directory whose name does not parse as a date
(`publication.rs:787-793`), so a `.rollup-` marker under such a directory is
never registered and never refuses reads. Its own doc says an unknown marker
must be an `Err` (`publication.rs:766-768`). A stricter date grammar must not
widen that hole. Discovery now inspects every directory child of a valid env
for the rollup marker prefix, whatever the child's name. A marker found under
a directory that is not a canonical date keeps corpus reads refused as
`rollup_pending`, like any marker recovery cannot resolve. Recovery leaves the
marker and every file in place for the operator.

**Marker hours share the partition grammar.** `rollup_marker::parse_input_name`
accepts the hours `00` to `99` (`ingest/rollup_marker.rs:296-301`), and
`MAX_INPUTS` = 100 is justified by that domain (`:56-59`). The rollup scan
now takes only canonical hours, so the reader narrows with it: an input named
`24/{service}.parquet` or `99/{service}.parquet` fails validation and sends the
marker to case d. `MAX_INPUTS` becomes 24, and the size bound and its tests are
derived again from it. The record encoding and the identity checks do not
change. The publication marker already accepts `00` to `23` only
(`ingest/publication_marker.rs:332-340`).

Compaction never writes an hour directory above `23`. An earlier release could
list a higher hour only by merging a stray directory someone else created. Such
a marker now fails closed and stays, with its files, for the operator.

**Considered, not adopted.** Filtering discovery by the strict date grammar.
It would hide a marker under a non-canonical parent and admit reads while a
rollup may be pending.
