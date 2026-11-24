// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one expression that binds a pinned column to its catalog type
//! (ADR-0009 slice 2, ADR-0011).
//!
//! Every reading these casts produce has a live mirror in [`crate::compare`],
//! executed side by side against the bundled `DuckDB`, never assumed.

use crate::schema::CanonicalType;
use crate::severity::Dialect;

/// The session setting every connection that conforms — or that reads a
/// conformed hot branch — MUST install before running this module's SQL.
pub const SESSION_TIME_ZONE_SQL: &str = "SET TimeZone='UTC'";

/// The ONE numeric comparison space (ADR-0011 ruling #6).
pub const DECIMAL_COMPARISON_SPACE: &str = "DECIMAL(38,6)";

/// The live mirror is [`crate::compare::decimal_micros`].
#[must_use]
pub fn decimal_reading(expr: &str) -> String {
    format!("TRY_CAST({expr} AS {DECIMAL_COMPARISON_SPACE})")
}

/// The canonical TEXT of a pinned column, in BOTH conform lanes.
#[must_use]
pub fn untyped_text(quoted: &str) -> String {
    format!("json_extract_string(to_json({quoted}), '$')")
}

#[must_use]
pub fn guarded_cast(text: &str, pin: CanonicalType) -> String {
    guarded_cast_in(text, pin, Dialect::Otel)
}

#[must_use]
pub fn guarded_cast_in(text: &str, pin: CanonicalType, dialect: Dialect) -> String {
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
        CanonicalType::Severity => severity_reading_sql(text, dialect),
    }
}

/// The RE2 character class matching [`crate::severity::WHITESPACE`] —
/// the SQL spelling of the same set.
///
/// A class rather than `trim(x, <chars>)`: a `chr()` chain of 25 terms
/// costs ~80ms per PREPARE in `DuckDB`'s binder, and a literal set of 25
/// characters would put a raw newline inside generated SQL, which
/// `EmitterState::finalize` re-indents line by line.
macro_rules! whitespace_class {
    () => {
        r"[\p{Zs}\x{9}-\x{d}\x{85}\x{2028}\x{2029}]"
    };
}

/// A regex the emitter's own SQL carries as a literal, never a query's
/// (ADR-0063).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedRegex {
    pub function: &'static str,
    /// The pattern: the SQL string literal's body, which holds no quote.
    pub pattern: &'static str,
}

const TRIM_LEADING: FixedRegex = FixedRegex {
    function: "regexp_replace",
    pattern: concat!("^", whitespace_class!(), "+"),
};

const TRIM_TRAILING: FixedRegex = FixedRegex {
    function: "regexp_replace",
    pattern: concat!(whitespace_class!(), "+$"),
};

const STRICT_INTEGER: FixedRegex = FixedRegex {
    function: "regexp_full_match",
    pattern: "[+-]?[0-9]+",
};

const ASCII_ALNUM: FixedRegex = FixedRegex {
    function: "regexp_full_match",
    pattern: "[A-Za-z0-9]+",
};

/// Every [`FixedRegex`] this module's SQL writes. A new one belongs here,
/// so the RE2 harness measures it
/// (`crates/trawl-engine/tests/re2_memory.rs`, part `fixed_sites`).
pub const FIXED_REGEXES: [FixedRegex; 4] =
    [TRIM_LEADING, TRIM_TRAILING, STRICT_INTEGER, ASCII_ALNUM];

fn trimmed_sql(expr: &str) -> String {
    format!(
        "regexp_replace(regexp_replace({expr}, '{}', ''), '{}', '')",
        TRIM_LEADING.pattern, TRIM_TRAILING.pattern
    )
}

/// The result is CAST to BIGINT because it types a stored column: an
/// INTEGER-typed conform would make the hot branch disagree with the
/// parquet side and throw the union.
///
/// `text_expr` lands in the result up to five times, which is free for a
/// column but wrong for an expression carrying bound `?` parameters, since
/// the emitter pushes one value per call and not per occurrence. A caller
/// whose subject can carry parameters takes
/// [`severity_reading_sql_bind_once`] instead.
#[must_use]
pub fn severity_reading_sql(text_expr: &str, dialect: crate::severity::Dialect) -> String {
    let trimmed = trimmed_sql(text_expr);
    format!("CAST({} AS BIGINT)", reading_case(&trimmed, dialect))
}

/// [`severity_reading_sql`] with its subject bound ONCE — the form
/// `sev()` emits. It is measurably slower than the repeated form (~2x
/// over 3M rows, probed), which is why the conform rung does not pay it.
#[must_use]
pub fn severity_reading_sql_bind_once(
    text_expr: &str,
    dialect: crate::severity::Dialect,
) -> String {
    format!(
        "CAST(list_transform([{}], _sev -> {})[1] AS BIGINT)",
        trimmed_sql(text_expr),
        reading_case("_sev", dialect)
    )
}

fn reading_case(trimmed: &str, dialect: crate::severity::Dialect) -> String {
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
    // the cast, and both have no reading in Rust. Deliberately NOT behind
    // the ASCII gate below — a sign is not alphanumeric, and `+17` reads.
    let numeric_arm = format!(
        "(CASE WHEN regexp_full_match({trimmed}, '{}') THEN {numeric} END)",
        STRICT_INTEGER.pattern
    );
    // The token subject is gated on being ASCII alphanumeric, because
    // `DuckDB`'s `lower()` is UNICODE and the kernel's fold is ASCII:
    // `lower('İ')` (U+0130) is `i`, which would read as INFO in SQL and
    // as nothing in Rust. A failed gate yields a NULL subject, so no
    // `WHEN` matches and the numeric arm decides.
    format!(
        "(CASE (CASE WHEN regexp_full_match({trimmed}, '{}') \
         THEN lower({trimmed}) END) {} ELSE {numeric_arm} END)",
        ASCII_ALNUM.pattern,
        arms.join(" ")
    )
}

/// The canonical token TEXT of a `SEVERITY`-pinned column — the SQL half
/// of `compare::PatternForm::SeverityText`.
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

fn sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

#[must_use]
pub fn raw_json_pointer(field: &str) -> String {
    format!("/{}", field.replace('~', "~0").replace('/', "~1"))
}

/// Two arms under one `json_valid` guard (safe under vectorized execution —
/// probed over a mixed-validity batch).
///
/// The case-variant arm is best-effort and second: `lower()` is Unicode
/// where the ingest fold is ASCII, so an over-match there recovers a value
/// that was never this field's.
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

/// `text_expr` lands in the result nine times over, which is free for a
/// column and wrong for an expression carrying bound `?` parameters.
#[must_use]
pub fn severity_dialect_ambiguous_sql(text_expr: &str) -> String {
    let otel = severity_reading_sql(text_expr, Dialect::Otel);
    let syslog = severity_reading_sql(text_expr, Dialect::Syslog);
    format!("({otel} IS NOT NULL AND {syslog} IS NOT NULL AND {otel} <> {syslog})")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepinTarget {
    pub pin: CanonicalType,
    pub stored: Dialect,
    pub raw: Dialect,
}

impl RepinTarget {
    #[must_use]
    pub fn otel(pin: CanonicalType) -> Self {
        Self {
            pin,
            stored: Dialect::Otel,
            raw: Dialect::Otel,
        }
    }
}

/// The repin rewrite's target-column expression (ADR-0011 slice B).
#[must_use]
pub fn resurrection_expr(
    quoted: &str,
    raw_quoted: &str,
    field: &str,
    target: RepinTarget,
) -> String {
    format!(
        "COALESCE({}, {})",
        guarded_cast_in(&untyped_text(quoted), target.pin, target.stored),
        guarded_cast_in(&raw_extract(raw_quoted, field), target.pin, target.raw)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varchar_conform_is_the_text_itself() {
        assert_eq!(guarded_cast("t", CanonicalType::Varchar), "t");
    }

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

    #[test]
    fn severity_rung_is_the_reading_kernel() {
        assert_eq!(
            guarded_cast("t", CanonicalType::Severity),
            severity_reading_sql("t", crate::severity::Dialect::Otel)
        );
    }

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
        assert!(sql.contains("regexp_full_match"), "{sql}");
        assert!(sql.contains("BETWEEN 1 AND 24"), "{sql}");
        assert!(sql.starts_with("CAST("), "{sql}");
        assert!(sql.ends_with(" AS BIGINT)"), "{sql}");
    }

    #[test]
    fn severity_reading_sql_shapes_differ_only_in_where_the_subject_lands() {
        let repeated = severity_reading_sql("SUBJ", crate::severity::Dialect::Otel);
        let once = severity_reading_sql_bind_once("SUBJ", crate::severity::Dialect::Otel);
        assert_eq!(repeated.matches("SUBJ").count(), 5, "{repeated}");
        assert_eq!(once.matches("SUBJ").count(), 1, "{once}");
        for (token, number) in crate::severity::token_entries() {
            let arm = format!("WHEN '{token}' THEN {number}");
            assert!(repeated.contains(&arm) && once.contains(&arm), "{arm}");
        }
        assert!(once.contains("list_transform"), "{once}");
        assert!(!repeated.contains("list_transform"), "{repeated}");
    }

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

    #[test]
    fn timestamp_rung_parses_through_timestamptz() {
        assert_eq!(
            guarded_cast("t", CanonicalType::Timestamp),
            "TRY_CAST(TRY_CAST(t AS TIMESTAMPTZ) AS TIMESTAMP)"
        );
    }

    #[test]
    fn untyped_text_unquotes_through_json() {
        assert_eq!(
            untyped_text("\"dur\""),
            "json_extract_string(to_json(\"dur\"), '$')"
        );
    }

    #[test]
    fn raw_json_pointer_escapes_rfc_6901() {
        assert_eq!(raw_json_pointer("status"), "/status");
        assert_eq!(raw_json_pointer("a/b"), "/a~1b");
        assert_eq!(raw_json_pointer("a~b"), "/a~0b");
        assert_eq!(raw_json_pointer("a~1b"), "/a~01b");
        assert_eq!(raw_json_pointer("$weird"), "/$weird");
    }

    #[test]
    fn raw_extract_escapes_sql_quotes() {
        let sql = raw_extract("r", "a'b");
        assert!(sql.contains("'/a''b'"), "exact arm must escape: {sql}");
        assert!(!sql.contains("'/a'b'"), "unescaped literal leaked: {sql}");
    }

    #[test]
    fn resurrection_expr_is_stored_reading_then_guarded_raw_arm() {
        let sql = resurrection_expr(
            "\"v\"",
            "\"_raw\"",
            "dur",
            RepinTarget::otel(CanonicalType::Varchar),
        );
        assert_eq!(
            sql,
            format!(
                "COALESCE({}, {})",
                untyped_text("\"v\""),
                raw_extract("\"_raw\"", "dur")
            )
        );
        let typed = resurrection_expr(
            "\"v\"",
            "\"_raw\"",
            "dur",
            RepinTarget::otel(CanonicalType::BigInt),
        );
        assert_eq!(typed.matches("DECIMAL(38,6)").count(), 4);
    }

    #[test]
    fn guarded_cast_is_the_otel_reading_of_every_pin() {
        for pin in CanonicalType::ALL {
            assert_eq!(
                guarded_cast("t", pin),
                guarded_cast_in("t", pin, crate::severity::Dialect::Otel),
                "{pin:?}"
            );
        }
    }

    #[test]
    fn only_the_severity_rung_reads_a_dialect() {
        for pin in [
            CanonicalType::Boolean,
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Timestamp,
            CanonicalType::Varchar,
        ] {
            assert_eq!(
                guarded_cast_in("t", pin, crate::severity::Dialect::Otel),
                guarded_cast_in("t", pin, crate::severity::Dialect::Syslog),
                "{pin:?} is not dialect-invariant"
            );
        }
        assert_ne!(
            guarded_cast_in("t", CanonicalType::Severity, crate::severity::Dialect::Otel),
            guarded_cast_in(
                "t",
                CanonicalType::Severity,
                crate::severity::Dialect::Syslog
            )
        );
    }

    #[test]
    fn resurrection_arms_take_their_own_dialects() {
        let target = RepinTarget {
            pin: CanonicalType::Severity,
            stored: crate::severity::Dialect::Otel,
            raw: crate::severity::Dialect::Syslog,
        };
        let sql = resurrection_expr("\"v\"", "\"_raw\"", "sev", target);
        assert_eq!(
            sql,
            format!(
                "COALESCE({}, {})",
                guarded_cast_in(
                    &untyped_text("\"v\""),
                    CanonicalType::Severity,
                    crate::severity::Dialect::Otel
                ),
                guarded_cast_in(
                    &raw_extract("\"_raw\"", "sev"),
                    CanonicalType::Severity,
                    crate::severity::Dialect::Syslog
                )
            )
        );
        assert_eq!(sql.matches("WHEN 7 THEN 5").count(), 1, "{sql}");
        assert_eq!(sql.matches("BETWEEN 1 AND 24").count(), 1, "{sql}");
        assert_eq!(
            RepinTarget::otel(CanonicalType::Severity).raw,
            crate::severity::Dialect::Otel
        );
    }

    #[test]
    fn dialect_ambiguity_sql_compares_both_readings() {
        let sql = severity_dialect_ambiguous_sql("t");
        let otel = severity_reading_sql("t", crate::severity::Dialect::Otel);
        let syslog = severity_reading_sql("t", crate::severity::Dialect::Syslog);
        assert_eq!(
            sql,
            format!("({otel} IS NOT NULL AND {syslog} IS NOT NULL AND {otel} <> {syslog})")
        );
    }

    #[test]
    fn the_severity_readings_write_exactly_the_fixed_regexes() {
        for dialect in [
            crate::severity::Dialect::Otel,
            crate::severity::Dialect::Syslog,
        ] {
            for sql in [
                severity_reading_sql("t", dialect),
                severity_reading_sql_bind_once("t", dialect),
            ] {
                let calls = sql.matches("regexp_").count();
                let mut carried = 0;
                for fixed in FIXED_REGEXES {
                    let call = format!("{}(", fixed.function);
                    let literal = format!(", '{}'", fixed.pattern);
                    let n = sql.matches(&literal).count();
                    assert!(n > 0, "{fixed:?} is not written: {sql}");
                    assert!(sql.matches(&call).count() >= n, "{fixed:?}: {sql}");
                    carried += n;
                }
                assert_eq!(carried, calls, "a regex call outside the list: {sql}");
            }
        }
        assert_eq!(
            FIXED_REGEXES.map(|fixed| fixed.pattern),
            [
                r"^[\p{Zs}\x{9}-\x{d}\x{85}\x{2028}\x{2029}]+",
                r"[\p{Zs}\x{9}-\x{d}\x{85}\x{2028}\x{2029}]+$",
                "[+-]?[0-9]+",
                "[A-Za-z0-9]+",
            ]
        );
    }
}
