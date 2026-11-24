# Repin lifecycle: cooperative cancel, ceiling-bounded force, dead-pin GC — and no automation

status: accepted (2026-08-18) — prep ruling record for #95; amended (2026-11-23): rulings moved from code comments, see the Amendment

ADR-0011 slice B shipped the single-field, operator-triggered repin core
and parked lifecycle extensions. This ADR rules on all five.

## Rejections (recorded so they stay rejected)

1. **Automatic repin is rejected, not deferred.** The degraded-pin verdict
   is computed from sender-influenceable data (ADR-0011 C1 ruling 3
   deliberately accepts single-sender evidence); actuating an irreversible
   whole-corpus rewrite — double-held bytes, retention fully suppressed,
   archive I/O — from that signal hands a remote sender a storage-work
   amplification primitive. And nothing is urgent: a degraded pin is a
   slow leak measured in days, the manual command takes seconds. The
   substitute is **one-click, not zero-click**: the analyzer renders the
   exact `repin --dry-run` line; the SPA may make it a button a human
   presses.
2. **Auto-scheduled dry-runs are rejected too.** The scan is a full-corpus
   pass holding the one-running slot; automating it is a denial of service
   requiring no operator action.
3. **Pin tombstones are rejected.** They either count against
   `MAX_PINNED_FIELDS` (nothing freed) or become a new unbounded
   client-named table — the identical attack, relocated.
4. **Drop-with-rewrite reclamation is rejected.** Rewriting a column out
   reclaims nothing in steady state (the next event re-pins it) and needs
   the tombstone above to pretend otherwise.

## Cancellation (ships first)

5. Cooperative and **pre-cutover only**: honored at file boundaries in the
   scan and build loops, and checked once immediately before the Cutover
   marker write. Past the durable marker a cancel is **refused (409,
   "past the point of no return"), never queued** — a deferred cancel
   firing after the swap would be a second, undesigned rollback path.
   Three wire outcomes: accepted-stopping / refused-too-late /
   no-job-running.
6. The cancel token is **in-process and non-durable** (a restart is a
   stronger cancel than a cancel; boot reconciliation already abandons a
   `Building` shadow and fails the orphan row). `cancel_requested_at`/
   `cancelled_by` persist on the job row for the record; terminal status
   `cancelled` routes through the existing `abandon_build` unwind — no new
   recovery state. Latency contract stated in the response: effect at the
   next file boundary; a DuckDB COPY in flight is not interrupted.
7. Authorization: `SchemaWrite` cancels (any holder may cancel any job on
   a single node; identity recorded). Dry-run scans are cancellable too —
   the token reaches the scan loop.

## Force is a ceiling, not a blank check

8. `--force` carries the dry-run's counts as **quantitative ceilings**
   (nulled rows, dialect-ambiguous rows, plus a stated slack): the
   finished-shadow gate refuses when the shadow exceeds what the operator
   actually saw, instead of blessing any post-scan loss. The re-asked gate
   already exists; this bounds what it may accept.

## Dead-pin GC

9. `trawl schema gc-pins [--dry-run]` deletes a `field_types` row iff the
   field is provably dead on **both axes**: no `field_services`
   observation inside the effective dead window — operator-set
   `--older-than` (default 30d), floored at the maximum effective
   retention age when retention is enabled, so a pin cannot be declared
   dead while retained-by-policy files may still carry it — AND no standing
   parquet footer under any env dir names the column — evaluated under
   the compaction corpus gate. Metadata-only: no rewrite, no marker, no
   query exclusion. Being wrong costs a harmless re-pin, never a type
   conflict, because the footer check is what prevents deleting a pin a
   standing file still carries.
10. GC is the remedy for the **accident** (a decommissioned service's dead
    slots), explicitly not a mitigation for hostile catalog exhaustion —
    the cap, the half-of-free ration and the fill gauges remain that.

## Multi-field: ruled atomic, deferred

11. When multi-field ships, it is **one atomic job over a bounded field
    set (≤16), never a serialized queue**: every per-job scarce cost —
    cutover exclusion drain, double-held bytes (union of affected files,
    counted once), retention and rollup suppression windows — is paid once
    instead of N times, and N pins flip in one transaction. The force gate
    is per-field with an all-or-nothing job verdict naming refusing
    subjects; the `data/REPIN` marker gains an explicit `version` field
    (an unknown version refuses the boot loudly; upgrade note: never
    upgrade with a repin in flight). **No implementation is scheduled** —
    repins are rare; this records the shape so a future need doesn't
    relitigate it.

## Decision record

12. No general receipts table: `repin_jobs` already records every decision
    repin makes (actor, force, dialect, plan counts, terminal status). The
    one missing judgment gets a narrow row: `field_degraded_ack(field,
    acked_at, acked_by, note, evidence_through)` — the analyzer suppresses
    the degraded badge while no episode is newer than `evidence_through`;
    a new episode re-raises it; a successful repin clears the ack in the
    same transaction that clears the conflict evidence.
13. **Every lifecycle decision emits a structured log event** — cancel
    (requested/effective/refused), force acceptance with its ceilings and
    the shadow's actual counts, gc-pins deletions (per field), ack
    created/cleared. With `internal_telemetry` on these are ordinary
    `service=trawld` records through the WAL: the audit trail is durable
    and queryable with the same DSL as everything else.

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

**Cancellation**

- **The cancel request, the Cutover marker write that latches the job, and the settling of a pre-cutover terminal verdict are decided under the one lock of the in-process `CancelRegistry`.** A request and the latch are two sides of one race, and a flag plus a watch channel cannot be compared atomically. A cancel pending when a pre-cutover verdict settles wins, including over the finished-shadow force refusal, and the job ends `cancelled`. A verdict that settled first keeps the job, and a later cancel answers no-job-running (404). A latched job answers 409.
- **A Cutover marker write that fails leaves the job unlatched, so the job unwinds through `abandon_build`, where a still-pending cancel settles it as `cancelled`.** A marker that was never published means the job never crossed the point of no return. Answering 409 would tell the operator the corpus is moving when it is not.
- **`cancel_requested_at` and `cancelled_by` record only an accepted cancel, are written together while the row is `running`, and keep the first request that landed.** A refused cancel writes nothing to the row. The row names the request that took effect, and a request that took no effect must not read as one that did.
- **Only a live process whose unwind ran writes terminal status `cancelled` and the `repin_cancelled` event.** `cancelled` asserts that a file boundary observed the request. A decision task that panics with a cancel pending ends `failed`, keeps the request fields, and emits `repin_cancel_unobserved`. `RepinStore::reconcile_orphans` fails an orphaned job even when `cancel_requested_at` is set. The `repin_jobs` constraints admit a `failed` row that carries cancel request fields.
- **`POST /api/v1/schema/repin/cancel` takes no request body.** At most one job runs, and naming a job would let an operator cancel a newer job that took the slot of the one they meant. The handler answers 202, 409 or 404 with a `RepinCancelOutcome` that always agrees with the status, and 503 on a query-only node, which owns no data root and runs no job. The registry has already decided the verdict, so a failed read of the job row drops the row from the body and never changes the verdict.
- **Every repin cancel audit event names the acting key's prefix beside its display name.** The operator chooses the display name and can reuse or rename it, so only the prefix identifies the credential that acted.
- **A repin start request whose own ladder observed a cancel answers 200 with the terminal `cancelled` job row.** 409 already means refused-needs-force to a client that reads the body, so a new status code was rejected. The client decodes a 200 by the row's status in `decode_repin_start`. `succeeded` is a report, `cancelled` is cancelled, `running` is a protocol error, and any other status is `RepinStart::Failed`. No status falls through to `Report`, because a `failed` row read as a report prints as a dry run and exits 0.

**Force ceilings**

- **The force verdict is one function, `force_refusal`.** The scan gate, the finished-shadow gate and the wire's `requires_force` all ask it, the last over the job row's own persisted numbers. A second condition on the wire could drift from the gates, so a dry run could report a verdict the executing request does not reach. A forced plan is refused when its counts exceed the ceilings. An unforced plan is refused on any nulled row or binding ambiguity.
- **The finished-shadow gate is the authoritative force gate.** Ingest and compaction run for the whole job, so a file written after the scan can carry values the new pin cannot read. Only the finished shadow's own counts describe what the cutover would publish, and refusing there is safe because nothing visible has moved.
- **`requires_force` is absent, not `false`, until the scan has recorded its plan.** Before the plan lands, the row's counts are zeros that mean "not measured", and a poll would read a confident `false` off a job about to refuse.
- **A force ceiling the request does not state resolves from the job's own scan as `scan + max(ceil(scan / 10), 10)` in saturating integer arithmetic, and is never unlimited.** The finished shadow almost always drifts a little from the scan on a live install, so refusing on a one-row drift would make force useless. The 10% covers growth on a large corpus, and the floor of 10 rows covers a small one.
- **An explicit force ceiling is held exactly as stated, and each dimension resolves on its own.** A ceiling below the scan, including zero, is a refusal the operator asked for, such as "force the ambiguity but not one row of loss". A ceiling on an unforced request is a 400, because it contradicts the rest of the request. A ceiling above `i64::MAX` is a 400, not a clamp, because a clamped value in the BIGINT column is a bound nobody set.
- **A client that offers force runs a forced dry run, shows the ceiling pair that scan resolved, and sends both ceilings on execution.** The SPA repin dialog and `trawl schema repin --yes --force` both do this. If the server re-derived a ceiling at execution, the execution's own scan would set a bound the operator never saw, which ruling 8 exists to stop. The cost is a second scan on this path. A refusal that carries ceilings is a run that exceeded them, not consent to run again.
- **`repin_force_accepted` is emitted only for a forced job, after its Cutover marker is latched and before the swap, and carries counts only.** Past the latched marker the corpus becomes the new generation, by this process or by boot replay, so this is the point where force licensed the mutation. A marker failure or a cancel that won the latch never reaches it. The event carries the accepted ceilings, the rewrite's actual counts and the scan's projected counts, and no sample values or free text, so the agreed pair stays legible after later writes overwrite the job row.

**Dead-pin GC**

- **`catalog::gc::candidacy` decides in this order: a contract-typed envelope field is never a candidate, a pin observed at or after the cutoff is alive, and every other pin is a candidate that the footer axis must still clear.** The envelope's seeded pins must survive a corpus that never carried them. A pin never observed at all is a candidate, unlike in `/api/v1/schema`, which always lists it, because a pin with no observation and no carrier file is exactly the accident gc exists to free, such as a `curl` typo.
- **The effective dead window is the larger of the requested window and the retention floor, computed only on the server, and a requested 0 is honoured.** A shorter window would call a field dead while its data sits under a live env. A 0 is safe because the footer axis is a second, independent proof, and an operator cleaning up after a bad shipper should not wait for a field the corpus never carried. `GcPinsResponse` reports the requested, floor and effective windows, and the CLI and SPA render them as given, so a client never shows a window no run used.
- **The retention floor is the maximum of the global `max_age_days` and every `[retention.env.*]` entry, read from configuration only.** The shortest-lived env says nothing about data a longer-lived env keeps. The floor is absent when any of them is 0, because then no finite age bounds the corpus and the footer axis alone guards the pin. Disk-pressure retention contributes nothing, because it deletes by free space and names no window. A horizon read from the filesystem would move as data lands and ages out.
- **The footer axis scans every `*.parquet` under each valid live `data/{env}/` tree, not only files in the hourly and daily layout, and skips reserved env names (`scheduled`, `wal`).** Presence in the query glob is what makes a file's footer evidence, so a stray file the glob reads can carry the column. Other artefacts, such as `.corrupt` quarantines, `.tmp` leftovers and markers, are skipped because the glob never reads them. Pin gc refuses to run while repin staging exists, because a corpus a repin is rewriting proves nothing.
- **The footer walk fails the whole run closed, and deletes nothing, on any path whose columns it cannot know.** The proof ends in a deletion, so a partly read corpus must not answer "no carrier found". An absent or unlistable data root is never an empty corpus, because an unmounted volume would otherwise become a mass delete. The same holds for a symlink under an env directory, an unreadable directory or entry, a file that vanished mid-walk and a footer that does not parse. A `.parquet` that is not a regular file by `lstat` is refused and never opened, because opening a FIFO blocks with the corpus gate held, which stalls ingest with no timeout.
- **Pin gc holds the compaction corpus gate from the footer scan through the purge commit and the cache eviction, in its own spawned task.** If compaction published a file between the scan and the delete, that file could carry a column the proof called dead. axum drops a handler future on disconnect, and a drop between commit and eviction would leave a pin deleted from the store and present in the cache.
- **Pin gc admits one run at a time by try-lock, so a second caller gets an immediate 409 instead of queuing.** No compaction batch can publish while the gate is held. A queued run would hold the gate again for a full footer scan over a candidate set read before the first run deleted anything.
- **Each read-only postgres call pin gc makes under the corpus gate is bounded to 5 seconds (`IN_GATE_TIMEOUT`).** An unresponsive postgres would otherwise become an unbounded ingest stall, and a per-call bound is one an operator can read off the refusal.
- **The gc-pins report, the pins-deleted metric, the cache eviction and the per-pin `catalog_pin_gc` audit events come from the purge's own `RETURNING` set, never from the walk's projection.** A candidate deleted underneath the run, by a concurrent purge or by an operator with psql, is not a pin this run reclaimed, and the audit trail is the record an operator has to trust. A dry run writes no row, bumps no generation or metric and emits no audit event, because the dry run is the safety step.
- **A repin claim and a gc-pins purge each take the transaction-scoped advisory lock `CATALOG_LIFECYCLE_LOCK_KEY`, a key distinct from the session-held `ADVISORY_LOCK_KEY`.** Both check, then act on, which fields the catalog pins. A claim that lands after gc's last look would leave `finish_cutover` updating a deleted `field_types` row, so the new pin would exist only in memory and die with the process. Session and transaction advisory locks share one lock space, and trawld holds the session lock for its whole life, so reusing that key would deadlock every claim and purge.

**Degraded-pin acknowledgement**

- **`field_degraded_ack.evidence_through` is the episode count the ack was written against, and the analyzer suppresses the degraded verdict while `episodes <= evidence_through`.** Postgres `now()` is the transaction clock, so evidence recorded in the same second as the ack is an ordinary tie, and a timestamp would suppress that evidence unseen. The episode count only grows, and a repin deletes the ack with the evidence, so an ack never suppresses more than the operator saw.
- **Correction.** Ruling 12 says the analyzer suppresses the degraded badge "while no episode is newer than `evidence_through`". `catalog/analyzer.rs` compares counts, not times: an ack suppresses the verdict while the field's episode count is at most `evidence_through`.
