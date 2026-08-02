// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `OTel` severity ladder: token tables, bands, and syslog inversion.
//!
//! Single source of truth for every consumer — the SQL emitter (`level`
//! band predicates), the in-memory filter (SSE parity), ingest severity
//! derivation, and the syslog listener. The token and syslog tables are
//! the issue-verbatim ADR-0009 tables; a change here changes ingest and
//! query behaviour together, which is the point.

#[cfg(test)]
mod tests;

/// Inclusive severity bands on the `OTel` `SeverityNumber` ladder.
pub const TRACE_BAND: (u8, u8) = (1, 4);
pub const DEBUG_BAND: (u8, u8) = (5, 8);
pub const INFO_BAND: (u8, u8) = (9, 12);
pub const WARN_BAND: (u8, u8) = (13, 16);
pub const ERROR_BAND: (u8, u8) = (17, 20);
pub const FATAL_BAND: (u8, u8) = (21, 24);

/// The complete severity token table (ADR-0009, issue-verbatim).
///
/// Tokens are matched case-insensitively; each maps to an exact
/// `SeverityNumber`. Single letters cover `klog`/`glog`-style prefixes.
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

/// The canonical token spellings, for error messages naming valid tokens.
pub const CANONICAL_TOKENS: &[&str] = &[
    "trace", "debug", "info", "notice", "warn", "error", "fatal", "alert", "emerg",
];

/// Map a severity token to its exact `SeverityNumber`.
///
/// Tokens are lowercased before lookup (severity name tokens are matched
/// case-insensitively; comparison operators elsewhere stay case-sensitive).
/// Returns `None` for anything outside the table.
pub fn number_for_token(token: &str) -> Option<u8> {
    let lowered = token.to_ascii_lowercase();
    TOKEN_TABLE
        .iter()
        .find(|(t, _)| *t == lowered)
        .map(|&(_, n)| n)
}

/// Map a syslog severity numeral (0-7) to the `OTel` `SeverityNumber`.
///
/// Syslog counts **down** from Emergency 0 while `OTel` counts up — this
/// inversion is deliberate; a naive numeric passthrough would silently
/// invert every severity in the corpus.
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

/// The inclusive band `(lo, hi)` containing a `SeverityNumber`.
///
/// Returns `None` when the number is outside the 1-24 ladder.
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

/// The band name for a `SeverityNumber` (display/coloring), or `None` when
/// outside the ladder.
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

/// Is this integer a valid `SeverityNumber` (1-24)?
pub fn is_valid_number(n: i64) -> bool {
    (1..=24).contains(&n)
}
