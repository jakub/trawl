// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Footer status and count, derived from the active result source.
//!
//! The search page has one active result source per mode — the snapshot
//! page in snapshot, the live ring or the latest aggregation frame in
//! live — and the footer describes that source and nothing else
//! (ADR-0027, amended 2026-09-12). Both derivations live here as pure
//! functions so the precedence table is testable without a browser.
//!
//! Only the wasm32 build consumes these helpers — on native they exist
//! purely so their tests run under plain `cargo test`. Matches the
//! `facets.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// Parse a server start instant before presenting it as a UTC fact.
/// Invalid timestamps have no truthful display value.
#[must_use]
pub fn execution_started(raw: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|started| {
            started
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string()
        })
}

/// What the footer's status label reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// Connected to trawld; idle, ready to run.
    Connected,
    /// Snapshot query in flight.
    Hauling,
    /// Live SSE stream open.
    Live,
    /// The active result source is showing an alert; the label reports Error.
    Error,
}

/// Everything [`search_status`] reads, in one struct so the precedence
/// table below is one expression rather than a nest of conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Five independent predicates read off the page, not a state machine
// hiding in a struct: the precedence between them IS `search_status`.
#[allow(clippy::struct_excessive_bools)]
pub struct StatusInputs {
    /// The link's structured state could not be read, so nothing ran.
    pub unreadable: bool,
    /// `mode=live` — the stream, not the snapshot resource, is active.
    pub live: bool,
    /// The live stream is disconnected, unavailable, or delivered an
    /// unreadable frame.
    pub stream_failed: bool,
    /// The last snapshot request errored.
    pub snapshot_failed: bool,
    /// A snapshot request is in flight.
    pub snapshot_pending: bool,
}

/// Derive the footer status from the active result source.
///
/// Precedence, highest first:
/// 1. An unreadable link ran nothing, so there is no source to describe:
///    `Connected` even when a stale failure is still on the page.
/// 2. The alert wins: the active source is showing one. Note that a
///    resource keeps its previous `Some(Err)` while a refetch is
///    pending, so failed-and-pending reads `Error`, not `Hauling`.
/// 3. A snapshot request in flight is `Hauling`.
/// 4. Live streaming is `Live`; anything else is `Connected`.
#[must_use]
pub fn search_status(i: StatusInputs) -> StatusKind {
    if i.unreadable {
        return StatusKind::Connected;
    }
    if (i.live && i.stream_failed) || (!i.live && i.snapshot_failed) {
        return StatusKind::Error;
    }
    if !i.live && i.snapshot_pending {
        return StatusKind::Hauling;
    }
    if i.live {
        StatusKind::Live
    } else {
        StatusKind::Connected
    }
}

/// Which source the footer's number was counted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountSource {
    /// Rows the last snapshot returned.
    Last,
    /// Raw events delivered since the stream opened, including those
    /// that rolled off the ring.
    Received,
    /// Aggregation frames delivered since the stream opened.
    Updates,
}

/// The footer's count and the source it names. `value` is `None` before
/// anything has run — the shell's own default, and what a non-search
/// page carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterCount {
    pub source: CountSource,
    pub value: Option<u64>,
}

impl FooterCount {
    /// A snapshot count: `Last N`, or `Last —` when nothing has run.
    #[must_use]
    pub const fn last(value: Option<u64>) -> Self {
        Self {
            source: CountSource::Last,
            value,
        }
    }
}

/// Render a footer count as its `(label, value)` pair. A count with no
/// value reads as an em dash so the label still names the source.
#[must_use]
pub fn footer_count_label(count: &FooterCount) -> (&'static str, String) {
    let label = match count.source {
        CountSource::Last => "Last",
        CountSource::Received => "Received",
        CountSource::Updates => "Updates",
    };
    (
        label,
        count
            .value
            .map_or_else(|| "—".to_string(), |value| value.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CountSource, FooterCount, StatusInputs, StatusKind, footer_count_label, search_status,
    };

    #[test]
    fn execution_start_is_full_utc_seconds() {
        assert_eq!(
            super::execution_started("2026-09-15T12:34:56.789Z").as_deref(),
            Some("2026-09-15 12:34:56 UTC")
        );
        assert_eq!(
            super::execution_started("2026-09-15T05:34:56-07:00").as_deref(),
            Some("2026-09-15 12:34:56 UTC")
        );
        assert_eq!(super::execution_started("not a timestamp"), None);
        assert_eq!(super::execution_started("2026-09-15 12:34:56"), None);
    }

    /// Everything false — the idle page.
    const IDLE: StatusInputs = StatusInputs {
        unreadable: false,
        live: false,
        stream_failed: false,
        snapshot_failed: false,
        snapshot_pending: false,
    };

    #[test]
    fn an_idle_readable_snapshot_page_is_connected() {
        assert_eq!(search_status(IDLE), StatusKind::Connected);
    }

    #[test]
    fn an_unreadable_link_is_connected_even_with_a_failure_on_the_page() {
        // The banner is the results pane; nothing ran, so there is no
        // source to call failed.
        let inputs = StatusInputs {
            unreadable: true,
            live: true,
            stream_failed: true,
            snapshot_failed: true,
            snapshot_pending: true,
        };
        assert_eq!(search_status(inputs), StatusKind::Connected);
    }

    #[test]
    fn a_failed_stream_in_live_is_an_error() {
        let inputs = StatusInputs {
            live: true,
            stream_failed: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Error);
    }

    #[test]
    fn a_failed_snapshot_in_snapshot_mode_is_an_error() {
        let inputs = StatusInputs {
            snapshot_failed: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Error);
    }

    #[test]
    fn a_failed_snapshot_still_pending_a_refetch_is_an_error_not_hauling() {
        // The resource keeps its previous `Some(Err)` while the refetch
        // runs, so the alert is on screen for that whole window.
        let inputs = StatusInputs {
            snapshot_failed: true,
            snapshot_pending: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Error);
    }

    #[test]
    fn a_stale_snapshot_failure_does_not_reach_the_live_footer() {
        let inputs = StatusInputs {
            live: true,
            snapshot_failed: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Live);
    }

    #[test]
    fn a_stale_stream_failure_does_not_reach_the_snapshot_footer() {
        let inputs = StatusInputs {
            stream_failed: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Connected);
    }

    #[test]
    fn a_pending_snapshot_request_is_hauling() {
        let inputs = StatusInputs {
            snapshot_pending: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Hauling);
    }

    #[test]
    fn a_healthy_stream_is_live() {
        let inputs = StatusInputs { live: true, ..IDLE };
        assert_eq!(search_status(inputs), StatusKind::Live);
    }

    #[test]
    fn a_pending_snapshot_underneath_live_does_not_read_hauling() {
        // Nothing should issue one while live, but a response already in
        // flight when the mode changed must not retitle the footer.
        let inputs = StatusInputs {
            live: true,
            snapshot_pending: true,
            ..IDLE
        };
        assert_eq!(search_status(inputs), StatusKind::Live);
    }

    #[test]
    fn every_count_source_renders_its_own_label() {
        let cases = [
            (FooterCount::last(Some(42)), ("Last", "42")),
            (FooterCount::last(None), ("Last", "—")),
            (
                FooterCount {
                    source: CountSource::Received,
                    value: Some(6000),
                },
                ("Received", "6000"),
            ),
            (
                FooterCount {
                    source: CountSource::Updates,
                    value: Some(2),
                },
                ("Updates", "2"),
            ),
        ];
        for (count, (label, value)) in cases {
            let rendered = footer_count_label(&count);
            assert_eq!(rendered, (label, value.to_string()), "for {count:?}");
        }
    }
}
