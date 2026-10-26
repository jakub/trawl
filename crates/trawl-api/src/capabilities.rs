// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for `GET /api/v1/capabilities` (ADR-0058).
//!
//! One answer a client plans around before it sends anything: the server
//! version, the DSL this server parses, the bounds a query or capture runs
//! under, the capture quotas and admission numbers, the origin for links to
//! Search, the evidence policy of investigations, and the health report. Any key with a trawl grant reads it. The
//! server answers from its own process: the numbers are the configuration
//! it applied at boot, and the DSL lists are trawl-core's grammar.

use serde::{Deserialize, Serialize};

use crate::HealthResponse;

/// Response body for `GET /api/v1/capabilities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    /// The trawld package version, such as `"0.9.3"`.
    pub version: String,
    /// The DSL this server parses.
    pub dsl: DslCapabilities,
    /// The bounds a query and a capture run under.
    pub bounds: CapabilityBounds,
    /// The captured-result quotas (ADR-0057).
    pub quotas: CaptureQuotas,
    /// The capture admission numbers, and this key's request rate.
    pub admission: CaptureAdmission,
    /// The evidence clocks and budgets of investigations (ADR-0059).
    pub evidence: EvidencePolicy,
    /// `[server] link_origin`: the origin links to Search are built on, in
    /// its normalized form, such as `"https://trawl.example.com"`. `null`
    /// when none is configured, and then a client builds no link.
    pub link_origin: Option<String>,
    /// The report `GET /api/v1/health` answers at the same moment. Its HTTP
    /// status is not carried: this route answers 200 whatever it says.
    pub health: HealthResponse,
}

/// The DSL this server parses, from trawl-core's grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslCapabilities {
    /// Every pipe stage keyword the parser accepts, such as `stats` and
    /// `where`, in the grammar's order.
    pub stages: Vec<String>,
    /// Every function name the parser accepts, aggregates and scalars, in
    /// the grammar's order. Aliases, such as `dc` for `distinct_count`, are
    /// listed apart.
    pub functions: Vec<String>,
    /// The longest query text the parser reads, in bytes. A longer text is
    /// a parse error.
    pub max_query_bytes: u64,
    /// The most pipeline stages one query may carry. A query over it is a
    /// validation error.
    pub max_pipeline_stages: u32,
}

/// The bounds a query and a capture run under. The capture numbers are
/// the `[capture]` keys of the same names (ADR-0057).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityBounds {
    /// `[capture] default_window_secs`: seconds of history a capture covers,
    /// ending now, when the request names no time bounds.
    pub default_window_secs: u64,
    /// `[capture] max_span_secs`: the longest time span, in seconds, one
    /// capture may cover.
    pub max_span_secs: u64,
    /// Seconds one capture may take, from resolution through the file
    /// write: `[capture] deadline_secs`, capped by `[server] timeout_secs`.
    pub deadline_secs: u64,
    /// `[capture] max_rows`: the most rows one capture holds.
    pub max_rows: u64,
    /// `[capture] max_wire_bytes`: the most bytes of cell data one capture
    /// holds, counted as the rows travel on the wire.
    pub max_wire_bytes: u64,
    /// `[capture] page_max_rows`: the most rows one page of a captured
    /// result returns.
    pub page_max_rows: u64,
    /// `[capture] ttl_secs`: seconds a capture stays readable after it is
    /// created.
    pub ttl_secs: u64,
    /// `[server] timeout_secs`: the deadline of one query request, in
    /// seconds.
    pub query_timeout_secs: u64,
    /// `[server] max_result_rows`: the most rows one query result holds.
    pub max_result_rows: u64,
}

/// The captured-result quotas, the `[capture]` keys of the same names. A
/// capture reserves against both the key's quota and the installation's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureQuotas {
    /// The most live captures one key may hold.
    pub per_key_results: u64,
    /// The most capture file bytes one key may hold.
    pub per_key_bytes: u64,
    /// The most live captures the installation may hold.
    pub installation_results: u64,
    /// The most capture file bytes the installation may hold.
    pub installation_bytes: u64,
}

/// The capture admission numbers, the `[capture]` keys of the same names,
/// and the request rate that applies to this key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAdmission {
    /// The most captures one key may execute at once.
    pub per_key_running: u64,
    /// The most captures the installation may execute at once.
    pub installation_running: u64,
    /// The most captures one key may have waiting for its running slot.
    pub queue_depth: u64,
    /// Requests per minute this key may send to the interactive routes,
    /// this one included: the largest `rate_rpm` among the key's roles, or
    /// `[server.rate_limit] default_rpm` when no role sets one. `null` when
    /// neither applies, because the default is `0` and limiting is off.
    pub rate_rpm: Option<u64>,
}

/// The evidence policy of investigations, the `[investigation]` keys of the
/// same names with an `evidence_` prefix (ADR-0059). Kept evidence is
/// reserved against both budgets before an attached capture scans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePolicy {
    /// Seconds evidence files stay after the investigation's last change.
    pub idle_secs: u64,
    /// Seconds after an investigation's creation past which its evidence
    /// files never stay, unless `server_manage` extends them.
    pub ceiling_secs: u64,
    /// The most evidence bytes one investigation may keep: files and their
    /// `correlation` documents.
    pub per_investigation_bytes: u64,
    /// The most evidence bytes the installation may keep: files and their
    /// `correlation` documents.
    pub installation_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HealthStatus;

    fn response(link_origin: Option<&str>) -> CapabilitiesResponse {
        CapabilitiesResponse {
            version: "0.9.3".to_owned(),
            dsl: DslCapabilities {
                stages: vec!["stats".to_owned()],
                functions: vec!["count".to_owned()],
                max_query_bytes: 65_536,
                max_pipeline_stages: 128,
            },
            bounds: CapabilityBounds {
                default_window_secs: 900,
                max_span_secs: 604_800,
                deadline_secs: 15,
                max_rows: 5000,
                max_wire_bytes: 4_194_304,
                page_max_rows: 500,
                ttl_secs: 3600,
                query_timeout_secs: 30,
                max_result_rows: 10_000,
            },
            quotas: CaptureQuotas {
                per_key_results: 64,
                per_key_bytes: 67_108_864,
                installation_results: 512,
                installation_bytes: 536_870_912,
            },
            admission: CaptureAdmission {
                per_key_running: 1,
                installation_running: 2,
                queue_depth: 4,
                rate_rpm: None,
            },
            evidence: EvidencePolicy {
                idle_secs: 604_800,
                ceiling_secs: 2_592_000,
                per_investigation_bytes: 67_108_864,
                installation_bytes: 1_073_741_824,
            },
            link_origin: link_origin.map(str::to_owned),
            health: HealthResponse {
                status: HealthStatus::Ok,
                checks: None,
                version: None,
            },
        }
    }

    /// An unset origin and an unlimited rate are written as `null`, never
    /// left out: a client reads "not configured", not "an older server".
    #[test]
    fn unset_options_serialize_as_null() {
        let json = serde_json::to_value(response(None)).unwrap();
        assert_eq!(json["link_origin"], serde_json::Value::Null);
        assert!(json.as_object().unwrap().contains_key("link_origin"));
        assert_eq!(json["admission"]["rate_rpm"], serde_json::Value::Null);
        assert!(
            json["admission"]
                .as_object()
                .unwrap()
                .contains_key("rate_rpm")
        );
    }

    #[test]
    fn a_response_round_trips() {
        let sent = response(Some("https://trawl.example.com"));
        let json = serde_json::to_value(&sent).unwrap();
        let back: CapabilitiesResponse = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(back).unwrap(), json);
        assert_eq!(json["link_origin"], "https://trawl.example.com");
        assert_eq!(
            json["evidence"],
            serde_json::json!({
                "idle_secs": 604_800,
                "ceiling_secs": 2_592_000,
                "per_investigation_bytes": 67_108_864,
                "installation_bytes": 1_073_741_824,
            })
        );
    }
}
