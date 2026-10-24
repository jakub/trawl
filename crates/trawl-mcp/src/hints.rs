// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Computed hints for a `search` result (ADR-0058).
//!
//! A hint is computed from facts trawld answered, never generated, and
//! each one names the population it was computed over:
//!
//! - [`dominance`]: one value of `service` or `host` holds at least
//!   [`DOMINANCE_PERCENT`] of the captured rows, from the capture's column
//!   facts.
//! - [`lag`]: a sender whose `_ingested` runs more than
//!   [`LAG_THRESHOLD_SECS`] ahead of `_time`, over the rows the response
//!   read.
//! - [`never_observed`]: a field the query names that the catalog holds
//!   no observation of in the service the query names, or no pin for at
//!   all. The catalog evicts pins and observations, so the hint states
//!   what it holds now, not what was ever sent.
//! - [`nearest_bounds`]: for a capture with no rows whose query is a plain
//!   search stage, the last event before and the first event after its
//!   bounds that match the same search, within [`NEAREST_REACH_DAYS`]
//!   either side. Any other shape says it was not computed.
//!
//! The two that read trawld ([`never_observed`] and [`nearest_bounds`])
//! make a bounded number of calls and stay quiet when a call is refused:
//! a hint never turns a search into an error.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeDelta, Utc};
use trawl_api::value::{
    QueryResult, TRAILING_LOG_FIELDS, Value, WELL_KNOWN_LOG_FIELDS, wire_cell_text,
};
use trawl_client::{CapturedResult, ErrorCode};
use trawl_core::ast::{FilterOp, FilterValue, Query, SearchToken, Spanned};
use trawl_core::field_refs::referenced_fields;
use trawl_core::format::format_query;
use trawl_core::parser::parse;

use crate::backend::Backend;
use crate::render::rows::{json_string, json_text};
use crate::tools::failure_summary;

/// A value dominates when it holds at least this percentage of the
/// captured rows.
pub const DOMINANCE_PERCENT: u64 = 80;

/// Dominance needs at least this many distinct non-null values: one value
/// out of one is the filter, not a finding.
pub const DOMINANCE_MIN_DISTINCT: u64 = 2;

/// The fields whose dominance is reported.
pub const DOMINANCE_FIELDS: [&str; 2] = ["service", "host"];

/// A sender lags when `_ingested - _time` is more than this many seconds
/// on some row it sent.
pub const LAG_THRESHOLD_SECS: i64 = 300;

/// How far before the earliest bound and after the latest bound the
/// nearest-bounds query looks, in days. The far end is clipped at now.
pub const NEAREST_REACH_DAYS: i64 = 7;

/// The most fields the never-observed check asks the catalog about.
pub const MAX_FIELD_CHECKS: usize = 4;

/// Observations the never-observed check reads per catalog page.
pub const CATALOG_PAGE_ROWS: usize = 100;

/// The most catalog pages the never-observed check reads per field; past
/// it the answer is unknown and no hint is given.
pub const CATALOG_MAX_PAGES: usize = 3;

/// The hint for a zero-row result whose query is not a plain search stage.
pub const NEAREST_NOT_COMPUTED: &str = "nearest bounds: not computed for this shape; only a \
plain search stage with no pipe stages is widened to find the nearest events";

/// Dominance hints over the capture's column facts: for `service` and
/// `host`, a value holding at least [`DOMINANCE_PERCENT`] of the captured
/// rows among at least [`DOMINANCE_MIN_DISTINCT`] distinct values.
///
/// The facts list values only for a column with at most five distinct
/// values; a column with more is not judged.
#[must_use]
pub fn dominance(result: &CapturedResult) -> Vec<String> {
    let captured = result.captured_rows;
    let mut hints = Vec::new();
    if captured == 0 {
        return hints;
    }
    for field in DOMINANCE_FIELDS {
        let Some(position) = result.columns.iter().position(|c| c.name == field) else {
            continue;
        };
        let Some(facts) = result.column_facts.get(position) else {
            continue;
        };
        if facts.distinct_capped || facts.distinct < DOMINANCE_MIN_DISTINCT {
            continue;
        }
        let Some(top) = facts.values.as_ref().and_then(|values| values.first()) else {
            continue;
        };
        if top.count.saturating_mul(100) < captured.saturating_mul(DOMINANCE_PERCENT) {
            continue;
        }
        let column = &result.columns[position];
        let value = wire_cell_text(&column.name, &column.ty, &top.value)
            .ok()
            .and_then(|wire| serde_json::from_str::<serde_json::Value>(&wire).ok())
            .unwrap_or_else(|| top.value.to_plain_json());
        hints.push(format!(
            "dominance: {} {} holds {} of the {captured} captured rows ({}%) among {} distinct \
             values",
            json_string(field),
            json_text(&value),
            top.count,
            top.count.saturating_mul(100) / captured,
            facts.distinct
        ));
    }
    hints
}

/// The lag hint over the rows of `page`: the sender, a (`service`, `host`)
/// pair, whose largest `_ingested - _time` is the largest of all, when it
/// is over [`LAG_THRESHOLD_SECS`]. `None` when no sender lags or the page
/// lacks a column it needs.
#[must_use]
pub fn lag(page: &QueryResult) -> Option<String> {
    let column = |name: &str| page.columns.iter().position(|c| c.name == name);
    let (time, ingested) = (column("_time")?, column("_ingested")?);
    let (service, host) = (column("service"), column("host"));
    let mut worst: BTreeMap<(String, String), i64> = BTreeMap::new();
    for row in &page.rows {
        let (Some(at), Some(arrived)) = (
            row.get(time).and_then(instant),
            row.get(ingested).and_then(instant),
        ) else {
            continue;
        };
        let lag = (arrived - at).num_seconds();
        let key = (cell_json(row, service), cell_json(row, host));
        let entry = worst.entry(key).or_insert(lag);
        *entry = (*entry).max(lag);
    }
    let lagging: Vec<(&(String, String), &i64)> = worst
        .iter()
        .filter(|(_, lag)| **lag > LAG_THRESHOLD_SECS)
        .collect();
    let ((service, host), secs) = lagging.iter().max_by_key(|(_, lag)| **lag)?;
    let others = lagging.len() - 1;
    let mut hint = format!(
        "lag: service {service} on host {host} sent an event {secs} s after its _time (the \
         largest _ingested - _time over the {} rows this response read)",
        page.rows.len()
    );
    if others > 0 {
        let _ = write!(
            hint,
            "; {others} other {} also lag more than {LAG_THRESHOLD_SECS} s",
            if others == 1 { "sender" } else { "senders" }
        );
    }
    Some(hint)
}

/// A cell of `row` at `index` as JSON text; a missing column is `null`.
fn cell_json(row: &[Value], index: Option<usize>) -> String {
    index
        .and_then(|i| row.get(i))
        .map_or_else(|| "null".to_owned(), |v| json_text(&v.to_plain_json()))
}

/// A timestamp cell as an instant in UTC: RFC 3339, or a naive date and
/// time read as UTC.
fn instant(cell: &Value) -> Option<DateTime<Utc>> {
    let Value::String(text) = cell else {
        return None;
    };
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Utc));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
        .map(|naive| naive.and_utc())
}

/// Hints for fields the query names in its search stage that are not in
/// the catalog: with no observation in the one service the search stage
/// names with `service=<value>`, or with no pin when it names none or the
/// catalog has no pin for the field.
///
/// The hint states what the catalog holds now. Pins and observations are
/// evicted, so stored events can still carry a field the catalog no
/// longer holds.
///
/// Envelope fields are not checked. At most [`MAX_FIELD_CHECKS`] fields
/// are checked, each over at most [`CATALOG_MAX_PAGES`] pages of
/// observations; a refused or unfinished check gives no hint.
pub async fn never_observed<B: Backend>(backend: &B, query: &str) -> Vec<String> {
    let Ok(mut parsed) = parse(query) else {
        return Vec::new();
    };
    let service = named_service(&parsed);
    parsed.pipeline.clear();
    let fields: Vec<String> = referenced_fields(&parsed)
        .into_iter()
        .filter(|field| {
            !field.starts_with('_')
                && !WELL_KNOWN_LOG_FIELDS.contains(&field.as_str())
                && !TRAILING_LOG_FIELDS.contains(&field.as_str())
        })
        .take(MAX_FIELD_CHECKS)
        .collect();
    let mut hints = Vec::new();
    for field in fields {
        if let Some(hint) = field_check(backend, &field, service.as_deref()).await {
            hints.push(hint);
        }
    }
    hints
}

/// The service a search stage names with one top-level `service=<value>`,
/// when it has one group.
fn named_service(query: &Query) -> Option<String> {
    let [group] = query.search.groups.as_slice() else {
        return None;
    };
    group.iter().find_map(|token| match &token.node {
        SearchToken::FieldFilter(filter)
            if filter.field == "service" && filter.op == FilterOp::Eq =>
        {
            match &filter.value {
                FilterValue::Literal(value) => Some(value.clone()),
                FilterValue::List(_) => None,
            }
        }
        _ => None,
    })
}

/// The not-in-the-catalog hint for one field, if the catalog's answer
/// proves it.
async fn field_check<B: Backend>(
    backend: &B,
    field: &str,
    service: Option<&str>,
) -> Option<String> {
    let mut after: Option<String> = None;
    let mut seen_in = 0_usize;
    for _ in 0..CATALOG_MAX_PAGES {
        let page = match backend
            .catalog_field(field, Some(CATALOG_PAGE_ROWS), after.as_deref())
            .await
        {
            Ok(page) => page,
            Err(error) if error.error_code() == Some(&ErrorCode::NotFound) => {
                return Some(format!(
                    "the catalog has no pin for field {}",
                    json_string(field)
                ));
            }
            Err(_) => return None,
        };
        let service = service?;
        if page.services.iter().any(|row| row.service == service) {
            return None;
        }
        seen_in += page.services.len();
        match page.services_cursor {
            Some(cursor) => after = Some(cursor),
            None => {
                return Some(format!(
                    "the catalog has no observation of field {} in service {}; it has \
                     observations of it in {seen_in} other {}",
                    json_string(field),
                    json_string(service),
                    if seen_in == 1 { "service" } else { "services" }
                ));
            }
        }
    }
    None
}

/// The nearest-bounds hint for a capture with no rows.
///
/// Only a plain search stage is widened: the caller's query without pipe
/// stages. Its time clauses are removed from the parsed search stage and
/// bounds [`NEAREST_REACH_DAYS`] before the earliest bound and after the
/// latest bound, clipped at `now`, put in their place; one aggregate over
/// that search reports the newest event before the bounds, the oldest
/// after, and any that now fall inside them. Any other shape answers
/// [`NEAREST_NOT_COMPUTED`].
pub async fn nearest_bounds<B: Backend>(
    backend: &B,
    result: &CapturedResult,
    now: DateTime<Utc>,
) -> String {
    let Some(plan) = NearestPlan::new(result, now) else {
        return NEAREST_NOT_COMPUTED.to_owned();
    };
    match backend
        .query_paginated_tz(&plan.dsl, Some(3), None, Some("UTC".to_owned()))
        .await
    {
        Ok(response) => plan.describe(&response.result),
        Err(error) => format!(
            "nearest bounds: not computed; the widened query failed: {}",
            failure_summary(&error)
        ),
    }
}

/// The widened query and the bounds it was built from.
struct NearestPlan {
    dsl: String,
    from: String,
    earliest: String,
    latest: String,
    to: String,
}

impl NearestPlan {
    fn new(result: &CapturedResult, now: DateTime<Utc>) -> Option<Self> {
        let earliest = parse_instant(result.earliest.as_deref()?)?;
        let latest = parse_instant(result.latest.as_deref()?)?;
        let mut query = parse(&result.query).ok()?;
        if !query.pipeline.is_empty() {
            return None;
        }
        let reach = TimeDelta::days(NEAREST_REACH_DAYS);
        let from = earliest.checked_sub_signed(reach)?;
        let to = latest.checked_add_signed(reach)?.min(now).max(latest);
        let (from, earliest, latest, to) =
            (micros(from), micros(earliest), micros(latest), micros(to));
        let search = &mut query.search;
        search.time_filter = None;
        search.hoisted.clear();
        search.earliest = Some(Spanned::new(from.clone(), 0..0));
        search.latest = Some(Spanned::new(to.clone(), 0..0));
        // The aggregate is built from text that holds only the bounds this
        // function formatted, and joins the caller's search as AST: the
        // caller's text reaches the DSL only through the formatter.
        let aggregate = parse(&format!(
            "* | let side = case(_time < \"{earliest}\", \"before\", _time >= \"{latest}\", \
             \"after\", \"inside\") | stats max(_time) as newest, min(_time) as oldest, count() \
             as events by side"
        ))
        .ok()?;
        query.pipeline = aggregate.pipeline;
        let dsl = format_query(&query);
        // The rebuilt text must say what was built: the same search terms,
        // the widened bounds, the one aggregate.
        let reparsed = parse(&dsl).ok()?;
        let bounds = (
            reparsed.search.earliest.as_ref().map(|b| b.node.as_str()),
            reparsed.search.latest.as_ref().map(|b| b.node.as_str()),
        );
        if reparsed.search.time_filter.is_some()
            || bounds != (Some(from.as_str()), Some(to.as_str()))
            || reparsed.pipeline.len() != 2
            || search_terms(&reparsed.search.groups) != search_terms(&query.search.groups)
        {
            return None;
        }
        Some(Self {
            dsl,
            from,
            earliest,
            latest,
            to,
        })
    }

    /// The hint from the aggregate's rows.
    fn describe(&self, answer: &QueryResult) -> String {
        let column = |name: &str| answer.columns.iter().position(|c| c.name == name);
        let (Some(side), Some(newest), Some(oldest), Some(events)) = (
            column("side"),
            column("newest"),
            column("oldest"),
            column("events"),
        ) else {
            return "nearest bounds: not computed; the widened query answered without its \
                    columns"
                .to_owned();
        };
        let group = |name: &str| {
            answer
                .rows
                .iter()
                .find(|row| matches!(row.get(side), Some(Value::String(s)) if s == name))
        };
        let text = |row: &[Value], index: usize| match row.get(index) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let mut parts = Vec::new();
        parts.push(match group("before") {
            Some(row) => format!(
                "the last event before the bounds is at {} ({} events from {} to {})",
                text(row, newest),
                text(row, events),
                self.from,
                self.earliest
            ),
            None => format!("no event from {} to {}", self.from, self.earliest),
        });
        parts.push(match group("after") {
            Some(row) => format!(
                "the first event after them is at {} ({} events from {} to {})",
                text(row, oldest),
                text(row, events),
                self.latest,
                self.to
            ),
            None => format!("no event from {} to {}", self.latest, self.to),
        });
        if let Some(row) = group("inside") {
            parts.push(format!(
                "{} events now fall inside the bounds, so they arrived after the capture",
                text(row, events)
            ));
        }
        format!(
            "nearest bounds over events matching the same search stage from {} to {}: {}",
            self.from,
            self.to,
            parts.join("; ")
        )
    }
}

/// A search stage's groups with every span cleared, so two parses of
/// different text compare by the terms they hold.
fn search_terms(groups: &[Vec<Spanned<SearchToken>>]) -> Vec<Vec<SearchToken>> {
    groups
        .iter()
        .map(|group| group.iter().map(|token| unspanned(&token.node)).collect())
        .collect()
}

fn unspanned(token: &SearchToken) -> SearchToken {
    match token {
        SearchToken::Not(inner) => {
            SearchToken::Not(Box::new(Spanned::new(unspanned(&inner.node), 0..0)))
        }
        SearchToken::Group(groups) => SearchToken::Group(
            search_terms(groups)
                .into_iter()
                .map(|group| {
                    group
                        .into_iter()
                        .map(|token| Spanned::new(token, 0..0))
                        .collect()
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn parse_instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn micros(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}
