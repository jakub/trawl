// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The degraded-field analyzer: a read-time verdict over the durable conflict
//! evidence, with no daemon, no verdict table and no state of its own.
//!
//! A pin that keeps nulling values is not itself news: a burst of bad data
//! from one deploy is what the conform is for, and the values stay in `_raw`.
//! What deserves an operator's attention is a pin that has been losing data
//! long enough to be the system's model of the field rather than an incident.
//! Hence a gate with two independent halves: the evidence must span
//! [`DEGRADED_MIN_SPAN`], and it must carry volume, either
//! [`DEGRADED_MIN_ROWS_SHELVED`] rows shelved or [`DEGRADED_MIN_EPISODES`]
//! distinct episodes. The span alone would badge a field that lost two rows
//! a month apart; the volume alone would badge one bad deploy.
//!
//! There is no multi-sender gate (ADR-0011): a homelab or SMB install commonly
//! has exactly one legitimate producer per field, and requiring two would make
//! the badge unreachable precisely where it is most useful. Sender count is
//! displayed evidence. The verdict is advisory and sender-influenceable; the
//! control against a hostile sender steering an operator into rewriting an
//! archive is the human approval plus `schema_write`, never this function.
//!
//! Pure by construction: no async, no pool, no clock beyond the instants it
//! is handed. The store reads the rows ([`crate::store::CatalogStore::conflict_aggregates`]
//! and `conflict_evidence_for`); the route handlers compose the two.

use chrono::{DateTime, TimeDelta, Utc};
use trawl_api::DegradedVerdict;
use trawl_core::schema::{CanonicalType, TypeResolution, normalize_duckdb_type};

use crate::store::MAX_CONFLICT_SAMPLES;

/// Minimum evidence span (`last_at - first_at`) before a pin can be called
/// degraded.
///
/// A day is the shortest window that cannot be one incident: a bad deploy,
/// a misconfigured shipper caught the same afternoon, a backfill of malformed
/// history. Anything still conflicting a day later is the steady state.
pub const DEGRADED_MIN_SPAN: TimeDelta = TimeDelta::hours(24);

/// Volume floor, rows: lifetime rows the pin has shelved.
pub const DEGRADED_MIN_ROWS_SHELVED: i64 = 100;

/// Volume floor, episodes: distinct conforming casts that nulled something.
///
/// The OR sibling of [`DEGRADED_MIN_ROWS_SHELVED`], and not redundant with
/// it: a low-volume sender losing one row per hour for a week never reaches
/// a hundred rows, and is exactly as wrong about its pin as a noisy one.
pub const DEGRADED_MIN_EPISODES: i64 = 3;

/// One field's conflict evidence, aggregated across every service that
/// contributed it — the shape the analyzer judges.
///
/// Per field, never per `(field, service)`: the pin is global, so the
/// verdict is, and `field_conflict_stats`' service axis is client-chosen
/// and unbounded (the aggregation happens in SQL for that reason).
#[derive(Debug, Clone)]
pub struct ConflictAggregate {
    /// Field name (a catalog key).
    pub field: String,
    /// Earliest evidence across the field's services.
    pub first_at: DateTime<Utc>,
    /// Latest evidence across the field's services.
    pub last_at: DateTime<Utc>,
    /// Distinct services that ever conflicted on the field.
    pub services: i64,
    /// Conflict episodes summed across those services.
    pub episodes: i64,
    /// Rows nulled summed across those services — lifetime, unlike the
    /// `field_conflicts` window's sum.
    pub rows_nulled_total: i64,
    /// The episode high-water an operator has acknowledged for this field
    /// (`field_degraded_ack.evidence_through`), or `None` when nobody has.
    ///
    /// Carried, not yet judged: [`is_degraded`] still reads the evidence
    /// alone. The suppression rule that consumes this (`episodes <=
    /// evidence_through`) lands with the ack routes, which is also where
    /// this gate splits into a threshold half and a suppression half.
    pub ack_evidence_through: Option<i64>,
}

/// Whether the evidence indicts the pin: sustained AND consequential.
#[must_use]
pub fn is_degraded(agg: &ConflictAggregate) -> bool {
    agg.last_at - agg.first_at >= DEGRADED_MIN_SPAN
        && (agg.rows_nulled_total >= DEGRADED_MIN_ROWS_SHELVED
            || agg.episodes >= DEGRADED_MIN_EPISODES)
}

/// The type the evidence suggests repinning to.
///
/// `observed_types` are the `DuckDB` types the conflicting batches carried,
/// per-column inferences rather than per-value ones, so they are read through
/// [`normalize_duckdb_type`], never `CanonicalType::from_catalog` (which
/// matches canonical spellings exactly and would drop `INTEGER`, `HUGEINT`
/// and `JSON` on the floor). One rung, unanimously, and different from the
/// current pin, is a suggestion; anything else resolves to `VARCHAR`: mixed
/// rungs, `JSON` (which `read_json` infers for mixed scalars), an
/// out-of-range integer, no evidence at all, or a rung that is the current
/// pin.
///
/// `VARCHAR` is the fallback by choice, not as a shrug: a field whose values
/// genuinely disagree about their type is an enum-shaped field, and text is
/// where it stops losing data.
#[must_use]
pub fn suggested_target(current: CanonicalType, observed_types: &[String]) -> CanonicalType {
    let mut uniform: Option<CanonicalType> = None;
    for observed in observed_types {
        let TypeResolution::Pin(t) = normalize_duckdb_type(observed) else {
            return CanonicalType::Varchar;
        };
        match uniform {
            None => uniform = Some(t),
            Some(seen) if seen == t => {}
            Some(_) => return CanonicalType::Varchar,
        }
    }
    match uniform {
        Some(t) if t != current => t,
        _ => CanonicalType::Varchar,
    }
}

/// Build the verdict for a field the caller has already found degraded.
///
/// `evidence` is the field's retained conflict rows as
/// `(observed_type, samples)`, newest first — the rows the recency window
/// kept, which is why the samples are a window while `rows_shelved` is a
/// lifetime total. The two numbers will disagree, and should: one says what
/// the pin has cost, the other shows what it is costing right now.
#[must_use]
pub fn verdict(
    agg: &ConflictAggregate,
    current: CanonicalType,
    evidence: &[(String, Vec<String>)],
) -> DegradedVerdict {
    let observed: Vec<String> = evidence.iter().map(|(t, _)| t.clone()).collect();

    // Distinct, newest first, capped by the same budget one conflict row
    // carries — the case file is a sample of a sample, never a dump.
    let mut samples: Vec<String> = Vec::with_capacity(MAX_CONFLICT_SAMPLES);
    for value in evidence.iter().flat_map(|(_, s)| s) {
        if samples.len() == MAX_CONFLICT_SAMPLES {
            break;
        }
        if !samples.iter().any(|kept| kept == value) {
            samples.push(value.clone());
        }
    }

    DegradedVerdict {
        since: agg
            .first_at
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        services: u64::try_from(agg.services).unwrap_or(0),
        episodes: u64::try_from(agg.episodes).unwrap_or(0),
        rows_shelved: u64::try_from(agg.rows_nulled_total).unwrap_or(0),
        samples,
        suggested_to: suggested_target(current, &observed).as_catalog().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg(span: TimeDelta, episodes: i64, rows: i64) -> ConflictAggregate {
        let last_at = Utc::now();
        ConflictAggregate {
            field: "duration".to_owned(),
            first_at: last_at - span,
            last_at,
            services: 1,
            episodes,
            rows_nulled_total: rows,
            ack_evidence_through: None,
        }
    }

    /// Both halves of the gate, at their boundaries: the span is inclusive,
    /// the volume floors are inclusive and independent, and single-service
    /// evidence qualifies (sender count is never a gate).
    #[test]
    fn the_degraded_gate_is_span_and_volume() {
        let day = TimeDelta::hours(24);
        let cases: [(TimeDelta, i64, i64, bool, &str); 8] = [
            (day, 1, 100, true, "span met, rows met"),
            (day, 3, 0, true, "span met, episodes met"),
            (day, 1, 99, false, "one row short"),
            (day, 2, 0, false, "one episode short"),
            (
                day - TimeDelta::seconds(1),
                1000,
                100_000,
                false,
                "a second short of the span, whatever the volume",
            ),
            (day + TimeDelta::hours(1), 3, 0, true, "past the span"),
            (TimeDelta::zero(), 3, 100, false, "one episode, no span"),
            (
                TimeDelta::days(30),
                1,
                1,
                false,
                "a month apart, two rows lost — an incident, not a model",
            ),
        ];
        for (span, episodes, rows, expect, why) in cases {
            assert_eq!(
                is_degraded(&agg(span, episodes, rows)),
                expect,
                "{why}: span={span}, episodes={episodes}, rows={rows}"
            );
        }
    }

    /// One sender is enough: there is no multi-sender gate.
    #[test]
    fn single_service_evidence_qualifies() {
        let mut a = agg(TimeDelta::hours(25), 5, 500);
        a.services = 1;
        assert!(is_degraded(&a));
    }

    #[test]
    fn suggested_target_is_the_uniform_rung_else_varchar() {
        let big = CanonicalType::BigInt;
        let cases: [(&[&str], CanonicalType, CanonicalType, &str); 9] = [
            (
                &["VARCHAR", "VARCHAR"],
                big,
                CanonicalType::Varchar,
                "the archetype: strings under a numeric pin",
            ),
            (
                &["DOUBLE", "DOUBLE"],
                big,
                CanonicalType::Double,
                "fractions under BIGINT widen to DOUBLE",
            ),
            (
                &["INTEGER", "BIGINT"],
                CanonicalType::Varchar,
                CanonicalType::BigInt,
                "spellings of one rung are uniform",
            ),
            (
                &["VARCHAR", "DOUBLE"],
                big,
                CanonicalType::Varchar,
                "mixed rungs have no honest number",
            ),
            (
                &["JSON"],
                big,
                CanonicalType::Varchar,
                "read_json's mixed-scalar inference is not a rung",
            ),
            (
                &["HUGEINT"],
                big,
                CanonicalType::Varchar,
                "out of range resolves to the ladder, not a rung",
            ),
            (
                &[],
                big,
                CanonicalType::Varchar,
                "no evidence suggests nothing better than text",
            ),
            (
                &["BIGINT", "BIGINT"],
                big,
                CanonicalType::Varchar,
                "a suggestion that IS the current pin is no suggestion",
            ),
            (
                &["TIMESTAMP WITH TIME ZONE"],
                CanonicalType::Varchar,
                CanonicalType::Timestamp,
                "timestamp spellings normalize",
            ),
        ];
        for (observed, current, expect, why) in cases {
            let observed: Vec<String> = observed.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(suggested_target(current, &observed), expect, "{why}");
        }
    }

    /// The verdict carries the facts and nothing else: the lifetime totals,
    /// the sender count as evidence, and a de-duplicated, capped sample set
    /// drawn newest-first across the retained rows.
    #[test]
    fn the_verdict_is_capped_distinct_facts() {
        let mut a = agg(TimeDelta::hours(48), 9, 900);
        a.services = 3;
        let evidence = vec![
            (
                "VARCHAR".to_owned(),
                vec!["n/a".to_owned(), "pending".to_owned()],
            ),
            (
                "VARCHAR".to_owned(),
                vec![
                    "n/a".to_owned(),
                    "accepted".to_owned(),
                    "queued".to_owned(),
                    "held".to_owned(),
                    "gone".to_owned(),
                ],
            ),
        ];
        let v = verdict(&a, CanonicalType::BigInt, &evidence);
        assert_eq!((v.services, v.episodes, v.rows_shelved), (3, 9, 900));
        assert_eq!(v.suggested_to, "VARCHAR");
        assert_eq!(
            v.samples,
            vec!["n/a", "pending", "accepted", "queued", "held"],
            "newest evidence first, distinct, capped"
        );
        assert!(v.since.ends_with('Z'), "since is ISO 8601 UTC: {}", v.since);
    }
}
