# Issue #52 evidence — event schema cutover (ADR-0009 slice 1)

Captured against a live smoke stack: `trawld` (fresh epoch-2 data root)
+ `trawl-web` (release binary, embedded fresh SPA) + the `trawl` CLI/TUI,
with a vector-shaped ingest batch (wire aliases `timestamp`/`level`,
one `severity_text` sender, one env=lab event, one broken-clock event,
and one `env=nope` event that was per-event rejected).

- `search.level-gte-warn.png` — web UI results for `level>=warn last=1h`:
  the new well-known column order (`_time`, `env`, `service`, `host`,
  `severity`, `severity_text`, `message`), numeric severity colored by
  OTel band (17 = ERROR red, 13 = WARN amber), `severity_text` kept
  verbatim (`warn`/`warning`/`error` spellings all land at-or-above the
  WARN band), and env/severity facets. The `trawld` rows are the
  server's own telemetry flowing through the same envelope.
- `cli-transcript.txt` — CLI table output: the band query, `env=lab`
  pruning, `_repairs` codes (`time.from_ingest,env.defaulted`) on the
  broken-clock event, and a bare-word search that matches through
  `message`/`_raw`.
- `tui-transcript.txt` — TUI (tmux capture) running
  `level>=warn last=1h | stats count() by env, service, host, severity,
  severity_text`.

Server-side checks during the same session:

- compaction wrote `data/{env}/{date}/{HH}/{service}.parquet` for both
  `prod` and `lab`; no `data/nope/` was ever created for the rejected env
- `/metrics` exposed `trawl_ingest_repairs_total{code="time.from_ingest",
  service="cron"}` and `{code="env.defaulted",service="cron"}` plus
  `trawl_ingest_events_rejected_total{reason="env_not_allowed"}`
- boot logged the `epoch_gate` outcome and `data/EPOCH` contains `2`
