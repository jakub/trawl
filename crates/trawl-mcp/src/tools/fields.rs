// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `fields`: the fields of one service, or the columns of a captured
//! result, each with where its facts come from (ADR-0058).
//!
//! `source: service` reads two independent records of the service and
//! keeps them apart, because they count different things:
//!
//! - the field catalog scoped to the service (`GET /api/v1/schema/fields`):
//!   the pinned type, whether the pin is degraded, the rows the catalog
//!   observed carrying the field in this service and when it last saw one,
//!   with provenance `catalog_observation`;
//! - the per-service schema the daemon builds from stored parquet footers
//!   (`GET /api/v1/schema/services`): the stored type, the rows and the
//!   null count in those footers, with provenance `parquet_footer`.
//!
//! A field both records know prints twice, catalog first. Either record
//! failing leaves the other, with the outcome naming what failed; both
//! failing is the catalog's refusal.
//!
//! `source: result` reads a zero-row page of the capture for its columns
//! and column facts, with provenance `capture`: nothing runs again.

use rmcp::model::CallToolResult;
use serde_json::json;
use trawl_api::value::field_display_rank;
use trawl_client::types::{ServiceSchema, ServiceSchemaResponse};
use trawl_client::{CapturedResult, CatalogFieldSummary, CatalogFieldsResponse, ClientError};
use trawl_core::ast::{FilterValue, SearchToken};
use trawl_core::format::format_query;
use trawl_core::parser::parse;

use super::orient::object;
use super::schema::{FIELDS, FieldsInput, READ, SEARCH};
use super::{failure_summary, listed_values};
use crate::backend::Backend;
use crate::render::rows::{as_u64, json_string};
use crate::render::{self, Budget, refusal};
use crate::reply::{FieldView, FieldsReply, FieldsSource, NextAction, Provenance};

/// The most catalog fields one `source: service` call reads.
pub const CATALOG_FIELDS_LIMIT: usize = 500;

/// Run `fields` against `backend`.
pub async fn run<B: Backend>(backend: &B, input: FieldsInput) -> CallToolResult {
    match (input.source, input.service, input.result_id) {
        (FieldsSource::Service, Some(service), None) => for_service(backend, &service).await,
        (FieldsSource::Result, None, Some(result_id)) => for_result(backend, &result_id).await,
        (FieldsSource::Service, None, _) => refusal::error_result(
            "fields with source=service needs service, the service name; orient lists them."
                .to_owned(),
        ),
        (FieldsSource::Service, Some(_), Some(_)) => refusal::error_result(
            "fields with source=service takes service, not result_id; for a result's columns \
             call it with source=result and result_id alone."
                .to_owned(),
        ),
        (FieldsSource::Result, None, None) => refusal::error_result(
            "fields with source=result needs result_id, the id search returned.".to_owned(),
        ),
        (FieldsSource::Result, Some(_), _) => refusal::error_result(
            "fields with source=result takes result_id, not service; for a service's fields \
             call it with source=service and service alone."
                .to_owned(),
        ),
    }
}

/// `source: service`.
async fn for_service<B: Backend>(backend: &B, service: &str) -> CallToolResult {
    let (catalog, stored) = tokio::join!(
        backend.catalog_fields(Some(service), None, Some(CATALOG_FIELDS_LIMIT)),
        backend.schema_services()
    );
    if let (Err(error), Err(_)) = (&catalog, &stored) {
        return refusal::refusal(FIELDS, error, None);
    }
    let footer = stored
        .as_ref()
        .ok()
        .and_then(|response| response.services.iter().find(|s| s.name == service));
    let degraded = |name: &str, summary: Option<&CatalogFieldSummary>| {
        summary.is_some_and(|summary| summary.verdict.is_some())
            || footer.is_some_and(|footer| footer.degraded_fields.iter().any(|f| f == name))
    };

    let mut views: Vec<(String, u8, FieldView)> = Vec::new();
    if let Ok(response) = &catalog {
        for summary in &response.fields {
            views.push((
                summary.name.clone(),
                0,
                catalog_view(summary, degraded(&summary.name, Some(summary))),
            ));
        }
    }
    if let Some(footer) = footer {
        for column in &footer.columns {
            let summary = catalog.as_ref().ok().and_then(|response| {
                response
                    .fields
                    .iter()
                    .find(|summary| summary.name == column.name)
            });
            let is_degraded = degraded(&column.name, summary);
            views.push((
                column.name.clone(),
                1,
                FieldView {
                    name: column.name.clone(),
                    position: None,
                    ty: column.data_type.clone(),
                    provenance: Provenance::ParquetFooter,
                    degraded: is_degraded,
                    rows: Some(column.total_count),
                    nulls: Some(column.null_count),
                    distinct: None,
                    last_seen: None,
                    values: Vec::new(),
                },
            ));
        }
    }
    views.sort_by(|a, b| {
        (field_display_rank(&a.0), &a.0, a.1).cmp(&(field_display_rank(&b.0), &b.0, b.1))
    });
    let fields: Vec<FieldView> = views.into_iter().map(|(_, _, view)| view).collect();

    let reply = FieldsReply {
        outcome: service_outcome(service, &catalog, &stored, footer, &fields),
        source: FieldsSource::Service,
        service: Some(service.to_owned()),
        result_id: None,
        fields,
        fields_dropped_to_fit: 0,
        next_actions: service_search(service)
            .map(|query| NextAction {
                tool: SEARCH.to_owned(),
                arguments: object(json!({"mode": "execute", "query": query})),
                reason: "read the service's events of the last hour".to_owned(),
            })
            .into_iter()
            .collect(),
    };
    render::fields(reply, Budget::default())
}

/// One catalog summary as a field view.
fn catalog_view(summary: &CatalogFieldSummary, degraded: bool) -> FieldView {
    FieldView {
        name: summary.name.clone(),
        position: None,
        ty: summary.data_type.clone(),
        provenance: Provenance::CatalogObservation,
        degraded,
        rows: Some(summary.row_count),
        nulls: None,
        distinct: None,
        last_seen: summary.last_seen.clone(),
        values: Vec::new(),
    }
}

/// The outcome line of `source: service`: what each record holds, and
/// which one could not be read.
fn service_outcome(
    service: &str,
    catalog: &Result<CatalogFieldsResponse, ClientError>,
    stored: &Result<ServiceSchemaResponse, ClientError>,
    footer: Option<&ServiceSchema>,
    fields: &[FieldView],
) -> String {
    let name = json_string(service);
    let mut parts = vec![format!("Fields of service {name}.")];
    match catalog {
        Ok(response) if response.fields.is_empty() => {
            parts.push("The field catalog never observed a field in this service.".to_owned());
        }
        Ok(response) => {
            parts.push(format!(
                "The field catalog observed {} in it{}.",
                counted(as_u64(response.fields.len()), "field"),
                if response.truncated {
                    format!("; it holds more than the {CATALOG_FIELDS_LIMIT} read here")
                } else {
                    String::new()
                }
            ));
        }
        Err(error) => parts.push(format!(
            "The field catalog could not be read: {}.",
            failure_summary(error)
        )),
    }
    match (stored, footer) {
        (Ok(_), Some(footer)) => parts.push(format!(
            "Its stored parquet footers hold {} over {} in {}.",
            counted(as_u64(footer.columns.len()), "column"),
            counted(footer.total_events, "event"),
            counted(footer.file_count, "file")
        )),
        (Ok(_), None) => parts.push("No stored parquet file belongs to it.".to_owned()),
        (Err(error), _) => parts.push(format!(
            "The stored schema could not be read: {}.",
            failure_summary(error)
        )),
    }
    if fields.is_empty() {
        parts.push("trawld knows no field of this service; orient lists the services.".to_owned());
    }
    parts.join(" ")
}

/// `n` and `noun`, the noun plural unless `n` is 1.
fn counted(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `service=<service> last=1h`, built from the parsed template so the
/// service name is quoted the way the DSL formatter quotes it; `None`
/// when the rebuilt text does not name exactly that service.
fn service_search(service: &str) -> Option<String> {
    let mut query = parse("service=\"placeholder\" last=1h").ok()?;
    let token = query.search.groups.first_mut()?.first_mut()?;
    let SearchToken::FieldFilter(filter) = &mut token.node else {
        return None;
    };
    filter.value = FilterValue::Literal(service.to_owned());
    let text = format_query(&query);
    let reparsed = parse(&text).ok()?;
    let [group] = reparsed.search.groups.as_slice() else {
        return None;
    };
    match group.as_slice() {
        [token] => match &token.node {
            SearchToken::FieldFilter(filter)
                if filter.field == "service"
                    && filter.value == FilterValue::Literal(service.to_owned()) =>
            {
                Some(text)
            }
            _ => None,
        },
        _ => None,
    }
}

/// `source: result`.
async fn for_result<B: Backend>(backend: &B, result_id: &str) -> CallToolResult {
    let result = match backend.result_page(result_id, 0, 0, None).await {
        Ok(page) => page.result,
        Err(error) => return refusal::refusal(FIELDS, &error, None),
    };
    let fields = result_views(&result);
    let reply = FieldsReply {
        outcome: format!(
            "Columns of result {}: {} over its {}.",
            json_string(&result.id),
            counted(as_u64(result.columns.len()), "column"),
            counted(result.captured_rows, "captured row")
        ),
        source: FieldsSource::Result,
        service: None,
        result_id: Some(result.id.clone()),
        fields,
        fields_dropped_to_fit: 0,
        next_actions: if result.captured_rows > 0 {
            vec![NextAction {
                tool: READ.to_owned(),
                arguments: object(json!({
                    "action": "page",
                    "result_id": result.id,
                    "offset": 0,
                })),
                reason: "print the captured rows".to_owned(),
            }]
        } else {
            Vec::new()
        },
    };
    render::fields(reply, Budget::default())
}

/// A capture's columns, in position order, with the capture's facts.
fn result_views(result: &CapturedResult) -> Vec<FieldView> {
    result
        .columns
        .iter()
        .zip(&result.column_facts)
        .enumerate()
        .map(|(position, (column, facts))| FieldView {
            name: column.name.clone(),
            position: Some(as_u64(position)),
            ty: column.ty.to_string(),
            provenance: Provenance::Capture,
            degraded: false,
            rows: Some(result.captured_rows),
            nulls: Some(facts.nulls),
            distinct: (!facts.distinct_capped).then_some(facts.distinct),
            last_seen: None,
            values: facts
                .values
                .as_deref()
                .and_then(|values| listed_values(column, values))
                .unwrap_or_default(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_search_quotes_the_name() {
        assert_eq!(
            service_search("api").as_deref(),
            Some("service=api last=1h")
        );
        let hostile = service_search("a\" OR 1=1 | rm \\").unwrap();
        let reparsed = parse(&hostile).unwrap();
        assert!(reparsed.pipeline.is_empty(), "{hostile}");
    }
}
