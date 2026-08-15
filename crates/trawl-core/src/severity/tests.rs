// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;

/// The `OTel` short names are INJECTIVE over the whole 1-24 ladder — the
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
    // table carries are NOT exact names.
    assert_eq!(number_for_exact("error1"), None);
    assert_eq!(number_for_exact("error5"), None);
    assert_eq!(number_for_exact("gold"), None);
    assert_eq!(number_for_exact("err"), None);
    assert_eq!(number_for_exact("notice"), None);
    assert_eq!(number_for_exact(""), None);
}

/// Every token in the issue table maps to its exact number.
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

/// Syslog numerals invert: 0 (emerg) is the TOP of the `OTel` ladder,
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
    // notice (10) falls inside the INFO band — documented.
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
