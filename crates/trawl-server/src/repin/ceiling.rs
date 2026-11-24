// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Force ceilings: the numbers a forced repin agreed to.

use std::fmt;

use trawl_core::schema::CanonicalType;
use trawl_core::severity::Dialect;

/// Absent means "no opinion, derive one from this job's own scan".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestedCeilings {
    pub max_nulled: Option<u64>,
    pub max_ambiguous: Option<u64>,
}

/// The ceilings a job is actually held to. An unstated one is the
/// scan-derived default, never "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceilings {
    /// Rows the new pin may null before the cutover is refused.
    pub max_nulled: u64,
    /// Dialect-ambiguous numerals the job may carry before it is refused.
    /// Only consulted when [`ambiguity_binds`] says the dimension applies.
    pub max_ambiguous: u64,
}

/// The force terms of a measured repin plan. Forced plans always have bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceTerms {
    /// Any loss requires the operator to accept a forced plan.
    Unforced,
    /// Force accepts at most these recorded counts.
    Forced(Ceilings),
}

impl ForceTerms {
    #[must_use]
    pub fn unforced() -> Self {
        Self::Unforced
    }

    #[must_use]
    pub fn forced(ceilings: Ceilings) -> Self {
        Self::Forced(ceilings)
    }

    #[must_use]
    pub fn ceilings(self) -> Option<Ceilings> {
        match self {
            Self::Unforced => None,
            Self::Forced(ceilings) => Some(ceilings),
        }
    }

    /// Decode the persisted plan state without inventing accepted bounds.
    /// An unfinished scan has no terms, regardless of the requested force flag.
    pub(crate) fn from_record(
        force: bool,
        planned: bool,
        max_nulled: Option<i64>,
        max_ambiguous: Option<i64>,
    ) -> Result<Option<Self>, &'static str> {
        match (force, planned, max_nulled, max_ambiguous) {
            (_, false, None, None) => Ok(None),
            (false, true, None, None) => Ok(Some(Self::Unforced)),
            (true, true, Some(nulled), Some(ambiguous)) => {
                let max_nulled =
                    u64::try_from(nulled).map_err(|_| "negative accepted nulled ceiling")?;
                let max_ambiguous =
                    u64::try_from(ambiguous).map_err(|_| "negative accepted ambiguity ceiling")?;
                Ok(Some(Self::Forced(Ceilings {
                    max_nulled,
                    max_ambiguous,
                })))
            }
            _ => Err("repin force bounds do not match its recorded plan state"),
        }
    }
}

/// The ceiling a scan of `scan` rows implies: 10% headroom, never less than
/// 10 rows.
///
/// Integer-exact, no floats, so that no value of `scan` can round through
/// an f64 mantissa. A corpus near `u64::MAX` saturates rather than
/// wrapping into a ceiling below its own scan.
#[must_use]
pub fn default_ceiling(scan: u64) -> u64 {
    let ten = scan / 10 + u64::from(!scan.is_multiple_of(10));
    scan.saturating_add(ten.max(10))
}

/// Settle the ceilings for a job from its scan and its request.
#[must_use]
pub fn resolve(scanned_nulled: u64, scanned_ambiguous: u64, req: RequestedCeilings) -> Ceilings {
    Ceilings {
        max_nulled: req
            .max_nulled
            .unwrap_or_else(|| default_ceiling(scanned_nulled)),
        max_ambiguous: req
            .max_ambiguous
            .unwrap_or_else(|| default_ceiling(scanned_ambiguous)),
    }
}

/// Whether the ambiguity dimension applies to this target at all.
///
/// The forced ceiling arm and the unforced gate both ask this one
/// question, so a syslog exemption cannot hold in one arm and not the other.
#[must_use]
pub fn ambiguity_binds(pin: CanonicalType, dialect: Option<Dialect>) -> bool {
    pin == CanonicalType::Severity && dialect != Some(Dialect::Syslog)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKind {
    /// Rows the new pin cannot read.
    Nulled,
    /// Numerals the two severity dialects read differently.
    Ambiguous,
}

impl CountKind {
    /// The word for this dimension's rows in a refusal sentence.
    const fn noun(self) -> &'static str {
        match self {
            Self::Nulled => "nulled row(s)",
            Self::Ambiguous => "ambiguous numeral(s)",
        }
    }

    /// What the dimension is called when it leads the sentence.
    const fn dimension(self) -> &'static str {
        match self {
            Self::Nulled => "loss",
            Self::Ambiguous => "dialect ambiguity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub nulled: u64,
    pub ambiguous: u64,
}

/// A forced job that went past what it accepted.
///
/// Carries both dimensions even though `kind` names the one that tripped:
/// the operator's next move is to re-run with corrected flags, and a message
/// that reported only the failing number would send them back for the other
/// one. The pair is what the sentence prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exceeded {
    /// The dimension that went over. Loss is checked first.
    pub kind: CountKind,
    pub accepted: Ceilings,
    pub actual: Counts,
}

impl fmt::Display for Exceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the rewrite exceeds the {} this force accepted: {} {} against an accepted {}, \
             and {} {} against an accepted {}",
            self.kind.dimension(),
            self.actual.nulled,
            CountKind::Nulled.noun(),
            self.accepted.max_nulled,
            self.actual.ambiguous,
            CountKind::Ambiguous.noun(),
            self.accepted.max_ambiguous
        )
    }
}

/// Whether these counts are past the accepted ceilings.
#[must_use]
pub fn exceeds(
    pin: CanonicalType,
    dialect: Option<Dialect>,
    ceilings: Ceilings,
    nulled: u64,
    ambiguous: u64,
) -> Option<Exceeded> {
    let actual = Counts { nulled, ambiguous };
    let over = |kind| {
        Some(Exceeded {
            kind,
            accepted: ceilings,
            actual,
        })
    };
    if nulled > ceilings.max_nulled {
        return over(CountKind::Nulled);
    }
    if ambiguity_binds(pin, dialect) && ambiguous > ceilings.max_ambiguous {
        return over(CountKind::Ambiguous);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_force_terms_distinguish_unplanned_and_bounded_plans() {
        for force in [false, true] {
            assert_eq!(ForceTerms::from_record(force, false, None, None), Ok(None));
        }
        assert_eq!(
            ForceTerms::from_record(false, true, None, None),
            Ok(Some(ForceTerms::Unforced))
        );
        assert_eq!(
            ForceTerms::from_record(true, true, Some(0), Some(10)),
            Ok(Some(ForceTerms::Forced(Ceilings {
                max_nulled: 0,
                max_ambiguous: 10
            })))
        );
    }

    #[test]
    fn recorded_force_terms_reject_every_inconsistent_shape() {
        for force in [false, true] {
            for planned in [false, true] {
                for nulled in [None, Some(-1), Some(0)] {
                    for ambiguous in [None, Some(-1), Some(0)] {
                        let valid = matches!(
                            (force, planned, nulled, ambiguous),
                            (_, false, None, None)
                                | (false, true, None, None)
                                | (true, true, Some(0), Some(0))
                        );
                        assert_eq!(
                            ForceTerms::from_record(force, planned, nulled, ambiguous).is_ok(),
                            valid,
                            "force={force}, planned={planned}, nulled={nulled:?}, ambiguous={ambiguous:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_default_ceiling_is_ten_percent_over_a_floor_of_ten() {
        for (scan, want) in [
            (0, 10),
            (1, 11),
            (5, 15),
            (50, 60),
            (99, 109),
            (100, 110),
            (101, 112),
        ] {
            assert_eq!(default_ceiling(scan), want, "scan={scan}");
        }
        assert_eq!(
            default_ceiling(u64::MAX),
            u64::MAX,
            "a saturating ceiling, never one that wraps below its own scan"
        );
        assert_eq!(default_ceiling(100), 100 + 10);
        assert!(default_ceiling(101) - 101 > 10);
    }

    #[test]
    fn an_explicit_ceiling_wins_including_zero_and_below_scan() {
        let derived = resolve(40, 7, RequestedCeilings::default());
        assert_eq!(derived.max_nulled, 50);
        assert_eq!(derived.max_ambiguous, 17);

        let zero = resolve(
            40,
            7,
            RequestedCeilings {
                max_nulled: Some(0),
                max_ambiguous: Some(0),
            },
        );
        assert_eq!(zero.max_nulled, 0, "zero is a statement, not a mistake");
        assert_eq!(zero.max_ambiguous, 0);

        let below = resolve(
            40,
            7,
            RequestedCeilings {
                max_nulled: Some(3),
                max_ambiguous: None,
            },
        );
        assert_eq!(below.max_nulled, 3, "below the scan is allowed");
        assert_eq!(
            below.max_ambiguous, 17,
            "the unstated dimension keeps its scan-derived default"
        );

        let other = resolve(
            40,
            7,
            RequestedCeilings {
                max_nulled: None,
                max_ambiguous: Some(1),
            },
        );
        assert_eq!(other.max_nulled, 50);
        assert_eq!(other.max_ambiguous, 1);
    }

    #[test]
    fn the_ceiling_boundary_passes_at_accepted_and_refuses_one_above() {
        const SEVERITY: CanonicalType = CanonicalType::Severity;
        const VARCHAR: CanonicalType = CanonicalType::Varchar;
        let c = Ceilings {
            max_nulled: 10,
            max_ambiguous: 4,
        };

        assert_eq!(exceeds(VARCHAR, None, c, 10, 0), None, "equality passes");
        let over = exceeds(VARCHAR, None, c, 11, 0).expect("one row above refuses");
        assert_eq!(over.kind, CountKind::Nulled);

        assert_eq!(exceeds(SEVERITY, Some(Dialect::Otel), c, 0, 4), None);
        let amb = exceeds(SEVERITY, Some(Dialect::Otel), c, 0, 5)
            .expect("the ambiguity dimension refuses on its own");
        assert_eq!(amb.kind, CountKind::Ambiguous);

        let both = exceeds(SEVERITY, Some(Dialect::Otel), c, 11, 5).unwrap();
        assert_eq!(both.kind, CountKind::Nulled);

        // The sentence carries both pairs whichever dimension tripped.
        let text = amb.to_string();
        for fragment in [
            "0 nulled row(s)",
            "accepted 10",
            "5 ambiguous",
            "accepted 4",
        ] {
            assert!(text.contains(fragment), "{fragment} missing from {text}");
        }
    }

    #[test]
    fn ambiguity_binds_for_severity_under_every_reading_but_syslog() {
        assert!(ambiguity_binds(
            CanonicalType::Severity,
            Some(Dialect::Otel)
        ));
        assert!(
            ambiguity_binds(CanonicalType::Severity, None),
            "an absent dialect reads as OTel, so the gate must not go silent"
        );
        assert!(!ambiguity_binds(
            CanonicalType::Severity,
            Some(Dialect::Syslog)
        ));
        for pin in [
            CanonicalType::Varchar,
            CanonicalType::BigInt,
            CanonicalType::Double,
            CanonicalType::Boolean,
            CanonicalType::Timestamp,
        ] {
            assert!(!ambiguity_binds(pin, None), "{pin:?}");
            assert!(!ambiguity_binds(pin, Some(Dialect::Otel)), "{pin:?}");
        }
        // A non-severity target with a nonzero count is still clear.
        let c = Ceilings {
            max_nulled: 0,
            max_ambiguous: 0,
        };
        assert_eq!(exceeds(CanonicalType::Varchar, None, c, 0, 9), None);
    }
}
