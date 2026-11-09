// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The closed set of aggregate functions (ADR-0001, Amendment 2026-11-08).
//!
//! The SQL translator ([`crate::emitter`]) and the live accumulators
//! ([`crate::stream`]) both match [`Aggregate`] with no wildcard arm, so
//! an aggregate added here does not compile until both lanes implement
//! it. A name outside the set is not an aggregate, whatever slot it is
//! written in.

/// One aggregate function. Two have a second spelling: `dc` and
/// `distinct_count` are [`Self::DistinctCount`], `values` and `list` are
/// [`Self::Values`].
///
/// The AST keeps the spelling the query wrote ([`crate::ast::AggExpr`]),
/// because an output's default name derives from it
/// ([`crate::projection::agg_output_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Aggregate {
    Count,
    Avg,
    Sum,
    Min,
    Max,
    DistinctCount,
    P50,
    P90,
    P95,
    P99,
    First,
    Last,
    Values,
    Median,
    Stddev,
}

impl Aggregate {
    /// Every aggregate, in declaration order.
    pub const ALL: [Self; 15] = [
        Self::Count,
        Self::Avg,
        Self::Sum,
        Self::Min,
        Self::Max,
        Self::DistinctCount,
        Self::P50,
        Self::P90,
        Self::P95,
        Self::P99,
        Self::First,
        Self::Last,
        Self::Values,
        Self::Median,
        Self::Stddev,
    ];

    /// The aggregate a function name spells, matched exactly: `Count`
    /// is not `count`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "count" => Self::Count,
            "avg" => Self::Avg,
            "sum" => Self::Sum,
            "min" => Self::Min,
            "max" => Self::Max,
            "dc" | "distinct_count" => Self::DistinctCount,
            "p50" => Self::P50,
            "p90" => Self::P90,
            "p95" => Self::P95,
            "p99" => Self::P99,
            "first" => Self::First,
            "last" => Self::Last,
            "values" | "list" => Self::Values,
            "median" => Self::Median,
            "stddev" => Self::Stddev,
            _ => return None,
        })
    }

    /// Every name [`Self::from_name`] reads as this aggregate.
    #[must_use]
    pub const fn spellings(self) -> &'static [&'static str] {
        match self {
            Self::Count => &["count"],
            Self::Avg => &["avg"],
            Self::Sum => &["sum"],
            Self::Min => &["min"],
            Self::Max => &["max"],
            Self::DistinctCount => &["dc", "distinct_count"],
            Self::P50 => &["p50"],
            Self::P90 => &["p90"],
            Self::P95 => &["p95"],
            Self::P99 => &["p99"],
            Self::First => &["first"],
            Self::Last => &["last"],
            Self::Values => &["values", "list"],
            Self::Median => &["median"],
            Self::Stddev => &["stddev"],
        }
    }
}
