// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `OTel` severity ladder: token tables, bands, syslog inversion, and the
//! one severity reader.

#[cfg(test)]
mod tests;

/// Inclusive severity bands on the `OTel` `SeverityNumber` ladder.
pub const TRACE_BAND: (u8, u8) = (1, 4);
pub const DEBUG_BAND: (u8, u8) = (5, 8);
pub const INFO_BAND: (u8, u8) = (9, 12);
pub const WARN_BAND: (u8, u8) = (13, 16);
pub const ERROR_BAND: (u8, u8) = (17, 20);
pub const FATAL_BAND: (u8, u8) = (21, 24);

/// The complete severity token table (ADR-0009).
const TOKEN_TABLE: &[(&str, u8)] = &[
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

pub fn token_entries() -> impl Iterator<Item = (&'static str, u8)> {
    TOKEN_TABLE.iter().copied()
}

pub const CANONICAL_TOKENS: &[&str] = &[
    "trace", "debug", "info", "notice", "warn", "error", "fatal", "alert", "emerg",
];

pub fn number_for_token(token: &str) -> Option<u8> {
    let lowered = token.to_ascii_lowercase();
    TOKEN_TABLE
        .iter()
        .find(|(t, _)| *t == lowered)
        .map(|&(_, n)| n)
}

const BAND_BASES: [&str; 6] = ["trace", "debug", "info", "warn", "error", "fatal"];

/// The `OTel` short name for a `SeverityNumber` — injective over 1-24.
/// A band-name rendering would make `_severity=error2` filter rows it
/// displays as `error`, and `_severity=error2*` unmatchable.
#[must_use]
pub fn otel_name(number: u8) -> Option<&'static str> {
    const NAMES: [&str; 24] = [
        "trace", "trace2", "trace3", "trace4", "debug", "debug2", "debug3", "debug4", "info",
        "info2", "info3", "info4", "warn", "warn2", "warn3", "warn4", "error", "error2", "error3",
        "error4", "fatal", "fatal2", "fatal3", "fatal4",
    ];
    if number == 0 {
        return None;
    }
    NAMES.get(usize::from(number) - 1).copied()
}

#[must_use]
pub fn token_text(number: i64) -> Option<&'static str> {
    u8::try_from(number).ok().and_then(otel_name)
}

/// The match is ASCII-case-insensitive, and has to be: `| let S = sev(level)`
/// returns a column spelled `S` while the pin scope declares `s`.
#[must_use]
pub fn renders_as_severity(column: &str, severity_columns: &[String]) -> bool {
    column.eq_ignore_ascii_case(crate::schema::SEVERITY)
        || severity_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
}

#[must_use]
pub fn number_for_exact(token: &str) -> Option<u8> {
    let lowered = token.to_ascii_lowercase();
    let (base, offset) = match lowered.as_bytes().last() {
        Some(d @ b'2'..=b'4') => (&lowered[..lowered.len() - 1], (*d - b'0') - 1),
        _ => (lowered.as_str(), 0),
    };
    let index = BAND_BASES.iter().position(|b| *b == base)?;
    #[allow(clippy::cast_possible_truncation)] // index < 6
    Some((index as u8) * 4 + offset + 1)
}

pub fn from_syslog(severity: u8) -> Option<u8> {
    match severity {
        7 => Some(5),  // debug
        6 => Some(9),  // info
        5 => Some(10), // notice
        4 => Some(13), // warning
        3 => Some(17), // err
        2 => Some(21), // crit
        1 => Some(23), // alert
        0 => Some(24), // emerg
        _ => None,
    }
}

pub fn band_of(number: u8) -> Option<(u8, u8)> {
    match number {
        1..=4 => Some(TRACE_BAND),
        5..=8 => Some(DEBUG_BAND),
        9..=12 => Some(INFO_BAND),
        13..=16 => Some(WARN_BAND),
        17..=20 => Some(ERROR_BAND),
        21..=24 => Some(FATAL_BAND),
        _ => None,
    }
}

pub fn band_name(number: u8) -> Option<&'static str> {
    match number {
        1..=4 => Some("trace"),
        5..=8 => Some("debug"),
        9..=12 => Some("info"),
        13..=16 => Some("warn"),
        17..=20 => Some("error"),
        21..=24 => Some("fatal"),
        _ => None,
    }
}

pub fn is_valid_number(n: i64) -> bool {
    (1..=24).contains(&n)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Otel,
    Syslog,
}

pub const DIALECT_TOKENS: &[&str] = &["otel", "syslog"];

impl Dialect {
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "otel" => Some(Self::Otel),
            "syslog" => Some(Self::Syslog),
            _ => None,
        }
    }

    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Otel => "otel",
            Self::Syslog => "syslog",
        }
    }
}

/// The whitespace [`reading_text`] trims: the Unicode `White_Space`
/// property, enumerated. The SQL mirror trims the same set, and the two have
/// to be the same set or a padded value reads one way in a live tail and
/// another in its own batch query.
pub const WHITESPACE: [char; 25] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{20}', '\u{85}', '\u{a0}', '\u{1680}',
    '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
    '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
];

#[must_use]
pub fn reading(value: &serde_json::Value, dialect: Dialect) -> Option<u8> {
    match value {
        serde_json::Value::String(s) => reading_text(s, dialect),
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| reading_number(n, dialect)),
        _ => None,
    }
}

/// Strict means `str::parse::<i64>`: the SQL mirror admits exactly
/// `[+-]?[0-9]+`, so any wider Rust-side parse would be a batch/live split.
#[must_use]
pub fn reading_text(text: &str, dialect: Dialect) -> Option<u8> {
    let trimmed = text.trim_matches(WHITESPACE.as_slice());
    if let Some(number) = number_for_token(trimmed) {
        return Some(number);
    }
    if let Some(number) = number_for_exact(trimmed) {
        return Some(number);
    }
    trimmed
        .parse::<i64>()
        .ok()
        .and_then(|n| reading_number(n, dialect))
}

#[must_use]
pub fn dialect_ambiguous(text: &str) -> bool {
    match (
        reading_text(text, Dialect::Otel),
        reading_text(text, Dialect::Syslog),
    ) {
        (Some(otel), Some(syslog)) => otel != syslog,
        _ => false,
    }
}

#[must_use]
pub fn reading_number(number: i64, dialect: Dialect) -> Option<u8> {
    match dialect {
        Dialect::Otel => is_valid_number(number)
            .then(|| u8::try_from(number).ok())
            .flatten(),
        Dialect::Syslog => u8::try_from(number).ok().and_then(from_syslog),
    }
}
