// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The event canonicalizer: one place where an arriving JSON object either
//! becomes a declared-envelope event (ADR-0009) or is rejected with a typed
//! reason.
//!
//! Principle: **repair when the server has an honest answer; reject when it
//! would guess.** Every repair is recorded as a [`RepairCode`] in the
//! event's `_repairs` column and counted per `(code, service)`.
//!
//! Three producers feed events through here as profiles (ADR-0013): the
//! HTTP ingest handler, the syslog listener, and internal telemetry
//! (`service:trawld`). A profile asserts the identity it can prove and
//! contributes fixed derivation sources ([`crate::ingest::producer`]); it
//! bypasses no gate here. Producers contribute repair codes, but this door
//! is the only place `_repairs` is assembled.
//!
//! This module never calls `tracing`. Telemetry's own events pass through
//! it on their way to the WAL, so a log line emitted from inside would be
//! an ingestion loop; pinned by `tests/canonicalize_no_tracing.rs`.

use std::fmt;

use serde_json::{Map, Value, json};

use crate::ingest::pipeline;
use crate::ingest::producer::{self, Asserted, Derivation, Producer};

/// What the server changed about an accepted event — a closed enum, same
/// principle as ADR-0006's "roles are data, permissions are code": codes
/// are code, so `_repairs` cannot become a junk drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairCode {
    /// `host` was missing; filled from the sender IP.
    HostFromPeer,
    /// `env` was missing; filled from `default_env`.
    EnvDefaulted,
    /// `_time` was missing or unparseable; used arrival time.
    TimeFromIngest,
    /// `_time` was implausible (>10y past / >1d future); kept, but flagged.
    TimeOutOfRange,
    /// a value exceeded the length cap (`_raw`).
    FieldTruncated,
    /// a field name arrived in trawl's `_` namespace and is not a slot the
    /// sender may propose (ADR-0013 §5). The leading underscore run was
    /// stripped and the value stored under the bare remainder
    /// (`_HOSTNAME` → `hostname`, `__name__` → `name__`); a name with no
    /// remainder at all (`_`, `___`) was dropped. Nothing is lost either
    /// way — `_raw` carries the original name and value.
    ReservedPrefix,
    /// stripping a reserved prefix produced a name the same event already
    /// carries bare, so the prefixed loser was dropped (the case-collision
    /// precedent). Its value stays findable in `_raw`.
    ReservedPrefixCollision,
    /// a field's name exceeded [`trawl_core::schema::MAX_FIELD_NAME_BYTES`];
    /// the field was dropped (its value stays findable in `_raw`).
    FieldNameTooLong,
    /// a field's name carried ASCII uppercase and was folded to lowercase —
    /// `DuckDB` identifiers are ASCII case-insensitive, so the lowercase
    /// form is the one spelling every downstream layer (catalog, parquet,
    /// hot snapshot) agrees on. The original spelling stays in `_raw`.
    FieldNameCaseFolded,
    /// two field names in one event differed only in ASCII case — one
    /// column as far as `DuckDB` is concerned — so the losing key was
    /// dropped (its value stays findable in `_raw`). The exact-lowercase
    /// spelling wins when present; otherwise the ASCII-lexicographically
    /// first variant does.
    FieldNameCaseCollision,
    /// a payload key named a slot the producer asserts (`env`, `service`,
    /// `host`, `message`) and carried a different value, so the assertion
    /// won (ADR-0013). Identity is protected by precedence, not by a
    /// namespace: telemetry's own fields are ordinary sender vocabulary,
    /// and the displaced value stays findable in `_raw`. An identical
    /// value is not a collision and earns no code, and neither is a JSON
    /// `null`, which is absence rather than a competing claim.
    ProducerAsserted,
    /// the producer had no honest `host` to assert, so the event was kept
    /// with `host` absent (ADR-0013). Reached by a hostname-less syslog
    /// frame behind a trusted relay and by a failed hostname lookup for
    /// trawld: absent but honest beats both the peer-fill lie and dropping
    /// the event. The HTTP door never gets here, since an HTTP sender can
    /// be rejected and resend.
    HostOmitted,
    /// the producer could not derive a usable `service` from the frame
    /// and fell back to its profile's configured default (a syslog
    /// APP-NAME that fails the service charset). The original stays
    /// findable in `_raw`.
    ServiceFromProfile,
}

impl RepairCode {
    /// The wire/metric label spelling of the code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostFromPeer => "host.from_peer",
            Self::EnvDefaulted => "env.defaulted",
            Self::TimeFromIngest => "time.from_ingest",
            Self::TimeOutOfRange => "time.out_of_range",
            Self::FieldTruncated => "field.truncated",
            Self::ReservedPrefix => "field.reserved_prefix",
            Self::ReservedPrefixCollision => "field.reserved_prefix_collision",
            Self::FieldNameTooLong => "field.name_too_long",
            Self::FieldNameCaseFolded => "field.name_case_folded",
            Self::FieldNameCaseCollision => "field.name_case_collision",
            Self::ProducerAsserted => "field.producer_asserted",
            Self::HostOmitted => "host.omitted",
            Self::ServiceFromProfile => "service.from_profile",
        }
    }
}

impl fmt::Display for RepairCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an event was rejected — used as a prometheus label value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    MissingService,
    /// `service` present but not a JSON string — type-specific, never
    /// conflated with [`Self::MissingService`].
    ServiceNotString,
    EmptyService,
    ServiceTooLong,
    InvalidChars,
    /// `env` present but not a string, or failing the env charset.
    InvalidEnv,
    /// `env` valid in shape but not in the configured allowlist —
    /// repairing it into `default_env` would misfile data in the wrong
    /// path root permanently.
    EnvNotAllowed,
    /// `host` missing and the peer is a configured trusted relay: filling
    /// from the peer would stamp the relay's address as the origin.
    HostMissingFromRelay,
    NotObject,
    InvalidJson,
    WalFailure,
    /// A valid event in an HTTP request the hot buffer refused for lack of
    /// free space (503 `hot_buffer_full`, ADR-0043). Nothing was written.
    HotBufferFull,
    /// A valid event in an HTTP request larger than the hot buffer admits
    /// for one request (413 `ingest_batch_too_large`, ADR-0043).
    IngestBatchTooLarge,
}

impl RejectReason {
    /// The prometheus label spelling of the reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingService => "missing_service",
            Self::ServiceNotString => "service_not_string",
            Self::EmptyService => "empty_service",
            Self::ServiceTooLong => "service_too_long",
            Self::InvalidChars => "invalid_chars",
            Self::InvalidEnv => "invalid_env",
            Self::EnvNotAllowed => "env_not_allowed",
            Self::HostMissingFromRelay => "host_missing_from_relay",
            Self::NotObject => "not_object",
            Self::InvalidJson => "invalid_json",
            Self::WalFailure => "wal_failure",
            Self::HotBufferFull => "hot_buffer_full",
            Self::IngestBatchTooLarge => "ingest_batch_too_large",
        }
    }

    /// Every reason, for exhaustive metric/summary iteration.
    pub const ALL: &'static [Self] = &[
        Self::MissingService,
        Self::ServiceNotString,
        Self::EmptyService,
        Self::ServiceTooLong,
        Self::InvalidChars,
        Self::InvalidEnv,
        Self::EnvNotAllowed,
        Self::HostMissingFromRelay,
        Self::NotObject,
        Self::InvalidJson,
        Self::WalFailure,
        Self::HotBufferFull,
        Self::IngestBatchTooLarge,
    ];
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-event context the canonicalizer needs: arrival instant, which door
/// the event came in at, the env allowlist, and the derivation policy.
/// Threaded through every call rather than read from a global, so the
/// config a canonicalization depends on is visible at the call site.
#[derive(Debug)]
pub struct EnvelopeContext<'a> {
    /// RFC 3339 UTC arrival time at microsecond precision — stamped as
    /// `_ingested` and substituted for a missing/unparseable `_time`.
    pub arrival: &'a str,
    /// The same arrival instant, for plausibility windows.
    pub arrival_instant: chrono::DateTime<chrono::Utc>,
    /// The effective env allowlist (never empty).
    pub envs: &'a [String],
    /// Fills a missing `env` (recorded as `env.defaulted`). Unreachable
    /// for a profile producer, which asserts a boot-validated env.
    pub default_env: &'a str,
    /// Which door this event arrived at, with whatever that door asserts
    /// (ADR-0013). Only the HTTP variant carries a peer identity, and it
    /// is evidence for a fill rather than an assertion.
    pub producer: Producer<'a>,
    /// The boot-resolved, per-profile `_severity`/`_time` source lists.
    /// Borrowed, because one resolved policy serves every event on every
    /// door for the life of the process.
    pub derivation: &'a Derivation,
}

/// A canonicalized event: the declared envelope plus the client's own
/// fields, ready for WAL serialization.
#[derive(Debug)]
pub struct Canonical {
    /// Path segment 1 (validated against the allowlist).
    pub env: String,
    /// Path segment 2 (validated against the service charset).
    pub service: String,
    /// The full event object, envelope fields canonicalized. Optional
    /// envelope fields are omitted when absent, never written as JSON
    /// null — `DuckDB` infers JSON for an all-null column (ADR-0009).
    pub obj: Map<String, Value>,
    /// Repair codes applied, in application order (also joined into the
    /// object's `_repairs`).
    pub repairs: Vec<RepairCode>,
    /// The event carried a severity source that mapped to nothing on the
    /// `OTel` ladder, so `_severity` was omitted (ADR-0013 §2).
    ///
    /// Not a repair: derivation into the `_` namespace is an annotation,
    /// and nothing sender-visible was touched — the source column is
    /// stored verbatim. The ops signal is a metrics counter instead.
    pub severity_unmapped: bool,
}

/// Maximum preserved length of `_raw` (chars). Generous — `_raw` is the
/// re-extraction lifeline — but bounded, so one pathological event cannot
/// dominate a parquet row group. Truncation is recorded as
/// `field.truncated`.
pub const MAX_RAW_CHARS: usize = 65_536;

/// How far in the past a parsed `_time` may sit before it is flagged
/// `time.out_of_range` (kept, never substituted).
const OUT_OF_RANGE_PAST_DAYS: i64 = 3653; // ~10 years
/// How far in the future a parsed `_time` may sit before it is flagged.
const OUT_OF_RANGE_FUTURE_DAYS: i64 = 1;

/// Date-time formats carrying an explicit UTC offset, tried after RFC 3339.
///
/// `chrono::DateTime::parse_from_rfc3339` only accepts the extended offset
/// spelling (`+05:30`), but the ISO 8601 *basic* spelling (`+0530`, `+05`) is
/// what Java's default logging encoders and Go's `-0700` layouts emit. `%#z`
/// is chrono's parse-only offset that takes `+HH`, `+HHMM` and `+HH:MM` alike.
const OFFSET_TIMESTAMP_FORMATS: [&str; 6] = [
    "%Y-%m-%dT%H:%M:%S%.f%#z",
    "%Y-%m-%d %H:%M:%S%.f%#z",
    "%Y/%m/%d %H:%M:%S%.f%#z",
    "%Y-%m-%dT%H:%M%#z",
    "%Y-%m-%d %H:%M%#z",
    "%Y/%m/%d %H:%M%#z",
];

/// Offset-less date-time formats accepted alongside RFC 3339, read as UTC.
///
/// `%.f` matches an optional fractional-second suffix, so each second-precision
/// entry covers both the with- and without-fraction spelling; the `%H:%M`
/// entries cover minute precision. The slash-dated spellings are what Go's
/// standard `log` package emits.
const NAIVE_TIMESTAMP_FORMATS: [&str; 6] = [
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y/%m/%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M",
    "%Y/%m/%d %H:%M",
];

/// Date-only formats, read as midnight UTC.
const DATE_ONLY_TIMESTAMP_FORMATS: [&str; 2] = ["%Y-%m-%d", "%Y/%m/%d"];

/// Parse a time value per the ADR-0008 grammar.
///
/// A value is valid iff it is a JSON string that — after trimming
/// surrounding whitespace — chrono parses as RFC 3339, as one of
/// [`OFFSET_TIMESTAMP_FORMATS`], [`NAIVE_TIMESTAMP_FORMATS`] or
/// [`DATE_ONLY_TIMESTAMP_FORMATS`]. Returns `None` for anything malformed.
fn parse_event_time(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = v.as_str()?.trim();
    let utc: chrono::DateTime<chrono::Utc> = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
    {
        dt.with_timezone(&chrono::Utc)
    } else if let Some(dt) = OFFSET_TIMESTAMP_FORMATS
        .iter()
        .find_map(|fmt| chrono::DateTime::parse_from_str(s, fmt).ok())
    {
        dt.with_timezone(&chrono::Utc)
    } else if let Some(naive) = NAIVE_TIMESTAMP_FORMATS
        .iter()
        .find_map(|fmt| chrono::NaiveDateTime::parse_from_str(s, fmt).ok())
    {
        naive.and_utc()
    } else {
        DATE_ONLY_TIMESTAMP_FORMATS
            .iter()
            .find_map(|fmt| chrono::NaiveDate::parse_from_str(s, fmt).ok())?
            .and_hms_opt(0, 0, 0)?
            .and_utc()
    };
    Some(utc)
}

/// Truncate a string to `max_chars` on a char boundary. Returns the
/// original when it fits.
fn truncate_chars(s: String, max_chars: usize) -> (String, bool) {
    if s.chars().count() > max_chars {
        (s.chars().take(max_chars).collect(), true)
    } else {
        (s, false)
    }
}

/// Maximum length (chars) of a client-supplied value quoted back inside a
/// reject message. The handler returns one message per rejected event,
/// uncapped in count, so a message that echoes its input verbatim makes the
/// response an amplifier of whatever the client sent: every other reject
/// message on this path is bounded, and this keeps the echoing ones so.
const MAX_REJECT_ECHO_CHARS: usize = 64;

/// A client value as it may appear in a reject message: bounded, cut on a
/// char boundary, and marked when cut. Enough to recognize the offending
/// value without repeating it back wholesale.
fn echo(value: &str) -> String {
    let (shown, truncated) = truncate_chars(value.to_owned(), MAX_REJECT_ECHO_CHARS);
    if truncated {
        format!("{shown}...")
    } else {
        shown
    }
}

/// The JSON type of a value, for a "wrong type" reject message. The type
/// only — `Display` on a `Value` serializes the whole thing, and an object
/// or array the client chose is unbounded by construction.
const fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Validate the `service` value: present, a string, non-empty, within the
/// length cap, charset-clean, and not a dot-name (`.`, `..`, dot-leading).
///
/// Path encoding is injective by validation (ADR-0009): the on-disk name
/// is the value, so the constraints here are the whole safety argument.
fn validate_service(obj: &Map<String, Value>) -> Result<String, (String, RejectReason)> {
    let v = obj.get("service").ok_or_else(|| {
        (
            "missing 'service' field".to_owned(),
            RejectReason::MissingService,
        )
    })?;
    let Some(svc) = v.as_str() else {
        return Err((
            format!("'service' must be a string, got {}", json_type_name(v)),
            RejectReason::ServiceNotString,
        ));
    };

    if svc.is_empty() {
        return Err((
            "service name cannot be empty".into(),
            RejectReason::EmptyService,
        ));
    }
    if svc.len() > pipeline::MAX_SERVICE_NAME_LEN {
        return Err((
            format!(
                "service name too long ({} chars, max {})",
                svc.len(),
                pipeline::MAX_SERVICE_NAME_LEN,
            ),
            RejectReason::ServiceTooLong,
        ));
    }
    if !svc.bytes().all(pipeline::is_valid_service_char) {
        return Err((
            format!(
                "service '{svc}' contains invalid characters \
                 (only alphanumeric, dash, underscore, dot allowed)"
            ),
            RejectReason::InvalidChars,
        ));
    }
    if svc.starts_with('.') {
        return Err((
            format!("service '{svc}' cannot be '.', '..', or start with a dot"),
            RejectReason::InvalidChars,
        ));
    }

    Ok(svc.to_owned())
}

/// Validate/derive the `env` value against the allowlist. Returns the env
/// and whether it was defaulted.
fn resolve_env(
    obj: &Map<String, Value>,
    ctx: &EnvelopeContext<'_>,
) -> Result<(String, bool), (String, RejectReason)> {
    match obj.get("env") {
        None | Some(Value::Null) => Ok((ctx.default_env.to_owned(), true)),
        Some(Value::String(env)) => {
            if !trawl_config::is_valid_env_name(env) {
                // The value failed the charset/length rule, so it is exactly
                // the case where the original may be arbitrarily long —
                // echo a bounded prefix, never the whole thing.
                return Err((
                    format!(
                        "env '{}' is not a valid env name (must match [a-z0-9_-]{{1,32}})",
                        echo(env)
                    ),
                    RejectReason::InvalidEnv,
                ));
            }
            if !ctx.envs.iter().any(|e| e == env) {
                return Err((
                    format!(
                        "env '{env}' is not in the configured allowlist ({:?})",
                        ctx.envs
                    ),
                    RejectReason::EnvNotAllowed,
                ));
            }
            Ok((env.clone(), false))
        }
        Some(other) => Err((
            format!("'env' must be a string, got {}", json_type_name(other)),
            RejectReason::InvalidEnv,
        )),
    }
}

/// Derive `_severity` from the event's own fields, reading and never
/// consuming them.
///
/// The first mappable source wins, over the list this profile reads
/// (ADR-0013 §2): the configured `[ingest] severity_from` chain with the
/// profile's fixed sources prepended. Every source stays exactly where it
/// is, so `{"service":"game","level":"gold"}` keeps a queryable
/// `level="gold"` column and simply gets no `_severity`. Returns whether a
/// source existed and none mapped, which is the ops counter's input. That
/// is not a repair, because nothing sender-visible was touched.
///
/// Each source carries its own dialect and every value goes through
/// `trawl_core::severity`. A word maps through the token table (`error` →
/// 17) or the `OTel` exact short names (`error2` → 18); a number maps as
/// that source's dialect says, `OTel` 1-24 by default (so `3` is trace),
/// and syslog's inverted 0-7 only where the source declares it. The two
/// numeric ranges overlap, so no value-shape rule could tell the dialects
/// apart; provenance is asserted in config, or by the syslog profile's
/// fixed source, and never guessed.
///
/// Ingest and the `sev()` query function read through that same code, so
/// the number a query computes from a raw `level` is the number derivation
/// would have stored for it.
fn derive_severity(out: &mut Map<String, Value>, sources: &[producer::Source]) -> bool {
    let mut saw_source = false;
    for source in sources {
        let Some(value) = out.get(&source.field) else {
            continue;
        };
        saw_source = true;
        if let Some(number) = trawl_core::severity::reading(value, source.dialect) {
            out.insert(trawl_core::schema::SEVERITY.into(), json!(number));
            return false;
        }
    }
    saw_source
}

/// Stamp what the producer asserts over the payload, returning whether any
/// payload value was displaced (ADR-0013).
///
/// Runs after the reserved-prefix strip and before the validators, so a
/// stripped `_service` has already landed on its bare slot and the
/// assertion overrides that too, and so `validate_service`/`resolve_env`
/// run on the asserted values, giving the profile doors exactly the
/// checks the HTTP door gets.
///
/// Identity is protected by precedence, not by a namespace: telemetry's
/// `service` field is ordinary sender vocabulary that happens to collide,
/// and it loses. An identical value is not a collision, because the sender
/// agreed with the profile and a repair code for that would land on every
/// well-formed syslog frame.
///
/// The two `None`s mean different things, deliberately:
/// - `host: None` asserts absence (the producer knows it has no honest
///   hostname), so any payload `host` is displaced and the slot is left
///   empty for the caller to confess as `host.omitted`. A payload
///   `host: null` displaces nothing, being absence itself;
/// - `message: None` asserts nothing at all (the payload is the message,
///   as for telemetry), so whatever the payload carries stands.
fn apply_assertions(out: &mut Map<String, Value>, asserted: &Asserted<'_>) -> bool {
    let mut displaced = claim_slot(out, trawl_core::schema::ENV, Some(asserted.env));
    displaced |= claim_slot(out, trawl_core::schema::SERVICE, Some(asserted.service));
    displaced |= claim_slot(out, trawl_core::schema::HOST, asserted.host);
    if let Some(message) = asserted.message {
        displaced |= claim_slot(out, trawl_core::schema::MESSAGE, Some(message));
    }
    displaced
}

/// Claim one asserted slot. `Some` stamps the value, `None` empties the
/// slot. Returns whether a different payload value was displaced.
///
/// A JSON `null` counts as absence everywhere the collision is judged:
/// `{"host": null}` asserts nothing, so it displaces nothing and earns no
/// `field.producer_asserted`. It is still removed, because an optional
/// envelope field is omitted rather than written as JSON null (`DuckDB`
/// infers JSON for an all-null column, ADR-0009), and the whole payload
/// stays in `_raw` either way. The alternative would put a repair code on
/// every sender whose serializer emits explicit nulls for unset fields,
/// which is most of them.
fn claim_slot(out: &mut Map<String, Value>, key: &str, value: Option<&str>) -> bool {
    match value {
        Some(value) => {
            let displaced = match out.get(key) {
                None | Some(Value::Null) => false,
                Some(Value::String(existing)) => existing != value,
                Some(_) => true,
            };
            out.insert(key.to_owned(), json!(value));
            displaced
        }
        None => matches!(out.remove(key), Some(displaced) if !displaced.is_null()),
    }
}

/// Stringify top-level object/array values to their JSON text (ADR-0009):
/// with nothing left to flatten, a `read_json` "Duplicate name" collision
/// is structurally impossible, so compaction never has to choose between
/// draining a batch and keeping its columns.
///
/// Runs after the `_raw` capture (the nesting stays findable there) and
/// after the reserved-prefix strip (a forged object `_raw` must strip like
/// any other reserved name rather than be laundered into a string). This
/// is a canonicalization like RFC 3339 time reformatting, not a repair: a
/// code firing on every k8s event would fill `_repairs` on most of the
/// corpus, and its value is that it is almost always null. Reach nested
/// values with `json_extract_string(k8s, '$.pod')`.
fn stringify_nested_values(out: &mut Map<String, Value>) {
    for value in out.values_mut() {
        if value.is_object() || value.is_array() {
            *value = Value::String(value.to_string());
        }
    }
}

/// What the reserved-prefix strip did to one event.
#[derive(Default)]
struct PrefixStrip {
    /// At least one `_x` key was stripped or dropped
    /// (`field.reserved_prefix`).
    stripped: bool,
    /// A stripped name collided with a bare name the same event carries,
    /// so the prefixed loser was dropped
    /// (`field.reserved_prefix_collision`).
    collided: bool,
}

/// Whether this profile lets the payload propose `_raw`.
///
/// `_raw` is the re-extraction lifeline, and what makes it one differs by
/// door. A remote sender (HTTP) or a transport frame (syslog) has an
/// original form trawl never saw, the collector's pre-parse line or the
/// wire datagram, so a string `_raw` it supplies is the most original form
/// available and is honoured verbatim.
///
/// Trawld's own telemetry has no such thing: the payload is the original,
/// and a tracing field named `_raw` is ordinary application vocabulary
/// that would shadow the lifeline. The pre-repair serialization carries
/// the values assertions and collisions displace, so letting a field claim
/// the slot would make "a displaced value stays findable in `_raw`" false
/// on exactly the door that asserts identity hardest. Non-proposable
/// means the standard reserved-prefix strip applies: the payload key lands
/// on bare `raw`, value intact, and the door writes the serialization.
const fn raw_is_proposable(kind: producer::ProducerKind) -> bool {
    match kind {
        producer::ProducerKind::Http | producer::ProducerKind::Syslog => true,
        producer::ProducerKind::Trawld => false,
    }
}

/// Whether a `_`-prefixed key is one the sender may propose.
///
/// At most two slots (ADR-0013 §3): `_time`, always — it is the event
/// time proposal, canonicalized downstream — and `_raw`, when the value
/// is a string and this door admits a proposal at all
/// ([`raw_is_proposable`]). Everything else in the namespace is trawl's:
/// server-stamped (`_ingested`, `_repairs`), derivation-only
/// (`_severity`), internal (`_trawl_wal_file`), or a slot that does not
/// exist yet.
fn is_proposable(key: &str, value: &Value, raw_proposable: bool) -> bool {
    key == trawl_core::schema::TIME
        || (raw_proposable && key == trawl_core::schema::RAW && value.is_string())
}

/// Seal the `_` namespace at the ingest door (ADR-0013 §5): a
/// non-proposable `_x` has its leading underscore run stripped and its
/// value stored under the bare remainder.
///
/// One rule covers every shape, including the journald/prometheus names
/// (`_HOSTNAME` → `hostname`, `_SYSTEMD_UNIT` → `systemd_unit`,
/// `__name__` → `name__`), and it keeps the promise that every accepted
/// field stays structurally queryable.
///
/// Three sub-cases, all deterministic:
///
/// - the bare remainder is empty (`_`, `___`): there is no name to store
///   under, so the field is dropped — its value is still in `_raw`;
/// - the bare name already exists in this event: the prefixed loser is
///   dropped (`field.reserved_prefix_collision`), the same tiebreak the
///   case-fold uses;
/// - two prefixed claimants for one bare name: the first in map order
///   wins (`serde_json::Map` iterates sorted, so that is the
///   ASCII-lexicographically first spelling), the rest collide out.
fn strip_reserved_prefixes(out: &mut Map<String, Value>, raw_proposable: bool) -> PrefixStrip {
    let reserved: Vec<String> = out
        .keys()
        .filter(|k| trawl_core::schema::is_reserved_name(k))
        .filter(|k| !is_proposable(k, &out[*k], raw_proposable))
        .cloned()
        .collect();
    let mut strip = PrefixStrip::default();
    for key in reserved {
        strip.stripped = true;
        let value = out.remove(&key).expect("key came from this map");
        let bare = key.trim_start_matches('_');
        if bare.is_empty() || out.contains_key(bare) {
            // No name to store under, or the bare name is already taken:
            // drop the prefixed value. `_raw` still carries it.
            if !bare.is_empty() {
                strip.collided = true;
            }
            continue;
        }
        out.insert(bare.to_owned(), value);
    }
    strip
}

/// One event's field names after ASCII case-folding ([`fold_field_names`]).
struct FoldedNames {
    /// The event with every field name ASCII-lowercased.
    obj: Map<String, Value>,
    /// Whether any name was actually folded (the `field.name_case_folded`
    /// repair — recorded only when folding changed something).
    folded: bool,
    /// Whether any key was dropped because another key claims the same
    /// folded name (the `field.name_case_collision` repair).
    collided: bool,
}

/// ASCII-lowercase every field name, dropping the losers of any resulting
/// collision.
///
/// `DuckDB` identifiers are ASCII case-insensitive while JSON keys are not,
/// so `Dur` and `dur`, or `_Time` and `_time`, name one column downstream,
/// while every layer that keys on the exact string (the postgres field
/// catalog, the envelope's own reserved-name handling, the hot snapshot's
/// key set) would treat them as two. Folding at the door means exactly one
/// code path ever sees one spelling: a client `_Time` becomes the `_time`
/// wire input, `_Ingested` strips to bare `ingested`, `Service` validates
/// as `service`, and a custom `Dur` pins and stores as `dur`.
/// Per-character ASCII lowercase is precisely `DuckDB`'s identifier
/// equivalence: `CAFÉ` folds to `cafÉ` (its class's canonical form) while
/// the distinct identifier `café` is untouched (probed in
/// `trawl-engine/tests/duckdb_probe.rs`).
///
/// In-event collisions after folding keep one value, deterministically: the
/// exact (already-lowercase) spelling wins when the event carries it;
/// otherwise the ASCII-lexicographically first variant does
/// (`serde_json::Map` iterates sorted, so "first in map order" is exactly
/// that). Nothing honest can be done with two values for what `DuckDB`
/// reads as one column; the dropped key and its value stay findable in
/// `_raw`, which is captured from the pre-fold object.
fn fold_field_names(obj: &Map<String, Value>) -> FoldedNames {
    let mut out = Map::new();
    // Pass 1: exact (already-folded) spellings — the collision winners.
    for (key, value) in obj {
        if !key.bytes().any(|b| b.is_ascii_uppercase()) {
            out.insert(key.clone(), value.clone());
        }
    }
    // Pass 2: fold the rest, first variant in (sorted) map order wins.
    let mut folded = false;
    let mut collided = false;
    for (key, value) in obj {
        if !key.bytes().any(|b| b.is_ascii_uppercase()) {
            continue;
        }
        let lower = key.to_ascii_lowercase();
        if out.contains_key(&lower) {
            collided = true;
        } else {
            out.insert(lower, value.clone());
            folded = true;
        }
    }
    FoldedNames {
        obj: out,
        folded,
        collided,
    }
}

/// Drop every field whose name cannot be a field-catalog key, returning
/// whether any were dropped (the `field.name_too_long` repair).
///
/// This is the only seam that sees a client-chosen key before it becomes a
/// column. Compaction pins every dynamic column in postgres before writing
/// the parquet that carries it, and an over-long name overflows the btree
/// key behind `field_types.field`: the insert errors, the batch is retained
/// for retry, and the same WAL re-fails on every tick — permanently, for
/// that service. Bounding the name here is what keeps that unreachable.
///
/// Dropping the field is a repair with an honest answer rather than a
/// guess: `_raw` is captured before this runs, so the name and its value
/// both stay recoverable, and no other field in the event is punished for
/// one bad key (rejecting the event would throw away good data).
fn drop_unstorable_names(out: &mut Map<String, Value>) -> bool {
    let unstorable: Vec<String> = out
        .keys()
        .filter(|k| !trawl_core::schema::is_storable_field_name(k))
        .cloned()
        .collect();
    for k in &unstorable {
        out.remove(k);
    }
    !unstorable.is_empty()
}

/// Decide what to do about `host`, per door (ADR-0013).
///
/// Returns whether the event arrived without one and, if so, the peer
/// address to fill it from.
///
/// Only the HTTP door has a peer to fill from or to refuse behind: an
/// HTTP sender can be rejected and resend, so trawl holds out for an
/// honest answer rather than stamping a relay's address as the origin. A
/// profile producer cannot reject to anyone, so its absent host stays
/// absent and the caller confesses `host.omitted`: absent but honest beats
/// both the peer-fill lie and dropping the event.
fn decide_host<'a>(
    out: &Map<String, Value>,
    producer: &Producer<'a>,
) -> Result<(bool, Option<&'a str>), (String, RejectReason)> {
    let host_missing = matches!(out.get(trawl_core::schema::HOST), None | Some(Value::Null));
    let peer_fill = match producer {
        Producer::Http {
            peer_host,
            peer_is_trusted_relay,
        } if host_missing => {
            if *peer_is_trusted_relay {
                return Err((
                    format!(
                        "event has no 'host' and peer {peer_host} is a configured trusted relay \
                         — filling from the peer would stamp the relay's address as the origin"
                    ),
                    RejectReason::HostMissingFromRelay,
                ));
            }
            Some(*peer_host)
        }
        _ => None,
    };
    Ok((host_missing, peer_fill))
}

/// Derive `_time` from the first present source in this profile's
/// `time_from` list, returning the canonical RFC 3339 UTC-microsecond
/// text and the repair it earned, if any (ADR-0013 §2, ADR-0008 grammar).
///
/// Derivation observes, it never consumes. Only `_time` itself is removed,
/// being the proposal slot the canonical value replaces;
/// `timestamp`/`@timestamp`, and the syslog profile's own
/// `syslog_timestamp`, stay as ordinary columns.
///
/// First present, not first parseable: an unparseable value claims the
/// derivation and falls to arrival time rather than reaching past itself
/// to a lower-precedence source, so what `_time` holds is always
/// explicable from one input.
fn derive_time(
    out: &mut Map<String, Value>,
    ctx: &EnvelopeContext<'_>,
) -> (String, Option<RepairCode>) {
    let time_input = ctx
        .derivation
        .time_from(ctx.producer.kind())
        .iter()
        .find_map(|source| out.get(&source.field).cloned());
    out.remove(trawl_core::schema::TIME);
    match &time_input {
        None => (ctx.arrival.to_owned(), Some(RepairCode::TimeFromIngest)),
        Some(v) => match parse_event_time(v) {
            Some(dt) => {
                let past = ctx.arrival_instant - chrono::Duration::days(OUT_OF_RANGE_PAST_DAYS);
                let future = ctx.arrival_instant + chrono::Duration::days(OUT_OF_RANGE_FUTURE_DAYS);
                // Implausible, but parseable: kept — flagged, never
                // substituted (the client said what it said).
                let repair = (dt < past || dt > future).then_some(RepairCode::TimeOutOfRange);
                (
                    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                    repair,
                )
            }
            None => (ctx.arrival.to_owned(), Some(RepairCode::TimeFromIngest)),
        },
    }
}

/// Canonicalize one parsed event object into the declared envelope.
///
/// Field order of operations is load-bearing:
/// 0. Field-name ASCII case-fold ([`fold_field_names`]), before anything
///    that keys on a name (service validation, reserved-prefix strip,
///    stringification), so one code path sees one spelling. Recorded as
///    `field.name_case_folded` / `field.name_case_collision` only when it
///    changed something.
/// 1. `_raw` capture, before the strip and every repair, so the server's
///    own fills never appear inside "what arrived" (the serialization
///    fallback uses the pre-fold object, so dropped and folded spellings
///    stay findable). Whether the payload may propose `_raw` is
///    per-profile ([`raw_is_proposable`]): trawld's own door never lets a
///    field claim the lifeline.
/// 2. Reserved-prefix strip ([`strip_reserved_prefixes`],
///    `field.reserved_prefix` / `field.reserved_prefix_collision`).
///    2.5. Producer assertions ([`apply_assertions`],
///    `field.producer_asserted`), after the strip, so a stripped
///    `_service` has already landed on its bare slot and the profile
///    overrides that too, and before the validators, so an asserted
///    `service`/`env` faces exactly the checks an HTTP sender's does.
///    The HTTP door asserts nothing and skips this.
/// 3. `service` validation (reject path), `env` and `host` resolution,
///    after the strip, because a stripped `_service`/`_env`/`_host` lands
///    on exactly the bare slot these read, the same way a stripped
///    `_severity` lands where derivation reads it (ADR-0013 §5). Resolving
///    first would let step 6 overwrite the stripped value and then confess
///    `env.defaulted`/`host.from_peer` about a sender that did assert one.
///    Over-long field-name drop (`field.name_too_long`) follows.
/// 4. `_time` derived from the first present source in this profile's
///    `time_from` list, ADR-0008 grammar,
///    `time.from_ingest`/`time.out_of_range`. Only `_time` is consumed.
/// 5. `_ingested` stamp.
/// 6. `env` default-or-reject, `host` peer-fill / relay-reject / omit.
/// 7. `_severity` derived from the first mappable source in this profile's
///    `severity_from` list, read-only: every source stays where it is, and
///    no mappable one means no key and no repair.
///    7.5. `_producer` stamp, after the strip, so an incoming `_producer`
///    has already become a bare `producer` and the column is unforgeable.
/// 8. `_repairs` assembly (omitted when clean).
pub fn canonicalize(
    obj: &Map<String, Value>,
    ctx: &EnvelopeContext<'_>,
) -> Result<Canonical, (String, RejectReason)> {
    // 0. One spelling per DuckDB identifier, before anything reads a name.
    let fold = fold_field_names(obj);
    let folded_obj = fold.obj;

    // 1. Capture `_raw` before anything is stripped or repaired. On a door
    // that admits the proposal a client-supplied string `_raw` is kept
    // verbatim; otherwise the pre-repair serialization of the pre-fold
    // object is the most original form available, since wire-exact bytes
    // do not exist (events arrive inside JSON arrays and the WAL
    // re-serializes anyway).
    let raw_proposable = raw_is_proposable(ctx.producer.kind());
    let raw_string = match folded_obj.get(trawl_core::schema::RAW) {
        Some(Value::String(s)) if raw_proposable => s.clone(),
        _ => serde_json::to_string(obj).unwrap_or_default(),
    };
    let (raw_string, truncated) = truncate_chars(raw_string, MAX_RAW_CHARS);

    let mut out = folded_obj;

    // 2. The `_` namespace is trawl's: strip the prefix, keep the data.
    let strip = strip_reserved_prefixes(&mut out, raw_proposable);

    let mut repairs: Vec<RepairCode> = Vec::new();
    let push_repair = |repairs: &mut Vec<RepairCode>, code: RepairCode| {
        if !repairs.contains(&code) {
            repairs.push(code);
        }
    };

    // 2.5. What the producer asserts wins over what the payload carries.
    // The codes the producer itself contributed lead `_repairs`: they
    // describe what happened to the event before it reached this door.
    if let Some(asserted) = ctx.producer.asserted() {
        for code in asserted.repairs {
            push_repair(&mut repairs, *code);
        }
        if apply_assertions(&mut out, asserted) {
            push_repair(&mut repairs, RepairCode::ProducerAsserted);
        }
    }

    // 3. Identity, read from the post-strip, post-assertion event: a
    // `_host`/`_env`/`_service` has landed on its bare slot by now, so it
    // is ordinary sender-asserted data and the fills below cannot
    // overwrite it (nor claim in `_repairs` that the sender asserted
    // nothing). A profile's assertion sits in those same slots, so it
    // faces exactly these validators.
    let service = validate_service(&out)?;
    let (env, env_defaulted) = resolve_env(&out, ctx)?;

    let (host_missing, peer_fill) = decide_host(&out, &ctx.producer)?;

    if fold.folded {
        push_repair(&mut repairs, RepairCode::FieldNameCaseFolded);
    }
    if fold.collided {
        push_repair(&mut repairs, RepairCode::FieldNameCaseCollision);
    }
    if strip.stripped {
        push_repair(&mut repairs, RepairCode::ReservedPrefix);
    }
    if strip.collided {
        push_repair(&mut repairs, RepairCode::ReservedPrefixCollision);
    }

    // 3.2. Names too long to be a catalog key never become columns.
    if drop_unstorable_names(&mut out) {
        push_repair(&mut repairs, RepairCode::FieldNameTooLong);
    }

    // 3.5. Nested values become JSON text (see [`stringify_nested_values`]).
    stringify_nested_values(&mut out);

    // 4. `_time` from this profile's own source list.
    let (canonical_time, time_repair) = derive_time(&mut out, ctx);
    if let Some(code) = time_repair {
        push_repair(&mut repairs, code);
    }
    out.insert(trawl_core::schema::TIME.into(), json!(canonical_time));

    // 5. Server-stamped arrival time.
    out.insert(trawl_core::schema::INGESTED.into(), json!(ctx.arrival));

    // 6. env / host.
    out.insert(trawl_core::schema::ENV.into(), json!(env));
    if env_defaulted {
        push_repair(&mut repairs, RepairCode::EnvDefaulted);
    }
    if let Some(peer_host) = peer_fill {
        out.insert(trawl_core::schema::HOST.into(), json!(peer_host));
        push_repair(&mut repairs, RepairCode::HostFromPeer);
    } else if host_missing {
        // Only a profile producer reaches here: the HTTP door either
        // filled from the peer or rejected above.
        push_repair(&mut repairs, RepairCode::HostOmitted);
    }

    // 7. `_severity` derivation is read-only: every source stays where
    // it is, and an event with no mappable one simply has no `_severity`.
    let severity_unmapped =
        derive_severity(&mut out, ctx.derivation.severity_from(ctx.producer.kind()));

    // 7.5. Provenance becomes data (ADR-0013). Stamped from the profile,
    // never from the payload: an incoming `_producer` was stripped to a
    // bare `producer` back at step 2, so the column cannot be forged.
    out.insert(
        trawl_core::schema::PRODUCER.into(),
        json!(ctx.producer.kind().as_str()),
    );

    // 8. `_raw` and `_repairs`.
    if truncated {
        push_repair(&mut repairs, RepairCode::FieldTruncated);
    }
    out.insert(trawl_core::schema::RAW.into(), json!(raw_string));
    if repairs.is_empty() {
        out.remove(trawl_core::schema::REPAIRS);
    } else {
        let joined = repairs
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(",");
        out.insert(trawl_core::schema::REPAIRS.into(), json!(joined));
    }

    Ok(Canonical {
        env,
        service,
        obj: out,
        repairs,
        severity_unmapped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival_instant() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    const ARRIVAL: &str = "2026-01-01T00:00:00.000000Z";

    /// The default derivation policy, shared by every test that does not
    /// configure its own. Held in a `OnceLock` so a test can borrow it for
    /// the life of a context without threading an owner through every
    /// helper.
    fn default_derivation() -> &'static Derivation {
        static DEFAULTS: std::sync::OnceLock<Derivation> = std::sync::OnceLock::new();
        DEFAULTS.get_or_init(Derivation::defaults)
    }

    /// The HTTP door with the packaged derivation policy, which is what
    /// every test below assumes unless it builds its own context.
    fn ctx_with(envs: &[String], relay: bool) -> EnvelopeContext<'_> {
        EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: arrival_instant(),
            envs,
            default_env: &envs[0],
            producer: Producer::Http {
                peer_host: "10.0.4.55",
                peer_is_trusted_relay: relay,
            },
            derivation: default_derivation(),
        }
    }

    fn envs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn event(json: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    fn canon(json: &str) -> Canonical {
        let e = envs(&["prod", "lab"]);
        canonicalize(&event(json), &ctx_with(&e, false)).expect("event must canonicalize")
    }

    fn reject(json: &str) -> (String, RejectReason) {
        let e = envs(&["prod", "lab"]);
        canonicalize(&event(json), &ctx_with(&e, false)).expect_err("event must reject")
    }

    fn codes(c: &Canonical) -> Vec<&'static str> {
        c.repairs.iter().map(|r| r.as_str()).collect()
    }

    // --- the declared schema, enforced ---

    #[test]
    fn fully_specified_event_is_clean() {
        let c = canon(
            r#"{"service":"nginx","env":"prod","host":"web01",
                "_time":"2025-12-31T23:00:00Z","severity":17,
                "severity_text":"error","message":"boom","_raw":"raw line"}"#,
        );
        assert!(c.repairs.is_empty(), "clean event: {:?}", c.repairs);
        assert!(
            !c.obj.contains_key("_repairs"),
            "_repairs must be OMITTED (not null) for clean events"
        );
        assert_eq!(c.obj["_time"], "2025-12-31T23:00:00.000000Z");
        assert_eq!(c.obj["_ingested"], ARRIVAL);
        assert_eq!(c.obj["_raw"], "raw line");
        assert_eq!(c.obj["env"], "prod");
        assert_eq!(c.obj["severity"], 17);
        assert_eq!(c.obj["severity_text"], "error");
    }

    #[test]
    fn missing_time_repairs_from_ingest() {
        let c = canon(r#"{"service":"s","env":"prod","host":"h"}"#);
        assert_eq!(c.obj["_time"], ARRIVAL);
        assert!(codes(&c).contains(&"time.from_ingest"));
        assert_eq!(c.obj["_repairs"], "time.from_ingest");
    }

    #[test]
    fn missing_env_repairs_from_default() {
        let c = canon(r#"{"service":"s","host":"h","_time":"2025-12-31T23:00:00Z"}"#);
        assert_eq!(c.obj["env"], "prod");
        assert_eq!(c.env, "prod");
        assert!(codes(&c).contains(&"env.defaulted"));
    }

    #[test]
    fn missing_host_repairs_from_peer() {
        let c = canon(r#"{"service":"s","env":"prod","_time":"2025-12-31T23:00:00Z"}"#);
        assert_eq!(c.obj["host"], "10.0.4.55");
        assert!(codes(&c).contains(&"host.from_peer"));
    }

    // --- nested-value stringification (ADR-0009) ---

    #[test]
    fn object_values_are_stringified_to_json_text() {
        // With nothing left to flatten, `read_json` can never raise a
        // "Duplicate name" collision, so compaction needs no fallback that
        // trades a batch's custom columns for draining it.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z",
                "k8s":{"pod":"x","ns":"default"}}"#,
        );
        let k8s = c.obj["k8s"]
            .as_str()
            .expect("object value becomes a string");
        let parsed: Value = serde_json::from_str(k8s).expect("the string is JSON text");
        assert_eq!(parsed["pod"], "x");
        assert_eq!(parsed["ns"], "default");
    }

    #[test]
    fn array_values_are_stringified_to_json_text() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","tags":["a","b"]}"#,
        );
        let tags = c.obj["tags"]
            .as_str()
            .expect("array value becomes a string");
        assert_eq!(
            serde_json::from_str::<Value>(tags).unwrap(),
            serde_json::json!(["a", "b"])
        );
    }

    #[test]
    fn stringification_is_canonicalization_not_a_repair() {
        // No repair code: this is a canonicalization like RFC 3339
        // reformatting, and a code firing on every k8s event would leave
        // `_repairs` almost never null.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","k8s":{"pod":"x"}}"#,
        );
        assert!(
            c.repairs.is_empty(),
            "no repair for stringification: {:?}",
            c.repairs
        );
        assert!(!c.obj.contains_key("_repairs"));
    }

    #[test]
    fn stringification_leaves_scalars_untouched() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z",
                "count":42,"ratio":1.5,"ok":true,"note":"plain"}"#,
        );
        assert_eq!(c.obj["count"], 42);
        assert_eq!(c.obj["ratio"], 1.5);
        assert_eq!(c.obj["ok"], true);
        assert_eq!(c.obj["note"], "plain");
    }

    #[test]
    fn raw_preserves_the_original_nesting() {
        // `_raw` is captured before stringification, so the original
        // structure stays findable there.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","k8s":{"pod":"x"}}"#,
        );
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(
            raw.contains(r#""k8s":{"pod":"x"}"#),
            "_raw must hold the pre-stringification nesting: {raw}"
        );
    }

    #[test]
    fn non_string_raw_is_still_stripped_before_stringification() {
        // A client object `_raw` takes the reserved-prefix strip — the
        // stringify pass must not first turn it into an honourable string.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_raw":{"forged":true}}"#,
        );
        assert!(codes(&c).contains(&"field.reserved_prefix"));
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(
            raw.contains("forged"),
            "server-filled _raw is the pre-repair serialization: {raw}"
        );
    }

    #[test]
    fn missing_service_rejects() {
        let (msg, reason) = reject(r#"{"message":"no service"}"#);
        assert_eq!(reason, RejectReason::MissingService);
        assert!(msg.contains("service"));
    }

    // --- service type-specific rejection ---

    #[test]
    fn service_object_rejects_with_type_reason() {
        let (msg, reason) = reject(r#"{"service":{"name":"x"}}"#);
        assert_eq!(reason, RejectReason::ServiceNotString);
        assert!(msg.contains("object"), "got: {msg}");
    }

    #[test]
    fn service_number_rejects_with_type_reason() {
        let (msg, reason) = reject(r#"{"service":42}"#);
        assert_eq!(reason, RejectReason::ServiceNotString);
        assert!(msg.contains("number"), "got: {msg}");
    }

    #[test]
    fn service_null_rejects_with_type_reason() {
        let (_, reason) = reject(r#"{"service":null}"#);
        assert_eq!(reason, RejectReason::ServiceNotString);
    }

    // --- service charset (injective paths) ---

    #[test]
    fn service_dot_names_reject() {
        for svc in [".", "..", ".hidden"] {
            let (msg, reason) = reject(&format!(r#"{{"service":"{svc}"}}"#));
            assert_eq!(reason, RejectReason::InvalidChars, "service {svc:?}");
            assert!(msg.contains("dot"), "got: {msg}");
        }
    }

    #[test]
    fn service_space_now_rejects() {
        let (_, reason) = reject(r#"{"service":"Activity Monitor"}"#);
        assert_eq!(reason, RejectReason::InvalidChars);
    }

    #[test]
    fn service_traversal_rejects() {
        for svc in ["../../etc/passwd", "a/b", "a\\b"] {
            let (_, reason) = reject(&format!(r#"{{"service":"{}"}}"#, svc.replace('\\', "\\\\")));
            assert_eq!(reason, RejectReason::InvalidChars, "service {svc:?}");
        }
    }

    #[test]
    fn service_dotted_names_accepted_verbatim() {
        let c =
            canon(r#"{"service":"api.v2","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z"}"#);
        assert_eq!(c.service, "api.v2");
    }

    // --- host behind a trusted relay ---

    #[test]
    fn missing_host_behind_trusted_relay_rejects() {
        let e = envs(&["prod"]);
        let ctx = ctx_with(&e, true);
        let (msg, reason) = canonicalize(
            &event(r#"{"service":"s","env":"prod","_time":"2025-12-31T23:00:00Z"}"#),
            &ctx,
        )
        .expect_err("must reject");
        assert_eq!(reason, RejectReason::HostMissingFromRelay);
        assert!(msg.contains("10.0.4.55"), "got: {msg}");
    }

    #[test]
    fn present_host_behind_trusted_relay_accepted() {
        let e = envs(&["prod"]);
        let ctx = ctx_with(&e, true);
        let c = canonicalize(
            &event(
                r#"{"service":"s","env":"prod","host":"origin-1","_time":"2025-12-31T23:00:00Z"}"#,
            ),
            &ctx,
        )
        .expect("must accept");
        assert_eq!(c.obj["host"], "origin-1");
        assert!(c.repairs.is_empty());
    }

    // --- env allowlist: an unknown env hard-rejects ---

    #[test]
    fn unlisted_env_rejects() {
        let (msg, reason) = reject(r#"{"service":"s","env":"prdo"}"#);
        assert_eq!(reason, RejectReason::EnvNotAllowed);
        assert!(msg.contains("prdo"), "got: {msg}");
    }

    #[test]
    fn invalid_env_shape_rejects() {
        for (env_json, label) in [
            (r#""Prod""#, "uppercase"),
            (r#""pro.d""#, "dot"),
            (r#""pro/d""#, "slash"),
            (r#""..""#, "dotdot"),
            (r#""""#, "empty"),
            ("42", "number"),
            (r#"{"x":1}"#, "object"),
        ] {
            let (_, reason) = reject(&format!(r#"{{"service":"s","env":{env_json}}}"#));
            assert_eq!(reason, RejectReason::InvalidEnv, "env case {label}");
        }
    }

    /// Reject messages go back to the client one per rejected event, with no
    /// cap on how many. Every message on this path is bounded, so no single
    /// event can amplify a request into an arbitrarily large response (or an
    /// arbitrarily large allocation on the way there).
    const MAX_REJECT_MSG_CHARS: usize = 256;

    fn reject_obj(obj: &Map<String, Value>) -> (String, RejectReason) {
        let e = envs(&["prod", "lab"]);
        canonicalize(obj, &ctx_with(&e, false)).expect_err("event must reject")
    }

    #[test]
    fn multi_megabyte_env_string_reject_message_is_bounded() {
        let mut obj = Map::new();
        obj.insert("service".into(), json!("nginx"));
        obj.insert("env".into(), json!("e".repeat(4 * 1024 * 1024)));

        let (msg, reason) = reject_obj(&obj);
        assert_eq!(reason, RejectReason::InvalidEnv);
        assert!(
            msg.chars().count() <= MAX_REJECT_MSG_CHARS,
            "reject message must not echo the whole value ({} chars)",
            msg.chars().count()
        );
        assert!(
            msg.contains("eeee"),
            "a bounded prefix still identifies the value: {msg}"
        );
    }

    #[test]
    fn deeply_nested_env_object_reject_message_is_bounded() {
        let mut nested = json!("leaf");
        for _ in 0..512 {
            nested = json!({ "deeper": nested });
        }
        let mut obj = Map::new();
        obj.insert("service".into(), json!("nginx"));
        obj.insert("env".into(), nested);

        let (msg, reason) = reject_obj(&obj);
        assert_eq!(reason, RejectReason::InvalidEnv);
        assert!(
            msg.chars().count() <= MAX_REJECT_MSG_CHARS,
            "reject message must not serialize the value ({} chars)",
            msg.chars().count()
        );
        assert!(
            !msg.contains("deeper") && !msg.contains("leaf"),
            "a wrong-typed value is reported by type only: {msg}"
        );
        assert!(msg.contains("an object"), "got: {msg}");
    }

    #[test]
    fn null_env_defaults_like_absent() {
        let c = canon(r#"{"service":"s","host":"h","env":null,"_time":"2025-12-31T23:00:00Z"}"#);
        assert_eq!(c.env, "prod");
        assert!(codes(&c).contains(&"env.defaulted"));
    }

    // --- `_severity` derivation: observe, never consume (ADR-0013 §2) ---

    /// The canonical example: a game server whose `level` means loot tier
    /// keeps a queryable `level` column, gets no `_severity`, and is
    /// repaired in no way at all.
    #[test]
    fn a_bare_level_that_is_not_a_severity_survives_untouched() {
        let c = canon(r#"{"service":"game","level":"gold"}"#);
        assert_eq!(c.obj["level"], "gold", "the sender's own field, verbatim");
        assert!(
            !c.obj.contains_key(trawl_core::schema::SEVERITY),
            "no mappable source → no `_severity` key at all"
        );
        assert!(
            c.severity_unmapped,
            "a source existed and mapped to nothing — the ops counter's input"
        );
        // `env`/`host`/`_time` are filled, which is confessed; nothing
        // about the severity derivation is.
        assert!(
            !codes(&c).iter().any(|code| code.starts_with("severity")),
            "derivation into the `_` namespace is never a repair: {:?}",
            c.repairs
        );
    }

    /// The other worked example: an OTel-native sender's `severity: 3`
    /// stays verbatim and derives `_severity = 3` — trace on the `OTel`
    /// ladder, never syslog's err. Both truths coexist.
    #[test]
    fn a_numeric_severity_is_stored_verbatim_and_derived_as_otel() {
        let c = canon(r#"{"service":"x","severity":3}"#);
        assert_eq!(c.obj["severity"], 3, "the sender's field is untouched");
        assert_eq!(
            c.obj[trawl_core::schema::SEVERITY],
            3,
            "OTel 1-24, never the syslog inversion that would say 17"
        );
        assert!(!c.severity_unmapped);
    }

    /// The numeric dialect matrix (ADR-0013 §4): words map through the
    /// token table, numerics map strictly as `OTel` 1-24, and everything
    /// else has no reading.
    #[test]
    fn severity_numeric_dialect_matrix() {
        for (input, expected) in [
            ("3", Some(3)),
            (r#""3""#, Some(3)),
            (r#""error""#, Some(17)),
            (r#""ERROR""#, Some(17)),
            (r#""err""#, Some(17)),
            (r#""error2""#, Some(18)),
            ("0", None),
            ("25", None),
            ("-1", None),
            (r#""gold""#, None),
            ("1.5", None),
            ("true", None),
        ] {
            let c = canon(&format!(r#"{{"service":"s","severity":{input}}}"#));
            let got = c
                .obj
                .get(trawl_core::schema::SEVERITY)
                .and_then(Value::as_i64);
            assert_eq!(got, expected, "severity {input}");
            assert_eq!(
                c.severity_unmapped,
                expected.is_none(),
                "the counter fires exactly when a source mapped to nothing: {input}"
            );
            // Whatever the reading, the source is never touched.
            assert!(c.obj.contains_key("severity"), "source kept: {input}");
        }
    }

    /// Source precedence is `severity` → `severity_text` → `level`, first
    /// mappable wins, and every source stays where it is.
    #[test]
    fn severity_source_precedence_is_first_mappable() {
        let cases: &[(&str, Option<i64>)] = &[
            // severity leads.
            (
                r#""severity":17,"severity_text":"info","level":"debug""#,
                Some(17),
            ),
            // an unmappable severity falls through to severity_text…
            (r#""severity":"gold","severity_text":"error""#, Some(17)),
            // …and on to level.
            (
                r#""severity":"gold","severity_text":"gold","level":"warn""#,
                Some(13),
            ),
            // severity_text leads level.
            (r#""severity_text":"error","level":"info""#, Some(17)),
            // nothing mappable anywhere.
            (r#""severity":"gold","level":"silver""#, None),
        ];
        for (fields, expected) in cases {
            let c = canon(&format!(r#"{{"service":"s",{fields}}}"#));
            assert_eq!(
                c.obj
                    .get(trawl_core::schema::SEVERITY)
                    .and_then(Value::as_i64),
                *expected,
                "{fields}"
            );
            for source in ["severity", "severity_text", "level"] {
                if fields.contains(&format!("\"{source}\":")) {
                    assert!(c.obj.contains_key(source), "{source} consumed by {fields}");
                }
            }
        }
    }

    /// An incoming `_severity` is derivation-only (ADR-0013 §3): it takes
    /// the standard prefix strip and lands as bare `severity`, which
    /// derivation then reads — so an OTel-native shipper still lands
    /// correctly, with zero special-casing and an unforgeable verdict.
    #[test]
    fn a_forged_severity_strips_to_the_bare_name_and_is_then_derived() {
        let c = canon(r#"{"service":"s","_severity":17}"#);
        assert_eq!(c.obj["severity"], 17, "the forged key lands bare");
        assert_eq!(c.obj[trawl_core::schema::SEVERITY], 17, "derived from it");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
    }

    /// An event with no severity-ish field at all is silent on every
    /// channel: no key, no repair, no counter.
    #[test]
    fn absent_severity_sources_are_silent() {
        let c = canon(r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z"}"#);
        assert!(!c.obj.contains_key(trawl_core::schema::SEVERITY));
        assert!(!c.severity_unmapped);
        assert!(c.repairs.is_empty());
    }

    /// Token spellings and the ASCII fold both reach the derivation.
    #[test]
    fn severity_token_spellings_normalize() {
        for spelling in [r#""WARN""#, r#""warning""#, r#""Warn""#, r#""W""#] {
            let c = canon(&format!(r#"{{"service":"s","level":{spelling}}}"#));
            assert_eq!(
                c.obj[trawl_core::schema::SEVERITY],
                13,
                "spelling {spelling} must map to 13"
            );
        }
    }

    // --- the sealed `_` namespace at the ingest door (ADR-0013 §5) ---

    /// One rule, every shape: journald names, a forged server slot, and
    /// the internal provenance key alike.
    #[test]
    fn reserved_prefixes_strip_to_the_bare_remainder() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z",
                "_HOSTNAME":"box","__name__":"up","_SYSTEMD_UNIT":"sshd.service",
                "_ingested":"1999-01-01T00:00:00Z","_repairs":"forged",
                "_trawl_wal_file":"x","keep":"me"}"#,
        );
        assert_eq!(c.obj["hostname"], "box", "_HOSTNAME folds then strips");
        assert_eq!(c.obj["name__"], "up", "only the LEADING run is stripped");
        assert_eq!(c.obj["systemd_unit"], "sshd.service");
        assert_eq!(c.obj["trawl_wal_file"], "x", "no silent hand-written drop");
        assert_eq!(c.obj["keep"], "me");
        // The server's own slots are stamped, never the client's values.
        assert_eq!(c.obj["_ingested"], ARRIVAL);
        assert_eq!(c.obj["ingested"], "1999-01-01T00:00:00Z");
        assert_eq!(c.obj["repairs"], "forged");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
    }

    /// The strip runs before identity resolution, so a stripped
    /// `_host`/`_env`/`_service` is sender-asserted data the server's own
    /// fills can neither overwrite nor misreport in `_repairs`.
    #[test]
    fn a_stripped_identity_name_is_sender_asserted_not_overwritten() {
        let c = canon(r#"{"service":"s","_host":"real-origin","message":"m"}"#);
        assert_eq!(
            c.obj["host"], "real-origin",
            "the peer must not stamp over it"
        );
        assert!(
            !codes(&c).contains(&"host.from_peer"),
            "the sender asserted a host: {:?}",
            c.repairs
        );
        assert!(codes(&c).contains(&"field.reserved_prefix"));

        let c = canon(r#"{"service":"s","host":"h","_env":"lab","message":"m"}"#);
        assert_eq!(c.obj["env"], "lab");
        assert_eq!(c.env, "lab", "the partition follows the stripped value");
        assert!(
            !codes(&c).contains(&"env.defaulted"),
            "the sender asserted an env: {:?}",
            c.repairs
        );

        // Case-folded first, so the prefixed spelling makes no difference.
        let c = canon(r#"{"service":"s","_HOST":"real-origin","message":"m"}"#);
        assert_eq!(c.obj["host"], "real-origin");
        assert!(!codes(&c).contains(&"host.from_peer"));

        // And the stripped value is validated like any other: an env
        // outside the allowlist rejects rather than silently defaulting.
        let (_, reason) = reject(r#"{"service":"s","host":"h","_env":"nope"}"#);
        assert!(matches!(reason, RejectReason::EnvNotAllowed), "{reason:?}");

        // `_service` is the same rule: it lands bare and is the service.
        let c = canon(r#"{"_service":"other","message":"m"}"#);
        assert_eq!(c.service, "other");
        assert_eq!(c.obj["service"], "other");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
    }

    /// A key with no bare remainder has nowhere to land, so it is
    /// dropped — its value is still in `_raw`.
    #[test]
    fn an_all_underscore_name_is_dropped_not_stored() {
        let c = canon(r#"{"service":"s","_":"a","___":"b","keep":"me"}"#);
        assert!(!c.obj.contains_key("_"));
        assert!(!c.obj.contains_key("___"));
        assert!(!c.obj.contains_key(""));
        assert_eq!(c.obj["keep"], "me");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
        match &c.obj["_raw"] {
            Value::String(raw) => assert!(raw.contains("\"___\":\"b\""), "{raw}"),
            other => panic!("_raw must be a string: {other:?}"),
        }
    }

    /// The bare name already present in the same event wins; the
    /// prefixed loser is dropped, deterministically.
    #[test]
    fn a_prefixed_name_loses_to_the_bare_one_it_would_shadow() {
        let c = canon(r#"{"service":"s","_dur":1,"dur":2}"#);
        assert_eq!(c.obj["dur"], 2, "the bare spelling wins");
        assert!(!c.obj.contains_key("_dur"));
        assert!(codes(&c).contains(&"field.reserved_prefix_collision"));

        // Two prefixed claimants for one bare name: map order decides
        // (serde_json::Map iterates sorted, and `_` < `d`, so `__dur`
        // is the ASCII-lexicographically first spelling).
        let c = canon(r#"{"service":"s","_dur":1,"__dur":2}"#);
        assert_eq!(c.obj["dur"], 2, "the first in map order wins");
        assert!(codes(&c).contains(&"field.reserved_prefix_collision"));
    }

    /// `_time` and a string `_raw` are the two slots a sender may
    /// propose, so neither strips.
    #[test]
    fn the_two_proposable_slots_are_not_stripped() {
        let c =
            canon(r#"{"service":"s","_time":"2025-12-31T23:00:00Z","_raw":"a line","keep":"me"}"#);
        assert_eq!(c.obj["_time"], "2025-12-31T23:00:00.000000Z");
        assert_eq!(c.obj["_raw"], "a line");
        assert!(!c.obj.contains_key("time"));
        assert!(!c.obj.contains_key("raw"));
        assert!(
            !codes(&c)
                .iter()
                .any(|code| code.starts_with("field.reserved")),
            "a proposable slot is not a strip: {:?}",
            c.repairs
        );
    }

    /// A non-string `_raw` is not a proposal, so it takes the standard
    /// strip and lands bare.
    #[test]
    fn a_non_string_raw_strips_like_any_other_reserved_name() {
        let c = canon(r#"{"service":"s","_raw":{"a":1},"keep":"me"}"#);
        match &c.obj["_raw"] {
            Value::String(raw) => assert!(raw.contains("service"), "server-filled: {raw}"),
            other => panic!("_raw must be the server's serialization: {other:?}"),
        }
        assert_eq!(c.obj["raw"], "{\"a\":1}", "the client value lands bare");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
    }

    // --- time derivation: observe, never consume (ADR-0013 §2) ---

    /// `timestamp`/`@timestamp` are read for the derivation and stored
    /// verbatim as ordinary columns; only `_time` — the proposal slot —
    /// is consumed.
    #[test]
    fn time_alias_sources_are_stored_verbatim() {
        let c = canon(r#"{"service":"s","timestamp":"2025-06-01T12:00:00Z"}"#);
        assert_eq!(c.obj["_time"], "2025-06-01T12:00:00.000000Z");
        assert_eq!(
            c.obj["timestamp"], "2025-06-01T12:00:00Z",
            "the source is the sender's own column, uncanonicalized"
        );

        let c = canon(r#"{"service":"s","@timestamp":"2025-06-01T12:00:00Z"}"#);
        assert_eq!(c.obj["_time"], "2025-06-01T12:00:00.000000Z");
        assert_eq!(c.obj["@timestamp"], "2025-06-01T12:00:00Z");
    }

    /// First present claims the derivation: an unparseable `_time` falls
    /// to arrival time rather than reaching past itself to a
    /// lower-precedence source.
    #[test]
    fn an_unparseable_time_proposal_does_not_fall_through_to_a_lower_source() {
        let c = canon(r#"{"service":"s","_time":"not a time","timestamp":"2025-06-01T12:00:00Z"}"#);
        assert_eq!(c.obj["_time"], ARRIVAL);
        assert!(codes(&c).contains(&"time.from_ingest"));
        assert_eq!(c.obj["timestamp"], "2025-06-01T12:00:00Z", "still stored");
    }

    // --- _raw: pre-defaults capture, client honour ---

    #[test]
    fn raw_captured_before_host_fill() {
        // No host: the server fills 10.0.4.55, but _raw must not contain it.
        let c = canon(r#"{"service":"s","env":"prod","_time":"2025-12-31T23:00:00Z"}"#);
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(
            !raw.contains("10.0.4.55"),
            "_raw must not contain the server-filled host: {raw}"
        );
        assert_eq!(c.obj["host"], "10.0.4.55");
    }

    #[test]
    fn client_string_raw_survives_verbatim() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_raw":"<134>1 the wire line"}"#,
        );
        assert_eq!(c.obj["_raw"], "<134>1 the wire line");
        assert!(c.repairs.is_empty());
    }

    #[test]
    fn non_string_raw_replaced_with_the_server_serialization() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_raw":{"nested":1}}"#,
        );
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.starts_with('{'), "server-filled canonical form: {raw}");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
    }

    #[test]
    fn client_ingested_and_repairs_strip_to_bare_names() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_ingested":"1999-01-01T00:00:00Z","_repairs":"forged"}"#,
        );
        assert_eq!(c.obj["_ingested"], ARRIVAL, "server stamp wins");
        assert_eq!(
            c.obj["_repairs"], "field.reserved_prefix",
            "forged _repairs replaced by the strip record"
        );
        assert_eq!(c.obj["repairs"], "forged", "the client value lands bare");
        assert_eq!(c.obj["ingested"], "1999-01-01T00:00:00Z");
    }

    #[test]
    fn raw_contains_stripped_client_meta() {
        // _raw is "what arrived" — including the forged fields.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_repairs":"forged"}"#,
        );
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.contains("forged"), "got: {raw}");
    }

    #[test]
    fn oversized_raw_truncated_on_char_boundary() {
        let big: String = "é".repeat(MAX_RAW_CHARS + 50);
        let c = canon(&format!(
            r#"{{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_raw":"{big}"}}"#
        ));
        let raw = c.obj["_raw"].as_str().unwrap();
        assert_eq!(raw.chars().count(), MAX_RAW_CHARS);
        assert_eq!(raw, "é".repeat(MAX_RAW_CHARS));
        assert!(codes(&c).contains(&"field.truncated"));
    }

    // --- field-name bound (catalog key safety) ---

    #[test]
    fn overlong_field_name_is_dropped_not_fatal() {
        // An unbounded key would be proposed as a catalog pin and overflow
        // the `field_types.field` btree, retaining the batch for retry on
        // every compaction tick forever.
        let long = "k".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES + 1);
        let c = canon(&format!(
            r#"{{"service":"s","env":"prod","host":"h",
                 "_time":"2025-12-31T23:00:00Z","{long}":"v","ok":"kept"}}"#
        ));
        assert!(!c.obj.contains_key(&long), "over-long key must not survive");
        assert_eq!(c.obj["ok"], "kept", "other fields are untouched");
        assert!(codes(&c).contains(&"field.name_too_long"));
        // The value stays recoverable: `_raw` is captured before the drop.
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.contains(&long), "dropped key must remain in _raw");
    }

    #[test]
    fn field_name_at_the_cap_is_kept() {
        let at_cap = "k".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES);
        let c = canon(&format!(
            r#"{{"service":"s","env":"prod","host":"h",
                 "_time":"2025-12-31T23:00:00Z","{at_cap}":"v"}}"#
        ));
        assert_eq!(c.obj[&at_cap], "v");
        assert!(c.repairs.is_empty(), "clean event: {:?}", c.repairs);
    }

    #[test]
    fn field_name_bound_counts_bytes_not_chars() {
        // The constraint respected is postgres' byte-sized btree key limit,
        // so a multi-byte name under the char count still has to fit.
        let multibyte = "é".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES / 2 + 1);
        assert!(multibyte.chars().count() <= trawl_core::schema::MAX_FIELD_NAME_BYTES);
        assert!(!trawl_core::schema::is_storable_field_name(&multibyte));
    }

    // --- field-name ASCII case-folding (DuckDB folds ASCII case) ---

    #[test]
    fn exact_envelope_keys_cannot_be_shadowed_by_case_variants() {
        // A case-variant of an envelope column names the same DuckDB
        // column; when the exact spelling is present too, the exact one
        // wins and the variant's value is dropped — anything else would let
        // a sender holding `ingest` forge `_time`/`service`/`host`/etc.
        let c = canon(
            r#"{"service":"realsvc","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_Time":"1999-01-01T00:00:00Z",
                "Service":"spoofed","HOST":"spoofed","Env":"lab",
                "_Repairs":"none","_Trawl_Wal_File":"x"}"#,
        );
        for forged in [
            "_Time",
            "Service",
            "HOST",
            "Env",
            "_Repairs",
            "_Trawl_Wal_File",
        ] {
            assert!(
                !c.obj.contains_key(forged),
                "{forged} must not reach the WAL"
            );
        }
        assert_eq!(c.obj["_time"], "2025-12-31T23:00:00.000000Z");
        assert_eq!(c.service, "realsvc");
        assert_eq!(c.obj["service"], "realsvc");
        assert_eq!(c.obj["host"], "h");
        assert_eq!(c.obj["env"], "prod");
        assert!(codes(&c).contains(&"field.name_case_collision"));
        // Nothing is lost: the dropped keys stay findable in `_raw`.
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.contains("spoofed"), "got: {raw}");
    }

    #[test]
    fn lone_case_variant_envelope_keys_fold_and_are_consumed_canonically() {
        // With no exact counterpart, a case-variant is the field: `_Time`
        // becomes the `_time` wire input, `Message` becomes `message`,
        // `SEVERITY` joins the severity chain — one code path, one spelling.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_Time":"2025-12-31T23:00:00Z","Message":"m",
                "SEVERITY":"error","Custom_Field":1}"#,
        );
        assert_eq!(c.obj["_time"], "2025-12-31T23:00:00.000000Z");
        assert_eq!(c.obj["message"], "m");
        assert_eq!(c.obj["severity"], "error", "the source is stored verbatim");
        assert_eq!(c.obj[trawl_core::schema::SEVERITY], 17, "and derived from");
        assert_eq!(c.obj["custom_field"], 1);
        for original in ["_Time", "Message", "SEVERITY", "Custom_Field"] {
            assert!(!c.obj.contains_key(original), "{original} must be folded");
        }
        assert!(codes(&c).contains(&"field.name_case_folded"));
        assert!(
            !codes(&c).contains(&"field.name_case_collision"),
            "no two keys collided: {:?}",
            codes(&c)
        );
        assert!(
            !codes(&c).contains(&"time.from_ingest"),
            "the folded _Time is a valid time input, not a repair"
        );
        // The original spellings stay findable in `_raw`.
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.contains("Custom_Field"), "got: {raw}");
    }

    #[test]
    fn case_variant_ingested_folds_then_takes_the_reserved_prefix_strip() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_Ingested":"1999-01-01T00:00:00Z"}"#,
        );
        assert_eq!(c.obj["_ingested"], ARRIVAL, "server stamp wins");
        assert_eq!(c.obj["ingested"], "1999-01-01T00:00:00Z");
        assert!(codes(&c).contains(&"field.reserved_prefix"));
        assert!(codes(&c).contains(&"field.name_case_folded"));
    }

    #[test]
    fn in_event_collision_keeps_the_exact_spelling_deterministically() {
        // {"Status":200,"status":"ok"}: one DuckDB column, two values. The
        // exact-lowercase spelling wins; the variant is dropped with a code.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","Status":200,"status":"ok"}"#,
        );
        assert_eq!(c.obj["status"], "ok", "the exact spelling's value wins");
        assert!(!c.obj.contains_key("Status"));
        assert!(codes(&c).contains(&"field.name_case_collision"));
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(
            raw.contains("200"),
            "the dropped value stays in _raw: {raw}"
        );
    }

    #[test]
    fn variant_only_collision_takes_the_lexicographically_first_key() {
        // No exact spelling present: serde_json::Map iterates sorted, so
        // "STATUS" < "Status" and the first variant's value wins.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","Status":2,"STATUS":1}"#,
        );
        assert_eq!(c.obj["status"], 1);
        assert!(codes(&c).contains(&"field.name_case_folded"));
        assert!(codes(&c).contains(&"field.name_case_collision"));
    }

    #[test]
    fn folding_is_ascii_only_and_lowercase_names_are_untouched() {
        // Per-character ASCII lowercase is exactly DuckDB's identifier
        // equivalence: `CAFÉ` folds to `cafÉ` (same DuckDB identifier),
        // which stays distinct from an all-lowercase `café`. Names with no
        // ASCII uppercase are untouched and record nothing.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","message":"m",
                "hostname":"h2","service_id":7,"café":"lower","CAFÉ":"upper"}"#,
        );
        assert_eq!(c.obj["message"], "m");
        assert_eq!(c.obj["hostname"], "h2");
        assert_eq!(c.obj["service_id"], 7);
        assert_eq!(c.obj["café"], "lower");
        assert_eq!(c.obj["cafÉ"], "upper", "ASCII chars fold, é does not");
        assert!(!c.obj.contains_key("CAFÉ"));
        assert_eq!(codes(&c), vec!["field.name_case_folded"]);
    }

    #[test]
    fn all_lowercase_event_records_no_fold_repair() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","duration":410}"#,
        );
        assert!(c.repairs.is_empty(), "clean event: {:?}", c.repairs);
    }

    #[test]
    fn folded_severity_inputs_join_the_derivation_chain() {
        // `Severity` is `severity` and `LEVEL` is `level`: one spelling
        // reaches the derivation, and both sources are stored verbatim.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","Severity":"error","LEVEL":"warn"}"#,
        );
        assert!(!c.obj.contains_key("Severity"));
        assert!(!c.obj.contains_key("LEVEL"));
        assert_eq!(c.obj["severity"], "error");
        assert_eq!(c.obj["level"], "warn", "sources are never consumed");
        assert_eq!(c.obj[trawl_core::schema::SEVERITY], 17, "severity leads");
        assert!(codes(&c).contains(&"field.name_case_folded"));
    }

    #[test]
    fn service_supplied_under_a_case_variant_validates() {
        let c = canon(
            r#"{"Service":"api.v2","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z"}"#,
        );
        assert_eq!(c.service, "api.v2");
        assert_eq!(c.obj["service"], "api.v2");
        assert!(codes(&c).contains(&"field.name_case_folded"));
    }

    #[test]
    fn client_raw_under_a_case_variant_is_honoured_verbatim() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_RAW":"<134>1 the wire line"}"#,
        );
        assert_eq!(c.obj["_raw"], "<134>1 the wire line");
        assert!(codes(&c).contains(&"field.name_case_folded"));
    }

    // --- _time grammar (the ADR-0008 corpus) ---

    #[test]
    fn time_valid_values_canonicalized() {
        let cases = [
            ("2025-06-01T12:00:00Z", "2025-06-01T12:00:00.000000Z"),
            (
                "2025-06-01T17:30:00.123456+05:30",
                "2025-06-01T12:00:00.123456Z",
            ),
            (
                "2025-06-01T12:00:00.123456789Z",
                "2025-06-01T12:00:00.123456Z",
            ),
            ("2025-06-01 12:00:00", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01 12:00:00.5", "2025-06-01T12:00:00.500000Z"),
            ("2025-06-01T12:00:00", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01T12:00:00.123", "2025-06-01T12:00:00.123000Z"),
            ("2025-06-01", "2025-06-01T00:00:00.000000Z"),
            (
                "2025-06-01T12:00:00.000+0000",
                "2025-06-01T12:00:00.000000Z",
            ),
            ("2025-06-01T17:30:00+0530", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01 14:00:00+02", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01T12:00", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01 12:00", "2025-06-01T12:00:00.000000Z"),
            ("2025-06-01T14:00+02:00", "2025-06-01T12:00:00.000000Z"),
            ("2025/06/01 12:00:00", "2025-06-01T12:00:00.000000Z"),
            (
                "2025/06/01 12:00:00.25+00:00",
                "2025-06-01T12:00:00.250000Z",
            ),
            ("2025/06/01", "2025-06-01T00:00:00.000000Z"),
            ("  2025-06-01T12:00:00Z  ", "2025-06-01T12:00:00.000000Z"),
            (" 2025-06-01 12:00:00 ", "2025-06-01T12:00:00.000000Z"),
        ];
        for (input, expected) in cases {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","timestamp":"{input}"}}"#
            ));
            assert_eq!(
                c.obj["_time"], expected,
                "input {input:?} should canonicalize"
            );
            assert!(
                !codes(&c).contains(&"time.from_ingest"),
                "valid input {input:?} is not a repair"
            );
        }
    }

    #[test]
    fn time_malformed_substituted_and_flagged() {
        for input in [
            r#""not-a-date""#,
            r#""2026-13-45T99:99:99Z""#,
            r#""2025-06-31""#,
            r#""1748779200""#,
            r#"{"nested":1}"#,
            "12345",
            "true",
            "null",
            r#""""#,
        ] {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","timestamp":{input}}}"#
            ));
            assert_eq!(
                c.obj["_time"], ARRIVAL,
                "malformed input {input} must be substituted with arrival"
            );
            assert!(
                codes(&c).contains(&"time.from_ingest"),
                "malformed input {input} must be flagged"
            );
        }
    }

    #[test]
    fn time_out_of_range_kept_but_flagged() {
        // >10y past and >1d future parse fine but are implausible.
        for input in ["1999-01-01T00:00:00Z", "2026-01-03T00:00:00Z"] {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","_time":"{input}"}}"#
            ));
            assert_ne!(c.obj["_time"], ARRIVAL, "out-of-range is KEPT: {input}");
            assert!(codes(&c).contains(&"time.out_of_range"), "input {input}");
            assert!(!codes(&c).contains(&"time.from_ingest"), "input {input}");
        }
    }

    #[test]
    fn time_sources_are_read_in_precedence_order_and_kept() {
        // _time wins over timestamp wins over @timestamp; only `_time` —
        // the proposal slot — is consumed (ADR-0013 §2).
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-06-01T10:00:00Z",
                "timestamp":"2025-06-01T11:00:00Z",
                "@timestamp":"2025-06-01T12:00:00Z"}"#,
        );
        assert_eq!(c.obj["_time"], "2025-06-01T10:00:00.000000Z");
        assert_eq!(c.obj["timestamp"], "2025-06-01T11:00:00Z");
        assert_eq!(c.obj["@timestamp"], "2025-06-01T12:00:00Z");

        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "timestamp":"2025-06-01T11:00:00Z",
                "@timestamp":"2025-06-01T12:00:00Z"}"#,
        );
        assert_eq!(c.obj["_time"], "2025-06-01T11:00:00.000000Z");

        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "@timestamp":"2025-06-01T12:00:00Z"}"#,
        );
        assert_eq!(c.obj["_time"], "2025-06-01T12:00:00.000000Z");
    }

    // --- multiple repairs aggregate ---

    #[test]
    fn multiple_repairs_join_comma_separated() {
        let e = envs(&["prod"]);
        let ctx = ctx_with(&e, false);
        let c = canonicalize(&event(r#"{"service":"s","_HOSTNAME":"box"}"#), &ctx).unwrap();
        let repairs = c.obj["_repairs"].as_str().unwrap();
        for code in [
            "time.from_ingest",
            "env.defaulted",
            "host.from_peer",
            "field.reserved_prefix",
        ] {
            assert!(repairs.contains(code), "missing {code} in {repairs}");
        }
        assert!(repairs.contains(','));
    }

    // --- producer profiles (ADR-0013) ---
    //
    // The matrix above covers the HTTP door; these cover what a profile
    // door adds on top of it.

    /// A profile context: asserted identity plus the packaged derivation.
    fn profile_ctx<'a>(
        envs: &'a [String],
        producer: Producer<'a>,
        derivation: &'a Derivation,
    ) -> EnvelopeContext<'a> {
        EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: arrival_instant(),
            envs,
            default_env: &envs[0],
            producer,
            derivation,
        }
    }

    fn asserted<'a>(
        env: &'a str,
        service: &'a str,
        host: Option<&'a str>,
        message: Option<&'a str>,
        repairs: &'a [RepairCode],
    ) -> Asserted<'a> {
        Asserted {
            env,
            service,
            host,
            message,
            repairs,
        }
    }

    /// Canonicalize a payload through a non-HTTP door.
    fn canon_profile(json: &str, producer: Producer<'_>) -> Canonical {
        let e = envs(&["prod", "lab"]);
        let d = default_derivation();
        canonicalize(&event(json), &profile_ctx(&e, producer, d)).expect("event must canonicalize")
    }

    #[test]
    fn every_door_stamps_its_own_producer() {
        // One spelling, the one `ProducerKind` publishes, so a query and
        // a metric label can never disagree about what to call a door.
        let http = canon(r#"{"service":"s","env":"prod","host":"h"}"#);
        assert_eq!(http.obj["_producer"], "http");

        let a = asserted("prod", "unifi", Some("gw"), Some("link down"), &[]);
        assert_eq!(
            canon_profile(r#"{"syslog_severity":3}"#, Producer::Syslog(a)).obj["_producer"],
            "syslog"
        );
        let t = asserted("prod", "trawld", Some("box"), None, &[]);
        assert_eq!(
            canon_profile(r#"{"message":"started"}"#, Producer::Trawld(t)).obj["_producer"],
            "trawld"
        );
    }

    #[test]
    fn an_incoming_producer_key_cannot_forge_the_column() {
        // `_producer` is server-stamped, so a client's copy takes the
        // standard reserved-prefix strip (ADR-0013 §5) and lands as a
        // bare `producer` column — queryable, but not the envelope slot.
        let c = canon(r#"{"service":"s","env":"prod","host":"h","_producer":"syslog"}"#);
        assert_eq!(
            c.obj["_producer"], "http",
            "the door stamps, not the sender"
        );
        assert_eq!(
            c.obj["producer"], "syslog",
            "the sender's value stays queryable"
        );
        assert!(codes(&c).contains(&"field.reserved_prefix"));

        // Same rule on a profile door, and the assertion order holds:
        // the strip runs before the stamp, so the two never race.
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(r#"{"_PRODUCER":"http","msg":"x"}"#, Producer::Syslog(a));
        assert_eq!(c.obj["_producer"], "syslog");
        assert_eq!(c.obj["producer"], "http");
    }

    #[test]
    fn a_profile_assertion_beats_a_colliding_payload_key() {
        // Telemetry's own `service`/`host` fields are ordinary sender
        // vocabulary, with no `trawld_` prefix: identity is protected by
        // precedence. The displaced values stay in `_raw`.
        let a = asserted("prod", "trawld", Some("box"), None, &[]);
        let c = canon_profile(
            r#"{"service":"nginx","host":"web01","env":"lab","message":"boom"}"#,
            Producer::Trawld(a),
        );
        assert_eq!(c.service, "trawld");
        assert_eq!(c.obj["service"], "trawld");
        assert_eq!(c.obj["host"], "box");
        assert_eq!(c.env, "prod");
        assert_eq!(c.obj["env"], "prod");
        // `message: None` asserts nothing: the payload is the message.
        assert_eq!(c.obj["message"], "boom");
        assert!(codes(&c).contains(&"field.producer_asserted"));
        let raw = c.obj["_raw"].as_str().unwrap();
        for displaced in ["nginx", "web01", "lab"] {
            assert!(
                raw.contains(displaced),
                "{displaced} must stay in _raw: {raw}"
            );
        }
    }

    #[test]
    fn an_identical_payload_value_is_not_a_collision() {
        // A code on every well-formed frame would leave `_repairs` almost
        // never null, and nothing was displaced: the sender agreed.
        let a = asserted("prod", "unifi", Some("gw"), Some("link down"), &[]);
        let c = canon_profile(
            r#"{"service":"unifi","host":"gw","env":"prod","message":"link down",
                "_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert!(c.repairs.is_empty(), "clean profile event: {:?}", c.repairs);
        assert!(!c.obj.contains_key("_repairs"));
    }

    #[test]
    fn trawld_never_lets_a_payload_field_claim_the_raw_lifeline() {
        // `_raw` proposability is per-profile: HTTP and syslog have an
        // original form trawl never saw, while trawld's payload is that
        // original. Letting a tracing field claim the slot would make "a
        // displaced value stays findable in `_raw`" false on the door that
        // asserts identity hardest.
        let a = asserted("prod", "trawld", Some("box"), None, &[]);
        let c = canon_profile(
            r#"{"_raw":"a tracing field, not a wire line","service":"nginx","message":"boom"}"#,
            Producer::Trawld(a),
        );
        let raw = c.obj["_raw"].as_str().expect("_raw is a string");
        assert!(
            raw.contains("\"service\":\"nginx\""),
            "_raw must be the pre-repair serialization carrying the displaced value: {raw}"
        );
        assert_eq!(
            c.obj["raw"], "a tracing field, not a wire line",
            "the payload key takes the ordinary reserved-prefix strip"
        );
        assert_eq!(c.obj["service"], "trawld");
        for code in ["field.reserved_prefix", "field.producer_asserted"] {
            assert!(codes(&c).contains(&code), "missing {code}: {:?}", codes(&c));
        }

        // On the other two doors a string `_raw` is the sender's own
        // pre-parse line (or the listener's frame) verbatim.
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(r#"{"_raw":"<13>the frame","msg":"x"}"#, Producer::Syslog(a));
        assert_eq!(c.obj["_raw"], "<13>the frame");
        assert!(!c.obj.contains_key("raw"));
        let c = canon(r#"{"service":"s","env":"prod","host":"h","_raw":"the line"}"#);
        assert_eq!(c.obj["_raw"], "the line");
    }

    #[test]
    fn a_payload_null_in_an_asserted_slot_is_absence_not_a_collision() {
        // An explicit null is what most serializers emit for an unset
        // field. Reading it as a competing claim would put
        // `field.producer_asserted` on a huge share of well-formed
        // events, so null counts as absence in the collision judgement,
        // and is still removed, because an optional envelope field is
        // omitted, never written as JSON null (ADR-0009).
        let a = asserted("prod", "unifi", None, None, &[]);
        let c = canon_profile(
            r#"{"host":null,"msg":"x","_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert!(
            !c.obj.contains_key("host"),
            "host must be OMITTED, not null"
        );
        assert_eq!(codes(&c), vec!["host.omitted"]);

        // Same rule on a slot the profile does assert.
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(
            r#"{"host":null,"service":null,"msg":"x","_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert_eq!(c.obj["host"], "gw");
        assert_eq!(c.obj["service"], "unifi");
        assert!(
            c.repairs.is_empty(),
            "a null claims nothing: {:?}",
            c.repairs
        );
    }

    #[test]
    fn a_profile_with_no_honest_host_keeps_the_event_and_omits_it() {
        // Absent but honest beats both the peer-fill lie and dropping the
        // event, so a hostname-less frame behind a trusted relay lands.
        let a = asserted("prod", "unifi", None, None, &[]);
        let c = canon_profile(
            r#"{"msg":"x","_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert!(
            !c.obj.contains_key("host"),
            "host must be OMITTED, not null"
        );
        assert_eq!(codes(&c), vec!["host.omitted"]);
        // Nothing peer-fills a profile door: there is no peer on it.
        assert!(!codes(&c).contains(&"host.from_peer"));

        // And a payload key cannot supply what the profile denied.
        let a = asserted("prod", "unifi", None, None, &[]);
        let c = canon_profile(
            r#"{"host":"whatever","_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert!(!c.obj.contains_key("host"));
        assert!(codes(&c).contains(&"host.omitted"));
        assert!(codes(&c).contains(&"field.producer_asserted"));
    }

    #[test]
    fn producer_contributed_codes_lead_the_repairs_list() {
        // Producers contribute codes; only the door assembles `_repairs`.
        // The producer's own codes describe what happened before the door,
        // so they come first.
        let a = asserted(
            "prod",
            "syslog",
            Some("gw"),
            None,
            &[RepairCode::ServiceFromProfile],
        );
        let c = canon_profile(r#"{"msg":"x"}"#, Producer::Syslog(a));
        assert_eq!(codes(&c), vec!["service.from_profile", "time.from_ingest"]);
        assert_eq!(
            c.obj["_repairs"], "service.from_profile,time.from_ingest",
            "the joined column mirrors the vector"
        );
    }

    #[test]
    fn a_profile_never_defaults_its_env() {
        // Env comes from boot-validated config, so `env.defaulted` is
        // structurally unreachable on a profile door, even when the payload
        // carries no env at all.
        let a = asserted("lab", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(
            r#"{"msg":"x","_time":"2025-12-31T23:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert_eq!(c.env, "lab");
        assert!(!codes(&c).contains(&"env.defaulted"));
    }

    #[test]
    fn a_profile_faces_every_universal_gate() {
        // No profile bypasses the fold, the strip, the name-length drop,
        // the nested stringify or the `_raw` cap.
        let long = "k".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES + 1);
        let payload = format!(
            r#"{{"_HOSTNAME":"box","Dur":12,"nest":{{"a":1}},"{long}":"x",
                "big":"{}"}}"#,
            "z".repeat(MAX_RAW_CHARS + 10)
        );
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(&payload, Producer::Syslog(a));
        assert_eq!(c.obj["hostname"], "box", "the sealed prefix is stripped");
        assert_eq!(c.obj["dur"], 12, "names are ASCII-folded");
        assert_eq!(c.obj["nest"], r#"{"a":1}"#, "nested values stringify");
        assert!(!c.obj.contains_key(&long), "an unstorable name is dropped");
        assert_eq!(
            c.obj["_raw"].as_str().unwrap().chars().count(),
            MAX_RAW_CHARS,
            "_raw is capped on every door"
        );
        for code in [
            "field.reserved_prefix",
            "field.name_case_folded",
            "field.name_too_long",
            "field.truncated",
        ] {
            assert!(codes(&c).contains(&code), "missing {code}: {:?}", codes(&c));
        }
    }

    #[test]
    fn an_invalid_asserted_service_rejects_through_the_doors_own_validator() {
        // Step 2.5 stamps the assertion into the same slot the validator
        // reads, so a profile gets exactly the HTTP door's checks. The
        // caller counts the drop; the door only refuses.
        let e = envs(&["prod"]);
        let d = default_derivation();
        let a = asserted("prod", "../escaped", Some("gw"), None, &[]);
        let (_, reason) = canonicalize(
            &event(r#"{"msg":"x"}"#),
            &profile_ctx(&e, Producer::Syslog(a), d),
        )
        .expect_err("an unusable asserted service must refuse");
        assert_eq!(reason, RejectReason::InvalidChars);
    }

    // --- per-profile derivation (ADR-0013) ---

    #[test]
    fn the_syslog_profile_derives_severity_from_its_own_artifact() {
        // The listener writes no `_severity`. It publishes the raw 0-7
        // numeral and the profile's fixed source inverts it: syslog 3 (err)
        // is OTel 17, not OTel 3 (trace3).
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(r#"{"syslog_severity":3}"#, Producer::Syslog(a));
        assert_eq!(c.obj["_severity"], 17);
        assert_eq!(c.obj["syslog_severity"], 3, "the artifact stays queryable");

        // The very same value on the HTTP door reads as OTel, because
        // nothing there proves the provenance (ADR-0013 §4).
        let c = canon(r#"{"service":"s","env":"prod","host":"h","syslog_severity":3}"#);
        assert!(
            !c.obj.contains_key("_severity"),
            "an unconfigured field is not a severity source at all"
        );
    }

    #[test]
    fn the_syslog_profile_reads_its_frame_timestamp_first() {
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(
            r#"{"syslog_timestamp":"2025-06-01T10:00:00Z","timestamp":"2025-06-01T11:00:00Z"}"#,
            Producer::Syslog(a),
        );
        assert_eq!(c.obj["_time"], "2025-06-01T10:00:00.000000Z");
        assert_eq!(
            c.obj["syslog_timestamp"], "2025-06-01T10:00:00Z",
            "derivation observes; the artifact stays"
        );

        // An omitted artifact (no frame timestamp, or an unparseable one)
        // falls through the configured chain honestly — no listener-side
        // `now()` substitution can hide behind it.
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let c = canon_profile(r#"{"msg":"x"}"#, Producer::Syslog(a));
        assert_eq!(c.obj["_time"], ARRIVAL);
        assert!(codes(&c).contains(&"time.from_ingest"));
    }

    #[test]
    fn a_syslog_over_http_forwarder_inverts_through_the_configured_dialect() {
        // The native listener and a collector forwarding syslog over HTTP
        // reach the same mechanism: configure the artifact as a
        // syslog-dialect source and the HTTP door inverts it too.
        let derivation = Derivation::resolve(&trawl_config::IngestConfig {
            severity_from: vec![
                trawl_config::DerivationSourceSpec::Typed {
                    field: "syslog_severity".to_owned(),
                    dialect: Some("syslog".to_owned()),
                },
                trawl_config::DerivationSourceSpec::Bare("level".to_owned()),
            ],
            ..trawl_config::IngestConfig::default()
        })
        .expect("the forwarder config must resolve");
        let e = envs(&["prod"]);
        let ctx = profile_ctx(
            &e,
            Producer::Http {
                peer_host: "10.0.4.55",
                peer_is_trusted_relay: false,
            },
            &derivation,
        );

        let c = canonicalize(
            &event(r#"{"service":"rsyslog","env":"prod","host":"relay","syslog_severity":3}"#),
            &ctx,
        )
        .unwrap();
        assert_eq!(c.obj["_severity"], 17, "syslog 3 is err, which is OTel 17");
        assert_eq!(c.obj["_producer"], "http");

        // Precedence is the configured order, and the lower-priority
        // source is still read when the first does not map.
        let c = canonicalize(
            &event(
                r#"{"service":"rsyslog","env":"prod","host":"relay",
                       "syslog_severity":"nonsense","level":"warn"}"#,
            ),
            &ctx,
        )
        .unwrap();
        assert_eq!(c.obj["_severity"], 13);
    }

    #[test]
    fn an_empty_configured_severity_list_derives_nothing_but_keeps_the_fixed_source() {
        let derivation = Derivation::resolve(&trawl_config::IngestConfig {
            severity_from: Vec::new(),
            ..trawl_config::IngestConfig::default()
        })
        .unwrap();
        let e = envs(&["prod"]);

        let ctx = profile_ctx(
            &e,
            Producer::Http {
                peer_host: "10.0.4.55",
                peer_is_trusted_relay: false,
            },
            &derivation,
        );
        let c = canonicalize(
            &event(r#"{"service":"s","env":"prod","host":"h","level":"error"}"#),
            &ctx,
        )
        .unwrap();
        assert!(!c.obj.contains_key("_severity"));
        assert_eq!(c.obj["level"], "error", "the source column is untouched");
        assert!(
            !c.severity_unmapped,
            "no source was CONSULTED, so nothing was unmapped"
        );

        // The syslog profile's fixed source is trawl's, not the
        // operator's, and survives an empty configured list.
        let a = asserted("prod", "unifi", Some("gw"), None, &[]);
        let ctx = profile_ctx(&e, Producer::Syslog(a), &derivation);
        let c = canonicalize(&event(r#"{"syslog_severity":4}"#), &ctx).unwrap();
        assert_eq!(c.obj["_severity"], 13, "syslog 4 is warning, OTel 13");
    }
}
