// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The single source of the `timechart` bucket width.
//!
//! An explicit `span=` always wins; otherwise the width is picked from a
//! fixed table keyed by how far back the query's time filter looks. Three
//! callers ask this same question and must all get the same answer
//! (ADR-0038): the SQL emitter (`emitter::pipeline::process_timechart`),
//! the live stream compiler (`stream::compile_aggregation`), and the web
//! chart, which asks it indirectly through [`query_span`] to size its
//! x-axis before any row has arrived.

use crate::ast::{PipeStage, Query, TimeUnit, TrawlDuration};

/// Resolve the bucket width for a `timechart` stage.
///
/// An explicit `span` is returned unchanged. Otherwise the width is
/// picked from the automatic band table, keyed by `last`'s duration (or
/// no filter at all, which is treated the same as "an hour or less").
#[must_use]
pub fn resolve_span(span: Option<TrawlDuration>, last: Option<TrawlDuration>) -> TrawlDuration {
    if let Some(span) = span {
        return span;
    }

    let seconds = last.map_or(3600, |d| d.to_seconds());

    let (quantity, unit) = if seconds <= 3600 {
        (1, TimeUnit::Minutes)
    } else if seconds <= 21_600 {
        (5, TimeUnit::Minutes)
    } else if seconds <= 86_400 {
        (15, TimeUnit::Minutes)
    } else if seconds <= 604_800 {
        (1, TimeUnit::Hours)
    } else if seconds <= 2_592_000 {
        (6, TimeUnit::Hours)
    } else {
        (1, TimeUnit::Days)
    };

    TrawlDuration { quantity, unit }
}

/// The bucket width a parsed query's `timechart` stage would use, if it
/// has one.
///
/// `None` when the pipeline carries no `timechart` stage at all. When it
/// carries more than one, the last stage is the one whose output the
/// query actually returns, so it is the one asked.
#[must_use]
pub fn query_span(query: &Query) -> Option<TrawlDuration> {
    let tc = query.pipeline.iter().rev().find_map(|s| match &s.node {
        PipeStage::Timechart(tc) => Some(tc),
        _ => None,
    })?;

    let last = query.search.time_filter.as_ref().map(|tf| tf.node.duration);
    Some(resolve_span(tc.span, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timechart_span_explicit_wins() {
        let explicit = TrawlDuration {
            quantity: 2,
            unit: TimeUnit::Minutes,
        };
        let last = TrawlDuration {
            quantity: 7,
            unit: TimeUnit::Days,
        };
        assert_eq!(resolve_span(Some(explicit), Some(last)), explicit);
        assert_eq!(resolve_span(Some(explicit), None), explicit);
    }

    #[test]
    fn timechart_span_bands() {
        let secs = |quantity: u64| TrawlDuration {
            quantity,
            unit: TimeUnit::Seconds,
        };

        let cases: &[(u64, u64, TimeUnit)] = &[
            (3600, 1, TimeUnit::Minutes),
            (3601, 5, TimeUnit::Minutes),
            (21_600, 5, TimeUnit::Minutes),
            (21_601, 15, TimeUnit::Minutes),
            (86_400, 15, TimeUnit::Minutes),
            (86_401, 1, TimeUnit::Hours),
            (604_800, 1, TimeUnit::Hours),
            (604_801, 6, TimeUnit::Hours),
            (2_592_000, 6, TimeUnit::Hours),
            (2_592_001, 1, TimeUnit::Days),
        ];

        for &(input_secs, quantity, unit) in cases {
            let resolved = resolve_span(None, Some(secs(input_secs)));
            assert_eq!(
                resolved,
                TrawlDuration { quantity, unit },
                "at {input_secs}s"
            );
        }
    }

    #[test]
    fn timechart_span_no_filter_is_60s() {
        assert_eq!(resolve_span(None, None).to_seconds(), 60);
    }
}
