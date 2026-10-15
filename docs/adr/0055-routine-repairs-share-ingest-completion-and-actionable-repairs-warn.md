# Routine repairs share ingest completion; actionable repairs warn

status: accepted (2026-10-14), prep record for #302

trawld logged one WARN, `ingest_repairs`, for every HTTP ingest request in which any event was repaired. Most repairs are the contract working as designed. `env.defaulted` is the zero-config path of ADR-0009, and `host.from_peer` fills a host from an honest peer. A sender that relied on `default_env` therefore produced a WARN on every batch for as long as it ran. On the homelab (v0.9.3, 2026-10-11) one Vector DaemonSet produced about 18 WARN lines a minute, with no rejections. trawld persists its own tracing as `service=trawld` events, so each WARN was also stored.

The same WARN was the only record that tied a repair to an ingest key. `trawl_ingest_repairs_total{code, service}` carries no key label on purpose: `/metrics` is unauthenticated, principal-derived labels are forbidden there, and the recorder never evicts a series. Ingest preview (ADR-0049) logs nothing and cannot attribute live traffic either.

## Decision

**Every repair code has a class: routine or actionable.** A routine repair fills an absence or adapts a name without loss, as the contract intends, and needs no action. An actionable repair means someone should inspect the sender: a value the sender sent was dropped, cut, replaced, or kept but flagged as implausible, a field conflicted with an asserted value, or a sender claimed one of trawl's stamps. Actionable does not mean the sender is at fault or hostile. The class is a function of the code alone, with no context, so `_repairs`, the metric and the logs all read the same class from the same code. The class lives in one exhaustive match on `RepairCode` with no wildcard arm, so a new code cannot compile without one.

| class | codes |
|---|---|
| routine | `env.defaulted`, `host.from_peer`, `host.omitted`, `time.from_ingest`, `field.name_case_folded`, `field.reserved_prefix` |
| actionable | `time.unparseable`, `time.out_of_range`, `field.truncated`, `field.name_too_long`, `field.name_empty`, `field.name_case_collision`, `field.reserved_prefix_collision`, `field.stamp_claimed`, `field.producer_asserted`, `service.from_profile` |

Three codes are new, because one old code covered both a routine and an actionable case. ADR-0013's amendment of 2026-10-14 defines them:

- `time.unparseable` splits from `time.from_ingest`. The old code meant either "no timestamp was sent" or "a timestamp was sent and did not parse". The canonicalizer knows which, but it discards that lineage after the event is built, so only a separate code lets `_repairs` and the metric tell the two apart.
- `field.name_empty` splits from `field.reserved_prefix`. A name made only of underscores has nothing left after the strip, so the field is dropped.
- `field.stamp_claimed` splits from `field.reserved_prefix`. It marks a sender that sent one of trawl's stamp names. The human ruled on 2026-10-14 that the attempt warns even though nothing is lost, because no ordinary sender sends these names and the WARN names the key that did.

**Two log lines on the HTTP door.** `finalize_ingest` writes them:

- `ingest_complete`, at INFO, for every finalized request, as before. It gains the key's stable `key_id` beside the existing `user` name, the total repair count, separate routine and actionable counts, an exact count for every code, and per-service detail. A clean request reports zero counts. A request with only routine repairs writes no other line. At the default filter, this line is the per-key record of routine repairs.
- `ingest_repairs`, at WARN, only when at least one actionable repair landed. It carries `user`, `key_id`, the actionable count with an exact count for every actionable code, actionable per-service detail, and the routine count for context. Its message says the repairs need inspection. It does not say data was lost, because `time.out_of_range` keeps the value it flags.

Both lines take `request_id` and the peer from the request span. Counts are occurrences of one code on one event, not counts of repaired events or changed fields.

**Bounded detail, exact totals.** Codes form a closed set, so each line always carries the exact count for every code. Services are an open set. Per-service detail lists at most 8 `(code, service)` pairs. It sorts by count, highest first, then by code, then by service. It also states how many pairs it left out and how many occurrences those pairs held. A large routine count cannot hide an actionable code, because the totals are always complete.

**No log limiter, no per-key log state.** One actionable WARN per request is the same cadence as `ingest_rejections`. Each key's ingest rate limit bounds it while that limit is on. `ingest_rpm = 0` or a role override can turn the limit off or raise it, so the bound depends on configuration. A shared budget would let one noisy key suppress another key's WARN. Per-key windows would need eviction and restart rules and a flush at shutdown, and a sender that keeps losing data should keep warning.

**Repair counts cover acknowledged groups only.** On the HTTP door, the repair tallies and the `severity_unmapped` tallies are held per `(env, service)` WAL group until that group's write returns. Only groups whose write succeeded add to the metrics and the two log lines. Before this decision, a failed group was removed from publication but its repairs were still counted and logged. A failed write is unacknowledged. It is not proof that no bytes reached the disk, so the docs say "unacknowledged", not "never landed". The syslog and trawld doors still count when they canonicalize. This decision does not change their timing.

**No logging below the handler.** The canonicalizer, the producer helpers and the metric-label helpers stay free of tracing (`canonicalize_no_tracing.rs`). The syslog and trawld doors write no per-event repair line. Their actionable codes appear in `_repairs` and in the metric only.

## Considered options

- **Demote the WARN to INFO.** This fixes the level but keeps one extra persisted line per request. Rejected.
- **Demote the WARN to DEBUG.** This removes the only per-key attribution at the default filter. Rejected.
- **Reuse ADR-0040's 60-a-minute cap.** That cap controls persistence only, so stdout still prints every line. It applies to one exact target, and it is one budget for the whole process, so repairs would crowd out the 5xx records it exists to keep. 60 a minute is also above the 18 a minute observed. Rejected.
- **Add a key label to `trawl_ingest_repairs_total`.** This breaks the rule that `/metrics` carries no principal-derived label. Rejected.
- **Classify the time and prefix cases from lineage at log time, and keep the old codes.** This leaves `_repairs` and the metric unable to separate the cases, because lineage is discarded after canonicalization. Rejected in favour of the three new codes.

## Consequences

- The homelab's routine-only traffic writes no WARN. Actionable traffic still warns once per request, per key.
- A collector that forwards trawld's stdout into the same installation still ships one `ingest_complete` per request. This decision removes the extra repair WARN from that path. It does not end the feedback. Operators must not drop all of trawld's stdout to stop it, because some diagnostics reach stdout only (`telemetry.rs`, `UNMETERED_TARGETS`).
- `host.from_peer` stays routine. Behind an unconfigured NAT or a load balancer that rewrites the source address (SNAT), the peer address is the translator's, and the host looks right but is wrong. The `trusted_relays` docs name that case.
- Stored events from before the split keep `time.from_ingest` for both causes. Nothing migrates them.
