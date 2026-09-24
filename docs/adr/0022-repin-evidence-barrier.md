# Repin terminal `succeeded` is an evidence barrier

status: accepted (2026-08-25) — prep ruling record for #125's sibling (repin evidence contract)

`finish_cutover` commits the pin flip, the job's `succeeded` status, and
the CLEAR of the field's old degraded evidence in one transaction
(ADR-0011 C1), and only after the exclusion guards drop does
`record_outcome` insert the NEW conflict evidence, best-effort. Any
status poller — the test harness, and equally a real operator's
`repin --wait` — can therefore observe `succeeded` and read empty
evidence on a healthy postgres. #125 caught this as a "disguised"
test flake; it is a product contract gap, and the moment it
under-reports is the worst one: immediately after an operator forced a
lossy repin.

## Rulings

1. **`succeeded` implies the evidence is visible, unconditionally.**
   The terminal transaction does all four: clear old evidence,
   materialize new evidence, flip the pin, set `succeeded`. There is no
   observable instant where the job is terminal and the evidence
   absent.

2. **The transaction's input is durable before the point of no
   return.** The per-service nulled tallies live in the engine's
   volatile `BuildState`; a terminal transaction reading them directly
   would hold on the healthy path and silently not on boot replay
   after a cutover-window crash. So the engine stages the tallies onto
   the `repin_jobs` row at the final catch-up increment — inside the
   exclusion window, when the counts are final — and the terminal
   transaction materializes evidence from the staged row. Live cutover
   and boot replay commit the identical logical transaction.

3. **Replay clears and inserts only when it performs the transition.**
   The existing gate stands: evidence is touched by the call that
   completes the job, never by an idempotent re-run of a flip that
   already happened. A boot replay of an already-terminal job changes
   nothing.

4. **No wire-visible `finalizing` state.** Status stays `running` until
   the terminal commit; a pre-terminal state would export the barrier
   to every client (CLI `--wait`, SPA, API consumers) and buy nothing
   the commit boundary doesn't already give. Dry-run jobs are
   unchanged — their plan evidence is on the job row before
   terminalization already.

5. **Metrics stay post-transaction and best-effort.** Counters are not
   part of the barrier; nobody makes a decision from a mid-transaction
   counter.

6. **The compaction bookkeeping budget is untouched and becomes
   observable.** The 2-second `BOOKKEEPING_BUDGET` is a deliberate
   trade (a stalled compactor stops WAL drain until the hot buffer
   evicts, which is invisible events; since ADR-0043 it refuses ingest
   instead) and gains no knob — a test
   budget knob would hide the contention ADR-0021 removes instead.
   What it gains is the metric the docs already claimed:
   `trawl_catalog_bookkeeping_timeouts_total{write}` with
   `write ∈ {conflicts, observations}`, incremented where the budget
   abandons. Tests assert a DELTA of zero across their window before
   asserting on evidence values — the prometheus recorder is
   process-global, so an absolute zero breaks under plain `cargo
   test` — turning the disguised failure into a named one.

## Amendment

ADR-0011 slice C1's "a successful repin clears the field's evidence
inside `finish_cutover`'s transaction" becomes "…clears the old and
materializes the new evidence inside that transaction, from tallies
staged on the job row"; its gating clause (ruling 3 above) is
unchanged.
