// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What one read of a stored run's result came back as, and the single
//! reading of what a refusal settles.
//!
//! Pure (no `leptos`/`web_sys`), so it lives ungated and its contract is
//! exercised by native `cargo test` — the same split as `tone_vocab` and
//! fleet-ui's `badge::tone`. The component that produces it
//! (`components::net_drawer::RunResultPreview`) and the two surfaces
//! that consume it are wasm-only; this rule is the part both of them
//! have to agree on, so it is the part worth testing on its own.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// What the last read of a stored run said, for a caller that shows
/// something of its own beside the preview.
///
/// `None` in the signal that carries it means NO read has landed yet,
/// which is a third state and not a quiet [`Self::Unavailable`]: a
/// caller that cannot tell the two apart either prints its own guess as
/// though the server had confirmed it, or prints nothing once the
/// server has spoken. [`Self::Unavailable`] carries no summary because
/// the 409 has none to give, and what belongs in its place is the
/// caller's question rather than this component's.
#[derive(Clone, PartialEq)]
pub enum RunRead {
    /// The summary that read returned. Boxed so the refusal, which
    /// carries nothing, does not pay for it — as `RunPreview` boxes its
    /// own response for the same reason.
    Available(Box<trawl_api::ReportRunSummary>),
    /// The run succeeded and its stored result is gone (issue #227).
    Unavailable,
}

impl RunRead {
    /// This read's answer about the run, with `fallback` standing in
    /// where the read has nothing of its own.
    ///
    /// The one place the refusal's meaning is written down, because two
    /// surfaces show a run beside this preview and both have to say the
    /// same thing about it. `from_saved::unavailable_run_conflict`
    /// answers that 409 only for a run whose query SUCCEEDED and wrote
    /// a result file the server can no longer find, so the outcome is
    /// settled even though the summary is gone: a fallback snapshotted
    /// while the run was still going is stale about the status and
    /// about nothing else, and its empty duration and row count are
    /// honest.
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
        }
    }

    /// A read that came back with a summary is the answer, and the
    /// caller's fallback is not consulted at all.
    #[test]
    fn an_available_read_answers_for_itself() {
        let read = RunRead::Available(Box::new(summary(7, "running")));
        assert_eq!(
            read.settle(Some(summary(7, "success"))),
            Some(summary(7, "running")),
            "the read's own summary, fallback or no fallback"
        );
    }

    /// The 409 settles the outcome and nothing else: a fallback taken
    /// while the run was still going is stale about the status, and its
    /// empty duration and row count are the truth about a run that had
    /// recorded neither.
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

    /// With nothing to correct there is nothing to show: the refusal
    /// carries no summary of its own to put there.
    #[test]
    fn a_refusal_without_a_fallback_answers_nothing() {
        assert_eq!(RunRead::Unavailable.settle(None), None);
    }
}
