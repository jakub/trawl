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
//! Three producers feed events through here: the HTTP ingest handler, the
//! syslog listener, and internal telemetry (`service:trawld`).

use std::fmt;

use serde_json::{Map, Value, json};

use crate::ingest::compaction;
use crate::ingest::pipeline;

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
    /// severity text did not match the ladder.
    SeverityUnmapped,
    /// a value exceeded the length cap (`_raw`).
    FieldTruncated,
    /// client sent a server-owned field (`_ingested`, `_repairs`, or a
    /// non-string `_raw`); value dropped and replaced.
    MetaStripped,
    /// a field's NAME exceeded [`trawl_core::schema::MAX_FIELD_NAME_BYTES`];
    /// the field was dropped (its value stays findable in `_raw`).
    FieldNameTooLong,
    /// a field's NAME carried ASCII uppercase and was folded to lowercase —
    /// `DuckDB` identifiers are ASCII case-insensitive, so the lowercase
    /// form is the one spelling every downstream layer (catalog, parquet,
    /// hot snapshot) agrees on. The original spelling stays in `_raw`.
    FieldNameCaseFolded,
    /// two field NAMES in one event differed only in ASCII case — one
    /// column as far as `DuckDB` is concerned — so the losing key was
    /// dropped (its value stays findable in `_raw`). The exact-lowercase
    /// spelling wins when present; otherwise the ASCII-lexicographically
    /// first variant does.
    FieldNameCaseCollision,
}

impl RepairCode {
    /// The wire/metric label spelling of the code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostFromPeer => "host.from_peer",
            Self::EnvDefaulted => "env.defaulted",
            Self::TimeFromIngest => "time.from_ingest",
            Self::TimeOutOfRange => "time.out_of_range",
            Self::SeverityUnmapped => "severity.unmapped",
            Self::FieldTruncated => "field.truncated",
            Self::MetaStripped => "meta.stripped",
            Self::FieldNameTooLong => "field.name_too_long",
            Self::FieldNameCaseFolded => "field.name_case_folded",
            Self::FieldNameCaseCollision => "field.name_case_collision",
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
    ];
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-request context the canonicalizer needs: arrival instant, peer
/// identity, and the env allowlist. Threaded in deliberately — this is
/// `validate`'s first config-dependent check, and globals would hide it.
#[derive(Debug)]
pub struct EnvelopeContext<'a> {
    /// RFC 3339 UTC arrival time at microsecond precision — stamped as
    /// `_ingested` and substituted for a missing/unparseable `_time`.
    pub arrival: &'a str,
    /// The same arrival instant, for plausibility windows.
    pub arrival_instant: chrono::DateTime<chrono::Utc>,
    /// Peer IP as a string (from the TCP connection).
    pub peer_host: &'a str,
    /// Whether the peer is inside a configured `trusted_relays` CIDR:
    /// a missing `host` is then rejected instead of repaired.
    pub peer_is_trusted_relay: bool,
    /// The effective env allowlist (never empty).
    pub envs: &'a [String],
    /// Fills a missing `env` (recorded as `env.defaulted`).
    pub default_env: &'a str,
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
    /// envelope fields are OMITTED when absent, never written as JSON
    /// null — `DuckDB` infers JSON for an all-null column (ADR-0009).
    pub obj: Map<String, Value>,
    /// Repair codes applied, in application order (also joined into the
    /// object's `_repairs`).
    pub repairs: Vec<RepairCode>,
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
/// what Java's default logging encoders and Go's `-0700` layouts emit, and the
/// hard `CAST` this path replaced accepted it. `%#z` is chrono's parse-only
/// offset that takes `+HH`, `+HHMM` and `+HH:MM` alike.
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

/// Parse a time value per the ADR-0008 grammar (moved verbatim from the
/// pre-cutover handler, never narrowed).
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
/// IS the value, so the constraints here are the whole safety argument.
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

/// Map a client severity-ish value (`severity_text` / level) to an exact
/// `SeverityNumber`: name tokens case-insensitively, syslog numerals 0-7
/// (string or number) inverted onto the `OTel` ladder.
fn severity_from_value(v: &Value) -> Option<u8> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<u8>() {
                trawl_core::severity::from_syslog(n)
            } else {
                trawl_core::severity::number_for_token(s)
            }
        }
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .and_then(trawl_core::severity::from_syslog),
        _ => None,
    }
}

/// Render a severity-ish value as a stored `severity_text` label: strings
/// verbatim, numbers as their numeral. Anything else has no honest label
/// (it still survives inside `_raw`).
fn severity_label(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Resolve the severity chain onto `out`, consuming the `severity`,
/// `severity_text`, and `level` inputs: a client `severity` integer 1-24
/// wins → else derive from a *string* `severity` → else from
/// `severity_text` → else from `level` → else NULL. `level` is consumed at
/// ingest — the DSL alias would shadow it anyway; the original is always
/// in `_raw`.
///
/// A numeric `severity` is only ever read on the `OTel` ladder, never
/// syslog-inverted: `severity: 0` is `OTel`'s UNSPECIFIED as readily as it
/// is syslog's Emergency, and guessing would stamp FATAL on an OTel-native
/// client. A string `severity` carries no such ambiguity, so
/// `{"severity":"ERROR"}` — the shape GCP/Stackdriver structured logging
/// emits — maps through the same token/syslog table as the other
/// candidates instead of being dropped.
///
/// Stored `severity_text` is the client's verbatim when it is a string,
/// else the `level` value, else the client `severity` whenever that was
/// not a ladder integer. No severity-ish input is ever deleted from the
/// event: an unmappable value leaves `severity` NULL but stays queryable
/// as `severity_text`.
///
/// Omit-when-null: an all-null column in a batch must be absent, not JSON
/// null, or `DuckDB` infers JSON for the column type (ADR-0009). Returns
/// whether the event carried severity-ish input that failed to map
/// (`severity.unmapped`).
fn resolve_severity(out: &mut Map<String, Value>) -> bool {
    let client_severity = out.remove("severity");
    let client_text = out.remove("severity_text");
    let level = out.remove("level");

    let valid_client_number = client_severity
        .as_ref()
        .and_then(Value::as_i64)
        .filter(|n| trawl_core::severity::is_valid_number(*n));

    // Only a string `severity` joins the derivation chain (see above); an
    // out-of-ladder integer falls through to the text candidates.
    let mapped_client_severity = match &client_severity {
        Some(v @ Value::String(_)) => severity_from_value(v),
        _ => None,
    };

    let severity_number = valid_client_number.or_else(|| {
        mapped_client_severity
            .or_else(|| client_text.as_ref().and_then(severity_from_value))
            .or_else(|| level.as_ref().and_then(severity_from_value))
            .map(i64::from)
    });

    // A ladder-valid integer is a number, not a label — it already lives
    // in `severity` and must not also become `severity_text`.
    let unclaimed_severity = if valid_client_number.is_some() {
        None
    } else {
        client_severity.as_ref()
    };
    let stored_text: Option<String> = match &client_text {
        Some(Value::String(s)) => Some(s.clone()),
        _ => severity_label(level.as_ref()).or_else(|| severity_label(unclaimed_severity)),
    };

    let unmapped = severity_number.is_none()
        && (client_severity.is_some() || client_text.is_some() || level.is_some());

    if let Some(n) = severity_number {
        out.insert("severity".into(), json!(n));
    }
    if let Some(t) = stored_text {
        out.insert("severity_text".into(), json!(t));
    }
    unmapped
}

/// Stringify top-level object/array values to their JSON text (ADR-0009
/// slice 2): with nothing left to flatten, a `read_json` "Duplicate name"
/// collision is structurally impossible, so compaction never has to choose
/// between draining a batch and keeping its columns.
///
/// Runs AFTER the `_raw` capture (the nesting stays findable there) and
/// AFTER the reserved-key strip (a forged object `_raw` must strip as
/// meta, not be laundered into a string). This is a canonicalization like
/// RFC 3339 time reformatting, NOT a repair — a code firing on every k8s
/// event would destroy `_repairs`'s NULL-dominance. Reach nested values
/// with `json_extract_string(k8s, '$.pod')`.
fn stringify_nested_values(out: &mut Map<String, Value>) {
    for value in out.values_mut() {
        if value.is_object() || value.is_array() {
            *value = Value::String(value.to_string());
        }
    }
}

/// Remove the fields a client may never set, returning whether any were
/// present (the `meta.stripped` repair): honouring them would let a sender
/// forge its own handling history. `_trawl_wal_file` — compaction's
/// synthetic provenance column, trawl's key rather than the client's — goes
/// silently, since a row carrying it wedges `read_json`.
fn strip_server_owned(out: &mut Map<String, Value>) -> bool {
    let mut stripped = false;
    for key in trawl_core::schema::RESERVED_CLIENT_FIELDS {
        if out.remove(*key).is_some() {
            stripped = true;
        }
    }
    if matches!(out.get("_raw"), Some(v) if !v.is_string()) {
        out.remove("_raw");
        stripped = true;
    }
    out.remove(compaction::WAL_FILE_COL);
    stripped
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
/// `DuckDB` identifiers are ASCII case-INSENSITIVE while JSON keys are not,
/// so `Dur` and `dur` — or `_Time` and `_time` — name ONE column
/// downstream, while every layer that keys on the exact string (the
/// postgres field catalog, the envelope's own alias/reserved handling, the
/// hot snapshot's key set) would treat them as two. Folding at the door
/// means exactly one code path ever sees one spelling: a client `_Time`
/// becomes the `_time` wire input, `_Ingested` strips as server-owned meta,
/// `Service` validates as `service`, and a custom `Dur` pins and stores as
/// `dur`. Per-character ASCII lowercase is precisely `DuckDB`'s identifier
/// equivalence: `CAFÉ` folds to `cafÉ` (its class's canonical form) while
/// `café` — a DIFFERENT `DuckDB` identifier — is untouched (probed in
/// `trawl-engine/tests/duckdb_probe.rs`).
///
/// In-event collisions after folding keep ONE value, deterministically: the
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

/// Drop every field whose NAME cannot be a field-catalog key, returning
/// whether any were dropped (the `field.name_too_long` repair).
///
/// This is the only seam that sees a client-chosen key before it becomes a
/// column. Compaction pins every dynamic column in postgres BEFORE writing
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

/// Canonicalize one parsed event object into the declared envelope.
///
/// Field order of operations is load-bearing:
/// 0. Field-name ASCII case-fold ([`fold_field_names`]) — BEFORE anything
///    that keys on a name (service validation, reserved-key strip, wire
///    aliases, stringification), so one code path sees one spelling.
///    Recorded as `field.name_case_folded` / `field.name_case_collision`
///    only when it changed something.
/// 1. `service` validation (reject path — nothing else runs).
/// 2. `_raw` capture — FIRST, before reserved-key stripping and every
///    repair, so the server's own fills never appear inside "what arrived"
///    (the serialization fallback uses the PRE-fold object, so dropped and
///    folded spellings stay findable).
/// 3. Reserved-key strip (`meta.stripped`), `_trawl_wal_file` silent drop,
///    over-long field-name drop (`field.name_too_long`).
/// 4. `_time` from the wire aliases (`_time`/`timestamp`/`@timestamp`,
///    consumed), ADR-0008 grammar, `time.from_ingest`/`time.out_of_range`.
/// 5. `_ingested` stamp.
/// 6. `env` default-or-reject, `host` peer-fill-or-relay-reject.
/// 7. Severity chain (`severity` 1-24 → string `severity` → `severity_text`
///    → `level` → NULL); `level` is consumed, and no severity-ish input is
///    ever deleted (an unmappable one lands in `severity_text`).
/// 8. `_repairs` assembly (omitted when clean).
pub fn canonicalize(
    obj: &Map<String, Value>,
    ctx: &EnvelopeContext<'_>,
) -> Result<Canonical, (String, RejectReason)> {
    // 0. One spelling per DuckDB identifier, before anything reads a name.
    let fold = fold_field_names(obj);
    let folded_obj = fold.obj;

    let service = validate_service(&folded_obj)?;
    // Env and host decide rejection before any mutation happens.
    let (env, env_defaulted) = resolve_env(&folded_obj, ctx)?;
    let host_missing = matches!(folded_obj.get("host"), None | Some(Value::Null));
    if host_missing && ctx.peer_is_trusted_relay {
        return Err((
            format!(
                "event has no 'host' and peer {} is a configured trusted relay \
                 — filling from the peer would stamp the relay's address as \
                 the origin",
                ctx.peer_host
            ),
            RejectReason::HostMissingFromRelay,
        ));
    }

    let mut repairs: Vec<RepairCode> = Vec::new();
    let push_repair = |repairs: &mut Vec<RepairCode>, code: RepairCode| {
        if !repairs.contains(&code) {
            repairs.push(code);
        }
    };
    if fold.folded {
        push_repair(&mut repairs, RepairCode::FieldNameCaseFolded);
    }
    if fold.collided {
        push_repair(&mut repairs, RepairCode::FieldNameCaseCollision);
    }

    // 2. Capture `_raw` before anything is stripped or repaired: a
    // client-supplied string `_raw` (a collector preserving its pre-parse
    // line, under any spelling of the name) is kept verbatim; otherwise
    // the canonical pre-repair serialization of the parsed object — the
    // PRE-fold original, so folded-away spellings stay findable — is the
    // most original form available (wire-exact bytes do not exist —
    // events arrive inside JSON arrays and the WAL re-serializes anyway).
    let raw_string = match folded_obj.get("_raw") {
        Some(Value::String(s)) => s.clone(),
        _ => serde_json::to_string(obj).unwrap_or_default(),
    };
    let (raw_string, truncated) = truncate_chars(raw_string, MAX_RAW_CHARS);

    let mut out = folded_obj;

    // 3. Server-owned metadata is never client-settable.
    if strip_server_owned(&mut out) {
        push_repair(&mut repairs, RepairCode::MetaStripped);
    }

    // 3.2. Names too long to be a catalog key never become columns.
    if drop_unstorable_names(&mut out) {
        push_repair(&mut repairs, RepairCode::FieldNameTooLong);
    }

    // 3.5. Nested values become JSON text (see [`stringify_nested_values`]).
    stringify_nested_values(&mut out);

    // 4. `_time` from the first present wire alias; all aliases consumed.
    let time_input = trawl_core::schema::TIME_ALIASES
        .iter()
        .find_map(|k| out.get(*k).cloned());
    for k in trawl_core::schema::TIME_ALIASES {
        out.remove(*k);
    }
    let canonical_time = match &time_input {
        None => {
            push_repair(&mut repairs, RepairCode::TimeFromIngest);
            ctx.arrival.to_owned()
        }
        Some(v) => {
            if let Some(dt) = parse_event_time(v) {
                let past = ctx.arrival_instant - chrono::Duration::days(OUT_OF_RANGE_PAST_DAYS);
                let future = ctx.arrival_instant + chrono::Duration::days(OUT_OF_RANGE_FUTURE_DAYS);
                if dt < past || dt > future {
                    // Implausible, but parseable: kept — flagged, never
                    // substituted (the client said what it said).
                    push_repair(&mut repairs, RepairCode::TimeOutOfRange);
                }
                dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
            } else {
                push_repair(&mut repairs, RepairCode::TimeFromIngest);
                ctx.arrival.to_owned()
            }
        }
    };
    out.insert(trawl_core::schema::TIME.into(), json!(canonical_time));

    // 5. Server-stamped arrival time.
    out.insert(trawl_core::schema::INGESTED.into(), json!(ctx.arrival));

    // 6. env / host.
    out.insert("env".into(), json!(env));
    if env_defaulted {
        push_repair(&mut repairs, RepairCode::EnvDefaulted);
    }
    if host_missing {
        out.insert("host".into(), json!(ctx.peer_host));
        push_repair(&mut repairs, RepairCode::HostFromPeer);
    }

    // 7. Severity chain (`level` consumed).
    if resolve_severity(&mut out) {
        push_repair(&mut repairs, RepairCode::SeverityUnmapped);
    }

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

    fn ctx_with(envs: &[String], relay: bool) -> EnvelopeContext<'_> {
        EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: arrival_instant(),
            peer_host: "10.0.4.55",
            peer_is_trusted_relay: relay,
            envs,
            default_env: &envs[0],
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

    // --- the declared schema, enforced (acceptance criterion 1) ---

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

    // --- nested-value stringification (ADR-0009 slice 2) ---

    #[test]
    fn object_values_are_stringified_to_json_text() {
        // Killing the duplicate-name class structurally: with nothing to
        // flatten, `read_json` can never raise a "Duplicate name" collision
        // and the explicit-columns fallback is deleted rather than fixed.
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
        // NO RepairCode: this is a canonicalization like RFC 3339
        // reformatting — a code firing on every k8s event would destroy the
        // NULL-dominance of `_repairs`.
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
        // `_raw` is captured BEFORE stringification, so the original
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
        // A client object `_raw` must strip with meta.stripped — the
        // stringify pass must not first turn it into an honourable string.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_raw":{"forged":true}}"#,
        );
        assert!(codes(&c).contains(&"meta.stripped"));
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

    // --- service type-specific rejection (acceptance criterion 2) ---

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

    // --- host behind a trusted relay (acceptance criterion 3) ---

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

    // --- env allowlist (acceptance criterion: unknown env hard-rejects) ---

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

    // --- severity (acceptance criteria: normalization, precedence, unmapped) ---

    #[test]
    fn severity_spellings_normalize_to_thirteen() {
        for spelling in [
            r#""WARN""#,
            r#""warning""#,
            r#""Warn""#,
            r#""W""#,
            r#""4""#,
            "4",
        ] {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","level":{spelling}}}"#
            ));
            assert_eq!(c.obj["severity"], 13, "spelling {spelling} must map to 13");
            assert!(c.repairs.is_empty(), "mapping is not a repair");
        }
    }

    #[test]
    fn severity_text_preserved_verbatim() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity_text":"WARN"}"#,
        );
        assert_eq!(c.obj["severity"], 13);
        assert_eq!(c.obj["severity_text"], "WARN", "verbatim, not lowercased");
    }

    #[test]
    fn severity_integer_wins_over_text() {
        // severity: 17 + severity_text: "info" keeps 17.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":17,"severity_text":"info"}"#,
        );
        assert_eq!(c.obj["severity"], 17);
        assert_eq!(c.obj["severity_text"], "info");
        assert!(c.repairs.is_empty());
    }

    #[test]
    fn severity_text_wins_over_level() {
        // severity_text: "error" + level: "info" derives 17 from severity_text.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity_text":"error","level":"info"}"#,
        );
        assert_eq!(c.obj["severity"], 17);
        assert_eq!(c.obj["severity_text"], "error");
    }

    #[test]
    fn level_is_consumed_never_stored() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","level":"error"}"#,
        );
        assert!(!c.obj.contains_key("level"), "level must be consumed");
        assert_eq!(c.obj["severity"], 17);
        assert_eq!(
            c.obj["severity_text"], "error",
            "the level value used for derivation lands in severity_text"
        );
    }

    #[test]
    fn unmappable_severity_is_null_plus_code_never_reject() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","level":"SPICY"}"#,
        );
        assert!(
            !c.obj.contains_key("severity"),
            "unmappable severity must be OMITTED (NULL), got {:?}",
            c.obj.get("severity")
        );
        assert_eq!(c.obj["severity_text"], "SPICY", "severity_text intact");
        assert!(codes(&c).contains(&"severity.unmapped"));
    }

    #[test]
    fn string_client_severity_maps_like_a_token() {
        // GCP/Stackdriver structured logging emits `severity: "ERROR"`.
        for spelling in [r#""ERROR""#, r#""error""#, r#""err""#, r#""3""#] {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":{spelling}}}"#
            ));
            assert_eq!(c.obj["severity"], 17, "spelling {spelling} must map to 17");
            assert!(c.repairs.is_empty(), "mapping is not a repair");
        }
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":"ERROR"}"#,
        );
        assert_eq!(
            c.obj["severity_text"], "ERROR",
            "the client spelling is preserved verbatim"
        );
    }

    #[test]
    fn out_of_ladder_client_severity_falls_through() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":42}"#,
        );
        assert!(!c.obj.contains_key("severity"));
        assert!(codes(&c).contains(&"severity.unmapped"));
    }

    #[test]
    fn unmappable_client_severity_is_never_deleted() {
        // Neither an out-of-ladder integer nor an unknown token may vanish
        // from the stored event: the value stays queryable as severity_text.
        for (input, expected) in [("42", "42"), ("0", "0"), (r#""SPICY""#, "SPICY")] {
            let c = canon(&format!(
                r#"{{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":{input}}}"#
            ));
            assert!(
                !c.obj.contains_key("severity"),
                "unmappable severity must be OMITTED (NULL): {input}"
            );
            assert_eq!(
                c.obj["severity_text"], expected,
                "severity {input} must survive as severity_text"
            );
            assert!(codes(&c).contains(&"severity.unmapped"), "input {input}");
        }
    }

    #[test]
    fn ladder_client_severity_is_not_echoed_as_text() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":17}"#,
        );
        assert_eq!(c.obj["severity"], 17);
        assert!(
            !c.obj.contains_key("severity_text"),
            "a ladder numeral is not a label, got {:?}",
            c.obj.get("severity_text")
        );
    }

    #[test]
    fn severity_text_wins_over_string_severity_for_the_label() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","severity":"ERROR","severity_text":"warn"}"#,
        );
        assert_eq!(c.obj["severity"], 17, "severity leads the derivation");
        assert_eq!(
            c.obj["severity_text"], "warn",
            "the dedicated text field still owns the label"
        );
    }

    #[test]
    fn absent_severity_is_not_a_repair() {
        let c = canon(r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z"}"#);
        assert!(!c.obj.contains_key("severity"));
        assert!(!c.obj.contains_key("severity_text"));
        assert!(!codes(&c).contains(&"severity.unmapped"));
    }

    // --- _raw (acceptance criteria: pre-defaults capture, client honour) ---

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
    fn non_string_raw_replaced_with_meta_stripped() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_raw":{"nested":1}}"#,
        );
        let raw = c.obj["_raw"].as_str().unwrap();
        assert!(raw.starts_with('{'), "server-filled canonical form: {raw}");
        assert!(codes(&c).contains(&"meta.stripped"));
    }

    #[test]
    fn client_ingested_and_repairs_stripped() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_ingested":"1999-01-01T00:00:00Z","_repairs":"forged"}"#,
        );
        assert_eq!(c.obj["_ingested"], ARRIVAL, "server stamp wins");
        assert_eq!(
            c.obj["_repairs"], "meta.stripped",
            "forged _repairs replaced by the strip record"
        );
        assert!(codes(&c).contains(&"meta.stripped"));
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
        // A case-variant of an envelope column names the SAME DuckDB
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
        // With no exact counterpart, a case-variant IS the field: `_Time`
        // becomes the `_time` wire input, `Message` becomes `message`,
        // `SEVERITY` joins the severity chain — one code path, one spelling.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_Time":"2025-12-31T23:00:00Z","Message":"m",
                "SEVERITY":"error","Custom_Field":1}"#,
        );
        assert_eq!(c.obj["_time"], "2025-12-31T23:00:00.000000Z");
        assert_eq!(c.obj["message"], "m");
        assert_eq!(c.obj["severity"], 17, "folded severity joins the chain");
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
    fn case_variant_ingested_folds_then_strips_as_server_owned() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","_Ingested":"1999-01-01T00:00:00Z"}"#,
        );
        assert_eq!(c.obj["_ingested"], ARRIVAL, "server stamp wins");
        assert!(codes(&c).contains(&"meta.stripped"));
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
        // which stays DISTINCT from an all-lowercase `café`. Names with no
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
        // Pre-fold these were dropped as reserved variants; now `Severity`
        // IS `severity` and `LEVEL` IS `level` — same chain, one spelling.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-12-31T23:00:00Z","Severity":"error","LEVEL":"warn"}"#,
        );
        assert!(!c.obj.contains_key("Severity"));
        assert!(!c.obj.contains_key("LEVEL"));
        assert!(!c.obj.contains_key("level"), "level is consumed");
        assert_eq!(c.obj["severity"], 17, "string severity leads");
        assert_eq!(
            c.obj["severity_text"], "warn",
            "the level value still lands as the label"
        );
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

    // --- _time grammar (the ADR-0008 corpus, moved verbatim) ---

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
            // The original is findable in _raw.
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
    fn wire_aliases_consumed_in_precedence_order() {
        // _time wins over timestamp wins over @timestamp; all consumed.
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h",
                "_time":"2025-06-01T10:00:00Z",
                "timestamp":"2025-06-01T11:00:00Z",
                "@timestamp":"2025-06-01T12:00:00Z"}"#,
        );
        assert_eq!(c.obj["_time"], "2025-06-01T10:00:00.000000Z");
        assert!(!c.obj.contains_key("timestamp"));
        assert!(!c.obj.contains_key("@timestamp"));

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

    // --- reserved provenance key ---

    #[test]
    fn wal_file_key_silently_dropped() {
        let c = canon(
            r#"{"service":"s","env":"prod","host":"h","_time":"2025-12-31T23:00:00Z","_trawl_wal_file":"x","keep":"me"}"#,
        );
        assert!(!c.obj.contains_key(compaction::WAL_FILE_COL));
        assert_eq!(c.obj["keep"], "me");
        assert!(
            c.repairs.is_empty(),
            "the provenance key drop is silent, not meta.stripped"
        );
    }

    // --- multiple repairs aggregate ---

    #[test]
    fn multiple_repairs_join_comma_separated() {
        let e = envs(&["prod"]);
        let ctx = ctx_with(&e, false);
        let c = canonicalize(&event(r#"{"service":"s","level":"SPICY"}"#), &ctx).unwrap();
        let repairs = c.obj["_repairs"].as_str().unwrap();
        for code in [
            "time.from_ingest",
            "env.defaulted",
            "host.from_peer",
            "severity.unmapped",
        ] {
            assert!(repairs.contains(code), "missing {code} in {repairs}");
        }
        assert!(repairs.contains(','));
    }
}
