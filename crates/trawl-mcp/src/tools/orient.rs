// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `orient`: the installation in one call (ADR-0058).
//!
//! One call reads the key (`whoami`), what the server applies
//! (`capabilities`), health, and the per-service schema, all at once, and
//! then, as the key allows, one bounded inventory query and the retention
//! reach. Nothing is read at startup: a server that is down answers the
//! first `orient` with a refusal, not the process with an exit.
//!
//! - The key and the capabilities are the answer; when either is refused,
//!   `orient` is refused with trawld's code.
//! - The inventory is one `/query` aggregate over the last 24 hours,
//!   grouped by environment, service and host ([`INVENTORY_QUERY`]). It
//!   needs the `query` permission and a corpus trawld will read; without
//!   either, one line says why and the rest still answers.
//! - Retention reach is read from the dashboard only when the key holds
//!   `server_manage`, so `orient` widens nothing a key could learn;
//!   without it, one line says it is not readable with this key.
//! - Open investigations are listed when the key holds `query` and
//!   `investigation_read` (ADR-0059): at most [`OPEN_INVESTIGATIONS_MAX`],
//!   most recently changed first. A list trawld refuses is one line, never
//!   an error.
//! - Times print in UTC and in the display zone, the `ui.timezone` of the
//!   machine that runs `trawl mcp`, labelled as that machine's setting.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, Offset, SecondsFormat, TimeZone, Utc};
use rmcp::model::CallToolResult;
use serde_json::{Map, Value as Json, json};
use trawl_api::value::Value;
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

/// The inventory: every environment, service and host that sent events in
/// the last 24 hours, with the count and the newest `_time`, busiest first.
///
/// A constant: no caller text enters it. A test holds it to trawl-core's
/// grammar.
pub const INVENTORY_QUERY: &str = "last=24h \
| stats count() as events, max(_time) as last_seen by env, service, host \
| sort -events";

/// The most inventory rows `orient` asks for; the response says how many
/// groups there were in all.
pub const INVENTORY_MAX_ROWS: usize = 200;

/// The permission a key needs to run the inventory query.
const QUERY_PERMISSION: &str = "query";
/// The permission a key needs to read the dashboard and its retention
/// reach.
const SERVER_MANAGE_PERMISSION: &str = "server_manage";
/// The permission a key needs for `fields` on a service.
const SCHEMA_READ_PERMISSION: &str = "schema_read";
/// The permission a key needs, with `query`, to list investigations.
const INVESTIGATION_READ_PERMISSION: &str = "investigation_read";

/// The most open investigations `orient` lists: a glance that leaves
/// orient's budget to the installation, with the title of each at
/// trawl-api's 200-byte bound; `investigation action=list` lists the rest.
pub const OPEN_INVESTIGATIONS_MAX: usize = 5;

/// The note when the key cannot read retention reach.
pub const RETENTION_NOT_READABLE: &str = "not readable with this key";

/// The query `orient` proposes as a first search.
const FIRST_SEARCH: &str = "_severity>=error last=1h | stats count() by service | sort -count";

/// Run `orient` against `backend`, with `display_zone` the MCP host's
/// display setting and `now` the instant it reports as now.
pub async fn run<B: Backend>(
    backend: &B,
    display_zone: &str,
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

    let (offset, display_zone_label, display_zone_note) = display_offset(display_zone, now);
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
        now_utc: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        display_zone: display_zone_label,
        now_display: now
            .with_timezone(&offset)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        display_zone_note,
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

/// The reads a permission gates, run together: the inventory query when
/// the key holds `query`, the dashboard when it holds `server_manage`, the
/// investigation list when it may list them.
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
                        .query_paginated_tz(
                            INVENTORY_QUERY,
                            Some(INVENTORY_MAX_ROWS),
                            None,
                            Some("UTC".to_owned()),
                        )
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

/// The open investigations of `listed`, most recently changed first, at
/// most [`OPEN_INVESTIGATIONS_MAX`], and the line that says what the list
/// leaves out or why there is none; nothing for a key that cannot list.
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

/// The outcome line: the version, health, and how many groups sent events
/// in the last 24 hours, or `None` when the key cannot run queries.
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

/// The inventory and its note, from the inventory query's answer, or
/// `None` when the key may not run it.
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

/// Retention reach and its note, from the dashboard, or `None` when the
/// key may not read it.
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

/// The degraded fields across services, or the note saying why they are
/// unknown.
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

/// The display zone's offset at `now`, its label, and a note when the
/// setting did not parse and UTC stands in.
///
/// `zone` is the CLI's `ui.timezone`: `local` (the offset of the machine
/// that runs `trawl mcp`, at `now`), `UTC`, or a fixed offset such as
/// `+05:30`. The CLI refuses any other spelling, so the note is a guard,
/// never a panic.
#[must_use]
pub fn display_offset(zone: &str, now: DateTime<Utc>) -> (FixedOffset, String, Option<String>) {
    const SETTING: &str = "the mcp host's setting";
    let utc = FixedOffset::east_opt(0).unwrap_or_else(|| Utc.fix());
    let trimmed = zone.trim();
    if trimmed.eq_ignore_ascii_case("local") {
        let offset = Local.offset_from_utc_datetime(&now.naive_utc()).fix();
        return (offset, format!("local, {offset}, {SETTING}"), None);
    }
    if trimmed.eq_ignore_ascii_case("utc") {
        return (utc, format!("UTC, {SETTING}"), None);
    }
    match trimmed.parse::<FixedOffset>() {
        Ok(offset) => (offset, format!("{offset}, {SETTING}"), None),
        Err(_) => (
            utc,
            format!("UTC, standing in for {SETTING}"),
            Some(format!(
                "the setting {} did not parse, so UTC stands in",
                crate::render::rows::json_text(zone)
            )),
        ),
    }
}

/// The inventory rows and their note from the inventory query's answer.
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

/// A cell as text; null is absent.
fn text(cell: Option<&Value>) -> Option<String> {
    match cell? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// A count cell as a number; anything else is 0.
fn count(cell: Option<&Value>) -> u64 {
    match cell {
        Some(Value::Integer(n)) => u64::try_from(*n).unwrap_or(0),
        Some(Value::UInt(n)) => *n,
        _ => 0,
    }
}

/// Why the inventory is absent, from the inventory query's refusal.
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

/// Health as trawld reported it, with the checks that are not ok.
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

/// One environment's retention reach, counted to `today` in UTC.
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

/// The applied bounds, quotas and admission numbers, named by the setting
/// that holds each.
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

/// At most two first calls: a search, when the key may run one, and the
/// fields of the busiest service, when the key may read the schema.
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

/// A JSON object literal as the map a [`NextAction`] carries.
pub(crate) fn object(value: Json) -> Map<String, Json> {
    match value {
        Json::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_inventory_query_parses() {
        let query = trawl_core::parser::parse(INVENTORY_QUERY).expect("the inventory parses");
        assert!(query.has_aggregation());
        trawl_core::parser::parse(FIRST_SEARCH).expect("the first search parses");
    }

    #[test]
    fn utc_is_utc() {
        let (offset, label, note) = display_offset("UTC", at("2026-03-14T03:00:00Z"));
        assert_eq!(offset.local_minus_utc(), 0);
        assert_eq!(label, "UTC, the mcp host's setting");
        assert!(note.is_none());
    }

    #[test]
    fn a_fixed_offset_is_that_offset() {
        let (offset, label, note) = display_offset("+05:30", at("2026-03-14T03:00:00Z"));
        assert_eq!(offset.local_minus_utc(), 5 * 3600 + 30 * 60);
        assert_eq!(label, "+05:30, the mcp host's setting");
        assert!(note.is_none());
        let (offset, _, _) = display_offset("-07:00", at("2026-03-14T03:00:00Z"));
        assert_eq!(offset.local_minus_utc(), -7 * 3600);
    }

    #[test]
    fn local_is_the_machines_offset_at_that_instant() {
        let now = at("2026-07-01T12:00:00Z");
        let (offset, label, note) = display_offset("local", now);
        let expected = Local.offset_from_utc_datetime(&now.naive_utc()).fix();
        assert_eq!(offset, expected);
        assert!(label.starts_with("local, "), "{label}");
        assert!(label.ends_with("the mcp host's setting"), "{label}");
        assert!(note.is_none());
    }

    #[test]
    fn an_unparsable_zone_falls_back_to_utc_and_says_so() {
        let (offset, label, note) = display_offset("Mars/Olympus\n", at("2026-03-14T03:00:00Z"));
        assert_eq!(offset.local_minus_utc(), 0);
        assert!(label.starts_with("UTC"), "{label}");
        let note = note.expect("a note");
        assert!(note.contains(r#""Mars/Olympus\n""#), "{note}");
        assert!(note.contains("UTC stands in"), "{note}");
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
