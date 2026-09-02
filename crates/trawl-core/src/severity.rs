// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `OTel` severity ladder: token tables, bands, syslog inversion, and the
//! one severity reader.
//!
//! Single source of truth for every consumer — the SQL emitter
//! (`_severity` band predicates), the in-memory filter (SSE parity), and
//! ingest's `_severity` derivation, including the syslog profile's
//! inverted numerals. A change here changes ingest and query behaviour
//! together, which is the point.
//!
//! [`reading`] is the reader (ADR-0013): ingest's derivation, the DSL's
//! `sev()` in all three lanes, and the `SEVERITY` pin's conform rung all
//! resolve a value through it. The SQL form
//! ([`crate::conform::severity_reading_sql`]) is generated from these same
//! tables and probe-pinned against this function in
//! `trawl-engine/tests/duckdb_probe.rs`, so "what is this value's
//! severity" has exactly one answer whichever engine asks.

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

/// The token table, read-only — `(spelling, SeverityNumber)` in table
/// order.
///
/// Exposed so [`crate::conform::severity_reading_sql`] can generate its
/// `CASE` arms from the same rows this module matches against: a token
/// added here reaches the SQL reader without a second edit, which is what
/// keeps the two engines reading one kernel.
pub fn token_entries() -> impl Iterator<Item = (&'static str, u8)> {
    TOKEN_TABLE.iter().copied()
}

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

/// The six band base names, in ladder order — the stem of every `OTel`
/// short name (`error`, `error2`, `error3`, `error4` for 17-20).
const BAND_BASES: [&str; 6] = ["trace", "debug", "info", "warn", "error", "fatal"];

/// The `OTel` short name for a `SeverityNumber` — injective over 1-24.
///
/// This is the canonical text of a `SEVERITY`-pinned value: what results
/// display, what a glob or regex matches, and what `number_for_exact`
/// reads back. Injectivity is the load-bearing property — a band-name
/// rendering would make `_severity=error2` filter rows it displays as
/// `error`, and `_severity=error2*` unmatchable.
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

/// The display text for a `_severity` cell holding `number`, or `None`
/// where the ladder has no reading for it.
///
/// The one owner of the integer-cell display rule every result renderer
/// applies (ADR-0013 §6): the column is BIGINT on the wire, so the
/// narrowing to the ladder's `u8` domain belongs beside [`otel_name`]
/// rather than being re-derived per surface. A caller with no reading
/// renders the value the way it renders any other — never a guess.
#[must_use]
pub fn token_text(number: i64) -> Option<&'static str> {
    u8::try_from(number).ok().and_then(otel_name)
}

/// Whether a result column renders as severity tokens — the one rule
/// every renderer asks (CLI table, TUI, SPA).
///
/// Two sources, deliberately: the envelope's own `_severity`, keyed by
/// name because it is unforgeable and reaches surfaces that never saw a
/// pipeline; and the response's advisory `severity_columns` list
/// (ADR-0013), which names the `sev()` outputs and any column that took
/// the slot's pin. A renderer with no list renders `_severity` and
/// nothing else.
///
/// The match is ASCII-case-insensitive, and has to be: `DuckDB`
/// identifiers are, so `| let S = sev(level)` returns a column spelled
/// `S` while the pin scope — which folds through
/// [`crate::schema::catalog_key`] — declares `s`, and an exact comparison
/// would render that column as numbers. Two spellings are one identifier
/// here.
#[must_use]
pub fn renders_as_severity(column: &str, severity_columns: &[String]) -> bool {
    column.eq_ignore_ascii_case(crate::schema::SEVERITY)
        || severity_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
}

/// The inverse of [`otel_name`]: a band base with an optional `2`-`4`
/// suffix, matched case-insensitively.
///
/// Deliberately narrower than [`number_for_token`] — the aliases
/// (`err`, `crit`, `notice`, …) are band tokens, not exact names, and a
/// caller that wants their band asks the token table.
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

/// Map a syslog severity numeral (0-7) to the `OTel` `SeverityNumber`.
///
/// Syslog counts down from Emergency 0 while `OTel` counts up, so a naive
/// numeric passthrough would silently invert every severity in the corpus.
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

// ── the reader (ADR-0013) ─────────────────────────────────────────────

/// Which dialect a numeric severity is read in.
///
/// Words are dialect-free — they always go through the one token table —
/// so this governs numerics alone. The two dialects overlap completely
/// over 1-7 (`3` is `trace3` to `OTel` and `err` to syslog), which is why
/// no value-shape rule can tell them apart and the caller must assert
/// provenance instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Dialect {
    /// `OTel` `SeverityNumber`: 1-24, counting up. The default everywhere
    /// a caller has no transport-level evidence of the other.
    #[default]
    Otel,
    /// Syslog PRI severity: 0-7, counting down, inverted through
    /// [`from_syslog`].
    Syslog,
}

/// The dialect vocabulary, for the DSL's second `sev()` argument and for
/// error messages naming the allowed set. Same closed set the ingest
/// config takes (ADR-0013).
pub const DIALECT_TOKENS: &[&str] = &["otel", "syslog"];

impl Dialect {
    /// Parse a dialect token, ASCII-case-insensitively; `None` for
    /// anything outside [`DIALECT_TOKENS`].
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "otel" => Some(Self::Otel),
            "syslog" => Some(Self::Syslog),
            _ => None,
        }
    }

    /// This dialect's canonical token — the inverse of [`Self::from_token`].
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Otel => "otel",
            Self::Syslog => "syslog",
        }
    }
}

/// The whitespace [`reading_text`] trims: the Unicode `White_Space`
/// property, enumerated.
///
/// Enumerated rather than `char::is_whitespace` because the SQL mirror
/// trims an explicit character set (`DuckDB`'s `trim(s, chars)`), and the
/// two have to be the same set or a padded value reads one way in a live
/// tail and another in its own batch query. `DuckDB` treats the set as
/// characters, multibyte ones included — probed by execution in
/// `trawl-engine/tests/duckdb_probe.rs`, which is what lets this be the
/// full property rather than the ASCII six: a `severity` padded with, say,
/// U+00A0 still reads, where narrowing the set would drop it silently,
/// with no repair code and nothing in the event to explain it.
///
/// It is `str::trim`'s set character for character (Rust's
/// `char::is_whitespace` is `White_Space`), so ingest can delegate here
/// without changing what it accepts.
pub const WHITESPACE: [char; 25] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{20}', '\u{85}', '\u{a0}', '\u{1680}',
    '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
    '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
];

/// The severity reader: one JSON value's point on the `OTel` ladder, or
/// nothing.
///
/// Every severity question in trawl resolves here — ingest's `_severity`
/// derivation, the DSL's `sev()` (SQL, SSE and the batch tail alike), and
/// the `SEVERITY` pin's conform rung. Total by construction:
///
/// - a string reads through [`reading_text`] (tokens, then exact `OTel`
///   short names, then a strict integer);
/// - a number reads through [`reading_number`] — via `as_i64`, so `17.0`
///   and `1.5` alike have no reading (a severity is a ladder position, and
///   a fractional one names none of them);
/// - a boolean, null, array or object has no reading at all.
///
/// `None` is always "no reading", never an error: an unreadable severity
/// leaves `_severity` unwritten at ingest and NULL at query time.
#[must_use]
pub fn reading(value: &serde_json::Value, dialect: Dialect) -> Option<u8> {
    match value {
        serde_json::Value::String(s) => reading_text(s, dialect),
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| reading_number(n, dialect)),
        _ => None,
    }
}

/// The reader's text half — the rung order every lane shares.
///
/// After trimming [`WHITESPACE`]: the band token table
/// ([`number_for_token`], with its aliases), then the `OTel` exact short
/// names ([`number_for_exact`], `error2` → 18), then a strict integer.
///
/// Strict means `str::parse::<i64>`: an optional sign and digits, nothing
/// else. `4.0`, `1e1`, `1_2`, `0x10` and the empty string have no reading
/// — the SQL mirror admits exactly `[+-]?[0-9]+` and defers everything
/// else, so any wider Rust-side parse would be a batch/live split. A sign
/// is accepted by the grammar and then resolved by range: `-1` and `+17`
/// both parse, and only `+17` lands on the ladder.
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

/// Whether `text` reads as a different severity in each dialect — the
/// values a repin to `SEVERITY` can only translate by asserting
/// provenance.
///
/// True iff both dialects have a reading and the two disagree, which over
/// the whole input space is exactly the integers 1-7: `3` is `trace3` to
/// `OTel` and `err` to syslog, and the two ladders overlap nowhere else
/// (`0` and 8-24 read in one dialect only, and every token is dialect-free
/// by construction). A value with one reading is not ambiguous — it is
/// translated or lost, and the repin's own `projected_nulls` already says
/// which.
///
/// The SQL mirror is [`crate::conform::severity_dialect_ambiguous_sql`],
/// paired against this function by execution in
/// `trawl-engine/tests/duckdb_probe.rs`.
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

/// The reader's numeric half — the one place a dialect changes anything.
///
/// `OTel` passes 1-24 through unchanged; syslog inverts 0-7 through
/// [`from_syslog`]. Anything outside the dialect's own range has no
/// reading, so an out-of-ladder number is NULL rather than a severity
/// nothing can render.
#[must_use]
pub fn reading_number(number: i64, dialect: Dialect) -> Option<u8> {
    match dialect {
        Dialect::Otel => is_valid_number(number)
            .then(|| u8::try_from(number).ok())
            .flatten(),
        Dialect::Syslog => u8::try_from(number).ok().and_then(from_syslog),
    }
}
