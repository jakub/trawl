// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for the ingest preview, `POST /api/v1/ingest/preview`
//! (ADR-0049).
//!
//! A preview canonicalizes a sample exactly as `POST /api/v1/ingest` would
//! and stores nothing of it. The response reports every input position in
//! input order: an accepted event with its canonical form, repair codes and
//! lineage, or a rejected one with the same `reason` and `message` real
//! ingest gives. Codes travel as their wire strings (`env.defaulted`,
//! `missing_service`), the spellings the events reference documents.

use std::net::Ipv4Addr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// The most events one preview reads. A sample with more non-blank
/// positions, valid or not, is refused whole with `413 preview_too_large`.
pub const MAX_PREVIEW_EVENTS: usize = 500;

/// The peer a preview canonicalizes against when the request names none:
/// `192.0.2.1`, from the RFC 5737 documentation range, so a filled `host`
/// can never be mistaken for a real sender.
pub const PLACEHOLDER_PEER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// The whole preview report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewResponse {
    /// The producer profile the sample was read as. Always `"http"`.
    pub producer: String,
    /// The peer the sample was canonicalized against.
    pub peer: PreviewPeer,
    /// The one arrival time every event shares, RFC 3339 UTC at microsecond
    /// precision: what `_ingested` holds and what fills a missing `_time`.
    pub arrival: String,
    /// The effective derivation lists the HTTP producer applies.
    pub derivation: PreviewDerivation,
    /// Accepted positions.
    pub accepted: usize,
    /// Rejected positions, parse failures included.
    pub rejected: usize,
    /// One entry per non-blank input position, in input order.
    pub events: Vec<PreviewEvent>,
}

/// The peer address a preview used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewPeer {
    /// The canonical address: the given `peer_ip`, or [`PLACEHOLDER_PEER`].
    pub ip: String,
    /// Whether the request named the peer. When false, a `host` filled from
    /// the peer depends on the real sender's address.
    pub given: bool,
    /// Whether the address sits inside the server's `trusted_relays`, which
    /// turns a missing `host` into a rejection instead of a fill.
    pub trusted_relay: bool,
}

/// The HTTP producer's effective source lists, in the order the
/// canonicalizer consults them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewDerivation {
    /// Fields consulted for `_time`; the first present one decides.
    pub time_from: Vec<String>,
    /// Fields consulted for `_severity`; the first mappable one decides.
    pub severity_from: Vec<SeveritySourceSpec>,
}

/// One configured severity source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeveritySourceSpec {
    /// The wire key read.
    pub field: String,
    /// The dialect its numerics are read in.
    pub dialect: String,
}

/// What became of one input position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PreviewEvent {
    /// The event would be stored as `event`.
    Accepted {
        /// The position, under the same rule as `IngestEventError::index`.
        index: usize,
        /// The parsed input object.
        input: Value,
        /// The canonical event, `_ingested` included.
        event: Map<String, Value>,
        /// Repair codes, in application order (the event's `_repairs`).
        repairs: Vec<String>,
        /// Where derived values came from and what happened to fields.
        lineage: PreviewLineage,
        /// The event had no `host`, the request named no peer, and so the
        /// stored `host` would come from the real sender's address.
        host_depends_on_sender: bool,
    },
    /// The event would be refused. No canonical event is reported.
    Rejected {
        /// The position, under the same rule as `IngestEventError::index`.
        index: usize,
        /// The parsed input, absent when the position was not valid JSON.
        /// A non-object element, JSON `null` included, is present.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present"
        )]
        input: Option<Value>,
        /// The reject reason's wire code, as on `IngestEventError`.
        reason: String,
        /// The message real ingest gives for the same event.
        message: String,
        /// The event had no `host` and the request named no peer, so with
        /// a different sender the outcome could differ.
        host_depends_on_sender: bool,
    },
}

/// A present field is `Some`, even when it holds `null`; an absent one
/// falls to the `default`. Plain `Option<Value>` would read `null` as
/// absent and turn a `null` array element into invalid JSON.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// An accepted event's lineage, as the canonicalizer recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewLineage {
    /// Where `_time` came from.
    pub time: TimeLineage,
    /// Where `_severity` came from, or why it is absent.
    pub severity: SeverityLineage,
    /// Field changes in decision order, one per step.
    pub fields: Vec<FieldChangeWire>,
}

/// Where `_time` came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum TimeLineage {
    /// The first present `time_from` source parsed.
    Field {
        /// The source field.
        field: String,
    },
    /// The arrival time filled `_time` (`time.from_ingest`).
    Arrival {
        /// The first present source when it failed to parse; absent when no
        /// source was present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unparseable: Option<String>,
    },
}

/// Where `_severity` came from, or why it is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SeverityLineage {
    /// `field` mapped and fed `_severity`.
    Field {
        /// The source field.
        field: String,
        /// Earlier sources that were present but mapped to nothing, in
        /// consultation order.
        skipped_unmappable: Vec<String>,
    },
    /// No `severity_from` source was present.
    Missing,
    /// Sources were present and none mapped, so `_severity` is absent.
    Unmapped {
        /// The present sources, in consultation order.
        sources: Vec<String>,
    },
}

/// One field-level decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChangeWire {
    /// The field as the deciding step saw it.
    pub field: String,
    /// What the step did.
    pub change: FieldChangeKind,
    /// The new name, for a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The repair code the change earned. Absent for stringification,
    /// which is canonicalization rather than a repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// What a field-level step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldChangeKind {
    /// The value moved to `to`.
    Renamed,
    /// The field is gone (its value stays in `_raw`).
    Dropped,
    /// The value was cut to a length cap.
    Truncated,
    /// An object or array value became its JSON text.
    Stringified,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap()
    }

    fn accepted() -> PreviewEvent {
        PreviewEvent::Accepted {
            index: 2,
            input: json!({"service": "api", "Level": "warn"}),
            event: json!({"service": "api", "level": "warn", "_severity": 13})
                .as_object()
                .unwrap()
                .clone(),
            repairs: vec!["env.defaulted".into(), "field.name_case_folded".into()],
            lineage: PreviewLineage {
                time: TimeLineage::Arrival { unparseable: None },
                severity: SeverityLineage::Field {
                    field: "level".into(),
                    skipped_unmappable: vec!["severity".into()],
                },
                fields: vec![
                    FieldChangeWire {
                        field: "Level".into(),
                        change: FieldChangeKind::Renamed,
                        to: Some("level".into()),
                        code: Some("field.name_case_folded".into()),
                    },
                    FieldChangeWire {
                        field: "ctx".into(),
                        change: FieldChangeKind::Stringified,
                        to: None,
                        code: None,
                    },
                ],
            },
            host_depends_on_sender: true,
        }
    }

    #[test]
    fn constants_hold_the_documented_values() {
        assert_eq!(MAX_PREVIEW_EVENTS, 500);
        assert_eq!(PLACEHOLDER_PEER.to_string(), "192.0.2.1");
        assert!(PLACEHOLDER_PEER.is_documentation());
    }

    #[test]
    fn accepted_event_is_tagged_by_outcome_and_skips_absent_options() {
        let wire = serde_json::to_value(accepted()).unwrap();
        assert_eq!(wire["outcome"], "accepted");
        assert_eq!(wire["lineage"]["time"], json!({"from": "arrival"}));
        assert_eq!(
            wire["lineage"]["severity"],
            json!({"from": "field", "field": "level", "skipped_unmappable": ["severity"]})
        );
        assert_eq!(
            wire["lineage"]["fields"],
            json!([
                {"field": "Level", "change": "renamed", "to": "level",
                 "code": "field.name_case_folded"},
                {"field": "ctx", "change": "stringified"},
            ])
        );
        assert_eq!(round_trip(&accepted()), accepted());
    }

    #[test]
    fn rejected_event_omits_input_only_when_it_was_not_json() {
        let invalid = PreviewEvent::Rejected {
            index: 4,
            input: None,
            reason: "invalid_json".into(),
            message: "invalid JSON: expected value at line 1 column 1".into(),
            host_depends_on_sender: false,
        };
        assert_eq!(
            serde_json::to_value(&invalid).unwrap(),
            json!({
                "outcome": "rejected",
                "index": 4,
                "reason": "invalid_json",
                "message": "invalid JSON: expected value at line 1 column 1",
                "host_depends_on_sender": false,
            })
        );
        assert_eq!(round_trip(&invalid), invalid);

        // A `null` array element is present input, not a parse failure.
        let null_element = PreviewEvent::Rejected {
            index: 0,
            input: Some(Value::Null),
            reason: "not_object".into(),
            message: "expected JSON object".into(),
            host_depends_on_sender: false,
        };
        let wire = serde_json::to_value(&null_element).unwrap();
        assert_eq!(wire.get("input"), Some(&Value::Null));
        assert_eq!(round_trip(&null_element), null_element);
    }

    #[test]
    fn lineage_tags_round_trip() {
        for time in [
            TimeLineage::Field {
                field: "timestamp".into(),
            },
            TimeLineage::Arrival {
                unparseable: Some("ts".into()),
            },
            TimeLineage::Arrival { unparseable: None },
        ] {
            assert_eq!(round_trip(&time), time);
        }
        assert_eq!(
            serde_json::to_value(TimeLineage::Field {
                field: "timestamp".into()
            })
            .unwrap(),
            json!({"from": "field", "field": "timestamp"})
        );
        for severity in [
            SeverityLineage::Missing,
            SeverityLineage::Unmapped {
                sources: vec!["level".into()],
            },
        ] {
            assert_eq!(round_trip(&severity), severity);
        }
        assert_eq!(
            serde_json::to_value(SeverityLineage::Missing).unwrap(),
            json!({"from": "missing"})
        );
    }

    /// The internal tag makes serde buffer each variant before decoding
    /// it; numbers inside the event must survive that exactly.
    #[test]
    fn large_numbers_inside_an_event_survive_the_tagged_round_trip() {
        let numbers = json!({
            "max_u64": u64::MAX,
            "min_i64": i64::MIN,
            "tenth": 0.1,
            "huge": 1e300,
            "tiny": 5e-324,
            "precise": 123_456_789.123_456_78,
        });
        let event = PreviewEvent::Accepted {
            index: 0,
            input: numbers.clone(),
            event: numbers.as_object().unwrap().clone(),
            repairs: Vec::new(),
            lineage: PreviewLineage {
                time: TimeLineage::Arrival { unparseable: None },
                severity: SeverityLineage::Missing,
                fields: Vec::new(),
            },
            host_depends_on_sender: false,
        };
        let text = serde_json::to_string(&event).unwrap();
        let back: PreviewEvent = serde_json::from_str(&text).unwrap();
        assert_eq!(back, event);
        let PreviewEvent::Accepted { event: map, .. } = back else {
            panic!("outcome changed");
        };
        assert_eq!(map["max_u64"].as_u64(), Some(u64::MAX));
        assert_eq!(map["min_i64"].as_i64(), Some(i64::MIN));
        assert_eq!(map["tenth"].as_f64(), Some(0.1));
    }

    #[test]
    fn whole_response_round_trips() {
        let response = PreviewResponse {
            producer: "http".into(),
            peer: PreviewPeer {
                ip: PLACEHOLDER_PEER.to_string(),
                given: false,
                trusted_relay: false,
            },
            arrival: "2026-10-01T00:00:00.000000Z".into(),
            derivation: PreviewDerivation {
                time_from: vec!["_time".into(), "timestamp".into()],
                severity_from: vec![SeveritySourceSpec {
                    field: "level".into(),
                    dialect: "otel".into(),
                }],
            },
            accepted: 1,
            rejected: 0,
            events: vec![accepted()],
        };
        let wire = serde_json::to_value(&response).unwrap();
        assert_eq!(
            wire["peer"],
            json!({"ip": "192.0.2.1", "given": false, "trusted_relay": false})
        );
        assert_eq!(
            wire["derivation"]["severity_from"],
            json!([{"field": "level", "dialect": "otel"}])
        );
        assert_eq!(round_trip(&response), response);
    }
}
