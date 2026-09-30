// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a doctor may show of trawld's health answer, and what each reported
//! value means (ADR-0047).
//!
//! `trawl doctor` and `trawld --doctor` both read `GET /api/v1/health` and
//! turn its `checks` map into one row per check name. They classify names
//! and values here, so the two vantages cannot disagree about which values
//! pass. Each binary builds its own rows from a [`ValueClass`]: the wording
//! of a row's next action belongs to the vantage.
//!
//! A server's check names and values are remote text. A name is shown only
//! when [`is_health_key`] accepts it, and a value the doctor does not know is
//! quoted only when [`is_quotable_value`] accepts it.

use super::Outcome;

/// The health check that trawld reports while its corpus recovers after a
/// restart (ADR-0041). Only this key has values beyond `ok`, `error`, and
/// `refusing`.
pub const CORPUS_KEY: &str = "corpus";

/// A check name the report may show: `[a-z][a-z0-9_]{0,63}`.
#[must_use]
pub fn is_health_key(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A check value the report may quote: `[a-z0-9_]{1,32}`.
#[must_use]
pub fn is_quotable_value(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What one reported health value means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueClass {
    /// `ok`: the server vouches for the check.
    Ok,
    /// The corpus is still recovering after a restart (`rollup_pending` or
    /// `restart_backlog` on [`CORPUS_KEY`]). The server cannot yet vouch
    /// for what it holds, so the doctor could not look.
    Recovering,
    /// `error` or `refusing`: the server reports the check failing.
    Refused,
    /// A value this doctor does not know. It cannot pass. Quote it only
    /// when [`is_quotable_value`] accepts it.
    Unknown,
}

impl ValueClass {
    /// The outcome a row with this value has.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::Ok => Outcome::Complete,
            Self::Recovering => Outcome::NotSampled,
            Self::Refused | Self::Unknown => Outcome::Failed,
        }
    }
}

/// Classify the value `value` that the health answer reports for the check
/// `name`.
#[must_use]
pub fn classify(name: &str, value: &str) -> ValueClass {
    match value {
        "ok" => ValueClass::Ok,
        "rollup_pending" | "restart_backlog" if name == CORPUS_KEY => ValueClass::Recovering,
        "error" | "refusing" => ValueClass::Refused,
        _ => ValueClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_values_classify_by_name_and_value() {
        let table = [
            ("duckdb", "ok", ValueClass::Ok, Outcome::Complete),
            ("duckdb", "error", ValueClass::Refused, Outcome::Failed),
            (
                "ingest_capacity",
                "refusing",
                ValueClass::Refused,
                Outcome::Failed,
            ),
            (
                "corpus",
                "rollup_pending",
                ValueClass::Recovering,
                Outcome::NotSampled,
            ),
            (
                "corpus",
                "restart_backlog",
                ValueClass::Recovering,
                Outcome::NotSampled,
            ),
            // Only the corpus check recovers; elsewhere the value is unknown.
            (
                "storage_db",
                "restart_backlog",
                ValueClass::Unknown,
                Outcome::Failed,
            ),
            ("wal", "recovering", ValueClass::Unknown, Outcome::Failed),
            ("corpus", "OK", ValueClass::Unknown, Outcome::Failed),
            ("corpus", "", ValueClass::Unknown, Outcome::Failed),
        ];
        for (name, value, class, outcome) in table {
            assert_eq!(classify(name, value), class, "{name}={value}");
            assert_eq!(class.outcome(), outcome, "{class:?}");
        }
    }

    #[test]
    fn health_names_and_values_follow_the_patterns() {
        assert!(is_health_key("duckdb"));
        assert!(is_health_key(&"a".repeat(64)));
        assert!(!is_health_key(&"a".repeat(65)));
        assert!(!is_health_key(""));
        assert!(!is_health_key("_invalid"));
        assert!(!is_health_key("0day"));
        assert!(!is_health_key("Bad-Key"));
        assert!(is_quotable_value("recovering"));
        assert!(is_quotable_value("0"));
        assert!(!is_quotable_value(""));
        assert!(!is_quotable_value(&"a".repeat(33)));
        assert!(!is_quotable_value("Weird Value!"));
    }
}
