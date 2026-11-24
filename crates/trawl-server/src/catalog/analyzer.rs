// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The degraded-field analyzer: a read-time verdict over the durable conflict
//! evidence, with no daemon, no verdict table and no state of its own.

use chrono::{DateTime, TimeDelta, Utc};
use trawl_api::DegradedVerdict;
use trawl_core::schema::{CanonicalType, TypeResolution, normalize_duckdb_type};

use crate::store::MAX_CONFLICT_SAMPLES;

/// Minimum evidence span (`last_at - first_at`) before a pin can be called
/// degraded.
pub const DEGRADED_MIN_SPAN: TimeDelta = TimeDelta::hours(24);

/// Volume floor, rows: lifetime rows the pin has shelved.
pub const DEGRADED_MIN_ROWS_SHELVED: i64 = 100;

/// Volume floor, episodes: distinct conforming casts that nulled something.
pub const DEGRADED_MIN_EPISODES: i64 = 3;

/// One field's conflict evidence, aggregated across every service that
/// contributed it — the shape the analyzer judges.
///
/// Per field, never per `(field, service)`: the pin is global, so the
/// verdict is, and `field_conflict_stats`' service axis is client-chosen
/// and unbounded (the aggregation happens in SQL for that reason).
#[derive(Debug, Clone)]
pub struct ConflictAggregate {
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
    pub ack_evidence_through: Option<i64>,
}

/// Whether the evidence alone indicts the pin: sustained AND consequential.
///
/// The raw rule, blind to acknowledgement. Two callers need it that way:
/// [`is_degraded`], which subtracts the operator's answer, and the ack
/// route's store method, which must be able to advance an existing
/// high-water on a field its own ack is currently suppressing.
#[must_use]
pub fn meets_degraded_threshold(agg: &ConflictAggregate) -> bool {
    agg.last_at - agg.first_at >= DEGRADED_MIN_SPAN
        && (agg.rows_nulled_total >= DEGRADED_MIN_ROWS_SHELVED
            || agg.episodes >= DEGRADED_MIN_EPISODES)
}

/// Whether the field is degraded as every read surface reports it: the
/// evidence meets the threshold and no acknowledgement covers it.
#[must_use]
pub fn is_degraded(agg: &ConflictAggregate) -> bool {
    meets_degraded_threshold(agg) && !acknowledged(agg)
}

fn acknowledged(agg: &ConflictAggregate) -> bool {
    agg.ack_evidence_through
        .is_some_and(|through| agg.episodes <= through)
}

/// The type the evidence suggests repinning to.
///
/// `observed_types` are the `DuckDB` types the conflicting batches carried,
/// per-column inferences rather than per-value ones, so they are read
/// through [`normalize_duckdb_type`], never `CanonicalType::from_catalog`
/// (which matches canonical spellings exactly and would drop `INTEGER`,
/// `HUGEINT` and `JSON` on the floor).
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
/// lifetime total.
#[must_use]
pub fn verdict(
    agg: &ConflictAggregate,
    current: CanonicalType,
    evidence: &[(String, Vec<String>)],
) -> DegradedVerdict {
    let observed: Vec<String> = evidence.iter().map(|(t, _)| t.clone()).collect();

    // Distinct, newest first, capped by the same budget one conflict row
    // carries.
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
            let a = agg(span, episodes, rows);
            assert_eq!(
                meets_degraded_threshold(&a),
                expect,
                "{why}: span={span}, episodes={episodes}, rows={rows}"
            );
            assert_eq!(
                is_degraded(&a),
                expect,
                "unacknowledged, the two halves agree: {why}"
            );
        }
    }

    /// The suppression half, at its boundary. The threshold is met in every
    /// row, so the ack is the only thing deciding — and it decides on the
    /// episode count, which is why the two suppressed rows carry different
    /// clocks and the same verdict.
    #[test]
    fn an_ack_suppresses_evidence_up_to_its_high_water() {
        let day = TimeDelta::hours(25);
        let cases: [(i64, Option<i64>, bool, &str); 5] = [
            (3, None, true, "threshold met, nobody acknowledged it"),
            (3, Some(3), false, "acknowledged through the last episode"),
            (4, Some(3), true, "one episode past the ack re-raises it"),
            (
                3,
                Some(9),
                false,
                "a high-water above the count still covers it (the counter \
                 only grows, so this is a cleared-evidence remnant)",
            ),
            (3, Some(0), true, "a zero high-water suppresses nothing"),
        ];
        for (episodes, through, expect, why) in cases {
            let mut a = agg(day, episodes, 0);
            a.ack_evidence_through = through;
            assert!(
                meets_degraded_threshold(&a),
                "the raw rule ignores the ack: {why}"
            );
            assert_eq!(is_degraded(&a), expect, "{why}");
        }
    }

    /// Two fields whose newest evidence lands on the same instant, one
    /// acknowledged through 3 episodes and one at 4: the count decides, and
    /// an identical clock changes nothing. This is the case a timestamp
    /// high-water would get wrong.
    #[test]
    fn equal_clocks_differing_counts_are_decided_by_the_count() {
        let shared_last_at = Utc::now();
        let build = |episodes: i64| ConflictAggregate {
            first_at: shared_last_at - TimeDelta::hours(48),
            last_at: shared_last_at,
            episodes,
            ..agg(TimeDelta::hours(48), episodes, 0)
        };
        let mut acked = build(3);
        acked.ack_evidence_through = Some(3);
        let mut raised = build(4);
        raised.ack_evidence_through = Some(3);
        assert_eq!(acked.last_at, raised.last_at, "same clock");
        assert!(!is_degraded(&acked));
        assert!(is_degraded(&raised));
    }

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
