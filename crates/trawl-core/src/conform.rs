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
//! answer for the same wire value, before it compares or globs anything —
//! lives in [`crate::compare`] ([`crate::compare::conformed_bigint`],
//! [`crate::compare::conformed_boolean`],
//! [`crate::compare::conformed_timestamp`],
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
        // The SEVERITY rung IS the kernel (ADR-0013 slice 2, rulings
        // 9-10): the pin's LIFETIME meaning is the full token-aware
        // reading, so a stored `"error"` conforms to 17 whether it
        // arrives live or is swept up by a repin. A numeric-only rung
        // would rewrite history correctly and null the very next live
        // row.
        CanonicalType::Severity => severity_reading_sql(text, crate::severity::Dialect::Otel),
    }
}

/// The ASCII whitespace set [`severity_reading_sql`] trims, as a `DuckDB`
/// string built from `chr()`.
///
/// Spelled with `chr()` rather than embedded control bytes so no
/// generated SQL — logged, snapshotted, or read by a human debugging a
/// compaction — ever carries a raw vertical tab. The set is
/// `crate::severity`'s `ASCII_WHITESPACE`, character for character.
const ASCII_WS_SQL: &str = "(' ' || chr(9) || chr(10) || chr(11) || chr(12) || chr(13))";

/// The SQL half of [`crate::severity::reading`]: `text_expr`'s point on
/// the `OTel` ladder as a BIGINT, or NULL where it has no reading.
///
/// `text_expr` is the column's TEXT form — [`untyped_text`] for a caller
/// with no `DESCRIBE`, a string literal for a probe. It is read exactly as
/// the Rust kernel reads a string: trim the ASCII whitespace set, match
/// the band token table, then the `OTel` exact short names, then a STRICT
/// integer (`[+-]?[0-9]+` and nothing else — `1.5`, `1e1`, `1_2` and
/// `0x10` are deferred here exactly as `str::parse::<i64>` refuses them
/// there), which the dialect then maps.
///
/// Every arm is generated from [`crate::severity`]'s own tables
/// ([`crate::severity::token_entries`], [`crate::severity::otel_name`],
/// [`crate::severity::from_syslog`]), and the pairing with the Rust kernel
/// is executed case by case against the bundled `DuckDB` in
/// `trawl-engine/tests/duckdb_probe.rs` — never assumed.
///
/// The result is CAST to BIGINT because it types a stored column: the
/// `SEVERITY` pin is physically BIGINT (`CanonicalType::as_duckdb`), and
/// an INTEGER-typed conform would make the hot branch disagree with the
/// parquet side and throw the union.
#[must_use]
pub fn severity_reading_sql(text_expr: &str, dialect: crate::severity::Dialect) -> String {
    let trimmed = format!("trim({text_expr}, {ASCII_WS_SQL})");
    // The band tokens first, then the exact short names the table does not
    // already carry — the kernel's own rung order, and the duplicates
    // (`error` is both) agree by construction.
    let mut arms: Vec<String> = Vec::with_capacity(44);
    let mut seen: Vec<&str> = Vec::with_capacity(44);
    for (token, number) in crate::severity::token_entries() {
        seen.push(token);
        arms.push(format!("WHEN '{token}' THEN {number}"));
    }
    for number in 1..=24u8 {
        let name = crate::severity::otel_name(number).expect("1-24 is the ladder");
        if !seen.contains(&name) {
            arms.push(format!("WHEN '{name}' THEN {number}"));
        }
    }
    let cast = format!("TRY_CAST({trimmed} AS BIGINT)");
    let numeric = match dialect {
        crate::severity::Dialect::Otel => {
            format!("(CASE WHEN {cast} BETWEEN 1 AND 24 THEN {cast} END)")
        }
        crate::severity::Dialect::Syslog => {
            let rungs: Vec<String> = (0..=7u8)
                .map(|n| {
                    let otel = crate::severity::from_syslog(n).expect("0-7 is the syslog range");
                    format!("WHEN {n} THEN {otel}")
                })
                .collect();
            format!("(CASE {cast} {} END)", rungs.join(" "))
        }
    };
    // A digits-only guard in front of the cast, because `TRY_CAST` reads a
    // vocabulary the kernel does not: `'1e1'` is 10 and `'0x10'` is 16 to
    // the cast, and both have no reading in Rust.
    let numeric_arm =
        format!("(CASE WHEN regexp_full_match({trimmed}, '[+-]?[0-9]+') THEN {numeric} END)");
    format!(
        "CAST((CASE lower({trimmed}) {} ELSE {numeric_arm} END) AS BIGINT)",
        arms.join(" ")
    )
}

/// The canonical token TEXT of a `SEVERITY`-pinned column — the SQL half
/// of `compare::PatternForm::SeverityText`.
///
/// A total 24-arm table with an explicit NULL fallthrough, generated from
/// [`crate::severity::otel_name`] so the SQL and the in-memory mirror
/// cannot drift; the pairing is executed against the bundled `DuckDB` in
/// `trawl-engine/tests/duckdb_probe.rs`. Out-of-ladder values cannot
/// exist in a conformed column ([`guarded_cast`]'s SEVERITY rung nulls
/// them), so the `ELSE NULL` arm is only ever reached by a NULL input.
#[must_use]
pub fn severity_token_text_sql(expr: &str) -> String {
    let arms: Vec<String> = (1..=24u8)
        .map(|n| {
            let name = crate::severity::otel_name(n).expect("1-24 is the ladder");
            format!("WHEN {n} THEN '{name}'")
        })
        .collect();
    format!("(CASE {expr} {} END)", arms.join(" "))
}

/// A SQL string literal's body: single quotes doubled.
fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

/// The RFC 6901 JSON Pointer naming `field` as a TOP-LEVEL key: `~` → `~0`,
/// `/` → `~1`, prefixed with `/`.
///
/// Pointer form, never `JSONPath`: a field name is a client-chosen JSON key,
/// and `JSONPath` would parse `.`/`[`/`$` out of it (a leading `$` is a
/// binder error outright, probed by execution). The pointer escape is total
/// — every key, hostile or not, resolves as itself.
#[must_use]
pub fn raw_json_pointer(field: &str) -> String {
    format!("/{}", field.replace('~', "~0").replace('/', "~1"))
}

/// The wire text of `field` inside a `_raw`-shaped column (`raw_quoted`,
/// already a quoted identifier), or NULL where `_raw` is not valid JSON,
/// not an object, or does not carry the key.
///
/// Two arms under one `json_valid` guard (safe under vectorized execution —
/// probed over a mixed-validity batch):
///
/// 1. the EXACT folded key, in pointer form;
/// 2. best-effort case-variant recovery: `_raw` written before the ingest
///    fold (or by a client whose spelling the fold collapsed) carries the
///    ORIGINAL spelling, so the first `json_keys` entry whose lowercase is
///    the folded name is re-read through a computed pointer. `lower()` is
///    Unicode where the ingest fold is ASCII — an over-match there recovers
///    a value that was never this field's, which is why this arm is
///    best-effort and second.
#[must_use]
pub fn raw_extract(raw_quoted: &str, field: &str) -> String {
    let exact = sql_str(&raw_json_pointer(field));
    let folded = sql_str(&field.to_ascii_lowercase());
    format!(
        "(CASE WHEN json_valid({raw_quoted}) THEN COALESCE(\
         json_extract_string({raw_quoted}, '{exact}'), \
         json_extract_string({raw_quoted}, '/' || \
         replace(replace(list_filter(json_keys({raw_quoted}), \
         k -> lower(k) = '{folded}')[1], '~', '~0'), '/', '~1'))) END)"
    )
}

/// The repin rewrite's target-column expression (ADR-0011 slice B): the
/// stored value's guarded reading under the NEW pin, and — where that is
/// NULL, i.e. the column never carried the value or a prior conform
/// shelved it — the `_raw` re-extraction's guarded reading.
///
/// One expression for the dry-run COUNT and the rewrite WRITE, by the same
/// doctrine that makes the hot branch and compaction one builder: a plan
/// that predicts with one expression and rewrites with another is a report
/// that lies. Both arms go through [`guarded_cast`], so resurrection can
/// never smuggle in a value the conform would have refused (`"1.5"` does
/// not round into a BIGINT 2 just because it came back from `_raw`).
#[must_use]
pub fn resurrection_expr(
    quoted: &str,
    raw_quoted: &str,
    field: &str,
    pin: CanonicalType,
) -> String {
    format!(
        "COALESCE({}, {})",
        guarded_cast(&untyped_text(quoted), pin),
        guarded_cast(&raw_extract(raw_quoted, field), pin)
    )
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

    /// The SEVERITY rung IS the reading kernel (ADR-0013 ruling 10) —
    /// byte for byte, not merely "equivalent": one expression means a
    /// stored `"error"` conforms to 17 live and under a repin alike.
    #[test]
    fn severity_rung_is_the_reading_kernel() {
        assert_eq!(
            guarded_cast("t", CanonicalType::Severity),
            severity_reading_sql("t", crate::severity::Dialect::Otel)
        );
    }

    /// The reader's arms are GENERATED from the kernel's tables — every
    /// band token and every exact short name, then a digits-only numeric
    /// arm the dialect maps. The Rust/SQL pairing itself is executed in
    /// `trawl-engine/tests/duckdb_probe.rs`; this pins the shape.
    #[test]
    fn severity_reading_sql_covers_both_name_tables_and_the_numeric_arm() {
        let sql = severity_reading_sql("t", crate::severity::Dialect::Otel);
        for (token, number) in crate::severity::token_entries() {
            assert!(
                sql.contains(&format!("WHEN '{token}' THEN {number}")),
                "missing band token {token}: {sql}"
            );
        }
        for n in 1..=24u8 {
            let name = crate::severity::otel_name(n).unwrap();
            assert!(
                sql.contains(&format!("WHEN '{name}' THEN {n}")),
                "missing exact name {name}: {sql}"
            );
        }
        // Digits only, and the OTel numeric arm is the ladder range.
        assert!(sql.contains("regexp_full_match"), "{sql}");
        assert!(sql.contains("BETWEEN 1 AND 24"), "{sql}");
        // A stored column is BIGINT, whatever the arms' literals type as.
        assert!(sql.starts_with("CAST("), "{sql}");
        assert!(sql.ends_with(" AS BIGINT)"), "{sql}");
    }

    /// The dialect governs the NUMERIC arm and nothing else: the name
    /// tables are identical, and syslog's 0-7 inversion is the one
    /// difference.
    #[test]
    fn severity_reading_sql_dialect_changes_only_the_numeric_arm() {
        let otel = severity_reading_sql("t", crate::severity::Dialect::Otel);
        let syslog = severity_reading_sql("t", crate::severity::Dialect::Syslog);
        for (token, number) in crate::severity::token_entries() {
            assert!(syslog.contains(&format!("WHEN '{token}' THEN {number}")));
        }
        assert!(!syslog.contains("BETWEEN 1 AND 24"), "{syslog}");
        for n in 0..=7u8 {
            let otel_number = crate::severity::from_syslog(n).unwrap();
            assert!(
                syslog.contains(&format!("WHEN {n} THEN {otel_number}")),
                "missing syslog rung {n}: {syslog}"
            );
        }
        assert_ne!(otel, syslog);
    }

    /// The canonical token text is a total 24-arm table with an explicit
    /// NULL fallthrough — the SQL half of `PatternForm::SeverityText`.
    #[test]
    fn severity_token_text_covers_the_whole_ladder() {
        let sql = severity_token_text_sql("s");
        for n in 1..=24u8 {
            let name = crate::severity::otel_name(n).unwrap();
            assert!(
                sql.contains(&format!("WHEN {n} THEN '{name}'")),
                "missing arm {n} → {name}: {sql}"
            );
        }
        assert!(sql.starts_with("(CASE s WHEN 1 THEN 'trace'"), "{sql}");
        assert!(sql.ends_with("WHEN 24 THEN 'fatal4' END)"), "{sql}");
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

    /// The pointer escape is RFC 6901: `~` first, then `/`, so the escapes
    /// cannot collide.
    #[test]
    fn raw_json_pointer_escapes_rfc_6901() {
        assert_eq!(raw_json_pointer("status"), "/status");
        assert_eq!(raw_json_pointer("a/b"), "/a~1b");
        assert_eq!(raw_json_pointer("a~b"), "/a~0b");
        assert_eq!(raw_json_pointer("a~1b"), "/a~01b");
        assert_eq!(raw_json_pointer("$weird"), "/$weird");
    }

    /// The extraction embeds the field as a SQL literal, so a quote in a
    /// client-chosen key cannot break out of the string.
    #[test]
    fn raw_extract_escapes_sql_quotes() {
        let sql = raw_extract("r", "a'b");
        assert!(sql.contains("'/a''b'"), "exact arm must escape: {sql}");
        assert!(!sql.contains("'/a'b'"), "unescaped literal leaked: {sql}");
    }

    /// The dry run counts and the rewrite writes with THIS one expression:
    /// stored reading first, `_raw` reading second, both guarded.
    #[test]
    fn resurrection_expr_is_stored_reading_then_guarded_raw_arm() {
        let sql = resurrection_expr("\"v\"", "\"_raw\"", "dur", CanonicalType::Varchar);
        assert_eq!(
            sql,
            format!(
                "COALESCE({}, {})",
                untyped_text("\"v\""),
                raw_extract("\"_raw\"", "dur")
            )
        );
        // Typed pins guard BOTH arms.
        let typed = resurrection_expr("\"v\"", "\"_raw\"", "dur", CanonicalType::BigInt);
        assert_eq!(typed.matches("DECIMAL(38,6)").count(), 4);
    }
}
