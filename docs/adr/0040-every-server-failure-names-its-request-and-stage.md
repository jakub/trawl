# Every server failure names its request and its stage; unmetered failures persist under a fixed cap

status: accepted (2026-09-22) — prep ruling record for #235; amended (2026-11-08): one public message for the wire and every record; amended (2026-11-09): every retained event logs foreign error text at DEBUG only, see the Amendments

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

## Amendment: a request-limit refusal is a pressure cause, 2026-10-08

ADR-0054 adds a 503 that trawld makes at the edge, before any key is counted, when the server-wide request count, the probe allowance or the control allowance is full. It records `error_class=service_unavailable` with the new closed `cause_kind=request_limit_reached`, in the same way that `hot_buffer_full` uses `service_unavailable`. The new cause joins the pressure set, so the event logs at WARN. Its stage is `pre_admission`, it is unmetered, and it shares the cap of 60 unmetered events a minute. A flood of refusals therefore writes at most as many rows as any other unmetered failure. The refusal counter in `/metrics` gives the exact rate.

## Amendment (2026-11-08): a server error has one public message, and the wire and every record carry it

The untangle prep of 2026-11-08 (defect D8) found that the Decision above
keeps error text out of failure events but says nothing about the text a
client reads or a record stores. Two redaction policies grew in that gap.
`rendered()` builds the wire message, and `safe_message()` builds the
stored one. `safe_message()` ends in `other => other.to_string()`
(`crates/trawl-server/src/error.rs:811`), so a variant nobody listed
stores its `Display` text. `EngineError::Io` is one: the wire says
`query execution failed`, and the tracker stores the `io::Error` text. The
same error also reads differently in the two places: a `BadRequest` sends
its text on the wire and stores `bad request`.

**One public message per error, decided per variant.** One exhaustive
disclosure projection covers `ServerError` and the `EngineError` and
`StoreError` variants it wraps. It has no wildcard arm at any level, and a
nested `Engine(_) => to_string()` counts as a wildcard. A new variant does
not compile until its author decides what it discloses. The projection
yields the public message. `rendered()` sends it as the envelope `message`.
Every record of the failure stores the same string: the tracker's recent
entries, report run rows, repin job rows (`repin/engine.rs:148`) and the
capture tracker outcome. Structured details that come from the request,
such as parse spans, emit hints and the `conflict` member, stay on the
wire only.

**Provenance decides disclosure, not the characters in the text.** A public
message may hold text that trawl wrote, values the request carried, and
permitted domain facts: configured limits, counts, sequence numbers, run
ids, window bounds, and object names relative to the data root. It never
holds `Display` text from DuckDB, `io::Error` or the store driver, an
absolute host path, generated SQL, or a panic payload. A variant that
carries internals gets the fixed sentence `rendered()` already sends for
it. A failed result persistence in the scheduler stores a fixed summary.

**Free-text variants keep their text in both places, and their
constructors are audited.** `BadRequest`, `NotFound`, `Forbidden`,
`Unauthorized`, `Ingest`, `Conflict` and `ServiceUnavailable` carry their
message on the wire and in records alike, so query history for a refused
request now says what was refused. No constructor of these variants
interpolates an error's `Display`. The known offenders are pin gc's:
`catalog/gc.rs:761` puts an `io::Error` into a 409, and the walk refusal
(`gc.rs:322-330`, mapped to `ServerError::Conflict` at `gc.rs:568`) lists
absolute paths with `io::Error` text (`gc.rs:313`, `:1005`, `:1097`). They
name the `io::ErrorKind` and paths relative to the data root instead, and
they keep their remedies. This replaces the comment at `gc.rs:318-321`,
"Data-root paths are operator-visible by design", because the absolute
root adds nothing the operator needs to find the file.

**The pre-start capacity refusal is a typed variant.** Today it is
`ServiceUnavailable(CAPACITY_NOT_STARTED)`, detected by comparing strings
(`error.rs:795`, `:933`). Every producer builds the typed variant instead:
`pool.rs:734`, `handlers.rs:229` and `results/service.rs:1086`, `:1094`
and `:1134`. Its status, code, sentence, `cause_kind`, timing outcome and
headers do not change.

**Default-level fault events carry class and cause only.** The Decision
above says error display text never enters the event. `log_fault`
(`error.rs:1571-1598`) still writes `error = %self` at ERROR for window
and internal faults, and `error = %source` for store faults. Those events
keep `error_class` and `cause_kind` at their level, and the `Display` text
moves to DEBUG. The scheduler and pin gc warn events that this change
touches follow the same rule.

**Unchanged.** `error_class`, `cause_kind`, every HTTP status and every
error code stay as they are. A generic 409 still sends `bad_request`. Rows
written before the change keep whatever text they hold: trawl is
forward-only, and this rule covers new writes.

**Why.** The audience is wide. Every key with `query` reads the recent
entries in `GET /api/v1/queries`, DSL included (`tracker.rs`
`recent_for`). Report run rows are readable through the runs API. A rule
that is opt-in at each call site fails open on the next variant, as `Io`
already did.

### Considered, not adopted

- **Filter the text for slashes or secret-looking words.** Rejected: run
  ids, window bounds and relative names are permitted, so only a decision
  per variant and per constructor separates them from internals.
- **A dedicated `ErrorCode::Conflict` for every 409.** Not adopted here.
  It is a wire change and does not close any leak. The specialized 409
  codes also drive recovery: `crates/trawl-web-ui/src/investigation_live.rs`
  branches on `sequence_conflict` and `investigation_closed`, and the MCP
  investigation tool gives separate instructions per code.
- **Make `Display` itself the public message**, with internals behind an
  explicit detail accessor. Not adopted here. It touches every log site
  and is larger than the fix.
- **Let the owner of a run read its raw error.** Rejected: an exception
  for the owner restores the disclosure this rule forbids.
- **Rewrite stored rows.** Rejected: trawl is forward-only.

### Premise checks at `b71f9e6e`

- `error.rs:767-811`: `safe_message()` ends `other => other.to_string()`.
- `trawl-engine/src/error.rs:144`: `#[error("I/O error: {0}")]`. The wire
  arm for the same variant sends a fixed sentence.
- `error.rs:1359-1362`: `ServerError::Conflict` renders as 409 with
  `ErrorCode::BadRequest` and the message as written.
- `error.rs:1922-1928`: `error_class_never_carries_user_content` pins a
  `/var/lib/trawl/data/...` path inside `Conflict.safe_message()`. The fix
  inverts that assertion.
- `scheduler.rs:475`, `:477`: run rows store `format!("{e}")`.
  `scheduler.rs:661`: `recover_ambiguous_finish` stores
  `format!("result persistence failed: {err}")` from a `StoreError`.
- `handlers.rs:486`, `handlers.rs:3507`, `results/service.rs:963` and
  `repin/engine.rs:148` are the current `safe_message()` consumers.

## Amendment (2026-11-09): every retained event logs foreign error text at DEBUG only

The 2026-11-08 amendment moved error `Display` text to DEBUG for
`log_fault` and for the scheduler and pin gc events that change touched.
The prep of 2026-11-09 found more retained events that still log such
text at WARN or ERROR: 16 that log a store, database driver, auth or
path-bearing error, and about 80 that log an `io::Error`. The human ruled
on 2026-11-09 that the rule covers every retained event, in two pull
requests: #412 for the 16, and #413 for the I/O errors.

**The rule.** An event is retained when its level is INFO or above and
its target passes `telemetry::is_persisted_target`, so the default filter
can put it in the `service=trawld` corpus. A retained event never carries
error text that trawl did not write, in any field or in its message. That
text is the `Display` or `Debug` output of a store, database driver,
auth, engine, DuckDB or I/O error, a string built from one, and any
absolute host path, generated SQL or panic payload such a string holds.
The event keeps its level and carries the closed `error_class` and
`cause_kind` instead, or `cause_kind` alone where the site has no class.
The text moves to a DEBUG event with the same `event_type` and target.
The rule follows what the event carries, not what it is called: a warning
that reports an error follows it as a fault event does.

**Provenance decides, as it does for public messages.** Fixed text that
trawl wrote may stay. The manual run's cursor event
(`store/schedule.rs:2363`) logs a `PlanError`, whose two variants each
have a fixed message, so it keeps that text. An error with no typed
cause, such as the `String` the cutover swap returns, logs
`error_class=internal` and `cause_kind=unknown`. Context fields that are
not error text, such as ids, counts and attempts, are outside this rule.
A cause comes only from a typed source, never from matching an error's
text.

**Unchanged.** An operator who enables DEBUG still persists the text,
the posture the Decision above accepts. Records written before the change
keep what they hold: neither the upgrade nor a revert rewrites them.

**Why.** Every key with `query` can read `service=trawld`. A Postgres
diagnostic can quote row values and the query text, and an `io::Error`
built into a message can carry an absolute path. A rule limited to events
named as faults, or to a list of sites, fails open on the next event:
the sites from #319 were found one by one after the first sweep.

### Considered, not adopted

- **A lint that bans `error = %` outside `debug!`.** Rejected: a text
  pattern cannot decide provenance. It would refuse the fixed `PlanError`
  text and miss error text interpolated into a message.
- **One logging helper per site.** Rejected: it refactors about 100 call
  sites to fix what each event carries.
- **A typed error for the cutover swap.** Not adopted here: the boot
  replay runs the swap again and stops boot with the same text on stderr
  (`repin/recover.rs:131`), and the type change is larger than the fix.
