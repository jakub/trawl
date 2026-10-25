// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What this server offers a client before it sends anything (ADR-0058):
//! the capability behind `GET /api/v1/capabilities`, transport-free.
//!
//! The route sits on the authenticated router and checks no permission
//! beyond the trawl grant, as `whoami` does: every key that may call
//! trawld may learn the bounds it will be held to. The report reads
//! in-process state only. The version and the DSL lists are compiled in,
//! the bounds, quotas, admission numbers and origin are the configuration
//! the state was built from, and the health block is
//! `handlers::health_report`, the computation
//! `GET /api/v1/health` answers with. Retention reach is not here: it stays
//! on the `server_manage` dashboard.

use fleet_auth::VerifiedKey;
use trawl_api::capabilities::{
    CapabilitiesResponse, CapabilityBounds, CaptureAdmission, CaptureQuotas, DslCapabilities,
};
use trawl_core::complexity::MAX_PIPELINE_STAGES;
use trawl_core::parser::MAX_QUERY_LEN;
use trawl_core::parser::suggest::{KNOWN_FUNCTIONS, KNOWN_PIPE_STAGES};

use crate::state::AppState;

/// The capabilities report for `key`.
///
/// Only the rate in `admission` depends on the key: the one the rate
/// limiter applies to its interactive requests.
pub async fn report(state: &AppState, key: &VerifiedKey) -> CapabilitiesResponse {
    let (_, health) = crate::handlers::health_report(state).await;
    let capture = &state.results.capture;
    CapabilitiesResponse {
        version: trawl_core::version::PKG_VERSION.to_owned(),
        dsl: dsl(),
        bounds: CapabilityBounds {
            default_window_secs: capture.default_window_secs,
            max_span_secs: capture.max_span_secs,
            deadline_secs: crate::results::service::deadline_secs(state),
            max_rows: capture.max_rows,
            max_wire_bytes: capture.max_wire_bytes,
            page_max_rows: capture.page_max_rows,
            ttl_secs: capture.ttl_secs,
            query_timeout_secs: state.query.timeout_secs,
            max_result_rows: widen(state.query.pool.max_result_rows()),
        },
        quotas: CaptureQuotas {
            per_key_results: capture.per_key_results,
            per_key_bytes: capture.per_key_bytes,
            installation_results: capture.installation_results,
            installation_bytes: capture.installation_bytes,
        },
        admission: CaptureAdmission {
            per_key_running: capture.per_key_running,
            installation_running: capture.installation_running,
            queue_depth: capture.queue_depth,
            rate_rpm: interactive_rpm(key.rate_rpm(), state.interactive_default_rpm),
        },
        link_origin: state.link_origin.as_deref().map(str::to_owned),
        health,
    }
}

/// The DSL trawl-core parses: its stage and function tables and its two
/// size limits.
fn dsl() -> DslCapabilities {
    let names = |table: &[&str]| table.iter().map(|&name| name.to_owned()).collect();
    DslCapabilities {
        stages: names(KNOWN_PIPE_STAGES),
        functions: names(KNOWN_FUNCTIONS),
        max_query_bytes: widen(MAX_QUERY_LEN),
        max_pipeline_stages: u32::try_from(MAX_PIPELINE_STAGES).unwrap_or(u32::MAX),
    }
}

/// The interactive rate that applies to a key, as the rate limiter resolves
/// it: a role's `rate_rpm` replaces the class default, and a default of 0
/// with no role override leaves the key unlimited (`None`).
fn interactive_rpm(role_rpm: Option<u32>, default_rpm: u32) -> Option<u64> {
    role_rpm
        .or_else(|| (default_rpm > 0).then_some(default_rpm))
        .map(u64::from)
}

/// A `usize` count on the wire. No count trawld holds exceeds `u64`.
fn widen(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_rate_replaces_the_default_in_both_directions() {
        assert_eq!(interactive_rpm(Some(500), 100), Some(500));
        assert_eq!(interactive_rpm(Some(10), 100), Some(10));
    }

    #[test]
    fn without_a_role_rate_the_default_applies() {
        assert_eq!(interactive_rpm(None, 100), Some(100));
    }

    #[test]
    fn a_zero_default_without_a_role_rate_is_unlimited() {
        assert_eq!(interactive_rpm(None, 0), None);
        assert_eq!(interactive_rpm(Some(60), 0), Some(60));
    }

    #[test]
    fn the_dsl_lists_are_trawl_cores_tables() {
        let dsl = dsl();
        assert_eq!(dsl.stages, KNOWN_PIPE_STAGES);
        assert_eq!(dsl.functions, KNOWN_FUNCTIONS);
        assert_eq!(dsl.max_query_bytes, MAX_QUERY_LEN as u64);
        assert_eq!(dsl.max_pipeline_stages as usize, MAX_PIPELINE_STAGES);
    }
}
