// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Visualization tab's Line refusal ladder, from the executed query
//! and the response it produced (ADR-0038).
//!
//! [`chart_hint`] is the whole decision `components/chart.rs` makes
//! before it touches uPlot: either the aligned [`SeriesSet`] to draw, or
//! the one sentence shown in its place. At the crate root rather than
//! under `components/`, which is wasm-gated, so the ladder is tested on
//! native `cargo test -p trawl-web-ui chart_hint`.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::PaginationMeta;
use trawl_api::value::QueryResult;
use trawl_core::ast::PipeStage;

use crate::fetch_plan::coverage_refusal;
use crate::series::{Lane, Refusal, SeriesSet, align_series};

/// Whether the LAST aggregation-bearing stage of `query` is a
/// `timechart`, which is what makes a result drawable as lines.
///
/// Not "the pipeline mentions a timechart anywhere": a later
/// aggregation re-aggregates the buckets away.
/// `… | timechart span=1m count() by host | stats sum(count) as total by
/// host` is a `stats … by` result with no `_time` column at all, which
/// Column draws and [`align_series`] refuses. `top`, `rare` and `pivot`
/// end a timechart the same way — and each of those has its own rung, so
/// the answer here only has to be "no".
///
/// A query that does not parse cannot have produced a result whose axis
/// anyone can read off it, so it is `false` as well.
#[must_use]
pub fn final_stage_is_timechart(query: &str) -> bool {
    let Ok(ast) = trawl_core::parser::parse(query) else {
        return false;
    };
    ast.pipeline
        .iter()
        .rev()
        .find_map(|stage| match &stage.node {
            PipeStage::Timechart(_) => Some(true),
            PipeStage::Stats(_) | PipeStage::Top(_) | PipeStage::Rare(_) | PipeStage::Pivot(_) => {
                Some(false)
            }
            _ => None,
        })
        .unwrap_or(false)
}

/// A chart that was not drawn: the sentence shown in its place, and
/// whether a control to the Events tab belongs beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub message: String,
    pub offers_events: bool,
}

impl From<&Refusal> for Hint {
    fn from(refusal: &Refusal) -> Self {
        Self {
            message: refusal.message(),
            offers_events: refusal.offers_events(),
        }
    }
}

/// Align `result` for the Line chart, or say why it is not drawn.
///
/// `query` is the DSL that produced `result` — the executed query, never
/// the text being typed. Every role the ladder reads comes from it, so
/// the answer is a function of the pair it is handed and of nothing
/// else.
///
/// The rungs are ordered, and the coverage one comes LAST on purpose
/// (ADR-0038, carried over from the chart this replaced). A result whose
/// shape cannot be drawn has something actionable to say about that
/// shape; "this is only part of the result" is the answer only once the
/// shape itself is chartable.
///
/// # Errors
///
/// A [`Hint`] carrying the sentence for the rung that stopped the draw.
pub fn chart_hint(
    query: &str,
    result: &QueryResult,
    lane: Lane,
    coverage: Option<&PaginationMeta>,
) -> Result<SeriesSet, Hint> {
    let set = align_series(query, result, lane).map_err(|refusal| Hint::from(&refusal))?;
    match coverage.and_then(coverage_refusal) {
        // A cut result is refused with the count the server measured,
        // and the Events tab is where the fetched rows are.
        Some(message) => Err(Hint {
            message,
            offers_events: true,
        }),
        None => Ok(set),
    }
}

#[cfg(test)]
mod chart_hint_tests {
    use super::*;
    use trawl_api::value::{Column, Value};

    const GROUPED: &str = "* | timechart span=1m count() by host";
    const UNGROUPED: &str = "* | timechart span=1m count()";

    fn result(names: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: names
                .iter()
                .map(|n| Column {
                    name: (*n).to_string(),
                })
                .collect(),
            rows,
        }
    }

    fn s(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    /// A grouped `timechart` result: two hosts over two minutes.
    fn grouped_result() -> QueryResult {
        result(
            &["_time", "host", "count"],
            vec![
                vec![s("2026-09-01 00:00:00"), s("a"), Value::Integer(5)],
                vec![s("2026-09-01 00:01:00"), s("a"), Value::Integer(7)],
                vec![s("2026-09-01 00:00:00"), s("b"), Value::Integer(3)],
            ],
        )
    }

    fn meta(offset: usize, returned: usize, total: usize) -> PaginationMeta {
        PaginationMeta {
            limit: 20_000,
            offset,
            returned,
            total,
        }
    }

    fn hint(query: &str, result: &QueryResult, coverage: Option<&PaginationMeta>) -> Hint {
        chart_hint(query, result, Lane::Snapshot, coverage)
            .expect_err("this result is not drawn as lines")
    }

    #[test]
    fn judges_the_query_that_produced_the_result() {
        let rows = grouped_result();
        // Judged by its own query, the grouped result draws: one series
        // per host, aligned on the minute grid.
        let set =
            chart_hint(GROUPED, &rows, Lane::Snapshot, None).expect("the grouped result draws");
        assert_eq!(set.series.len(), 2);
        assert_eq!(set.group_fields, vec!["host".to_owned()]);

        // The same rows judged by a query that did not group: `host` is
        // no longer a group, so it is read as a second metric — and it
        // is text, which is not a metric at all. The verdict follows the
        // query it is handed, not the shape of the cells.
        let other = hint(UNGROUPED, &rows, None);
        assert_eq!(
            other.message,
            "Visualization draws numeric metrics. Open Events for these values."
        );

        // Pure in the pair: repeating either call repeats its answer,
        // and neither call disturbs the other.
        assert_eq!(
            chart_hint(GROUPED, &rows, Lane::Snapshot, None).expect("still draws"),
            set
        );
        assert_eq!(hint(UNGROUPED, &rows, None), other);
    }

    #[test]
    fn line_fits_only_when_the_final_stage_is_a_timechart() {
        // A timechart re-aggregated by a later `stats … by`: the result
        // is one row per host with no `_time` column, so Line does not
        // fit it and Column does.
        let rolled_up = "* | timechart span=1m count() by host | stats sum(count) as total by host";
        assert!(!final_stage_is_timechart(rolled_up));
        let totals = result(
            &["host", "total"],
            vec![
                vec![s("a"), Value::Integer(12)],
                vec![s("b"), Value::Integer(3)],
            ],
        );
        assert!(
            crate::categorical::detect(rolled_up, &totals).is_some(),
            "the categorical detector admits the final shape"
        );
        // And the Line ladder refuses it, which is why the picker must
        // not offer Line for it.
        assert_eq!(
            hint(rolled_up, &totals, None).message,
            "This result has no time axis. Choose Column for stats by, or open Events."
        );

        // A plain timechart: Line fits, and the categorical detector
        // does not admit a bucketed result.
        assert!(final_stage_is_timechart(GROUPED));
        assert!(final_stage_is_timechart(UNGROUPED));
        assert!(crate::categorical::detect(GROUPED, &grouped_result()).is_none());

        // The other aggregations end a timechart the same way, and a
        // query that does not parse has no axis either.
        for query in [
            "* | timechart span=1m count() | top 5 host",
            "* | timechart span=1m count() | rare 5 host",
            "* | stats count() by status",
            "| | |",
        ] {
            assert!(!final_stage_is_timechart(query), "{query}");
        }

        // A stage that carries no aggregation does not end the
        // timechart: a sort or a head still leaves the buckets.
        for query in [
            "* | timechart span=1m count() | sort _time",
            "* | timechart span=1m count() | head 10",
        ] {
            assert!(final_stage_is_timechart(query), "{query}");
        }
    }

    #[test]
    fn coverage_comes_last() {
        let rows = grouped_result();
        // Shape is fine, the window is not: the coverage sentence, with
        // the counts the server measured.
        let cut = hint(GROUPED, &rows, Some(&meta(0, 20_000, 43_210)));
        assert_eq!(
            cut.message,
            "This query produced 43,210 rows; 20,000 were fetched. \
             The chart draws a whole result only. Open Events for the fetched rows."
        );
        assert!(cut.offers_events);

        // The same cut window under a shape the chart cannot draw: the
        // shape sentence wins, because it names a fix in the query.
        let shaped = hint(
            "* | top 5 host",
            &result(
                &["_time", "count"],
                vec![vec![s("2026-09-01 00:00:00"), Value::Integer(1)]],
            ),
            Some(&meta(0, 20_000, 43_210)),
        );
        assert_eq!(
            shaped.message,
            "Top results are not drawn as lines. Open Events for the table."
        );

        // A whole result is drawn, not refused: the rung only fires on a
        // window that left rows behind.
        assert!(chart_hint(GROUPED, &rows, Lane::Snapshot, Some(&meta(0, 3, 3))).is_ok());
    }

    #[test]
    fn the_refusal_copy_reaches_the_component() {
        let one_row = result(
            &["_time", "count"],
            vec![vec![s("2026-09-01 00:00:00"), Value::Integer(1)]],
        );
        for (query, rows, message) in [
            (
                "* | pivot count() on status by host",
                &one_row,
                "Pivot results are not drawn as lines. Open Events for the table.",
            ),
            (
                "* | top 5 host",
                &one_row,
                "Top results are not drawn as lines. Open Events for the table.",
            ),
            (
                "* | rare 5 host",
                &one_row,
                "Rare results are not drawn as lines. Open Events for the table.",
            ),
        ] {
            let refused = hint(query, rows, None);
            assert_eq!(refused.message, message, "{query}");
            assert!(refused.offers_events, "{query}");
        }

        // A grouped timechart with two metrics.
        let two = result(
            &["_time", "host", "count", "avg_bytes"],
            vec![vec![
                s("2026-09-01 00:00:00"),
                s("a"),
                Value::Integer(1),
                Value::Float(2.0),
            ]],
        );
        let refused = hint(
            "* | timechart span=1m count(), avg(bytes) by host",
            &two,
            None,
        );
        assert_eq!(
            refused.message,
            "Grouped charts draw one metric. Chart one metric, or open Events."
        );

        // A `_time` cell the chart cannot place exactly.
        let unreadable = result(
            &["_time", "count"],
            vec![vec![s("2026-09-01T00:00:00+02:00"), Value::Integer(1)]],
        );
        let refused = hint(UNGROUPED, &unreadable, None);
        assert_eq!(
            refused.message,
            "A _time value could not be read as a timestamp: 2026-09-01T00:00:00+02:00. \
             Open Events for the exact rows."
        );
        assert!(refused.offers_events);
    }
}
