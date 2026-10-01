// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/ingest/preview`: canonicalize a sample exactly as
//! `POST /api/v1/ingest` would, and store nothing of it (ADR-0049).
//!
//! The preview is a second consumer of the one parse-and-canonicalize step
//! in [`super::body`]. It builds the same [`HttpRequestContext`] real ingest
//! builds, from the given `peer_ip` or the documentation placeholder, and
//! turns each visited outcome into its wire report, in input order.
//!
//! What it never does is the point of the route: it takes no free-space,
//! reservation or publication step, writes no WAL, publishes nothing,
//! touches no counter or metric label, and logs nothing. Reject messages
//! quote sample values, so they go to the caller and nowhere else.
//! `tests/ingest_preview_touches_nothing.rs` holds this file to that.

use std::net::IpAddr;
use std::ops::ControlFlow;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use fleet_auth::VerifiedKey;
use serde::Deserialize;
use trawl_api::ingest_preview::{
    FieldChangeKind as WireChangeKind, FieldChangeWire, MAX_PREVIEW_EVENTS, PLACEHOLDER_PEER,
    PreviewDerivation, PreviewEvent, PreviewLineage, PreviewPeer, PreviewResponse, SeverityLineage,
    SeveritySourceSpec, TimeLineage,
};

use crate::error::ServerError;
use crate::ingest::body::{EventOutcome, EventReport, HttpRequestContext, visit_events};
use crate::ingest::envelope::{
    Canonical, FieldChange, FieldChangeKind, SeveritySource, TimeSource,
};
use crate::ingest::producer::{Derivation, ProducerKind};
use crate::policy::{Permission, TrawlAuthz as _};
use crate::state::AppState;

/// The preview's query string. Unknown parameters are refused, so a
/// misspelled `peer_ip` cannot silently fall back to the placeholder.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewParams {
    /// The sender's address as trawld would see it.
    pub peer_ip: Option<String>,
}

/// `POST /api/v1/ingest/preview` — the per-event report for a sample.
///
/// Refusals, in the order they are checked: no `server_manage` is 403; a
/// `Content-Encoding` other than `identity` is 415, before the body is
/// read as anything; a bad query string or `peer_ip` is 400; a framing
/// error is 400 (the shared step's own); more than
/// [`MAX_PREVIEW_EVENTS`] non-blank positions is 413 `preview_too_large`.
/// An all-rejected sample is a 200.
pub async fn preview(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    headers: HeaderMap,
    params: Result<Query<PreviewParams>, QueryRejection>,
    body: Bytes,
) -> Result<Json<PreviewResponse>, ServerError> {
    if !verified.has_permission(Permission::ServerManage) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    if !identity_encoded(&headers) {
        return Err(ServerError::UnsupportedEncoding);
    }
    let Query(params) =
        params.map_err(|_| ServerError::BadRequest("invalid preview parameters".into()))?;
    let (peer, given) = match params.peer_ip.as_deref() {
        None => (IpAddr::V4(PLACEHOLDER_PEER), false),
        // The value is the caller's own, but a 400 never echoes it.
        Some(text) => (
            text.parse::<IpAddr>()
                .map_err(|_| ServerError::BadRequest("peer_ip is not an IP address".into()))?,
            true,
        ),
    };

    // The same context real ingest builds: one arrival instant, the peer
    // through `canonical_peer`, relay membership from server config.
    let request = HttpRequestContext::new(&state.ingest, peer);
    tokio::task::spawn_blocking(move || report(&request, given, &body))
        .await
        .map_err(|e| ServerError::from_join("ingest preview", e))?
        .map(Json)
}

/// Whether every `Content-Encoding` coding is `identity` (or there is
/// none). Anything else is refused, never decoded.
fn identity_encoded(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value.to_str().is_ok_and(|text| {
                text.split(',')
                    .all(|coding| coding.trim().eq_ignore_ascii_case("identity"))
            })
        })
}

/// Visit the sample and build the report, refusing it whole past the
/// position limit.
fn report(
    request: &HttpRequestContext,
    given: bool,
    body: &[u8],
) -> Result<PreviewResponse, ServerError> {
    let ctx = request.envelope();
    let derivation = request.derivation();
    let mut events = Vec::new();
    let mut accepted = 0;
    let mut rejected = 0;
    let flow = visit_events(body, &ctx, |position| {
        // Count visits, not indices: blank lines are not positions, and
        // an invalid non-blank line is one.
        if events.len() == MAX_PREVIEW_EVENTS {
            return ControlFlow::Break(());
        }
        let event = wire_event(position, given, derivation);
        match event {
            PreviewEvent::Accepted { .. } => accepted += 1,
            PreviewEvent::Rejected { .. } => rejected += 1,
        }
        events.push(event);
        ControlFlow::Continue(())
    })?;
    if flow.is_break() {
        return Err(ServerError::PreviewTooLarge {
            limit: MAX_PREVIEW_EVENTS,
        });
    }
    Ok(PreviewResponse {
        producer: "http".to_owned(),
        peer: PreviewPeer {
            ip: request.peer_host().to_owned(),
            given,
            trusted_relay: request.peer_is_trusted_relay(),
        },
        arrival: request.arrival().to_owned(),
        derivation: wire_derivation(derivation),
        accepted,
        rejected,
        events,
    })
}

/// The HTTP profile's effective lists, in the order the canonicalizer
/// consults them.
fn wire_derivation(derivation: &Derivation) -> PreviewDerivation {
    PreviewDerivation {
        time_from: derivation
            .time_from(ProducerKind::Http)
            .iter()
            .map(|source| source.field.clone())
            .collect(),
        severity_from: derivation
            .severity_from(ProducerKind::Http)
            .iter()
            .map(|source| SeveritySourceSpec {
                field: source.field.clone(),
                dialect: source.dialect.token().to_owned(),
            })
            .collect(),
    }
}

/// One visited position as its wire report.
fn wire_event(position: EventReport<'_>, given: bool, derivation: &Derivation) -> PreviewEvent {
    let EventReport {
        index,
        input,
        outcome,
    } = position;
    match outcome {
        EventOutcome::Accepted(canonical) => {
            let Canonical {
                obj,
                repairs,
                lineage,
                host_absent,
                ..
            } = canonical;
            PreviewEvent::Accepted {
                index,
                // An accepted position always parsed as an object.
                input: input.cloned().unwrap_or_default(),
                event: obj,
                repairs: repairs
                    .iter()
                    .map(|code| code.as_str().to_owned())
                    .collect(),
                lineage: PreviewLineage {
                    time: wire_time(&lineage.time, derivation),
                    severity: wire_severity(&lineage.severity, derivation),
                    fields: lineage.fields.into_iter().map(wire_field_change).collect(),
                },
                host_depends_on_sender: !given && host_absent,
            }
        }
        EventOutcome::Rejected {
            reason,
            message,
            host_absent,
        } => PreviewEvent::Rejected {
            index,
            input: input.cloned(),
            reason: reason.as_str().to_owned(),
            message,
            host_depends_on_sender: !given && host_absent,
        },
    }
}

fn wire_time(time: &TimeSource, derivation: &Derivation) -> TimeLineage {
    let name = |index: usize| derivation.time_field(ProducerKind::Http, index).to_owned();
    match time {
        TimeSource::Field(index) => TimeLineage::Field {
            field: name(*index),
        },
        TimeSource::Arrival { unparseable } => TimeLineage::Arrival {
            unparseable: unparseable.map(name),
        },
    }
}

fn wire_severity(severity: &SeveritySource, derivation: &Derivation) -> SeverityLineage {
    let name = |index: &usize| {
        derivation
            .severity_field(ProducerKind::Http, *index)
            .to_owned()
    };
    match severity {
        SeveritySource::Field {
            index,
            skipped_unmappable,
        } => SeverityLineage::Field {
            field: name(index),
            skipped_unmappable: skipped_unmappable.iter().map(name).collect(),
        },
        SeveritySource::Missing => SeverityLineage::Missing,
        SeveritySource::Unmapped { sources } => SeverityLineage::Unmapped {
            sources: sources.iter().map(name).collect(),
        },
    }
}

fn wire_field_change(change: FieldChange) -> FieldChangeWire {
    let (kind, to) = match change.kind {
        FieldChangeKind::Renamed { to } => (WireChangeKind::Renamed, Some(to)),
        FieldChangeKind::Dropped => (WireChangeKind::Dropped, None),
        FieldChangeKind::Truncated => (WireChangeKind::Truncated, None),
        FieldChangeKind::Stringified => (WireChangeKind::Stringified, None),
    };
    FieldChangeWire {
        field: change.field,
        change: kind,
        to,
        code: change.code.map(|code| code.as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn with_encoding(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::CONTENT_ENCODING, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn only_identity_or_no_encoding_is_read() {
        assert!(identity_encoded(&HeaderMap::new()));
        assert!(identity_encoded(&with_encoding(&["identity"])));
        assert!(identity_encoded(&with_encoding(&[
            "Identity",
            " identity "
        ])));
        for refused in [
            &["gzip"][..],
            &["deflate"],
            &["br"],
            &["identity, gzip"],
            &["identity", "gzip"],
            &[""],
        ] {
            assert!(!identity_encoded(&with_encoding(refused)), "{refused:?}");
        }
    }
}
