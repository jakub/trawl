# Every server failure names its request and its stage; unmetered failures persist under a fixed cap

status: accepted (2026-09-22) — prep ruling record for #235

On 2026-09-21 a plain query on the homelab returned 500 once. The only
trace was `http_failure` with a status and a latency: no request id, no
route, no cause. The event was emitted outside the request span, so it
inherited nothing. No `query_failed`, `internal_error` or panic line
accompanied it, so nobody can tell which layer produced the status. The
same day, a `query_failed` event also lost its request id. Span
inheritance is not a reliable carrier of request context.

## Decision

**One failure event per server 5xx.** Every 5xx that trawld produces,
`/api/v1/health` included, emits exactly one `http_failure` event. The
event carries its context as explicit fields, not inherited ones: the
request id (the same value as the `X-Request-Id` header), the method,
the matched route template (a fixed sentinel when no route matched,
never the raw path), the status, the latency, the stage, the cause, and
the `query_id` when the request allocated one. ERROR for 500 and other
5xx, WARN for 503 and 504, which are expected pressure outcomes under
ADR-0024.

**Stage** says how far the request got: before admission, admitted
(metered), in the handler, returned from the handler with a typed
error, or panicked. A 5xx whose producer recorded nothing is still
logged, and its stage says so. That record points at a producer that
reports nothing about itself.

**Cause** is the existing closed `error_class` plus a closed `cause_kind`
built only from typed sources: an I/O error kind, an engine error
variant, a database driver error kind. Error display text never enters
the event. It can hold generated SQL, event values, the user's DSL, and
file paths. That text stays at DEBUG and in the opt-in query log.

**Context survives the failure.** The failure observer creates a
request-scoped record at entry. Inner layers write to it as the request
passes them: the rate limiter records that the request was metered, the
query handler records the `query_id`. That record, not the response, is
the source. A panic unwinds past every layer that would have stamped a
response, and the auth-error normalizer replaces responses, but neither
can erase what was already recorded.

**Panics.** A panic produces a diagnostic with its source location and
no payload. The diagnostic is never persisted. The failure event for
the caught request records the panic stage, and that event persists.

**Persistence.** A metered failure persists, because the per-key rate
limiter already bounds how many a client can cause. An unmetered 5xx is
a server fault seen before any key was metered, for example the auth
backend down. It persists under one process-wide cap of 60 events per
minute, fixed in code with no operator setting. It also carries the peer
address, the only lead when no key is known. Events past the cap go to
stdout only and are counted in the `telemetry_dropped` drop accounting.
Unmetered 401, 403 and TLS rejections stay stdout-only as before
(`UNMETERED_TARGETS`). Those are the caller's fault, and storing them
would give a stranger with a bad key a way to write rows.

## Considered options

- **Never persist unmetered 5xx** (the rule `UNMETERED_TARGETS` applies
  to rejections). Rejected by the operator: a server fault before
  admission, such as an auth backend outage, is exactly the incident an
  operator searches for afterwards. The cap bounds the amplifier the
  rule exists to close, at 60 rows a minute.
- **Per-peer caps.** Rejected: memory grows with distinct peers and needs
  its own eviction. A global cap is fixed memory.
- **Read context off the span in the failure callback.** Rejected: that
  is the mechanism that failed in the incident.
- **Scrub raw path and user-agent from the request span.** Not decided
  here. The failure event does not inherit span fields, so it adds no
  exposure. Every other request event is unchanged.
- **A per-boot identifier.** Rejected: the request id is a ULID and
  travels with the `query_id` in the same event.

## Consequences

When a 500 cannot be reproduced, the record says so plainly. The
traceability above is the deliverable, and the next occurrence names
its route, stage and cause class. In monitor mode stdout is suppressed
by design, so unmetered failures past the cap are visible only in the
drop counts there.
