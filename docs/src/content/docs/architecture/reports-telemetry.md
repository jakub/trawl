---
title: Reports and telemetry
description: Understand why scheduled runs tile and what the daemon observes about itself.
---

Why can you add up a week of hourly reports and trust the total? A tiling schedule hands each run the interval that starts where the last successful run stopped. The runs never overlap and never leave a gap, so summing them counts every event once. The [schedules API](/reference/api/#schedules) holds the field rules.

## Scheduled reports

A saved query plus a schedule produces stored report runs. PostgreSQL holds the metadata, and Parquet report files live under `data/scheduled/`. A stored run is the original result, so running its DSL again can give a different answer.

### Why windows tile

`window = "since_last"` keeps a watermark, `covered_through`, at the end of the newest window a successful run covered. Each run covers from there to its own fire time. The watermark advances only on success, so a failed run leaves it standing and the next success covers both intervals in one window. That is what makes the runs add up. A fixed trailing window such as `window = "2h"` re-measures from every fire and heals nothing.

The scheduler fires on a planned cursor, not on time elapsed since the last run. A slow run, a late poll, and a restart leave the cursor where it was. When several boundaries have passed, the tick takes the latest one at or before now, so missed fires coalesce into one run.

Coalescing has a limit, because a week of downtime would otherwise produce one week-wide query. A gap wider than `max_catchup_intervals` clamps the window start forward, and the run records `window_truncated: true` and increments `trawl_scheduler_window_truncated_total`. Alert on that counter. A truncated run leaves part of the catch-up outside any report, though those events stay in the corpus.

### Why lag delays coverage

Reports measure `_time`, the sender's own timestamp. That is what makes a report agree with an interactive query over the same interval, and it lets the partition layout prune the read. It also means an event can arrive after its window has closed.

`lag` is the allowance for that delay, and it shifts both bounds back rather than widening the window. With `lag = "5m"` the 03:00 run covers up to 02:55, so an event stamped 02:54 that arrived at 02:58 still lands in its window. Coverage arrives later and stays complete.

The [from saved stage](/reference/dsl/#from-saved) queries materialized runs instead of executing the saved query again. It clears the catalog pin scope, so a stored column that shares a name with an event field does not inherit its type.

## Internal telemetry

With internal telemetry enabled, a tracing layer turns daemon events into ordinary `service=trawld` records through the same [event contract](/reference/events/) as any producer. Payload fields cannot rename its service.

A flush writes its batch through the ingest WAL on Tokio's blocking pool, and only a successful write reaches the hot buffer and SSE. A failed write keeps its batch in a FIFO retry queue that drains oldest first, in coalesced writes of at most 4 MiB each. A cancelled write has already consumed its batch, so Trawl counts those events as lost.

`telemetry_buffer_max_bytes` bounds one memory charge across the active buffer, the queued batches, and any in-flight write. At capacity Trawl sheds the oldest queued batches first, then new events. `trawl_telemetry_events_dropped_total` separates `buffer_cap`, `preinit_cap`, `write_crashed`, and `unmetered_cap`. `trawl_telemetry_bytes_dropped_total` covers only the first three, because an `unmetered_cap` drop counts events and no bytes. Missing internal events therefore never prove the daemon was idle.

### What telemetry does not cover

- Internal telemetry covers `trawld`. The proxy and the CLIs log to stdout and stderr for your collector.
- A coalescing poll observes Fleet key changes, so a key created and deleted between polls is invisible.
- Query lifecycle events carry identity, query ID, lengths, outcome, row counts, and timing, but not the raw DSL. Read that through authenticated history or an enabled query debug log.
- Metrics are Prometheus only, with no OTLP log or trace export.
- The targets `fleet_auth`, `auth.backend`, `preauth.transport`, and `trawl_server::policy::unmetered` never reach the WAL, whatever `RUST_LOG` says. Authentication runs before a per-key bucket can meter a request, so persisting those failures would let an unmetered client grow the corpus. `trawl_auth_failures_total{reason}` counts them instead.
- An unmetered server 5xx is the server's fault, not the caller's, so its `http_failure` event on `trawl_server::transport::failure::unmetered` does reach the WAL, under one process-wide cap of 60 events per minute. Only that exact target persists. A target beneath it never reaches the WAL. Events past the cap stay on stdout and count as `unmetered_cap` drops. [Trace a server failure](/operate/health/#trace-a-server-failure) lists the event's fields.
- The panic diagnostic on `trawl_server::panic` names the source location and never the panic message. It goes to stdout and `log_file`, never to the WAL. When the terminal monitor owns stdout and no `log_file` is written, or `RUST_LOG` filters the target out, trawld also writes the location line to stderr. The failure event of the caught request is the stored record.
