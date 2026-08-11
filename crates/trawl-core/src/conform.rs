// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ONE expression that binds a pinned column to its catalog type
//! (ADR-0009 slice 2, ADR-0011).
//!
//! Two lanes conform the same value, and both build their SQL here:
//!
//! - **compaction** (`trawl-server`) conforms a WAL batch on its way to
//!   parquet, and the boot pass rewrites standing files the same way — this
//!   is what the corpus DURABLY holds;
//! - **the hot branch** ([`crate::emitter`]'s `REPLACE` list over the
//!   hot-buffer ndjson) conforms the same events while they are still in
//!   memory, so a query sees the value it will keep seeing after the
//!   compactor runs.
//!
//! They cannot be two expressions. A value that reads `true` while it is
//! hot and NULL once it compacts is a query whose answer changes with a
//! background timer, minutes after the fact, with nothing in the request to
//! explain it.
//!
//! Two rules make one expression deterministic:
//!
//! 1. **Text first.** Both lanes read their source through `read_json`,
//!    which types each column from the batch's CONTENTS: a field holding
//!    only strings infers VARCHAR, one holding a number beside a string
//!    infers JSON, and JSON's cast domain is strictly narrower than
//!    VARCHAR's (`TRY_CAST(JSON '"1.5"' AS BIGINT)` is NULL where
//!    `TRY_CAST('1.5' AS BIGINT)` rounds to 2 — and JSON's is WIDER for a
//!    numeric under a BOOLEAN pin, where `200` reads `true`; both probed by
//!    execution in `trawl-engine/tests/duckdb_probe.rs`). Casting the
//!    inferred type would make one event's reading a function of what
//!    happened to share its batch. Every pinned column therefore goes
//!    through its TEXT form first ([`untyped_text`], in BOTH lanes — a
//!    per-lane spelling is a per-lane stored value under the VARCHAR pin,
//!    whose guard is the identity), and the guarded cast applies to that
//!    text: the cast domain is VARCHAR, always.
//! 2. **The guard is the cast.** A bare `TRY_CAST` to a typed pin ROUNDS
//!    rather than fails (`'1.5'` → 2) and reads a vocabulary the rendering
//!    does not round-trip (`'TRUE'` → `true`), so every typed rung is
//!    wrapped in a round-trip check ([`guarded_cast`]): a value the cast
//!    would ALTER becomes NULL — counted as a conflict, and still findable
//!    in `_raw` — instead of being silently rewritten.
//!
//! The live mirror of these readings — what the in-memory SSE matcher must
//! answer for the same wire value — lives in [`crate::compare`]
//! ([`crate::compare::conformed_bigint`],
//! [`crate::compare::conformed_boolean`],
//! [`crate::compare::try_cast_double`]), and every pairing is executed
//! side by side against the bundled `DuckDB`, never assumed.

use crate::schema::CanonicalType;

/// The session setting every connection that conforms — or that reads a
/// conformed hot branch — MUST install before running this module's SQL.
///
/// The TIMESTAMP rung parses through `TIMESTAMPTZ` so an offset in the text
/// is APPLIED rather than ignored (ADR-0011), and both halves of that parse
/// consult the session zone: a text with NO offset is read as the session
/// zone, and the instant is rendered back to a zoneless `TIMESTAMP` in it.
/// The bundled `DuckDB` links ICU and defaults `TimeZone` to the HOST zone
/// (probed by execution), so without this pin what compaction writes — and
/// what a query reads out of the hot buffer — would be a function of
/// `/etc/localtime`.
///
/// UTC is not a preference: ingest canonicalizes `_time` to RFC 3339 UTC
/// (ADR-0008) and every stored timestamp is UTC, so the session that reads
/// them has to agree.
pub const SESSION_TIME_ZONE_SQL: &str = "SET TimeZone='UTC'";

/// The ONE numeric comparison space (ADR-0011 ruling #6).
///
/// Two questions in this codebase compare a TEXT against a number, and
/// both are answered here so they cannot drift apart: the BIGINT rung's
/// round-trip guard below, and the VARCHAR-pinned comparison rules in
/// [`crate::compare`] (`status=200` matching a stored `"200.0"`,
/// `id>1737000000123456789` ordering ids).
///
/// `DECIMAL(38,6)` because it is EXACT over every `i64` and far past it
/// (magnitudes below 10^32), where a DOUBLE comparison goes blind above
/// 2^53 and silently equates neighbours: `id=1737000000123456789` matched
/// three distinct stored ids, and `id!=9007199254740993` suppressed the
/// genuinely different `9007199254740992` — snowflake ids and nanosecond
/// epochs are exactly that shape. The costs are stated where they bite
/// ([`crate::compare`]'s module doc): fractions quantize at 10^-6, and a
/// magnitude at or above 10^32 — like `nan` and `inf` — has no reading at
/// all, which is a NULL, never a false match.
pub const DECIMAL_COMPARISON_SPACE: &str = "DECIMAL(38,6)";

/// `expr`'s reading in [`DECIMAL_COMPARISON_SPACE`] — NULL for a text the
/// space cannot hold, which every caller treats as UNKNOWN rather than
/// inventing an answer.
///
/// The live mirror is [`crate::compare::decimal_micros`], exact in `i128`
/// and executed against this expression in
/// `trawl-engine/tests/duckdb_probe.rs`.
#[must_use]
pub fn decimal_reading(expr: &str) -> String {
    format!("TRY_CAST({expr} AS {DECIMAL_COMPARISON_SPACE})")
}

/// The canonical TEXT of a pinned column, in BOTH conform lanes.
///
/// `json_extract_string(to_json(x), '$')` yields UNQUOTED strings
/// (`n/a`, not `"n/a"`) over every inference class a hot snapshot can
/// produce — VARCHAR, JSON from mixed values, and the homogeneous numeric,
/// boolean and timestamp classes — where a plain `CAST(x AS VARCHAR)` on a
/// JSON-inferred column would keep the quotes. It is also total over the
/// physical types a standing parquet file can hold (BLOB, INTERVAL, UUID,
/// DECIMAL, nested), rendering the complex ones as the JSON text
/// `json_extract_string` can read back. All probed by execution.
///
/// The hot lane has no choice — the emitter has no `DESCRIBE` — and
/// compaction, which does, must not use it: this rendering is NOT
/// `CAST(x AS VARCHAR)`'s (a DOUBLE `1e20` is `100000000000000000000.0`
/// here and `1e+20` there), and under the VARCHAR pin [`guarded_cast`] is
/// the IDENTITY, so the text form is the stored value itself. Two
/// spellings there are two corpora: `note=/^1e/` matched while the event
/// was hot and stopped matching minutes later, when the compactor ran.
#[must_use]
pub fn untyped_text(quoted: &str) -> String {
    format!("json_extract_string(to_json({quoted}), '$')")
}

/// The conform expression for one pinned column, over its canonical
/// `text` form — a typed cast that succeeds only when the value survives
/// the round trip unchanged.
///
/// The comparison space is chosen per rung, and every one of them is
/// executed against the bundled `DuckDB` in
/// `trawl-engine/tests/duckdb_probe.rs`:
///
/// - `BIGINT` compares the cast against the text re-parsed in
///   [`DECIMAL_COMPARISON_SPACE`] — an EXACT integer space across the whole BIGINT
///   range, where a DOUBLE-space comparison goes blind above 2^53 (both
///   sides collapse to the same double, so `1735689600123456710.7`
///   conformed to `…711` with no conflict). It keeps every representation
///   drift (`'4.0'` → 4, `'0404'` → 404, `'1e3'` → 1000, `' 200'` → 200,
///   `'200_000'` → 200000, `2^53 ± 1` exact, `i64::MAX`) and refuses every
///   value change (`'1.5'`, `'nan'`, `''`, `u64::MAX`). Texts outside
///   `DECIMAL`'s syntax are refused even where `BIGINT` reads them —
///   `'0x10'` is 16 to the cast and NULL to the guard — which is the
///   guard doing its job: hex is a spelling `DuckDB` will never write
///   back. Residual tolerance: a fraction below `DECIMAL(38,6)`'s
///   half-microstep (`'4.0000001'`) quantizes to the integer and is
///   absorbed as representation drift; `'4.0000005'` rounds away and is
///   refused;
/// - `DOUBLE` is the bare cast. Over a text source the round trip is the
///   identity — the guard would compare `TRY_CAST(text AS DOUBLE)` with
///   itself — and DOUBLE-space comparison is deliberately
///   precision-tolerant anyway: `u64::MAX` must conform to the DOUBLE pin
///   despite the precision loss, because pinning DOUBLE *means* accepting
///   it;
/// - `TIMESTAMP` is likewise the bare (zone-aware) cast for the same
///   reason, and it parses through `TIMESTAMPTZ` so an offset in the text
///   is applied and a zoneless text reads as UTC — the semantics ingest
///   already ratified for `_time` (ADR-0008). `TRY_CAST(text AS TIMESTAMP)`
///   alone is a WALL-CLOCK parse that IGNORES the offset, which made
///   `09:00:00+05:30` store `09:00` here and `03:30` through `read_json`'s
///   own inference. Requires [`SESSION_TIME_ZONE_SQL`];
/// - `BOOLEAN` compares strict text, so only the values `DuckDB` renders
///   back conform: `'true'`/`'false'` and nothing else. `'TRUE'`, `'t'`,
///   `'yes'` and `'1'` are all inside the CAST's vocabulary and all fail
///   the round trip — which is also what makes the Boolean ladder rung
///   reachable at all (`TRY_CAST(true AS BIGINT)` is 1, so under bare
///   counting a boolean batch scored ≥90% BIGINT first);
/// - `VARCHAR` is the text itself: stringification is lossless by
///   construction, so there is nothing to guard.
#[must_use]
pub fn guarded_cast(text: &str, pin: CanonicalType) -> String {
    match pin {
        CanonicalType::Varchar => text.to_owned(),
        CanonicalType::BigInt => {
            let cast = format!("TRY_CAST({text} AS BIGINT)");
            format!(
                "(CASE WHEN {} = {} THEN {cast} END)",
                decimal_reading(text),
                decimal_reading(&cast)
            )
        }
        CanonicalType::Boolean => {
            let cast = format!("TRY_CAST({text} AS BOOLEAN)");
            format!("(CASE WHEN CAST({cast} AS VARCHAR) = {text} THEN {cast} END)")
        }
        CanonicalType::Double => format!("TRY_CAST({text} AS DOUBLE)"),
        CanonicalType::Timestamp => {
            format!("TRY_CAST(TRY_CAST({text} AS TIMESTAMPTZ) AS TIMESTAMP)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VARCHAR pin is a pass-through: the text form IS the conform, so
    /// no lane may wrap it in a cast that could fail.
    #[test]
    fn varchar_conform_is_the_text_itself() {
        assert_eq!(guarded_cast("t", CanonicalType::Varchar), "t");
    }

    /// Every typed rung casts the TEXT, never the source column: the
    /// expression handed in is the only thing any cast sees, so the domain
    /// cannot vary with `read_json`'s inference.
    #[test]
    fn every_typed_rung_casts_only_the_text_form() {
        for pin in [
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Timestamp,
            CanonicalType::Boolean,
        ] {
            let sql = guarded_cast("TEXT_FORM", pin);
            assert!(
                !sql.contains("\"col\""),
                "{pin:?} reached past the text form: {sql}"
            );
            assert!(sql.contains("TEXT_FORM"), "{pin:?} dropped the text: {sql}");
        }
    }

    /// The guarded rungs re-read the cast in their comparison space.
    #[test]
    fn guarded_rungs_compare_the_cast_against_the_text() {
        assert_eq!(
            guarded_cast("t", CanonicalType::BigInt),
            "(CASE WHEN TRY_CAST(t AS DECIMAL(38,6)) \
             = TRY_CAST(TRY_CAST(t AS BIGINT) AS DECIMAL(38,6)) \
             THEN TRY_CAST(t AS BIGINT) END)"
        );
        assert_eq!(
            guarded_cast("t", CanonicalType::Boolean),
            "(CASE WHEN CAST(TRY_CAST(t AS BOOLEAN) AS VARCHAR) = t \
             THEN TRY_CAST(t AS BOOLEAN) END)"
        );
    }

    /// The timestamp rung is zone-aware: it parses through `TIMESTAMPTZ`,
    /// which applies an offset the plain TIMESTAMP cast would ignore.
    #[test]
    fn timestamp_rung_parses_through_timestamptz() {
        assert_eq!(
            guarded_cast("t", CanonicalType::Timestamp),
            "TRY_CAST(TRY_CAST(t AS TIMESTAMPTZ) AS TIMESTAMP)"
        );
    }

    /// The hot lane's text form unquotes JSON strings.
    #[test]
    fn untyped_text_unquotes_through_json() {
        assert_eq!(
            untyped_text("\"dur\""),
            "json_extract_string(to_json(\"dur\"), '$')"
        );
    }
}
