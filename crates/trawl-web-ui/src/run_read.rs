// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// `None` in the signal that carries it means NO read has landed yet,
/// which is a third state and not a quiet [`Self::Unavailable`].
#[derive(Clone, PartialEq)]
pub enum RunRead {
    Available(Box<trawl_api::ReportRunSummary>),
    /// The run succeeded and its stored result is gone.
    Unavailable,
}

impl RunRead {
    /// `from_saved::unavailable_run_conflict` answers that 409 only for a run
    /// whose query SUCCEEDED and wrote a result file the server can no longer
    /// find, so the outcome is settled even though the summary is gone.
    pub fn settle(
        self,
        fallback: Option<trawl_api::ReportRunSummary>,
    ) -> Option<trawl_api::ReportRunSummary> {
        match self {
            Self::Available(summary) => Some(*summary),
            Self::Unavailable => fallback.map(|summary| trawl_api::ReportRunSummary {
                status: "success".to_owned(),
                ..summary
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunRead;

    fn summary(id: i64, status: &str) -> trawl_api::ReportRunSummary {
        trawl_api::ReportRunSummary {
            id,
            query: "service=nginx | stats count()".to_owned(),
            status: status.to_owned(),
            started_at: "2026-09-01T10:00:00Z".to_owned(),
            finished_at: None,
            duration_ms: None,
            row_count: None,
            error_message: None,
            result_path: None,
            window_start: None,
            window_end: None,
            window_truncated: None,
            window_kind: None,
            origin: None,
        }
    }

    #[test]
    fn an_available_read_answers_for_itself() {
        let read = RunRead::Available(Box::new(summary(7, "running")));
        assert_eq!(
            read.settle(Some(summary(7, "success"))),
            Some(summary(7, "running")),
            "the read's own summary, fallback or no fallback"
        );
    }

    #[test]
    fn a_refusal_corrects_only_the_outcome() {
        let mut stale = summary(7, "running");
        stale.query = "service=nginx | stats count() by host".to_owned();
        let settled = RunRead::Unavailable
            .settle(Some(stale.clone()))
            .expect("a fallback to correct");
        assert_eq!(settled.status, "success", "the 409 is only ever a success");
        assert_eq!(settled.id, stale.id, "the run it is about");
        assert_eq!(settled.query, stale.query, "the recorded query stands");
        assert_eq!(settled.duration_ms, None, "no duration is invented");
        assert_eq!(settled.row_count, None, "no row count is invented");
    }

    #[test]
    fn a_refusal_without_a_fallback_answers_nothing() {
        assert_eq!(RunRead::Unavailable.settle(None), None);
    }
}
