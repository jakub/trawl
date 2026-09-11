---
title: Reports and telemetry
description: Scheduled coverage, stored results, and the limits of daemon self-observation.
---

## Scheduled reports

A saved query plus a schedule produces stored report runs. PostgreSQL holds metadata and the result representation; Parquet report files live under `data/scheduled/`. Window policy determines which event-time interval each run queries. A stored run is the original result; rerunning its DSL later can produce a different answer if the corpus changed.

### Window modes

A schedule has one of three modes.

`window = "since_last"` tiles. Each run covers `[the previous run's window
end, this fire - lag)`, so consecutive runs cover consecutive intervals
with no gap and no overlap. This is the mode for anything you intend to
add up across runs.

`window = "<duration>"` is a fixed trailing span, re-measured from every
fire: a run at 03:00 with `window = "2h"` covers `[01:00, 03:00)`
regardless of what the last run did. It keeps no watermark, so a missed
fire is simply a missing run and nothing heals it. A span that differs
from the interval is allowed and sometimes wanted: `interval = "1h",
window = "2h"` gives every report an hour of overlap with its predecessor,
which is how you write a rolling two-hour view; `interval = "1h", window =
"30m"` samples half of each hour on purpose. Neither is a mistake trawl
should correct, so it does not.

No `window` selects query mode: the saved
DSL executes verbatim, time clause and all, and the run records no bounds.

`lag` shifts both bounds back on either mode. With `lag = "5m"` the 03:00
run covers up to 02:55, not 03:00, so an event stamped 02:54 that only
reached the corpus at 02:58 is still inside the window that counts it. The
axis is `_time`, the sender's own timestamp, which is what makes a report
agree with an interactive query over the same interval and lets the
partition layout prune the read. `lag` is the allowance for that choice.

### Planned boundaries

The scheduler fires on a planned cursor, `next_fire_at`, not on elapsed
time since the last run started. A run that takes 90 seconds, a poll that
lands 8 seconds late, a restart: none of them move the cursor. Each tick
samples one instant, and every schedule in that tick is judged against it.

When several boundaries have passed (the daemon was down, or the previous
run was still going), the tick takes the *latest* boundary at or before
now and moves the cursor one interval past it. Missed fires never replay
as N runs.

### The watermark

A `since_last` schedule keeps `covered_through`, the end of the newest
window a successful run covered. It advances only on success, only for a
run claimed in `since_last` mode, and only when that run's end is later
than what is already covered, so a slow run finishing after a later one
cannot rewind coverage. A failed or timed-out run leaves it alone, and the
next success covers its own interval and the failed one in a single
window.

It is seeded when the schedule is created, at `next_fire_at - interval -
lag`, which is the origin of what the schedule owes. Seeding matters for
exactly one case: if the *first* run fails, an unset watermark would send
the next run back to "one interval ending at my own fire" and the failed
interval would be dropped with nothing recording the loss.

### Catch-up and the clamp

Without a cap, coalescing would be unbounded: a week of downtime would
hand the next run a week-wide window and one enormous query. So a gap
wider than `max_catchup_intervals` intervals (config, default 24) clamps
the window start forward to `window_end - max_catchup_intervals *
interval`. The run then carries `window_truncated: true` and increments
`trawl_scheduler_window_truncated_total`.

Alert on that counter. A truncated run explicitly leaves part of the requested catch-up window
uncovered. Its row records the missing coverage.

### A worked example

Hourly schedule, `window = "since_last"`, `lag = "5m"`, created at
2026-03-14T02:00:00Z. Creation seeds `covered_through` to 00:55 and sets
`next_fire_at` to 02:00.

| fire | window | outcome | `covered_through` after |
|------|--------|---------|-------------------------|
| 03-14 02:00 | `[00:55, 01:55)` | success | 01:55 |
| 03-14 03:00 | `[01:55, 02:55)` | success | 02:55 |
| 03-14 04:00 | `[02:55, 03:55)` | error | 02:55, unchanged |
| 03-14 05:00 | `[02:55, 04:55)` | success | 04:55 |
| 03-16 09:00 | `[03-15 08:55, 03-16 08:55)`, truncated | success | 03-16 08:55 |

The 05:00 run is the healing one: two intervals in a single window,
because 04:00 failed and left the watermark standing.

Then trawld is down from 03-14 05:30 until 03-16 09:03. The tick at 09:03
takes 09:00 as its boundary (the fires at 06:00, 07:00, 08:00 on the 14th
and every fire on the 15th are folded in, not replayed), and asks for
`[03-14 04:55, 03-16 08:55)`. That is 52 hours against a bound of 24, so
the start is clamped to 03-15 08:55 and the run is flagged. The 28 hours
from 03-14 04:55 to 03-15 08:55 are not in any report and will not be.
They are still in the corpus: query them interactively with
`earliest=`/`latest=`.

The 03:00 run's stored `query` is the saved DSL with
`earliest="2026-03-14T01:55:00.000000Z" latest="2026-03-14T02:55:00.000000Z" `
in front of it. Reusing those bounds queries the same interval. Later retention, repin, or
newly arrived events can change a fresh answer; read the stored run for the
original materialized result.

### Editing a schedule

Changing the interval or the window re-anchors `next_fire_at` to now: the
cadence you asked for starts from the edit. Changing `max_runs` or
flipping `enabled` leaves the cursor alone, so repeated edits cannot keep
a schedule permanently un-due.

An existing `covered_through` is never cleared by an edit, to the schedule
or to the saved DSL. The per-run resolved text is the audit trail, and a
watermark reset would silently re-report or skip coverage. An *absent* one
is seeded at the new origin when the edit re-anchors a `since_last`
schedule, for the reason seeding exists at creation. If an edit leaves the
watermark at or past the window a fire would cover, the tick advances the
cursor and runs nothing rather than asking the same question every poll.

Deleting the schedule (or the saved query above it) takes the watermark
and every run row with it, and the result files those rows pointed at are
unlinked after the transaction commits. Recreating the schedule starts a
fresh origin, not the old coverage.

## Reading stored results

The [from saved stage](/reference/dsl/#from-saved) queries materialized runs. `latest` selects the newest success, `run=N` selects a caller-owned successful run ID, and `all` unions successful runs with Parquet results. Empty or blob-only successes have distinct behavior documented there. This does not execute the saved query again.

The report source clears the live catalog pin scope. Stored result columns can be aggregates or projections and do not inherit a current event field's pin merely because the name matches.

## Internal telemetry

When internal telemetry is enabled, a tracing layer turns daemon events into ordinary `service=trawld` records. The `trawld` producer profile passes through the same [event contract](/reference/events/) as other producers. The profile asserts its identity; payload fields do not rename the daemon's service or environment.

### Durability before visibility

A flush writes its batch through the ingest WAL on Tokio's blocking pool. Only successful WAL writes publish to the hot buffer and SSE. Fsync does not run on an async executor worker.

An ordinary failed write retains the batch in a FIFO retry queue. After storage recovers, the queue drains oldest-first in coalesced writes of at most 4 MiB. Coalescing begins only after a successful write; repeated failures retain the original shedding units. A panicked or cancelled blocking write has consumed its batch, so the system accounts for unrecoverable events and bytes as `write_crashed` rather than pretending to retry them.

### Bounded memory and shutdown

`telemetry_buffer_max_bytes` bounds one estimated memory charge across the active buffer, queued batches, and an in-flight write. The budget applies when events arrive, including while a write is blocked. At capacity it sheds oldest queued batches first and then incoming events. There is also a bounded pre-initialization buffer before the WAL writer is installed.

`trawl_telemetry_events_dropped_total` and `trawl_telemetry_bytes_dropped_total` distinguish `buffer_cap`, `preinit_cap`, and `write_crashed`. WAL failures and buffer-depth gauges remain scrapeable while internal persistence is down. A searchable loss event after recovery reports dropped data. These counters measure loss; missing internal events are not evidence that the daemon was idle.

Shutdown races the periodic flush against the stop signal, limits final draining to five seconds, and limits runtime shutdown to ten seconds. A stuck filesystem call can outlive the request to stop its task. The bounded shutdown contract does not claim that an interrupt cancels a kernel write.

### Observation boundaries

- Internal telemetry covers `trawld`. The proxy and CLIs log to stdout or stderr for the deployment's collector.
- Fleet key changes are observed by a coalescing poll, default 30 seconds. A create-and-delete entirely between polls is invisible. This is not a transactional audit ledger.
- Query lifecycle events carry identity, query ID, lengths, outcome, row counts, and timing. Default persisted events omit raw DSL and failure messages that could embed it. Raw text is available through authenticated history, an explicitly enabled query debug log, or appropriate debug output.
- Metrics are Prometheus-based. There is no OTLP log or trace export.

### Unmetered events do not become corpus writes

Targets `fleet_auth`, `auth.backend`, `preauth.transport`, and `trawl_server::policy::unmetered` are excluded from WAL persistence regardless of `RUST_LOG`. Authentication, a grantless rejection, and a TLS handshake failure occur before a per-key bucket can meter the request. Persisting each failure would let an unmetered client grow the corpus with connection attempts.

The events remain on stdout. `trawl_auth_failures_total{reason}` exposes a fixed vocabulary of rejection counts without client-chosen key names or paths in metric labels. Persistence filtering is narrower than stdout filtering, and operator log directives cannot override this exclusion.

## Source and decision owners

Scheduling lives in `trawl-server/src/scheduler.rs` and the app-state store. Telemetry lives in `src/telemetry.rs`, with content-free failure classification in `src/error.rs`. See [ADR-0018](/contribute/decisions/#adr-0018) for report-window policy and [configuration](/reference/configuration/) for defaults and logging options.
