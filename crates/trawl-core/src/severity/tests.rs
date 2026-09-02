// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

/// The `OTel` short names are injective over the whole 1-24 ladder — the
/// property that lets `_severity` render as text and be filtered by that
/// same text without two numbers sharing a spelling.
#[test]
fn otel_names_are_injective_over_the_ladder() {
    let mut seen = std::collections::BTreeSet::new();
    for n in 1..=24u8 {
        let name = otel_name(n).expect("every ladder number has a name");
        assert!(seen.insert(name), "{name:?} named twice (n={n})");
        assert_eq!(number_for_exact(name), Some(n), "{name:?} must invert");
    }
    assert_eq!(otel_name(0), None);
    assert_eq!(otel_name(25), None);
}

/// The exact names are the band base plus an optional 2-4 suffix.
#[test]
fn exact_names_spell_the_band_base_and_suffix() {
    let cases: &[(u8, &str)] = &[
        (1, "trace"),
        (4, "trace4"),
        (5, "debug"),
        (9, "info"),
        (10, "info2"),
        (13, "warn"),
        (16, "warn4"),
        (17, "error"),
        (18, "error2"),
        (21, "fatal"),
        (24, "fatal4"),
    ];
    for &(n, name) in cases {
        assert_eq!(otel_name(n), Some(name), "{n}");
        assert_eq!(number_for_exact(name), Some(n), "{name}");
    }
    // Case-insensitive, like the token table.
    assert_eq!(number_for_exact("ERROR2"), Some(18));
    // A suffix outside 2-4, an unknown base, and the aliases the token
    // table carries are not exact names.
    assert_eq!(number_for_exact("error1"), None);
    assert_eq!(number_for_exact("error5"), None);
    assert_eq!(number_for_exact("gold"), None);
    assert_eq!(number_for_exact("err"), None);
    assert_eq!(number_for_exact("notice"), None);
    assert_eq!(number_for_exact(""), None);
}

/// The wire-shaped display rule: a `_severity` cell is BIGINT, so the
/// narrowing is part of the rule and anything off the ladder — including
/// a value outside `u8` entirely — has no reading.
#[test]
fn token_text_reads_a_wire_severity_cell() {
    assert_eq!(token_text(17), Some("error"));
    assert_eq!(token_text(18), Some("error2"));
    assert_eq!(token_text(13), Some("warn"));
    assert_eq!(token_text(99), None);
    assert_eq!(token_text(0), None);
    assert_eq!(token_text(-1), None);
    assert_eq!(token_text(i64::MAX), None);
    for n in 1..=24i64 {
        assert_eq!(
            token_text(n),
            otel_name(u8::try_from(n).unwrap()),
            "ladder {n}"
        );
    }
}

/// Every token in `TOKEN_TABLE` maps to its exact number.
#[test]
fn token_table_exact_numbers() {
    let cases: &[(&str, u8)] = &[
        ("trace", 1),
        ("t", 1),
        ("debug", 5),
        ("d", 5),
        ("info", 9),
        ("i", 9),
        ("notice", 10),
        ("warn", 13),
        ("warning", 13),
        ("w", 13),
        ("error", 17),
        ("err", 17),
        ("e", 17),
        ("fatal", 21),
        ("critical", 21),
        ("crit", 21),
        ("f", 21),
        ("alert", 23),
        ("emerg", 24),
        ("panic", 24),
    ];
    for &(token, number) in cases {
        assert_eq!(
            number_for_token(token),
            Some(number),
            "token {token:?} must map to {number}"
        );
    }
}

/// Tokens are matched case-insensitively.
#[test]
fn tokens_case_fold() {
    for (input, number) in [
        ("WARN", 13),
        ("Warning", 13),
        ("W", 13),
        ("ERROR", 17),
        ("Info", 9),
        ("EMERG", 24),
        ("PANIC", 24),
        ("T", 1),
    ] {
        assert_eq!(number_for_token(input), Some(number), "input {input:?}");
    }
}

/// Unknown tokens do not map.
#[test]
fn unknown_tokens_rejected() {
    for input in ["SPICY", "", "warnn", "4", "13", "verbose", "severe"] {
        assert_eq!(number_for_token(input), None, "input {input:?}");
    }
}

/// Syslog numerals invert: 0 (emerg) is the top of the `OTel` ladder,
/// 7 (debug) near the bottom. A naive passthrough fails this loudly.
#[test]
fn syslog_inversion() {
    let cases: &[(u8, u8)] = &[
        (7, 5),
        (6, 9),
        (5, 10),
        (4, 13),
        (3, 17),
        (2, 21),
        (1, 23),
        (0, 24),
    ];
    for &(syslog, otel) in cases {
        assert_eq!(from_syslog(syslog), Some(otel), "syslog {syslog}");
    }
    // The two poles, asserted by band so a passthrough cannot sneak by:
    assert_eq!(band_name(from_syslog(0).unwrap()), Some("fatal"));
    assert_eq!(band_name(from_syslog(7).unwrap()), Some("debug"));
    assert_eq!(from_syslog(8), None);
    assert_eq!(from_syslog(255), None);
}

/// Band bounds are the `OTel` ladder: TRACE 1-4 … FATAL 21-24.
#[test]
fn band_bounds() {
    assert_eq!(band_of(1), Some((1, 4)));
    assert_eq!(band_of(4), Some((1, 4)));
    assert_eq!(band_of(5), Some((5, 8)));
    assert_eq!(band_of(9), Some((9, 12)));
    // `notice` (10) sits in the info band: the OTel ladder has no band
    // of its own for it.
    assert_eq!(band_of(10), Some((9, 12)));
    assert_eq!(band_of(13), Some((13, 16)));
    assert_eq!(band_of(17), Some((17, 20)));
    assert_eq!(band_of(20), Some((17, 20)));
    assert_eq!(band_of(21), Some((21, 24)));
    assert_eq!(band_of(24), Some((21, 24)));
    assert_eq!(band_of(0), None);
    assert_eq!(band_of(25), None);
}

#[test]
fn valid_number_range() {
    assert!(is_valid_number(1));
    assert!(is_valid_number(24));
    assert!(!is_valid_number(0));
    assert!(!is_valid_number(25));
    assert!(!is_valid_number(-3));
}

#[test]
fn band_names() {
    assert_eq!(band_name(3), Some("trace"));
    assert_eq!(band_name(13), Some("warn"));
    assert_eq!(band_name(24), Some("fatal"));
    assert_eq!(band_name(0), None);
}

// ── the reader (ADR-0013) ─────────────────────────────────────────────

use serde_json::json;

/// The reader's text matrix, both dialects — the same list the SQL mirror
/// is probed against in `trawl-engine/tests/duckdb_probe.rs`, so a case
/// added here belongs there too.
#[test]
fn reading_text_matrix() {
    // (input, otel, syslog) — words are dialect-free, numerics are not.
    let cases: &[(&str, Option<u8>, Option<u8>)] = &[
        ("error", Some(17), Some(17)),
        ("ERR", Some(17), Some(17)),
        (" error ", Some(17), Some(17)),
        ("\t error \r\n", Some(17), Some(17)),
        ("error2", Some(18), Some(18)),
        ("warn", Some(13), Some(13)),
        ("notice", Some(10), Some(10)),
        // Numerics: OTel passes 1-24 through, syslog inverts 0-7.
        ("0", None, Some(24)),
        ("1", Some(1), Some(23)),
        ("7", Some(7), Some(5)),
        ("8", Some(8), None),
        ("24", Some(24), None),
        ("25", None, None),
        // Leading zeros are still digits.
        ("0404", None, None),
        ("007", Some(7), Some(5)),
        ("+17", Some(17), None),
        ("-1", None, None),
        // Spellings `TRY_CAST` would read and the kernel does not.
        ("1.5", None, None),
        ("1e1", None, None),
        ("1_2", None, None),
        ("0x10", None, None),
        ("9223372036854775807", None, None),
        ("9223372036854775808", None, None),
        ("", None, None),
        ("   ", None, None),
        ("gold", None, None),
        ("severe", None, None),
        // The fold is ASCII: a codepoint `DuckDB`'s Unicode `lower()`
        // would fold onto a token letter is not that token here, and the
        // SQL mirror gates on ASCII to agree (probed).
        ("İNFO", None, None),
        ("ı", None, None),
        ("\u{212a}", None, None),
        ("ＩＮＦＯ", None, None),
        ("１７", None, None),
    ];
    for &(input, otel, syslog) in cases {
        assert_eq!(
            reading_text(input, Dialect::Otel),
            otel,
            "otel reading of {input:?}"
        );
        assert_eq!(
            reading_text(input, Dialect::Syslog),
            syslog,
            "syslog reading of {input:?}"
        );
    }
}

/// The trim is the enumerated Unicode `White_Space` set — `str::trim`'s
/// set, character for character, which is what lets ingest delegate here
/// unchanged — and `DuckDB` trims the same one (probed), so a padded value
/// cannot read one way live and another in batch.
#[test]
fn reading_text_trims_the_unicode_whitespace_set() {
    for padded in [
        "\u{0b}error\u{0c}",
        "\u{a0}error",
        "error\u{2003}",
        "\u{3000}error\u{3000}",
        "\u{85}error",
        "\u{202f}\u{2009}error\t",
    ] {
        assert_eq!(reading_text(padded, Dialect::Otel), Some(17), "{padded:?}");
        // `str::trim` is the same set, so the two agree everywhere.
        assert_eq!(
            reading_text(padded, Dialect::Otel),
            reading_text(padded.trim(), Dialect::Otel),
            "{padded:?}"
        );
    }
    // Numerics trim too, and inner whitespace is part of the value.
    assert_eq!(reading_text("\u{a0}17\u{a0}", Dialect::Otel), Some(17));
    assert_eq!(reading_text("er\u{a0}ror", Dialect::Otel), None);
    // The set is exactly `char::is_whitespace`'s.
    for c in WHITESPACE {
        assert!(c.is_whitespace(), "{c:?} is not White_Space");
    }
    assert_eq!(
        WHITESPACE.len(),
        (0..=0x3000u32)
            .filter_map(char::from_u32)
            .filter(|c| c.is_whitespace())
            .count(),
        "the enumeration must be the WHOLE property"
    );
}

/// The JSON shapes: a string reads as text, an integer as a number, and
/// everything else — including a fractional number, which names no rung —
/// has no reading.
#[test]
fn reading_over_json_shapes() {
    assert_eq!(reading(&json!("error"), Dialect::Otel), Some(17));
    assert_eq!(reading(&json!(17), Dialect::Otel), Some(17));
    assert_eq!(reading(&json!("17"), Dialect::Otel), Some(17));
    assert_eq!(reading(&json!(3), Dialect::Syslog), Some(17));
    assert_eq!(reading(&json!("3"), Dialect::Syslog), Some(17));
    // 17.0 is a JSON float: `as_i64` declines it, so it names no rung.
    assert_eq!(reading(&json!(17.0), Dialect::Otel), None);
    assert_eq!(reading(&json!(1.5), Dialect::Otel), None);
    assert_eq!(reading(&json!(true), Dialect::Otel), None);
    assert_eq!(reading(&json!(null), Dialect::Otel), None);
    assert_eq!(reading(&json!([17]), Dialect::Otel), None);
    assert_eq!(reading(&json!({"n": 17}), Dialect::Otel), None);
}

/// The numeric half is the one place a dialect changes anything.
#[test]
fn reading_number_per_dialect() {
    for n in 1..=24i64 {
        assert_eq!(reading_number(n, Dialect::Otel), u8::try_from(n).ok());
    }
    assert_eq!(reading_number(0, Dialect::Otel), None);
    assert_eq!(reading_number(25, Dialect::Otel), None);
    assert_eq!(reading_number(i64::MAX, Dialect::Otel), None);
    assert_eq!(reading_number(i64::MIN, Dialect::Otel), None);
    for n in 0..=7i64 {
        assert_eq!(
            reading_number(n, Dialect::Syslog),
            from_syslog(u8::try_from(n).unwrap())
        );
    }
    assert_eq!(reading_number(8, Dialect::Syslog), None);
    assert_eq!(reading_number(-1, Dialect::Syslog), None);
    assert_eq!(reading_number(i64::MAX, Dialect::Syslog), None);
}

/// The dialect vocabulary is closed, ASCII-case-insensitive, and its
/// tokens round-trip — the same set the DSL's `sev()` second argument and
/// the ingest config accept.
#[test]
fn dialect_tokens_round_trip() {
    assert_eq!(Dialect::from_token("otel"), Some(Dialect::Otel));
    assert_eq!(Dialect::from_token("OTEL"), Some(Dialect::Otel));
    assert_eq!(Dialect::from_token("Syslog"), Some(Dialect::Syslog));
    assert_eq!(Dialect::from_token("rfc5424"), None);
    assert_eq!(Dialect::from_token(""), None);
    assert_eq!(Dialect::from_token(" otel"), None);
    for token in DIALECT_TOKENS {
        let dialect = Dialect::from_token(token).expect("vocabulary parses");
        assert_eq!(dialect.token(), *token);
    }
    assert_eq!(Dialect::default(), Dialect::Otel);
}

/// The table the SQL mirror generates its arms from is the table this
/// module matches against — exposed read-only, in table order.
#[test]
fn token_entries_expose_the_table() {
    let entries: Vec<(&str, u8)> = token_entries().collect();
    assert_eq!(entries.len(), 20);
    assert_eq!(entries[0], ("trace", 1));
    for (token, number) in entries {
        assert_eq!(number_for_token(token), Some(number));
    }
}

/// The render rule folds case, because `DuckDB` identifiers do: a
/// `let S = sev(x)` returns the column spelled `S` while the pin scope
/// declares the folded `s`, so an exact match would render numbers there.
#[test]
fn renders_as_severity_folds_ascii_case() {
    let declared = vec!["s".to_owned(), "myLevel".to_owned()];
    for column in ["s", "S", "myLevel", "MYLEVEL", "mylevel"] {
        assert!(
            renders_as_severity(column, &declared),
            "{column} must render as severity"
        );
    }
    // The envelope slot, by name, whatever the spelling.
    assert!(renders_as_severity("_severity", &[]));
    assert!(renders_as_severity("_SEVERITY", &[]));
    // Anything else is ordinary data.
    assert!(!renders_as_severity("severity", &declared));
    assert!(!renders_as_severity("st", &declared));
    assert!(!renders_as_severity("s2", &declared));
    assert!(!renders_as_severity("s", &[]));
}

/// The dialect-ambiguous set is exactly the integers 1-7.
///
/// The two ladders overlap only there: `0` is emerg to syslog and nothing
/// to `OTel`, 8-24 are `OTel` rungs syslog has no numeral for, and every
/// token is dialect-free by construction. Pinned over a wide integer
/// range and the whole token vocabulary, because this set is what the
/// repin's ambiguity gate refuses on — a rung leaking in either direction
/// is either a silent mistranslation or a refusal nothing can clear.
#[test]
fn dialect_ambiguity_is_exactly_the_syslog_overlap() {
    for n in -30..=30i64 {
        let text = n.to_string();
        assert_eq!(
            dialect_ambiguous(&text),
            (1..=7).contains(&n),
            "integer {n} misclassified"
        );
    }
    // Spelling never decides: a sign, padding and leading zeros all read
    // through to the same integer, so they carry its verdict.
    for (text, ambiguous) in [
        ("+3", true),
        (" 3 ", true),
        ("\u{a0}3\u{a0}", true),
        ("007", true),
        ("-3", false),
        ("+17", false),
        ("017", false),
        ("3.0", false),
        ("1e1", false),
        ("0x3", false),
        ("", false),
        ("gold", false),
    ] {
        assert_eq!(dialect_ambiguous(text), ambiguous, "{text:?}");
    }
    // Words are dialect-free: every token and every exact short name reads
    // the same in both dialects, whatever its case.
    for (token, _) in token_entries() {
        for spelling in [token.to_owned(), token.to_uppercase()] {
            assert!(!dialect_ambiguous(&spelling), "token {spelling}");
        }
    }
    for n in 1..=24u8 {
        assert!(!dialect_ambiguous(otel_name(n).unwrap()), "exact name {n}");
    }
}
