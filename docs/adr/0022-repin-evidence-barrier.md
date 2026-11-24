# Repin terminal `succeeded` is an evidence barrier

status: accepted (2026-08-25) — prep ruling record for #125's sibling (repin evidence contract); amended (2026-11-23): rulings moved from code comments, see the Amendment

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

## Amendment (2026-11-06): terminal is the evidence barrier, not an admission barrier (#345)

`succeeded` still means the evidence is visible. It does not mean the next
repin can start. Under the ADR-0011 amendment of the same date, the job's
task keeps the repin permit until its staging sweep, rollup pause drop and
cancel disarm finish. A caller that sees a terminal row and starts another
repin at once can get 409 because the previous repin is still cleaning up.
The caller retries. Ruling 4 stands: the wire gains no `finalizing` state,
and the 409 is the only signal. `repin --wait` still returns at the
terminal row.

## Amendment (2026-11-23): rulings moved from code comments

A comment audit on 2026-11-23 removed these rulings from the code, where they were the only record. They were already in force. None is a new decision.

- **The engine stages the five final counts and the per-service null tallies in one `UPDATE`, before the force gate and before the Cutover marker, and staging is not best-effort.** The counts and the tallies are one reading of the finished shadow. A row that carries totals from one pass beside tallies from another describes a corpus that never existed. Staging replaces the final progress write, so a failed staging aborts the job while the corpus still stands at its pre-repin generation.
- **`stage_cutover_input` that updates no row is an error, and the engine stops before it writes the Cutover marker.** The `WHERE` matches the job's `id` only when that job is `running` and is not a dry run. No match means the job row is missing, the job is already terminal, or the job is a dry run. In each case the engine must not write a marker for a job that postgres does not record as running. Restaging a job that is still `running` overwrites the same row, so a retry is safe.
- **`staged_tallies` orders services by rows nulled, highest first, then by service name, and keeps the first `MAX_CONFLICTS_PER_FIELD`.** The per-field conflict trim drops the overflow anyway. Cutting it here decides which rows survive: the services that lost the most. The accepted residual is that a repin lossy across more than 100 services records evidence for the worst 100 only.
- **NULL staged tally columns mean staging never ran. Empty arrays mean the rewrite proved the repin lossless.** `finish_cutover` tells the two apart. On NULL it still completes the job, invents no evidence and emits a `repin_evidence_unstaged` warning. The caller is past the Cutover marker, the corpus is already the new generation, and the shadow that counted the losses is gone. The engine stages before it writes the marker, so this case is a fallback and not a normal path. It is the one exception to ruling 1.
- **The call that completes the job clears the repinned field's old conflict evidence and its `field_degraded_ack` row, under the same gate as the new evidence.** The old evidence describes a pin that no longer exists. The analyzer's degraded gate is span-based, so evidence left in place would keep the field degraded after the operator applied the remedy. An ack suppresses a verdict about a pin, and that pin is gone too. A replay of an already-succeeded job gets no row back from the completing `UPDATE`, so it deletes neither the evidence this flip recorded for the new pin nor an ack that an operator wrote after the flip. Other fields are untouched.
- **The `field_degraded_ack_cleared` audit event is one line per observed clear, not one per clear.** `CutoverOutcome::cleared_ack` is true only on the call that completed the job, so a boot replay of a finished cutover never repeats the line. The engine and boot recovery emit the line after `finish_cutover` commits. A crash between the commit and the emit loses the line, and nothing is left to replay it from. Closing that gap needs a durable outbox, and ADR-0019 ruling 12 rejects a general receipts table. The ack is gone either way, so the residual is accepted: the audit trail can be one line short.
