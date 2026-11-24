// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `orient`: the installation in one call (ADR-0058).
//!
//! Nothing is read at startup: a server that is down answers the first
//! `orient` with a refusal, not the process with an exit. Retention reach
//! is read from the dashboard only when the key holds `server_manage`, so
//! `orient` widens nothing a key could learn.

use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use rmcp::model::CallToolResult;
use serde_json::{Map, Value as Json, json};
use trawl_api::value::Value;
use trawl_api::zone::{DisplayZone, wire_timestamp};
use trawl_api::{CapabilitiesResponse, HealthStatus, Reach, ReachEnd, WithheldReason};
use trawl_client::types::ServiceSchemaResponse;
use trawl_client::{
    ClientError, DashboardSnapshot, ErrorCode, InvestigationList, InvestigationStatus,
    QueryResponse,
};

use super::failure_summary;
use super::schema::{FIELDS, ORIENT, SEARCH};
use crate::backend::Backend;
use crate::render::activity::summary_view;
use crate::render::{self, Budget};
use crate::reply::{
    DslFeatures, HealthView, InventoryRow, InvestigationSummaryView, KeyView, NamedValue,
    NextAction, OrientReply, RetentionReach,
};
use crate::{dsl, render::refusal};

pub const INVENTORY_QUERY: &str = "last=24h \
| stats count() as events, max(_time) as last_seen by env, service, host \
| sort -events";

pub const INVENTORY_MAX_ROWS: usize = 200;

const QUERY_PERMISSION: &str = "query";
const SERVER_MANAGE_PERMISSION: &str = "server_manage";
const SCHEMA_READ_PERMISSION: &str = "schema_read";
const INVESTIGATION_READ_PERMISSION: &str = "investigation_read";

/// The most open investigations `orient` lists: a glance that leaves
/// orient's budget to the installation, with the title of each at
/// trawl-api's 200-byte bound; `investigation action=list` lists the rest.
pub const OPEN_INVESTIGATIONS_MAX: usize = 5;

pub const RETENTION_NOT_READABLE: &str = "not readable with this key";

const FIRST_SEARCH: &str = "_severity>=error last=1h | stats count() by service | sort -count";

pub async fn run<B: Backend>(
    backend: &B,
    display_zone: DisplayZone,
    now: DateTime<Utc>,
) -> CallToolResult {
    let (whoami, capabilities, health, services) = tokio::join!(
        backend.whoami(),
        backend.capabilities(),
        backend.health_with_status(),
        backend.schema_services(),
    );
    let whoami = match whoami {
        Ok(whoami) => whoami,
        Err(error) => return refusal::refusal(ORIENT, &error, None),
    };
    let capabilities = match capabilities {
        Ok(capabilities) => capabilities,
        Err(error) => return refusal::refusal(ORIENT, &error, None),
    };
    let permitted = |name: &str| whoami.permissions.iter().any(|p| p == name);
    let may_query = permitted(QUERY_PERMISSION);
    let may_manage = permitted(SERVER_MANAGE_PERMISSION);
    let may_list = may_query && permitted(INVESTIGATION_READ_PERMISSION);

    let (inventory, dashboard, investigations) =
        gated_reads(backend, may_query, may_manage, may_list).await;
    let (open_investigations, open_investigations_note) = open_part(investigations);

    let (inventory, inventory_note) = inventory_part(inventory);
    let (retention, retention_note) = retention_part(dashboard, now.date_naive());
    let (degraded_fields, degraded_note) = degraded_part(services);
    let health = health.map_or_else(
        // The capabilities answer carries the same report; only its HTTP
        // status is not carried, so it is the status health answers with.
        |_| {
            let status = if matches!(capabilities.health.status, HealthStatus::Unavailable) {
                503
            } else {
                200
            };
            health_view(status, &capabilities.health)
        },
        |(status, health)| health_view(status, &health),
    );

    let next_actions = next_actions(
        may_query,
        permitted(SCHEMA_READ_PERMISSION),
        inventory.first().and_then(|row| row.service.as_deref()),
    );
    let reply = OrientReply {
        outcome: outcome(
            &capabilities.version,
            &health.status,
            may_query.then_some(inventory.len()),
        ),
        server_version: Some(capabilities.version.clone()),
        now_utc: now_text(now, DisplayZone::Utc.offset_at(now)),
        display_zone: display_zone_label(display_zone, now),
        now_display: now_text(now, display_zone.offset_at(now)),
        key: KeyView {
            name: whoami.name.clone(),
            prefix: whoami.prefix.clone(),
            permissions: whoami.permissions.clone(),
        },
        health,
        inventory,
        inventory_note,
        retention,
        retention_note,
        degraded_fields,
        degraded_note,
        limits: limits(&capabilities),
        dsl: DslFeatures {
            stages: capabilities.dsl.stages.clone(),
            functions: capabilities.dsl.functions.clone(),
        },
        essentials: dsl::ESSENTIALS.to_owned(),
        resources: dsl::sections()
            .iter()
            .map(|section| section.uri.clone())
            .collect(),
        open_investigations,
        open_investigations_note,
        next_actions,
    };
    render::orient(reply, Budget::DEFAULT)
}

async fn gated_reads<B: Backend>(
    backend: &B,
    may_query: bool,
    may_manage: bool,
    may_list: bool,
) -> (
    Option<Result<QueryResponse, ClientError>>,
    Option<Result<DashboardSnapshot, ClientError>>,
    Option<Result<InvestigationList, ClientError>>,
) {
    tokio::join!(
        async {
            if may_query {
                Some(
                    backend
                        .query_paginated(INVENTORY_QUERY, Some(INVENTORY_MAX_ROWS), None)
                        .await,
                )
            } else {
                None
            }
        },
        async {
            if may_manage {
                Some(backend.dashboard().await)
            } else {
                None
            }
        },
        async {
            if may_list {
                Some(backend.list_investigations().await)
            } else {
                None
            }
        },
    )
}

fn open_part(
    listed: Option<Result<InvestigationList, ClientError>>,
) -> (Vec<InvestigationSummaryView>, Option<String>) {
    let list = match listed {
        None => return (Vec::new(), None),
        Some(Err(error)) => {
            return (
                Vec::new(),
                Some(format!("could not be listed: {}", failure_summary(&error))),
            );
        }
        Some(Ok(list)) => list,
    };
    let mut open: Vec<_> = list
        .investigations
        .iter()
        .filter(|summary| summary.status == InvestigationStatus::Open)
        .collect();
    // RFC 3339 in UTC with fixed precision sorts as text.
    open.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    let more = open.len().saturating_sub(OPEN_INVESTIGATIONS_MAX);
    let note = if open.is_empty() {
        Some("none".to_owned())
    } else if more > 0 {
        Some(format!(
            "{more} more open; investigation action=list lists them all"
        ))
    } else {
        None
    };
    let views = open
        .into_iter()
        .take(OPEN_INVESTIGATIONS_MAX)
        .map(summary_view)
        .collect();
    (views, note)
}

fn outcome(version: &str, health: &str, groups: Option<usize>) -> String {
    match groups {
        Some(1) => format!(
            "trawld {version} answers: health {health}, 1 environment, service and host with \
             events in the last 24 hours."
        ),
        Some(n) => format!(
            "trawld {version} answers: health {health}, {n} environment, service and host groups \
             with events in the last 24 hours."
        ),
        None => format!("trawld {version} answers: health {health}; this key cannot run queries."),
    }
}

fn inventory_part(
    answer: Option<Result<QueryResponse, ClientError>>,
) -> (Vec<InventoryRow>, Option<String>) {
    match answer {
        None => (
            Vec::new(),
            Some("absent: this key lacks the query permission".to_owned()),
        ),
        Some(Ok(response)) => inventory_rows(&response),
        Some(Err(error)) => (Vec::new(), Some(inventory_failure(&error))),
    }
}

fn retention_part(
    answer: Option<Result<DashboardSnapshot, ClientError>>,
    today: NaiveDate,
) -> (Vec<RetentionReach>, Option<String>) {
    match answer {
        None => (Vec::new(), Some(RETENTION_NOT_READABLE.to_owned())),
        Some(Ok(dashboard)) => {
            let reach: Vec<RetentionReach> = dashboard
                .capacity
                .environments
                .iter()
                .map(|env| retention_reach(env, today))
                .collect();
            let note = reach
                .is_empty()
                .then(|| "no environment has stored data yet".to_owned());
            (reach, note)
        }
        Some(Err(error)) => (
            Vec::new(),
            Some(format!(
                "the dashboard could not be read: {}",
                failure_summary(&error)
            )),
        ),
    }
}

fn degraded_part(
    answer: Result<ServiceSchemaResponse, ClientError>,
) -> (Vec<String>, Option<String>) {
    match answer {
        Ok(services) => {
            let mut fields: Vec<String> = services
                .services
                .iter()
                .flat_map(|service| service.degraded_fields.iter().cloned())
                .collect();
            fields.sort();
            fields.dedup();
            (fields, None)
        }
        Err(error) => (
            Vec::new(),
            Some(format!(
                "unknown: the per-service schema could not be read: {}",
                failure_summary(&error)
            )),
        ),
    }
}

fn display_zone_label(zone: DisplayZone, now: DateTime<Utc>) -> String {
    const SETTING: &str = "the mcp host's setting";
    match zone {
        DisplayZone::Local => format!("local, {}, {SETTING}", zone.offset_at(now)),
        DisplayZone::Utc | DisplayZone::Fixed(_) => format!("{zone}, {SETTING}"),
    }
}

fn now_text(now: DateTime<Utc>, offset: FixedOffset) -> String {
    wire_timestamp(now.timestamp_micros(), offset)
}

fn inventory_rows(response: &QueryResponse) -> (Vec<InventoryRow>, Option<String>) {
    let result = &response.result;
    let column = |name: &str| result.columns.iter().position(|c| c.name == name);
    let (Some(env), Some(service), Some(host), Some(events), Some(last_seen)) = (
        column("env"),
        column("service"),
        column("host"),
        column("events"),
        column("last_seen"),
    ) else {
        return (
            Vec::new(),
            Some("absent: the inventory query answered without its columns".to_owned()),
        );
    };
    let rows: Vec<InventoryRow> = result
        .rows
        .iter()
        .map(|row| InventoryRow {
            env: text(row.get(env)),
            service: text(row.get(service)),
            host: text(row.get(host)),
            events_24h: count(row.get(events)),
            last_seen: text(row.get(last_seen)),
        })
        .collect();
    let total = response.pagination.total;
    let note = if rows.is_empty() {
        Some("no events in the last 24 hours".to_owned())
    } else if total > rows.len() {
        Some(format!(
            "the {} busiest of {total} groups; the rest are not listed",
            rows.len()
        ))
    } else {
        None
    };
    (rows, note)
}

fn text(cell: Option<&Value>) -> Option<String> {
    match cell? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

fn count(cell: Option<&Value>) -> u64 {
    match cell {
        Some(Value::Integer(n)) => u64::try_from(*n).unwrap_or(0),
        Some(Value::UInt(n)) => *n,
        _ => 0,
    }
}

fn inventory_failure(error: &ClientError) -> String {
    match error.error_code() {
        Some(ErrorCode::CorpusRecovering) => "absent: trawld is refusing corpus reads while it \
                                              recovers the corpus (corpus_recovering)"
            .to_owned(),
        Some(ErrorCode::Forbidden) => "absent: this key lacks the query permission".to_owned(),
        _ => format!(
            "absent: the inventory query failed: {}",
            failure_summary(error)
        ),
    }
}

fn health_view(http_status: u16, health: &trawl_client::HealthResponse) -> HealthView {
    let status = match health.status {
        HealthStatus::Ok => "ok",
        HealthStatus::Degraded => "degraded",
        HealthStatus::Unavailable => "unavailable",
    };
    let mut problems: Vec<String> = health
        .checks
        .iter()
        .flatten()
        .filter(|(_, detail)| detail.as_str() != "ok")
        .map(|(name, detail)| format!("{name}: {detail}"))
        .collect();
    problems.sort();
    HealthView {
        status: status.to_owned(),
        http_status,
        problems,
    }
}

fn retention_reach(env: &trawl_api::EnvironmentCapacity, today: NaiveDate) -> RetentionReach {
    let days_kept = NaiveDate::parse_from_str(&env.oldest_date, "%Y-%m-%d")
        .ok()
        .map_or(0, |oldest| {
            u64::try_from((today - oldest).num_days() + 1).unwrap_or(0)
        });
    RetentionReach {
        env: env.env.clone(),
        days_kept,
        oldest: Some(env.oldest_date.clone()),
        max_age_days: env.max_age_days,
        projection: projection(&env.reach, env.max_age_days),
    }
}

/// trawld's reach projection in words: a conditional projection, never a
/// countdown (ADR-0042).
fn projection(reach: &Reach, max_age_days: u64) -> String {
    let end = |end: &ReachEnd| match end {
        ReachEnd::Days { days } => format!("about {days} days"),
        ReachEnd::FullPolicy => "all".to_owned(),
        ReachEnd::DiskFillsFirst => "less than all, because the disk fills first".to_owned(),
    };
    match reach {
        Reach::Projected {
            observed_first,
            observed_last,
            observed_days,
            low,
            high,
        } => format!(
            "if the {observed_days} observed days from {observed_first} to {observed_last} \
             repeat, the disk holds between {} and {} of the {max_age_days}-day policy",
            end(low),
            end(high)
        ),
        Reach::KeepForever {
            observed_first,
            observed_last,
            observed_days,
            mean_daily_bytes,
        } => format!(
            "grows about {mean_daily_bytes} bytes a day over the {observed_days} observed days \
             from {observed_first} to {observed_last}"
        ),
        Reach::Withheld { reason } => match reason {
            WithheldReason::InsufficientHistory => {
                "projection withheld: fewer than 3 observed days"
            }
            WithheldReason::RetentionSuppressed => {
                "projection withheld: a repin overlapped the measurement"
            }
            WithheldReason::MeasurementUnavailable => {
                "projection withheld: the measurement is not complete"
            }
        }
        .to_owned(),
    }
}

fn limits(capabilities: &CapabilitiesResponse) -> Vec<NamedValue> {
    let b = &capabilities.bounds;
    let q = &capabilities.quotas;
    let a = &capabilities.admission;
    let d = &capabilities.dsl;
    let named = |name: &str, value: String| NamedValue {
        name: name.to_owned(),
        value,
    };
    vec![
        named("dsl.max_query_bytes", d.max_query_bytes.to_string()),
        named("dsl.max_pipeline_stages", d.max_pipeline_stages.to_string()),
        named("server.timeout_secs", b.query_timeout_secs.to_string()),
        named("server.max_result_rows", b.max_result_rows.to_string()),
        named(
            "capture.default_window_secs",
            b.default_window_secs.to_string(),
        ),
        named("capture.max_span_secs", b.max_span_secs.to_string()),
        named("capture.deadline_secs", b.deadline_secs.to_string()),
        named("capture.max_rows", b.max_rows.to_string()),
        named("capture.max_wire_bytes", b.max_wire_bytes.to_string()),
        named("capture.page_max_rows", b.page_max_rows.to_string()),
        named("capture.ttl_secs", b.ttl_secs.to_string()),
        named("capture.per_key_results", q.per_key_results.to_string()),
        named("capture.per_key_bytes", q.per_key_bytes.to_string()),
        named(
            "capture.installation_results",
            q.installation_results.to_string(),
        ),
        named(
            "capture.installation_bytes",
            q.installation_bytes.to_string(),
        ),
        named("capture.per_key_running", a.per_key_running.to_string()),
        named(
            "capture.installation_running",
            a.installation_running.to_string(),
        ),
        named("capture.queue_depth", a.queue_depth.to_string()),
        named(
            "rate_rpm",
            a.rate_rpm
                .map_or_else(|| "unlimited".to_owned(), |rpm| rpm.to_string()),
        ),
        named(
            "link_origin",
            capabilities
                .link_origin
                .clone()
                .unwrap_or_else(|| "unset: search answers carry no search URL".to_owned()),
        ),
    ]
}

fn next_actions(may_query: bool, may_read_schema: bool, service: Option<&str>) -> Vec<NextAction> {
    let mut actions = Vec::new();
    if may_query {
        actions.push(NextAction {
            tool: SEARCH.to_owned(),
            arguments: object(json!({"mode": "execute", "query": FIRST_SEARCH})),
            reason: "count the last hour's errors by service".to_owned(),
        });
    }
    if may_read_schema && let Some(service) = service {
        actions.push(NextAction {
            tool: FIELDS.to_owned(),
            arguments: object(json!({"source": "service", "service": service})),
            reason: "list the fields of the busiest service".to_owned(),
        });
    }
    actions
}

pub(crate) fn object(value: Json) -> Map<String, Json> {
    match value {
        Json::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inventory_query_parses() {
        let query = trawl_core::parser::parse(INVENTORY_QUERY).expect("the inventory parses");
        assert!(query.has_aggregation());
        trawl_core::parser::parse(FIRST_SEARCH).expect("the first search parses");
    }

    #[test]
    fn days_kept_counts_the_oldest_date_through_today() {
        let env = trawl_api::EnvironmentCapacity {
            env: "prod".to_owned(),
            max_age_days: 90,
            oldest_date: "2026-03-01".to_owned(),
            stored_bytes: 1,
            reach: Reach::Withheld {
                reason: WithheldReason::InsufficientHistory,
            },
        };
        let reach = retention_reach(&env, NaiveDate::from_ymd_opt(2026, 3, 14).unwrap());
        assert_eq!(reach.days_kept, 14);
        assert_eq!(
            reach.projection,
            "projection withheld: fewer than 3 observed days"
        );
    }
}
