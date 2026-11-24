// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use crate::search_url::PAGE_SIZE;
use crate::service_card_fmt::format_exact;

pub const AGGREGATE_FETCH_ROWS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPlan {
    Page { page: usize },
    Whole,
}

impl FetchPlan {
    #[must_use]
    pub fn for_query(query: &str, page: usize) -> Self {
        if crate::facets::is_aggregation_shape(query) {
            Self::Whole
        } else {
            Self::Page { page }
        }
    }

    #[must_use]
    pub fn window(self) -> (usize, usize) {
        match self {
            Self::Page { page } => (PAGE_SIZE, page.saturating_mul(PAGE_SIZE)),
            Self::Whole => (AGGREGATE_FETCH_ROWS, 0),
        }
    }
}

fn preamble(total: usize, fetched: usize) -> String {
    format!(
        "This query produced {} rows; {} were fetched.",
        format_exact(total as u64),
        format_exact(fetched as u64)
    )
}

#[must_use]
pub fn coverage_refusal(meta: &trawl_api::PaginationMeta) -> Option<String> {
    (meta.offset != 0 || meta.returned != meta.total).then(|| {
        format!(
            "{} The chart draws a whole result only. Open Events for the fetched rows.",
            preamble(meta.total, meta.returned)
        )
    })
}

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
            }
            assert_eq!(
                FetchPlan::for_query(query, 0),
                FetchPlan::for_query(query, 2)
            );
        }
    }

    #[test]
    fn no_whole_fetched_shape_offers_row_facets() {
        for query in [
            "service=web | timechart span=2m count()",
            "* | stats count() by status",
            "* | stats count()",
            "* | top 10 host",
            "* | rare 5 status",
            "* | pivot count() on status by host",
        ] {
            assert_eq!(FetchPlan::for_query(query, 0), FetchPlan::Whole, "{query}");
            assert!(
                !crate::result_actions::Capabilities::for_query(query).raw_facets(),
                "{query}"
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
