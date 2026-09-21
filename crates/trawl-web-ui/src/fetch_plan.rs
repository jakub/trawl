// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How much of a query's result one request asks for, and what the
//! surfaces say when the answer on screen is not the whole of it.
//!
//! A raw-event query is read a page at a time: an operator scrolls, and
//! the rows past the page are a click away. An aggregation is not — a
//! chart drawn from 50 of 43,210 group rows is a picture of the page,
//! not of the result, and nothing on screen says so. So the two shapes
//! ask for different amounts, and this module owns that decision plus
//! the window arithmetic it implies. It is the ONLY place in the crate
//! that turns a page number into a `(limit, offset)` pair for a query.
//!
//! The refusal and the cap line live here too, beside the plan that
//! produces the numbers they quote: both are read off
//! `PaginationMeta`'s `offset`, `returned` and `total`, never off the
//! `limit` a request happened to ask for. A server free to clamp a
//! limit makes the asked-for number a claim about the request; the
//! three measured ones describe the answer.
//!
//! At the crate root rather than under `api/` or `components/`, which
//! are wasm-gated, so these decisions are tested on native
//! `cargo nextest run -p trawl-web-ui` — the `categorical.rs` /
//! `facets.rs` pattern.

// Unconditional, unlike the `not(target_arch = "wasm32")` form those
// modules use: the request path adopts this one in the following slice,
// so right now the tests are its only caller on either target. Narrow
// it back to the native-only form once a wasm caller exists.
#![allow(dead_code)]

use crate::search_url::PAGE_SIZE;
use crate::service_card_fmt::format_exact;

/// The ceiling one aggregation fetch asks for. A `limit` the server may clamp.
pub const AGGREGATE_FETCH_ROWS: usize = 20_000;

/// How much of a query's result one request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPlan {
    /// A raw-event query: one `PAGE_SIZE` page at a time.
    Page { page: usize },
    /// An aggregation: the whole result, once per effective query.
    Whole,
}

impl FetchPlan {
    /// The plan for `query`, where `page` is the page the URL asks for.
    ///
    /// An aggregation ignores the page: every page of one effective
    /// query asks for the same whole result, so the fetch does not
    /// repeat when a reader walks the table under the chart.
    #[must_use]
    pub fn for_query(query: &str, page: usize) -> Self {
        if crate::facets::is_aggregation_shape(query) {
            Self::Whole
        } else {
            Self::Page { page }
        }
    }

    /// The `(limit, offset)` this plan requests.
    #[must_use]
    pub fn window(self) -> (usize, usize) {
        match self {
            Self::Page { page } => (PAGE_SIZE, page.saturating_mul(PAGE_SIZE)),
            Self::Whole => (AGGREGATE_FETCH_ROWS, 0),
        }
    }

    /// Whether this plan asks for the whole result.
    #[must_use]
    pub const fn is_whole(self) -> bool {
        matches!(self, Self::Whole)
    }
}

/// The shared opening both lines are built from: what the execution
/// produced, and how much of it arrived.
fn preamble(total: usize, fetched: usize) -> String {
    format!(
        "This query produced {} rows; {} were fetched.",
        format_exact(total as u64),
        format_exact(fetched as u64)
    )
}

/// Why this result must not be charted, or `None` when it is whole.
///
/// Whole means the response carries every row the execution produced:
/// it starts at the beginning and nothing was left behind. A window cut
/// from a larger result is a page, and a chart of a page reads as a
/// chart of the result.
#[must_use]
pub fn coverage_refusal(meta: &trawl_api::PaginationMeta) -> Option<String> {
    (meta.offset != 0 || meta.returned != meta.total).then(|| {
        format!(
            "{} The chart draws a whole result only. Open Events for the fetched rows.",
            preamble(meta.total, meta.returned)
        )
    })
}

/// The line under the exact table when the result exceeds what was
/// fetched, or `None` when the table holds everything.
#[must_use]
pub fn cap_line(total: usize, fetched: usize) -> Option<String> {
    (total > fetched).then(|| {
        format!(
            "{} Paging covers the fetched rows.",
            preamble(total, fetched)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(offset: usize, returned: usize, total: usize) -> trawl_api::PaginationMeta {
        trawl_api::PaginationMeta {
            // The limit is deliberately a number no reader here uses: a
            // refusal keyed off it would fire on a clamped request that
            // still answered whole.
            limit: 12_345,
            offset,
            returned,
            total,
        }
    }

    #[test]
    fn a_raw_event_query_asks_for_one_page() {
        for page in [0, 1, 7] {
            let plan = FetchPlan::for_query("service=web status=500", page);
            assert_eq!(plan, FetchPlan::Page { page });
            assert_eq!(plan.window(), (50, page * 50));
            assert!(!plan.is_whole());
        }
    }

    #[test]
    fn an_aggregation_asks_for_the_whole_result_on_every_page() {
        for query in [
            "service=web | timechart span=2m count()",
            "* | stats count() by status",
        ] {
            for page in [0, 7] {
                let plan = FetchPlan::for_query(query, page);
                assert_eq!(plan, FetchPlan::Whole, "{query} at page {page}");
                assert_eq!(plan.window(), (20_000, 0));
                assert!(plan.is_whole());
            }
            // The page cannot move the request, which is what lets one
            // fetch serve every page of one effective query.
            assert_eq!(
                FetchPlan::for_query(query, 0),
                FetchPlan::for_query(query, 2)
            );
        }
    }

    #[test]
    fn nothing_to_read_pages_like_an_event_query() {
        for query in ["", "   ", "| | |"] {
            assert_eq!(FetchPlan::for_query(query, 3), FetchPlan::Page { page: 3 });
        }
    }

    #[test]
    fn a_whole_result_is_not_refused() {
        assert_eq!(coverage_refusal(&meta(0, 0, 0)), None);
        assert_eq!(coverage_refusal(&meta(0, 5, 5)), None);
        // The fetch ceiling reached exactly: every row the execution
        // produced is on screen, so there is nothing to warn about.
        assert_eq!(coverage_refusal(&meta(0, 20_000, 20_000)), None);
    }

    #[test]
    fn a_cut_result_is_refused_with_the_measured_counts() {
        let refusal = coverage_refusal(&meta(0, 20_000, 43_210)).expect("a cut result is refused");
        assert!(refusal.contains("43,210"), "{refusal}");
        assert!(refusal.contains("20,000"), "{refusal}");
        assert!(refusal.contains("whole result only"), "{refusal}");
    }

    #[test]
    fn a_later_window_is_refused_even_when_it_reaches_the_end() {
        // `returned == total - offset`: the response carries the tail of
        // the result, which is still not the result.
        let refusal = coverage_refusal(&meta(50, 10, 60)).expect("a nonzero offset is refused");
        assert!(refusal.contains("60 rows"), "{refusal}");
        assert!(refusal.contains("10 were fetched"), "{refusal}");
    }

    #[test]
    fn the_cap_line_appears_only_when_rows_were_left_behind() {
        assert_eq!(cap_line(0, 0), None);
        assert_eq!(cap_line(50, 20_000), None);
        assert_eq!(cap_line(20_000, 20_000), None);
        assert_eq!(
            cap_line(43_210, 20_000).as_deref(),
            Some(
                "This query produced 43,210 rows; 20,000 were fetched. \
                 Paging covers the fetched rows."
            )
        );
    }
}
