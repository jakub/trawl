# Repin lifecycle: cooperative cancel, ceiling-bounded force, dead-pin GC — and no automation

status: accepted (2026-08-18) — prep ruling record for #95

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
