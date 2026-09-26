# Ingest is admitted against hot-buffer capacity; nothing admitted is evicted

status: accepted (2026-09-24), prep record for #253

When the hot buffer reaches its caps, it evicts its oldest batches. Those events were acknowledged and are safe in the WAL, but no query can see them until compaction publishes them. Searches answer 200 with rows missing. ADR-0041 recorded this as a known limit and rejected refusing reads, because the pressure should fall on the sender, who can retry. This record replaces eviction with **admission**: a producer reserves hot-buffer space before it writes the WAL, and a batch that cannot reserve space is refused without writing anything.

## Decision

**One ledger, two dimensions.** The hot buffer keeps one process-wide ledger of event count and serialized ndjson bytes. These are the units it already counts. A reservation checks and charges both dimensions in one atomic step. The charge moves into the batch when it is inserted. Compaction's drain releases it, and so does a write that did not succeed. Nothing leaves the buffer except through drain. Eviction is removed.

**Caps stay global.** `hot_buffer_max_events` and `hot_buffer_max_bytes` keep their meaning and defaults. There are no per-environment caps: trawl is single-node, and one environment can fill the buffer for all of them. External producers (HTTP and syslog) may fill 15/16 of each cap. Self-telemetry may fill the whole cap. That reserve lets trawl's own records of a stall stay searchable during it. The reserve is fixed in code, and only the internal telemetry producer holds it. A service name sent by a client never grants it.

**Admission never waits under the publication gate.** Compaction needs the gate's write side to drain, and it is the only thing that frees space. So admission is try-or-refuse. The order is: parse, reserve, take the gate's ingest side, write the WAL, insert. A refused batch never touches the gate. Anything that waits for capacity (the syslog batcher, the telemetry queue) waits outside it. When the ledger has no free space at all, an HTTP request is refused before it is decompressed.

**HTTP refusals.**

- A request that fits the external ceiling but not the free space answers **503** with code `hot_buffer_full` and `Retry-After` equal to the compaction interval. No event from that request is written, so a retry cannot duplicate one. Space frees only when compaction drains, so a shorter hint only adds refused work.
- A request whose charge exceeds the external ceiling can never fit. It answers **413** with code `ingest_batch_too_large` and no `Retry-After`. Vector drops a 4xx batch, so the documentation tells senders to keep batches under the ceiling.
- Admission covers the whole request across all its (environment, service) groups. A real WAL failure keeps today's 500.
- Each 503 emits one WARN `http_failure` with `cause_kind` `hot_buffer_full` (ADR-0040). The 413 is a client error and emits none.

**Syslog.** The batcher reserves each group before writing it. A refused group stays pending, in order, and the batcher stops taking from its queue until space frees. Over TCP, the listener waits to enqueue instead of dropping, so it stops reading and the kernel's flow control reaches the sender. It no longer disconnects a client because the queue is full. Over UDP, a full queue still drops and counts the datagram, and the counter says whether admission was refusing at the time.

**Self-telemetry.** A refused flush keeps its batch at the front of the telemetry queue and ends the cycle. The existing 16 MiB bound and its oldest-first shedding still apply. Recording an event never blocks.

**Compaction under pressure.** At or above half of either cap, or after any refusal, the compaction loop wakes immediately. A pressure pass reads WAL files of any age. This is safe because publication re-checks, under the write guard, that every consumed file still exists, so a write that was withdrawn is never published. Pressure passes skip rollup work. A pass that frees nothing falls back to the normal interval. The exported pressure state clears below a quarter of both caps. _Amended 2026-09-25:_ a pass with a failed chunk also falls back to the normal interval, even when it freed other batches. A stall's own error lines reach self-telemetry, and those inserts would otherwise give every rerun something to free.

**Stalls are loud, reads stay whole.** When compaction cannot drain (catalog down, repin cutover, a publication marker block), ingest is refused. Every admitted event stays visible, and reads keep answering 200. Health reports `degraded` with an `ingest_capacity` check, still at HTTP 200, so probes do not restart the server. Gauges expose occupancy, caps, the admission state and the oldest resident batch's age. An alert fires on sustained refusal.

**A write that leaves its file visible.** A WAL write that fails and cannot withdraw its file (`LeftVisible`) releases its reservation. It was never acknowledged, so the invariant does not cover it. Compaction merges the file later, as it does today. The disk it uses is headroom's concern (ADR-0042), not the ledger's.

**Boot hydration charges the same ledger.** Slice 2 of ADR-0041 loads surviving WAL into this ledger. How hydration treats its overhang, and whether the telemetry reserve applies to it, is decided in that slice's own prep.

## Considered options

**Wait for capacity**, rejected: a wait while holding the gate deadlocks the only drainer, and a wait before the gate still pins a blocking worker.

**Reserve before parsing, from the wire size**, rejected: it is an estimate, and gzip makes it wrong by up to 10×.

**Keep eviction as a last resort**, rejected: it brings back acknowledged rows that no query can see.

**429 for a full buffer**, rejected: 429 `rate_limited` is a per-key verdict with a pinned shape (ADR-0006). A full buffer is server-wide.

**`Retry-After: 1`**, rejected. It recovers faster after a quick drain, but during a stall it makes senders decompress and parse repeatedly while compaction is competing for the CPU.

**Fail readiness while refusing**, rejected: it takes reads down, which is the outcome ADR-0041 rejected.

**Per-environment caps**, rejected: they split one memory budget and add configuration for a noisy-neighbour case that a homelab rarely has.

**Charge a `LeftVisible` batch as visible but unacknowledged**, rejected. It spends scarce memory on rows that were answered with an error, and it adds a ledger state that hydration would also have to model.

## Consequences

ADR-0022's trade ("a stalled compactor stops WAL drain until the hot buffer evicts") becomes ingest refusal. A long repin pause (ADR-0011) can now refuse ingest once the buffer fills. The health `checks` object gains a fifth key. Pressure passes write more, smaller hourly files until the daily rollup merges them. Under continuous small writes, a large request that is legal can keep being refused. That is accepted at this scale.
