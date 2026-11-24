// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Computed hints for a `search` reply (ADR-0058).
//!
//! The two that read trawld ([`never_observed`] and [`nearest_bounds`])
//! make a bounded number of calls and stay quiet when a call is refused:
//! a hint never turns a search into an error.
//!
//! A hint reads a stored query for what it means and runs none of its
//! patterns, so it parses with [`parse_unbuilt`] and builds no regex from
//! the stored text. trawld builds them when it runs the widened query.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeDelta, Utc};
use trawl_api::value::{
    QueryResult, TRAILING_LOG_FIELDS, Value, WELL_KNOWN_LOG_FIELDS, wire_cell_text,
};
use trawl_client::{CapturedResult, ErrorCode};
use trawl_core::advice::advise;
use trawl_core::ast::{
    AggExpr, Expr, ExtractMode, FilterOp, FilterValue, PipeStage, Query, Scalar, SearchToken,
    Spanned, TimeBound,
};
use trawl_core::field_refs::referenced_fields;
use trawl_core::format::checked_format_query;
use trawl_core::parser::parse_unbuilt;
use trawl_core::pattern::capture_names;
use trawl_core::projection::agg_output_name;
use trawl_core::schema::catalog_key;

use crate::backend::Backend;
use crate::blocking::parse_work;
use crate::render::rows::{json_string, json_text};
use crate::tools::failure_summary;

pub const DOMINANCE_PERCENT: u64 = 80;

/// One value out of one is the filter, not a finding.
pub const DOMINANCE_MIN_DISTINCT: u64 = 2;

pub const DOMINANCE_FIELDS: [&str; 2] = ["service", "host"];

pub const LAG_THRESHOLD_SECS: i64 = 300;

pub const NEAREST_REACH_DAYS: i64 = 7;

pub const MAX_FIELD_CHECKS: usize = 4;

pub const CATALOG_PAGE_ROWS: usize = 100;

pub const CATALOG_MAX_PAGES: usize = 3;

#[must_use]
pub fn advice(query: &str) -> Vec<String> {
    let Ok(parsed) = parse_unbuilt(query) else {
        return Vec::new();
    };
    let report = advise(&parsed);
    let mut lines: Vec<String> = report
        .findings
        .iter()
        .map(|finding| format!("advice: {}; {}", finding.message, finding.hint))
        .collect();
    if report.omitted > 0 {
        lines.push(format!(
            "advice: {} more advice {} not shown",
            report.omitted,
            if report.omitted == 1 { "note" } else { "notes" }
        ));
    }
    lines
}

pub(crate) async fn query_advice(query: &str) -> Vec<String> {
    let query = query.to_owned();
    parse_work(move || advice(&query)).await
}

pub const NEAREST_NOT_COMPUTED: &str = "nearest bounds: not computed for this shape; only a \
plain search stage with no pipe stages is widened to find the nearest events";

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

fn cell_json(row: &[Value], index: Option<usize>) -> String {
    index
        .and_then(|i| row.get(i))
        .map_or_else(|| "null".to_owned(), |v| json_text(&v.to_plain_json()))
}

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

pub async fn never_observed<B: Backend>(backend: &B, query: &str) -> Vec<String> {
    let query = query.to_owned();
    check_fields(backend, parse_work(move || fields_to_check(&query)).await).await
}

/// The hints of an executed search: the [`advice`] on `sent`, the text the
/// call sent, then the hints that read trawld for `result`:
/// [`nearest_bounds`] when it has no rows, and [`never_observed`]. All
/// three read their text in one task on the blocking pool, each from its
/// own parse, so one that finds nothing never empties another; then the
/// trawld calls run together, the nearest bounds' first.
pub(crate) async fn search_hints<B: Backend>(
    backend: &B,
    result: &CapturedResult,
    sent: &str,
    now: DateTime<Utc>,
) -> (Vec<String>, Option<String>, Vec<String>) {
    let zero_rows = result.total_rows == 0;
    let (query, earliest, latest) = (
        result.query.clone(),
        result.earliest.clone(),
        result.latest.clone(),
    );
    let sent = sent.to_owned();
    let (advice, plan, checks) = parse_work(move || {
        let advice = advice(&sent);
        let plan =
            zero_rows.then(|| NearestPlan::of(&query, earliest.as_deref(), latest.as_deref(), now));
        (advice, plan, fields_to_check(&query))
    })
    .await;
    let nearest = async {
        match plan {
            Some(plan) => Some(nearest_from(backend, plan).await),
            None => None,
        }
    };
    let (nearest, unobserved) = tokio::join!(nearest, check_fields(backend, checks));
    (advice, nearest, unobserved)
}

async fn check_fields<B: Backend>(
    backend: &B,
    checks: Option<(Option<String>, Vec<String>)>,
) -> Vec<String> {
    let Some((service, fields)) = checks else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    for field in fields {
        if let Some(hint) = field_check(backend, &field, service.as_deref()).await {
            hints.push(hint);
        }
    }
    hints
}

fn fields_to_check(query: &str) -> Option<(Option<String>, Vec<String>)> {
    let mut parsed = parse_unbuilt(query).ok()?;
    let service = named_service(&parsed);
    let pipeline = std::mem::take(&mut parsed.pipeline);
    let mut seen = BTreeSet::new();
    let fields = referenced_fields(&parsed)
        .into_iter()
        .chain(stage_reads(&pipeline))
        .filter(|field| seen.insert(field.clone()))
        .filter(|field| {
            !field.starts_with('_')
                && !WELL_KNOWN_LOG_FIELDS.contains(&field.as_str())
                && !TRAILING_LOG_FIELDS.contains(&field.as_str())
        })
        .take(MAX_FIELD_CHECKS)
        .collect();
    Some((service, fields))
}

/// The walk stops after a stage that replaces the relation, since every later
/// name is its output, and after an `extract` whose outputs it cannot name. A
/// pipeline with `from saved` reads a recorded run, not the events, and gives
/// nothing.
fn stage_reads(pipeline: &[Spanned<PipeStage>]) -> Vec<String> {
    let mut reads = Vec::new();
    if pipeline
        .iter()
        .any(|stage| matches!(stage.node, PipeStage::FromSaved(_)))
    {
        return reads;
    }
    let mut made = BTreeSet::new();
    for stage in pipeline {
        if !stage_read(&stage.node, &mut made, &mut reads) {
            break;
        }
    }
    reads
}

/// False when the walk stops here.
fn stage_read(stage: &PipeStage, made: &mut BTreeSet<String>, reads: &mut Vec<String>) -> bool {
    // Exhaustive on purpose: a new stage states its rule here.
    match stage {
        PipeStage::Where(s) => read(expr_reads(&s.condition.node), made, reads),
        PipeStage::Sort(s) => read(s.fields.iter().map(|f| f.field.as_str()), made, reads),
        PipeStage::Dedup(s) => read(fields(&s.fields), made, reads),
        PipeStage::Let(s) => {
            // A later assignment's read of an earlier target may be the
            // alias, so it is not a read of the events.
            for (target, expr) in &s.assignments {
                read(expr_reads(&expr.node), made, reads);
                made.insert(catalog_key(target));
            }
        }
        PipeStage::Rename(s) => {
            // Every pair renames in parallel, so each source is read
            // against the names as they stood before the stage.
            read(s.renames.iter().map(|(from, _)| from.as_str()), made, reads);
            for (from, to) in &s.renames {
                made.insert(catalog_key(from));
                made.insert(catalog_key(to));
            }
        }
        PipeStage::Drop(s) => made.extend(s.fields.iter().map(|name| catalog_key(name))),
        PipeStage::Extract(s) => {
            read(s.source_field.as_deref(), made, reads);
            // A kv extract's outputs are named by the data.
            let ExtractMode::Regex(body) = &s.mode else {
                return false;
            };
            let Some(names) = capture_names(body) else {
                return false;
            };
            made.extend(names.iter().map(|name| catalog_key(name)));
        }
        PipeStage::EventStats(s) => {
            read(aggs_reads(&s.aggregations), made, reads);
            read(fields(&s.group_by), made, reads);
            made.extend(
                s.aggregations
                    .iter()
                    .map(|agg| catalog_key(&agg_output_name(agg))),
            );
        }
        PipeStage::Stats(s) => {
            read(aggs_reads(&s.aggregations), made, reads);
            read(fields(&s.group_by), made, reads);
            return false;
        }
        PipeStage::Timechart(s) => {
            read(s.on.as_deref(), made, reads);
            read(aggs_reads(&s.aggregations), made, reads);
            read(fields(&s.group_by), made, reads);
            return false;
        }
        PipeStage::Top(s) => {
            read([s.field.as_str()], made, reads);
            read(fields(&s.by), made, reads);
            return false;
        }
        PipeStage::Rare(s) => {
            read([s.field.as_str()], made, reads);
            read(fields(&s.by), made, reads);
            return false;
        }
        PipeStage::Pivot(s) => {
            read(
                aggs_reads(std::slice::from_ref(&s.aggregation)),
                made,
                reads,
            );
            read([s.on_field.as_str()], made, reads);
            read(fields(&s.by), made, reads);
            return false;
        }
        PipeStage::Table(s) => {
            read(fields(&s.fields), made, reads);
            return false;
        }
        PipeStage::Limit(_) | PipeStage::Tail(_) | PipeStage::Sample(_) => {}
        PipeStage::FromSaved(_) => return false,
    }
    true
}

fn fields(names: &[String]) -> impl Iterator<Item = &str> {
    names.iter().map(String::as_str)
}

fn read<'a>(
    names: impl IntoIterator<Item = &'a str>,
    made: &BTreeSet<String>,
    reads: &mut Vec<String>,
) {
    reads.extend(
        names
            .into_iter()
            .map(catalog_key)
            .filter(|key| !made.contains(key)),
    );
}

fn aggs_reads(aggs: &[AggExpr]) -> impl Iterator<Item = &str> {
    aggs.iter()
        .flat_map(|agg| &agg.args)
        .flat_map(|arg| expr_reads(&arg.node))
}

fn expr_reads(expr: &Expr) -> Vec<&str> {
    fn walk<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) {
        match expr {
            Expr::FieldRef(name) => out.push(name),
            Expr::Binary { lhs, rhs, .. } => {
                walk(&lhs.node, out);
                walk(&rhs.node, out);
            }
            Expr::Unary { operand, .. } => walk(&operand.node, out),
            Expr::FunctionCall { args, .. } => {
                for arg in args {
                    walk(&arg.node, out);
                }
            }
            Expr::InList { expr, list } => {
                walk(&expr.node, out);
                for item in list {
                    walk(&item.node, out);
                }
            }
            Expr::Literal(_) => {}
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

fn named_service(query: &Query) -> Option<String> {
    let [group] = query.search.groups.as_slice() else {
        return None;
    };
    group.iter().find_map(|token| match &token.node {
        SearchToken::FieldFilter(filter)
            if filter.field == "service" && filter.op == FilterOp::Eq =>
        {
            match &filter.value {
                FilterValue::Single(Scalar::Exact(value)) => Some(value.clone()),
                FilterValue::Single(Scalar::Glob(_) | Scalar::Regex(_)) | FilterValue::List(_) => {
                    None
                }
            }
        }
        _ => None,
    })
}

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

pub async fn nearest_bounds<B: Backend>(
    backend: &B,
    result: &CapturedResult,
    now: DateTime<Utc>,
) -> String {
    let (query, earliest, latest) = (
        result.query.clone(),
        result.earliest.clone(),
        result.latest.clone(),
    );
    let plan =
        parse_work(move || NearestPlan::of(&query, earliest.as_deref(), latest.as_deref(), now))
            .await;
    nearest_from(backend, plan).await
}

async fn nearest_from<B: Backend>(backend: &B, plan: Option<NearestPlan>) -> String {
    let Some(plan) = plan else {
        return NEAREST_NOT_COMPUTED.to_owned();
    };
    match backend.query_paginated(&plan.dsl, Some(3), None).await {
        Ok(response) => plan.describe(&response.result),
        Err(error) => format!(
            "nearest bounds: not computed; the widened query failed: {}",
            failure_summary(&error)
        ),
    }
}

struct NearestPlan {
    dsl: String,
    from: String,
    earliest: String,
    latest: String,
    to: String,
}

impl NearestPlan {
    fn of(
        query: &str,
        earliest: Option<&str>,
        latest: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        Self::new(query, earliest?, latest?, now)
    }

    fn new(query: &str, earliest: &str, latest: &str, now: DateTime<Utc>) -> Option<Self> {
        let earliest = parse_instant(earliest)?;
        let latest = parse_instant(latest)?;
        let mut query = parse_unbuilt(query).ok()?;
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
        // A widened bound no query can hold builds no plan, so the hint
        // says it was not computed: the search never runs with that side
        // unbounded.
        search.earliest = Some(Spanned::new(TimeBound::read(&from)?, 0..0));
        search.latest = Some(Spanned::new(TimeBound::read(&to)?, 0..0));
        // The aggregate is built from text that holds only the bounds this
        // function formatted, and joins the caller's search as AST: the
        // caller's text reaches the DSL only through the formatter.
        let aggregate = parse_unbuilt(&format!(
            "* | let side = case(_time < \"{earliest}\", \"before\", _time >= \"{latest}\", \
             \"after\", \"inside\") | stats max(_time) as newest, min(_time) as oldest, count() \
             as events by side"
        ))
        .ok()?;
        query.pipeline = aggregate.pipeline;
        let dsl = checked_format_query(&query).ok()?;
        // The rebuilt text must say what was built: the same search terms,
        // the widened bounds, the one aggregate.
        let reparsed = parse_unbuilt(&dsl).ok()?;
        let bounds = (
            reparsed.search.earliest.as_ref().map(|b| b.node.text()),
            reparsed.search.latest.as_ref().map(|b| b.node.text()),
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

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use trawl_core::pattern::thread_builds;

    use super::{NearestPlan, fields_to_check};

    /// `thread_builds` counts per thread and [`crate::blocking::parse_work`]
    /// runs the hints' parses on pool threads, so this calls the functions it
    /// runs on the test's own thread.
    #[test]
    fn hints_build_no_regex_from_stored_text() {
        let search = r"service=api path=/v[0-9]+\.(users|orders)/ host=web-* error";
        let piped = format!(
            r#"{search} | where message matches "time(out)?" | extract "(?P<code>[0-9]{{3}})""#
        );
        for text in [search, piped.as_str()] {
            let before = thread_builds();
            trawl_core::parser::parse(text).expect("the stored text parses");
            assert!(thread_builds() > before, "parsing {text} for a run builds");
        }

        let now: DateTime<Utc> = "2026-03-14T10:00:00Z".parse().expect("an instant");
        let before = thread_builds();
        let checks = fields_to_check(&piped);
        let plan = NearestPlan::of(
            search,
            Some("2026-03-14T03:00:00.250000Z"),
            Some("2026-03-14T04:00:00.750000Z"),
            now,
        );
        assert_eq!(thread_builds(), before, "the hints built a regex");
        assert!(checks.is_some_and(|(_, fields)| fields.contains(&"path".to_owned())));
        assert!(plan.is_some());
    }

    /// The current-thread runtime polls the handler on the test's thread, so
    /// that thread is the reactor.
    #[tokio::test]
    async fn query_advice_builds_no_regex_and_runs_off_the_reactor() {
        use serde_json::json;
        use trawl_api::value::ColumnType;

        use crate::blocking::record;
        use crate::support::{Fake, RESULT_ID, capabilities, captured, no_values, ok, page, s};
        use crate::tools::search;

        let text = r"-service=coredns path=/v[0-9]+\.(users|orders)/";
        let before = thread_builds();
        let advice = super::advice(text);
        assert_eq!(thread_builds(), before, "the advice built a regex");
        assert_eq!(advice.len(), 1, "{advice:?}");

        let reactor = std::thread::current().id();
        let validates = Fake {
            validate: ok(serde_json::from_value(json!({"valid": true, "errors": []})).unwrap()),
            ..Fake::default()
        };
        let input = serde_json::from_value(json!({"mode": "validate", "query": text})).unwrap();
        let now: DateTime<Utc> = "2026-03-14T10:00:00Z".parse().expect("an instant");
        let (result, threads) = record::threads(search::run(
            &validates,
            &crate::session::Seen::default(),
            input,
            now,
        ))
        .await;
        let mirror = result.structured_content.expect("a mirror");
        assert_eq!(mirror["hints"], json!(advice));
        assert_eq!(threads.len(), 1, "{threads:?}");
        assert!(
            threads.iter().all(|thread| *thread != reactor),
            "{threads:?}"
        );

        let capture = captured(
            text,
            &[("_time", ColumnType::Timestamp)],
            vec![no_values(1)],
            1,
            1,
        );
        let executes = Fake {
            capture: ok(trawl_client::CaptureResponse {
                result: capture.clone(),
                execution: None,
                activity: None,
                step_seq: None,
            }),
            result_page: ok(page(&capture, vec![vec![s("2026-03-14T03:59:00Z")]], 0)),
            capabilities: ok(capabilities(None)),
            ..Fake::default()
        };
        let input = serde_json::from_value(json!({"mode": "execute", "query": text})).unwrap();
        let (result, threads) = record::threads(search::run(
            &executes,
            &crate::session::Seen::default(),
            input,
            now,
        ))
        .await;
        let mirror = result.structured_content.expect("a mirror");
        assert_eq!(mirror["hints"][0], json!(advice[0]), "{mirror}");
        assert_eq!(mirror["result"]["result_id"], json!(RESULT_ID));
        assert!(!threads.is_empty());
        assert!(
            threads.iter().all(|thread| *thread != reactor),
            "{threads:?}"
        );
    }

    const QUARTER_STACK: usize = 512 * 1024;

    const LIMIT: usize = 64;

    fn chain(n: usize) -> String {
        vec!["1"; n].join(" + ")
    }

    fn comparisons(n: usize, op: &str) -> String {
        (0..n)
            .map(|i| format!("a == {i}"))
            .collect::<Vec<_>>()
            .join(&format!(" {op} "))
    }

    fn calls(function: &str, n: usize, body: &str) -> String {
        format!(
            "{}{body}{}",
            format!("{function}(").repeat(n),
            ")".repeat(n)
        )
    }

    /// The hints parse on `parse_work`'s pool threads, whose stacks are 2 MiB.
    ///
    /// `where` after `extract kv` is parsed, but `stage_reads` stops at
    /// the kv extract, whose outputs the data names, so `expr_reads` does
    /// not walk that condition.
    #[test]
    fn hint_walks_at_the_tree_depth_limit_fit_a_quarter_stack() {
        let expressions = [
            format!("* | where {} > 0", chain(LIMIT)),
            format!("* | where {}", comparisons(LIMIT, "or")),
            format!("* | where {}", comparisons(LIMIT, "and")),
            format!("* | where {}a < 2", "-".repeat(LIMIT - 1)),
            format!("* | where {}a == 1", "not ".repeat(LIMIT - 1)),
            format!("* | where {} in (64)", chain(LIMIT)),
            format!("* | where a in (1, {})", chain(LIMIT)),
            format!("* | let x = {}", calls("abs", 16, &chain(LIMIT - 15))),
            format!("* | let x = {}", calls("coalesce", 16, &chain(LIMIT - 15))),
            format!(
                "* | where {} > 0",
                calls("coalesce", 16, &chain(LIMIT - 16))
            ),
            format!("* | let x = {}", chain(LIMIT + 1)),
            format!("* | stats sum({})", chain(LIMIT + 1)),
            format!("* | timechart span=1m sum({})", chain(LIMIT + 1)),
            format!("* | eventstats sum({}) as x", chain(LIMIT + 1)),
            format!("* | pivot sum({}) on a", chain(LIMIT + 1)),
            format!("* | extract kv | where {}", comparisons(LIMIT, "or")),
        ];
        let searches = [
            format!("{}a=1", "NOT ".repeat(LIMIT)),
            format!("{}a=1{}", "(".repeat(LIMIT), " )".repeat(LIMIT)),
            format!("{}a=1{}", "NOT (".repeat(LIMIT / 2), " )".repeat(LIMIT / 2)),
        ];
        let now: DateTime<Utc> = "2026-03-14T10:00:00Z".parse().expect("an instant");

        std::thread::Builder::new()
            .name("quarter-stack".to_owned())
            .stack_size(QUARTER_STACK)
            .spawn(move || {
                for dsl in &expressions {
                    eprintln!("fields_to_check: {dsl}");
                    assert!(fields_to_check(dsl).is_some(), "{dsl} must parse");
                }
                for dsl in &searches {
                    eprintln!("NearestPlan::of: {dsl}");
                    let plan = NearestPlan::of(
                        dsl,
                        Some("2026-03-14T03:00:00Z"),
                        Some("2026-03-14T04:00:00Z"),
                        now,
                    );
                    // A plan exists only once the formatted text reparsed
                    // and compared equal through `search_terms`.
                    assert!(plan.is_some(), "{dsl} must build a plan");
                }
            })
            .expect("spawn")
            .join()
            .expect("every hint walk must fit 512 KiB at depth 64");
    }

    /// A query over the 64-level limit gets no query-derived hint, and the
    /// hints never walk its tree: both entry points start with
    /// `parse_unbuilt(…).ok()?`, which refuses the text before any walk.
    ///
    /// The 65-level `where` and `NOT` run show the boundary. The same
    /// shapes thousands of levels deep show that nothing walks the tree:
    /// on a quarter of a worker's stack, any recursive walk of a 32 000-term
    /// chain or a 16 000-`NOT` run would overflow and abort the test
    /// binary, where the refusal costs heap.
    #[test]
    fn an_over_deep_query_gets_no_hint() {
        let deep_where = format!("* | where {}", comparisons(LIMIT + 1, "or"));
        let deep_not = format!("{}a=1", "NOT ".repeat(LIMIT + 1));
        // No spaces, so 32 000 terms fit the 64 KiB text limit.
        let long_where = format!("* | where {} > 0", vec!["1"; 32_000].join("+"));
        let long_not = format!("{}a=1", "NOT ".repeat(16_000));
        let now: DateTime<Utc> = "2026-03-14T10:00:00Z".parse().expect("an instant");

        std::thread::Builder::new()
            .name("quarter-stack".to_owned())
            .stack_size(QUARTER_STACK)
            .spawn(move || {
                for dsl in [&deep_where, &deep_not, &long_where, &long_not] {
                    let refusal = trawl_core::parser::parse_unbuilt(dsl)
                        .expect_err("the parser refuses a tree over the limit");
                    assert!(
                        refusal
                            .iter()
                            .any(|error| error.message.ends_with("is deeper than 64 levels")),
                        "the refusal is the depth refusal: {:?}",
                        refusal.iter().map(|e| &e.message).collect::<Vec<_>>()
                    );
                    assert!(fields_to_check(dsl).is_none(), "no field hint");
                    let plan = NearestPlan::of(
                        dsl,
                        Some("2026-03-14T03:00:00Z"),
                        Some("2026-03-14T04:00:00Z"),
                        now,
                    );
                    assert!(plan.is_none(), "no nearest-bounds plan");
                }
            })
            .expect("spawn")
            .join()
            .expect("an over-deep query is refused before any hint walk");
    }
}
