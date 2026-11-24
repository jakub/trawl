// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one parse-and-canonicalize step for an HTTP ingest body (ADR-0049).
//!
//! Index rule, the one documented on `trawl_api::IngestEventError`: an
//! NDJSON position is its physical line number, blank lines included; an
//! array position is the element's place in the array.

use std::net::IpAddr;
use std::ops::ControlFlow;
use std::sync::Arc;

use serde_json::Value;

use crate::error::ServerError;
use crate::ingest::envelope::{self, Canonical, EnvelopeContext, RejectReason, Rejection};
use crate::ingest::producer::{Derivation, Producer};
use crate::state::IngestState;

#[derive(Debug)]
pub(crate) struct HttpRequestContext {
    arrival_instant: chrono::DateTime<chrono::Utc>,
    arrival: String,
    peer_host: String,
    peer_is_trusted_relay: bool,
    envs: Arc<[String]>,
    default_env: Arc<str>,
    derivation: Arc<Derivation>,
}

impl HttpRequestContext {
    pub(crate) fn new(ingest: &IngestState, peer: IpAddr) -> Self {
        let arrival_instant = chrono::Utc::now();
        let peer = crate::syslog::canonical_peer(peer);
        Self {
            arrival_instant,
            arrival: arrival_instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            peer_host: peer.to_string(),
            peer_is_trusted_relay: ingest.trusted_relays.iter().any(|c| c.contains(peer)),
            envs: Arc::clone(&ingest.envs),
            default_env: Arc::clone(&ingest.default_env),
            derivation: Arc::clone(&ingest.derivation),
        }
    }

    pub(crate) fn envelope(&self) -> EnvelopeContext<'_> {
        EnvelopeContext {
            arrival: self.arrival(),
            arrival_instant: self.arrival_instant,
            envs: &self.envs,
            default_env: &self.default_env,
            producer: Producer::Http {
                peer_host: self.peer_host(),
                peer_is_trusted_relay: self.peer_is_trusted_relay(),
            },
            derivation: self.derivation(),
        }
    }

    pub(crate) fn arrival(&self) -> &str {
        &self.arrival
    }

    pub(crate) fn peer_host(&self) -> &str {
        &self.peer_host
    }

    pub(crate) const fn peer_is_trusted_relay(&self) -> bool {
        self.peer_is_trusted_relay
    }

    pub(crate) fn derivation(&self) -> &Derivation {
        &self.derivation
    }
}

#[derive(Debug)]
pub(crate) enum EventOutcome {
    Accepted(Canonical),
    /// `message` may quote a bounded client value, so it belongs in a response
    /// to that client only.
    Rejected {
        reason: RejectReason,
        message: String,
        /// The canonicalizer's `host_absent` for a rejected object; false
        /// for a position that never parsed as an object.
        host_absent: bool,
    },
}

impl From<Rejection> for EventOutcome {
    fn from(rejection: Rejection) -> Self {
        Self::Rejected {
            reason: rejection.reason,
            message: rejection.message,
            host_absent: rejection.host_absent,
        }
    }
}

#[derive(Debug)]
pub(crate) struct EventReport<'i> {
    pub index: usize,
    /// The parsed value at this position. `None` when it was not valid
    /// JSON: the line text is never echoed.
    pub input: Option<&'i Value>,
    pub outcome: EventOutcome,
}

pub(crate) fn visit_events(
    body: &[u8],
    ctx: &EnvelopeContext<'_>,
    mut visit: impl FnMut(EventReport<'_>) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, ServerError> {
    if body.is_empty() {
        return Err(ServerError::Ingest("empty request body".into()));
    }
    let text = std::str::from_utf8(body)
        .map_err(|e| ServerError::Ingest(format!("body is not valid UTF-8: {e}")))?;

    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        visit_json_array(trimmed, ctx, &mut visit)
    } else {
        // The untrimmed text: a leading blank line is a line, and counts.
        Ok(visit_ndjson(text, ctx, &mut visit))
    }
}

fn visit_json_array(
    text: &str,
    ctx: &EnvelopeContext<'_>,
    visit: &mut impl FnMut(EventReport<'_>) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, ServerError> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|e| ServerError::Ingest(format!("invalid JSON array: {e}")))?;
    let arr = parsed
        .as_array()
        .ok_or_else(|| ServerError::Ingest("expected JSON array".into()))?;
    if arr.is_empty() {
        return Err(ServerError::Ingest("empty event array".into()));
    }
    for (index, value) in arr.iter().enumerate() {
        if visit(judge(index, value, ctx)).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn visit_ndjson(
    text: &str,
    ctx: &EnvelopeContext<'_>,
    visit: &mut impl FnMut(EventReport<'_>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => visit(judge(index, &value, ctx))?,
            Err(e) => visit(EventReport {
                index,
                input: None,
                outcome: EventOutcome::Rejected {
                    reason: RejectReason::InvalidJson,
                    message: format!("invalid JSON: {e}"),
                    host_absent: false,
                },
            })?,
        }
    }
    ControlFlow::Continue(())
}

fn judge<'i>(index: usize, value: &'i Value, ctx: &EnvelopeContext<'_>) -> EventReport<'i> {
    let outcome = match value.as_object() {
        Some(obj) => match envelope::canonicalize(obj, ctx) {
            Ok(canonical) => EventOutcome::Accepted(canonical),
            Err(rejection) => rejection.into(),
        },
        None => EventOutcome::Rejected {
            reason: RejectReason::NotObject,
            message: "expected JSON object".into(),
            host_absent: false,
        },
    };
    EventReport {
        index,
        input: Some(value),
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARRIVAL: &str = "2026-01-01T00:00:00.000000Z";

    #[derive(Debug, PartialEq)]
    struct Seen {
        index: usize,
        input: Option<Value>,
        outcome: Result<String, (RejectReason, bool)>,
    }

    fn visit_all(body: &[u8]) -> Result<Vec<Seen>, ServerError> {
        visit_until(body, usize::MAX).map(|(seen, _)| seen)
    }

    fn visit_until(body: &[u8], limit: usize) -> Result<(Vec<Seen>, ControlFlow<()>), ServerError> {
        let envs = vec!["prod".to_owned()];
        let derivation = Derivation::defaults();
        let ctx = EnvelopeContext {
            arrival: ARRIVAL,
            arrival_instant: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            envs: &envs,
            default_env: &envs[0],
            producer: Producer::Http {
                peer_host: "127.0.0.1",
                peer_is_trusted_relay: false,
            },
            derivation: &derivation,
        };
        let mut seen = Vec::new();
        let flow = visit_events(body, &ctx, |report| {
            seen.push(Seen {
                index: report.index,
                input: report.input.cloned(),
                outcome: match report.outcome {
                    EventOutcome::Accepted(c) => Ok(c.service),
                    EventOutcome::Rejected {
                        reason,
                        host_absent,
                        ..
                    } => Err((reason, host_absent)),
                },
            });
            if seen.len() >= limit {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        Ok((seen, flow))
    }

    fn indices(seen: &[Seen]) -> Vec<usize> {
        seen.iter().map(|s| s.index).collect()
    }

    #[test]
    fn ndjson_index_counts_leading_blank_lines() {
        let body = b"\n\n  \n{\"service\":\"a\"}\n\nnot json\n{\"service\":\"b\"}";
        let seen = visit_all(body).unwrap();
        assert_eq!(indices(&seen), [3, 5, 6]);
        assert_eq!(seen[0].outcome, Ok("a".to_owned()));
        assert_eq!(seen[1].outcome, Err((RejectReason::InvalidJson, false)));
        assert_eq!(seen[2].outcome, Ok("b".to_owned()));
    }

    #[test]
    fn ndjson_index_counts_crlf_lines() {
        let body = b"\r\n{\"service\":\"a\"}\r\n\r\n{\"service\":\"b\"}\r\n";
        assert_eq!(indices(&visit_all(body).unwrap()), [1, 3]);
    }

    #[test]
    fn array_index_is_element_position_after_leading_whitespace() {
        let body = b"\n\n  [{\"service\":\"a\"}, 7, {\"service\":\"b\"}]";
        let seen = visit_all(body).unwrap();
        assert_eq!(indices(&seen), [0, 1, 2]);
        assert_eq!(seen[1].outcome, Err((RejectReason::NotObject, false)));
        assert_eq!(seen[1].input, Some(serde_json::json!(7)));
    }

    #[test]
    fn invalid_json_carries_no_input_and_non_object_carries_its_value() {
        let seen = visit_all(b"{\"service\":\nnull\n\"text\"").unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].input, None);
        assert_eq!(seen[0].outcome, Err((RejectReason::InvalidJson, false)));
        assert_eq!(seen[1].input, Some(Value::Null));
        assert_eq!(seen[1].outcome, Err((RejectReason::NotObject, false)));
        assert_eq!(seen[2].input, Some(serde_json::json!("text")));
    }

    #[test]
    fn rejected_object_carries_its_input_and_host_absence() {
        let seen = visit_all(b"{\"message\":\"no service\"}\n{\"host\":\"h\"}").unwrap();
        assert_eq!(
            seen[0].input,
            Some(serde_json::json!({"message": "no service"}))
        );
        assert_eq!(seen[0].outcome, Err((RejectReason::MissingService, true)));
        assert_eq!(seen[1].outcome, Err((RejectReason::MissingService, false)));
    }

    #[test]
    fn outcomes_arrive_in_input_order_across_services() {
        let body = b"{\"service\":\"c\"}\n{\"service\":\"a\"}\n{}\n{\"service\":\"c\"}";
        let seen = visit_all(body).unwrap();
        let order: Vec<_> = seen.iter().map(|s| s.outcome.clone()).collect();
        assert_eq!(
            order,
            [
                Ok("c".to_owned()),
                Ok("a".to_owned()),
                Err((RejectReason::MissingService, true)),
                Ok("c".to_owned()),
            ]
        );
    }

    #[test]
    fn a_break_stops_the_visit() {
        let body = b"{\"service\":\"a\"}\n{\"service\":\"b\"}\n{\"service\":\"c\"}";
        let (seen, flow) = visit_until(body, 2).unwrap();
        assert_eq!(indices(&seen), [0, 1]);
        assert_eq!(flow, ControlFlow::Break(()));

        let body = b"[{\"service\":\"a\"},{\"service\":\"b\"},{\"service\":\"c\"}]";
        let (seen, flow) = visit_until(body, 1).unwrap();
        assert_eq!(indices(&seen), [0]);
        assert_eq!(flow, ControlFlow::Break(()));

        let (_, flow) = visit_until(body, 5).unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
    }

    #[test]
    fn framing_errors_refuse_the_whole_body() {
        for (body, needle) in [
            (b"".as_slice(), "empty request body"),
            (b"\xff\xfe".as_slice(), "not valid UTF-8"),
            (b"[{\"service\":\"a\"}".as_slice(), "invalid JSON array"),
            (b"[]".as_slice(), "empty event array"),
        ] {
            let err = visit_all(body).unwrap_err();
            assert!(err.to_string().contains(needle), "{body:?}: {err}");
        }
    }

    #[test]
    fn whitespace_alone_visits_nothing() {
        assert!(visit_all(b"\n  \n\t\n").unwrap().is_empty());
    }
}
