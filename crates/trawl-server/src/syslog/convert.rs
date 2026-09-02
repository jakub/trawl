// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a parsed syslog frame into a payload the one canonicalizer admits.
//!
//! This module builds no envelope. It owns exactly what the transport
//! proves: the service the frame belongs to, the host it came from, the
//! message body, and the parse artifacts (`syslog_severity`,
//! `syslog_timestamp`, `syslog_facility`, `syslog_pid`, `syslog_msgid`,
//! `syslog_source_ip`, the `sd_*` structured-data pairs). It hands them
//! to [`crate::ingest::envelope::canonicalize`] as an ordinary payload
//! under the `syslog` profile (ADR-0013).
//!
//! Two consequences are the whole point:
//!
//! - the listener writes no `_severity`. It publishes the raw 0-7 PRI
//!   numeral as an ordinary column and the syslog profile's fixed
//!   derivation source reads it back with `dialect = "syslog"`. What
//!   licenses the inversion is the config declaring that dialect, not the
//!   writer's privilege, so a syslog-over-HTTP forwarder reaches it
//!   through the same mechanism;
//! - every universal gate applies to syslog: the ASCII fold, the
//!   sealed-prefix strip, the field-name length drop, nested
//!   stringification, the `_raw` cap and `_repairs` assembly. Only the
//!   frame-level caps ([`MAX_SD_ELEMENTS`], [`MAX_SD_PARAMS_TOTAL`]) live
//!   here, because they bound work done before there is a payload at all.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{Map, Value, json};
use syslog_loose::Message;

use super::batch::SyslogEvent;
use super::{CidrEntry, parse};
use crate::ingest::envelope::{self, EnvelopeContext, RepairCode};
use crate::ingest::pipeline;
use crate::ingest::producer::{
    self, Asserted, Derivation, Producer, ProducerKind, SYSLOG_SEVERITY_FIELD,
    SYSLOG_TIMESTAMP_FIELD,
};

/// Maximum number of RFC 5424 structured data elements to extract.
const MAX_SD_ELEMENTS: usize = 32;
/// Maximum total structured data params across all elements.
const MAX_SD_PARAMS_TOTAL: usize = 128;

/// What the syslog transport proves about one frame, ready for the door.
///
/// The identity fields are assertions (the profile is the authority on
/// them); `map` holds only sender-visible payload, artifacts and
/// structured data, and carries no envelope field. `_raw` is the one
/// exception, and it is a proposal rather than an envelope write: the
/// pre-parse wire line is the most original form of a syslog event that
/// exists, and the door caps it like any other.
#[derive(Debug)]
pub struct SyslogPayload {
    /// The service this frame belongs to (see [`derive_service`]).
    pub service: String,
    /// The frame's hostname, else the peer address, else `None` — a
    /// hostname-less frame behind a trusted relay is kept with `host`
    /// absent rather than stamped with the relay's address.
    pub host: Option<String>,
    /// The parsed message body.
    pub message: String,
    /// The payload the door canonicalizes: `_raw`, the `syslog_*`
    /// artifacts and the `sd_*` pairs.
    pub map: Map<String, Value>,
    /// Codes the producer contributed. The door assembles `_repairs`.
    pub repairs: Vec<RepairCode>,
}

/// Derive the service name for a syslog frame, and the repair it earned.
///
/// Priority (ADR-0013):
/// 1. `source_service_map` lookup by source IP: explicit operator config,
///    boot-validated as a service name, used verbatim;
/// 2. a valid APP-NAME / tag from the frame, verbatim;
/// 3. `default_service` (also boot-validated), with `service.from_profile`
///    when an APP-NAME was present but unusable, and no repair at all when
///    the frame simply carried none.
///
/// An unusable APP-NAME is never sanitized: rewriting `Living Room AP` to
/// `LivingRoomAP` would invent a service the sender never named and file
/// data under it silently. The configured default plus a repair code is
/// the honest answer, and the original APP-NAME stays findable in `_raw`.
pub fn derive_service<S: BuildHasher>(
    source_ip: IpAddr,
    appname: Option<&str>,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> (String, Option<RepairCode>) {
    if let Some(mapped) = source_service_map.get(&source_ip.to_string()) {
        return (mapped.clone(), None);
    }
    match appname {
        Some(name) if pipeline::is_valid_service_name(name) => (name.to_owned(), None),
        Some(_) => (
            default_service.to_owned(),
            Some(RepairCode::ServiceFromProfile),
        ),
        None => (default_service.to_owned(), None),
    }
}

/// The raw PRI severity numeral, 0-7, exactly as the frame spelled it.
///
/// Deliberately not inverted here: the numeral is published as an ordinary
/// column and the profile's fixed derivation source, which declares
/// `dialect = "syslog"`, is what maps it onto the `OTel` ladder.
const fn severity_numeral(sev: syslog_loose::SyslogSeverity) -> u8 {
    use syslog_loose::SyslogSeverity as S;
    match sev {
        S::SEV_EMERG => 0,
        S::SEV_ALERT => 1,
        S::SEV_CRIT => 2,
        S::SEV_ERR => 3,
        S::SEV_WARNING => 4,
        S::SEV_NOTICE => 5,
        S::SEV_INFO => 6,
        S::SEV_DEBUG => 7,
    }
}

/// Convert a parsed syslog message into a door-facing payload.
///
/// `raw` is the pre-parse wire line — the most original form available —
/// and is proposed as `_raw`. `peer_is_trusted_relay` decides the
/// hostname-less case: behind a relay the peer address is confidently
/// wrong, so the event is kept with no `host` at all.
pub fn syslog_to_payload<S: BuildHasher>(
    raw: &str,
    msg: &Message<&str>,
    source_ip: IpAddr,
    peer_is_trusted_relay: bool,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> SyslogPayload {
    let mut map = Map::new();
    let mut repairs = Vec::new();

    let (service, service_repair) =
        derive_service(source_ip, msg.appname, source_service_map, default_service);
    if let Some(code) = service_repair {
        repairs.push(code);
    }

    // Host: the frame's own hostname is the only first-hand answer. The
    // peer address is second-hand but honest for a direct sender; behind
    // a configured relay it is the relay's, so absence beats the lie.
    let host = match msg.hostname {
        Some(hostname) => Some(hostname.to_owned()),
        None if peer_is_trusted_relay => None,
        None => {
            repairs.push(RepairCode::HostFromPeer);
            Some(source_ip.to_string())
        }
    };

    // The pre-parse wire line: the syslog profile's `_raw` proposal. The
    // door truncates it at `MAX_RAW_CHARS` as it does every producer's,
    // which is what a 64 KB datagram needs.
    map.insert(trawl_core::schema::RAW.into(), json!(raw));

    // The severity artifact: raw 0-7, omitted when the frame carried no
    // PRI at all. An absent source is not an unmapped one: a placeholder
    // would make every PRI-less frame look like a mapping failure to
    // `trawl_severity_unmapped_total`.
    if let Some(sev) = msg.severity {
        map.insert(SYSLOG_SEVERITY_FIELD.into(), json!(severity_numeral(sev)));
    }

    // The timestamp artifact, canonicalized to the envelope's spelling so
    // the profile's fixed `_time` source can read it. Omitted when the
    // frame carried none: substituting `Utc::now()` here would make an
    // absent frame timestamp indistinguishable from a present one and hide
    // `time.from_ingest` from `_repairs`.
    if let Some(ts) = msg.timestamp {
        map.insert(
            SYSLOG_TIMESTAMP_FIELD.into(),
            json!(
                ts.with_timezone(&Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
            ),
        );
    }

    if let Some(facility) = parse::facility_to_str(msg.facility) {
        map.insert("syslog_facility".into(), json!(facility));
    }
    if let Some(pid) = parse::procid_to_string(&msg.procid) {
        map.insert("syslog_pid".into(), json!(pid));
    }
    if let Some(msgid) = msg.msgid {
        map.insert("syslog_msgid".into(), json!(msgid));
    }
    map.insert("syslog_source_ip".into(), json!(source_ip.to_string()));

    // Flatten RFC 5424 structured data, bounded so a malicious frame with
    // a huge SD payload cannot make the parse expensive. Keys go in as
    // the frame spelled them: SD-IDs and param names are conventionally
    // mixed-case (`exampleSDID@32473`, `eventID`); folding them, dropping
    // over-long ones and resolving collisions is the door's job, under the
    // same rule every producer gets.
    let mut sd_param_count: usize = 0;
    'outer: for element in msg.structured_data.iter().take(MAX_SD_ELEMENTS) {
        for (param_name, param_value) in &element.params {
            if sd_param_count >= MAX_SD_PARAMS_TOTAL {
                break 'outer;
            }
            map.entry(format!("sd_{}_{}", element.id, param_name))
                .or_insert_with(|| json!(param_value));
            sd_param_count += 1;
        }
    }

    SyslogPayload {
        service,
        host,
        message: msg.msg.to_string(),
        map,
        repairs,
    }
}

/// The boot-resolved policy a syslog listener needs to reach the door.
///
/// Threaded rather than reached through `AppState` so the listeners keep
/// no handle on server state: the env allowlist and `default_env` come
/// from `[ingest]` (the listener asserts a boot-validated env, so
/// `env.defaulted` can never fire for it), `trusted_relays` is the same
/// boot-parsed CIDR list the HTTP door consults, and `derivation` is the
/// one resolved source policy every profile shares.
#[derive(Debug)]
pub struct SyslogDoor {
    /// The effective env allowlist (never empty).
    pub envs: Arc<[String]>,
    /// The env every syslog event is filed under.
    pub default_env: Arc<str>,
    /// Peers whose address must never be stamped as an event's origin.
    pub trusted_relays: Arc<[CidrEntry]>,
    /// The boot-resolved per-profile derivation policy.
    pub derivation: Arc<Derivation>,
}

impl SyslogDoor {
    /// Parse one frame, canonicalize it under the `syslog` profile, and
    /// return the event to batch — or `None` when the door refused it.
    ///
    /// A refusal here is a server bug, not a sender's: everything the
    /// profile asserts is boot-validated, and there is nobody to reject
    /// to. It is therefore counted on `trawl_ingest_profile_reject_total`
    /// (whose whole closed matrix is published at zero, so a flat series
    /// is the invariant holding) and the frame is dropped.
    pub fn admit<S: BuildHasher>(
        &self,
        raw: &str,
        source_ip: IpAddr,
        source_service_map: &HashMap<String, String, S>,
        default_service: &str,
        transport: &'static str,
    ) -> Option<SyslogEvent> {
        let arrival_instant = Utc::now();
        let parsed = parse::parse_syslog(raw, arrival_instant);
        // Membership, not `is_allowed`: an empty relay list means "no
        // relays configured", the exact opposite of the empty CIDR
        // allowlist's "everything is allowed". Same predicate the HTTP
        // door uses, spelled the same way.
        let peer_is_trusted_relay = self
            .trusted_relays
            .iter()
            .any(|cidr| cidr.contains(source_ip));
        let payload = syslog_to_payload(
            raw,
            &parsed,
            source_ip,
            peer_is_trusted_relay,
            source_service_map,
            default_service,
        );

        let arrival = arrival_instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let ctx = EnvelopeContext {
            arrival: &arrival,
            arrival_instant,
            envs: &self.envs,
            default_env: &self.default_env,
            producer: Producer::Syslog(Asserted {
                env: &self.default_env,
                service: &payload.service,
                host: payload.host.as_deref(),
                message: Some(&payload.message),
                repairs: &payload.repairs,
            }),
            derivation: &self.derivation,
        };

        match envelope::canonicalize(&payload.map, &ctx) {
            Ok(canonical) => {
                producer::count_event_outcome(&canonical);
                Some(SyslogEvent {
                    env: canonical.env,
                    service: canonical.service,
                    map: canonical.obj,
                    transport,
                })
            }
            Err((_, reason)) => {
                producer::count_profile_reject(ProducerKind::Syslog, reason);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn door() -> SyslogDoor {
        SyslogDoor {
            envs: vec!["prod".to_owned(), "lab".to_owned()].into(),
            default_env: "prod".into(),
            trusted_relays: Vec::new().into(),
            derivation: Arc::new(Derivation::defaults()),
        }
    }

    /// Drive a frame through the whole lane (parse, payload, door) as the
    /// listeners do. Every assertion below is on what lands, because the
    /// payload alone is not an event.
    fn admit(raw: &str, source_ip: &str) -> SyslogEvent {
        door()
            .admit(
                raw,
                source_ip.parse().unwrap(),
                &HashMap::new(),
                "syslog",
                "udp",
            )
            .expect("the syslog profile must not reject")
    }

    fn admit_with(
        raw: &str,
        source_ip: &str,
        service_map: &HashMap<String, String>,
    ) -> SyslogEvent {
        door()
            .admit(
                raw,
                source_ip.parse().unwrap(),
                service_map,
                "syslog",
                "udp",
            )
            .expect("the syslog profile must not reject")
    }

    fn repairs(event: &SyslogEvent) -> Vec<&str> {
        event.map.get("_repairs").map_or_else(Vec::new, |v| {
            v.as_str().unwrap_or_default().split(',').collect()
        })
    }

    #[test]
    fn convert_rfc3164_unifi() {
        let raw = "<134>Mar 12 10:00:00 UGW kernel: [UFW BLOCK] IN=eth0 SRC=192.168.1.100";
        let mut service_map = HashMap::new();
        service_map.insert("192.168.1.1".to_string(), "unifi-gateway".to_string());

        let event = admit_with(raw, "192.168.1.1", &service_map);

        assert_eq!(event.service, "unifi-gateway");
        assert_eq!(event.map["service"], "unifi-gateway");
        assert_eq!(event.env, "prod");
        assert_eq!(event.map["env"], "prod");
        assert_eq!(event.map["host"], "UGW");
        assert_eq!(
            event.map[trawl_core::schema::SEVERITY],
            9,
            "syslog 6 (info) inverts to OTel 9 — through DERIVATION now"
        );
        assert_eq!(
            event.map[SYSLOG_SEVERITY_FIELD], 6,
            "the raw numeral stays queryable"
        );
        assert!(event.map["_time"].is_string());
        assert!(event.map["_ingested"].is_string());
        assert_eq!(
            event.map["_raw"], raw,
            "the pre-parse wire line lands in _raw"
        );
        assert!(
            event.map["message"]
                .as_str()
                .unwrap()
                .contains("[UFW BLOCK]")
        );
        assert_eq!(event.map["syslog_facility"], "local0");
        assert_eq!(event.map["syslog_source_ip"], "192.168.1.1");
    }

    #[test]
    fn convert_uses_appname_when_no_ip_mapping() {
        let event = admit(
            "<13>Mar 12 10:00:00 myhost sshd[1234]: Accepted password",
            "10.0.0.50",
        );
        assert_eq!(event.service, "sshd");
        assert_eq!(event.map["service"], "sshd");
        assert_eq!(event.map["host"], "myhost");
    }

    #[test]
    fn convert_falls_back_to_default_service() {
        let event = admit("<13>test message only", "10.0.0.1");
        assert_eq!(event.service, "syslog");
    }

    #[test]
    fn convert_host_falls_back_to_source_ip() {
        // No hostname in the frame and a direct (non-relay) peer: the
        // source address is second-hand but honest, and confessed.
        let event = admit("<13>test message", "10.0.0.1");
        assert_eq!(event.map["host"], "10.0.0.1");
        assert!(repairs(&event).contains(&"host.from_peer"));
    }

    #[test]
    fn a_hostname_less_frame_behind_a_trusted_relay_keeps_the_event() {
        // Absent-but-honest beats both the peer-fill lie (the relay's own
        // address) and dropping the event.
        let relay = SyslogDoor {
            trusted_relays: vec![super::super::parse_cidr("10.0.0.0/8").unwrap()].into(),
            ..door()
        };
        let event = relay
            .admit(
                "<13>test message",
                "10.0.0.1".parse().unwrap(),
                &HashMap::new(),
                "syslog",
                "udp",
            )
            .expect("a hostname-less frame behind a relay still lands");
        assert!(!event.map.contains_key("host"), "host must be OMITTED");
        assert!(repairs(&event).contains(&"host.omitted"));
        assert!(!repairs(&event).contains(&"host.from_peer"));

        // A frame that does carry a hostname is unaffected by the relay.
        let event = relay
            .admit(
                "<13>Mar 12 10:00:00 myhost app: hi",
                "10.0.0.1".parse().unwrap(),
                &HashMap::new(),
                "syslog",
                "udp",
            )
            .unwrap();
        assert_eq!(event.map["host"], "myhost");
    }

    /// The listener writes no `_severity`: it publishes the raw 0-7 numeral
    /// and the profile's fixed derivation source inverts it. A passthrough
    /// that skipped the inversion would leave every syslog severity in the
    /// corpus upside down on the `OTel` ladder.
    #[test]
    fn syslog_numerics_invert_onto_the_otel_ladder() {
        // (PRI severity, the OTel number derivation must land on).
        let matrix = [
            (0, 24), // emerg  → FATAL4
            (1, 23), // alert  → FATAL3
            (2, 21), // crit   → FATAL
            (3, 17), // err    → ERROR
            (4, 13), // warning→ WARN
            (5, 10), // notice → INFO2
            (6, 9),  // info   → INFO
            (7, 5),  // debug  → DEBUG
        ];
        for (pri, otel) in matrix {
            let raw = format!("<{pri}>Mar 12 10:00:00 h app: a line");
            let event = admit(&raw, "10.0.0.1");
            assert_eq!(
                event.map[trawl_core::schema::SEVERITY],
                otel,
                "syslog {pri} must derive to OTel {otel}"
            );
            assert_eq!(
                event.map[SYSLOG_SEVERITY_FIELD], pri,
                "the artifact keeps the RAW numeral, uninverted"
            );
            // The frame that proves the dialect stays findable.
            assert!(
                event.map["_raw"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("<{pri}>"))
            );
        }

        // No PRI at all: the artifact is omitted rather than defaulted,
        // so an absent source can never look like an unmapped one.
        let event = admit("no pri here", "10.0.0.1");
        assert!(!event.map.contains_key(SYSLOG_SEVERITY_FIELD));
        assert!(!event.map.contains_key(trawl_core::schema::SEVERITY));
    }

    /// The full ten-field envelope, assembled by the door.
    #[test]
    fn converted_event_carries_the_envelope() {
        let raw = "<134>Mar 12 10:00:00 web01 nginx: GET /";
        let lab = SyslogDoor {
            default_env: "lab".into(),
            ..door()
        };
        let event = lab
            .admit(
                raw,
                "10.0.0.1".parse().unwrap(),
                &HashMap::new(),
                "syslog",
                "udp",
            )
            .unwrap();
        for key in [
            "_time",
            "_ingested",
            "_raw",
            "_severity",
            "_producer",
            "env",
            "service",
            "host",
            "message",
        ] {
            assert!(event.map.contains_key(key), "envelope key {key} missing");
        }
        assert_eq!(event.map["env"], "lab");
        assert_eq!(
            event.map["_producer"], "syslog",
            "provenance is data (ruling 6)"
        );
        // `_repairs` is the tenth slot and is omitted when clean: this
        // frame carries a hostname, a timestamp and a valid APP-NAME.
        assert!(
            !event.map.contains_key("_repairs"),
            "a well-formed frame must be repair-free: {:?}",
            event.map.get("_repairs")
        );
        assert!(!event.map.contains_key("level"), "level is never stored");
        assert!(
            !event.map.contains_key("timestamp"),
            "timestamp is never stored"
        );
    }

    #[test]
    fn the_frame_timestamp_is_an_artifact_the_profile_reads_first() {
        let event = admit(
            "<165>1 2026-02-15T12:00:00+05:30 web01 app 1234 ID47 - boom",
            "10.0.0.1",
        );
        assert_eq!(
            event.map[SYSLOG_TIMESTAMP_FIELD], "2026-02-15T06:30:00.000000Z",
            "the artifact is canonicalized to the envelope's spelling"
        );
        assert_eq!(event.map["_time"], "2026-02-15T06:30:00.000000Z");
        assert!(!repairs(&event).contains(&"time.from_ingest"));

        // A frame with no parseable timestamp omits the artifact, so the
        // door falls to arrival time and records the repair.
        let event = admit("<13>no timestamp here", "10.0.0.1");
        assert!(!event.map.contains_key(SYSLOG_TIMESTAMP_FIELD));
        assert!(repairs(&event).contains(&"time.from_ingest"));
    }

    /// The `_raw` cap is the door's, and it reaches syslog.
    ///
    /// Worth pinning because the two bounds look interchangeable and are
    /// not: the UDP receive buffer is 65 536 bytes while `MAX_RAW_CHARS`
    /// is 65 536 chars, so an all-ASCII datagram fits by coincidence, and
    /// a TCP frame read to the same byte bound has no such relationship.
    #[test]
    fn an_oversized_frame_is_truncated_by_the_doors_raw_cap() {
        let long = format!(
            "<13>Mar 12 10:00:00 h app: {}",
            "x".repeat(envelope::MAX_RAW_CHARS + 1_000)
        );
        let event = admit(&long, "10.0.0.1");
        assert_eq!(
            event.map["_raw"].as_str().unwrap().chars().count(),
            envelope::MAX_RAW_CHARS
        );
        assert!(repairs(&event).contains(&"field.truncated"));
    }

    /// RFC 5424 SD-IDs and param names are conventionally mixed-case
    /// (`exampleSDID@32473`, `eventID`). The door's universal fold handles
    /// them, so there is one rule and one place it can drift from.
    #[test]
    fn sd_keys_are_folded_by_the_door() {
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [exampleSDID@32473 eventID="1011" eventSource="Application"] boom"#;
        let event = admit(raw, "10.0.0.1");

        assert_eq!(event.map["sd_examplesdid@32473_eventid"], "1011");
        assert_eq!(event.map["sd_examplesdid@32473_eventsource"], "Application");
        assert!(
            event
                .map
                .keys()
                .all(|k| !k.bytes().any(|b| b.is_ascii_uppercase())),
            "no key may carry ASCII uppercase: {:?}",
            event.map.keys().collect::<Vec<_>>()
        );
        assert!(repairs(&event).contains(&"field.name_case_folded"));
    }

    /// Two params folding to one key keep one value, by the door's global
    /// case-collision rule, and the event records the drop in `_repairs`.
    ///
    /// The tiebreak is the door's: the exact-lowercase spelling when the
    /// event carries it, otherwise the ASCII-lexicographically first
    /// variant. Nothing honest can be done with two values for one
    /// `DuckDB` column, and the loser stays in `_raw`.
    #[test]
    fn sd_key_collision_after_folding_is_the_doors_global_rule() {
        // Variant-only: `EVENTID` sorts before `eventID` in ASCII.
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [x@1 eventID="doc-first" EVENTID="ascii-first"] boom"#;
        let event = admit(raw, "10.0.0.1");
        assert_eq!(
            event.map["sd_x@1_eventid"], "ascii-first",
            "with no exact spelling present, the lexicographically first variant wins"
        );
        assert!(repairs(&event).contains(&"field.name_case_collision"));
        assert!(
            event.map["_raw"].as_str().unwrap().contains("doc-first"),
            "the losing value stays findable in _raw"
        );

        // With the exact (already-lowercase) spelling present, it wins
        // regardless of where it sits.
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [x@1 EVENTID="variant" eventid="exact"] boom"#;
        let event = admit(raw, "10.0.0.1");
        assert_eq!(event.map["sd_x@1_eventid"], "exact");
    }

    /// An SD key too long to be a catalog key is dropped by the door's
    /// name gate, which says so in `_repairs`; the value stays in `_raw`.
    #[test]
    fn an_over_long_sd_key_is_dropped_by_the_doors_name_gate() {
        let long_id = "z".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES);
        let raw =
            format!(r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [{long_id}@1 k="v"] boom"#);
        let event = admit(&raw, "10.0.0.1");
        assert!(
            event
                .map
                .keys()
                .all(|k| k.len() <= trawl_core::schema::MAX_FIELD_NAME_BYTES),
            "an unstorable name must never become a column"
        );
        assert!(repairs(&event).contains(&"field.name_too_long"));
        assert!(
            event.map["_raw"].as_str().unwrap().contains(&long_id),
            "the dropped key stays findable in _raw"
        );
    }

    /// Every universal gate reaches syslog, including the sealed `_`
    /// prefix, which a structured-data param can spell.
    #[test]
    fn a_frame_faces_the_sealed_prefix_strip() {
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [_@1 x="y"] boom"#;
        let event = admit(raw, "10.0.0.1");
        assert!(
            event
                .map
                .keys()
                .all(|k| !k.starts_with("sd__") || !trawl_core::schema::is_reserved_name(k)),
            "keys: {:?}",
            event.map.keys().collect::<Vec<_>>()
        );
        // The `sd_` prefix means an SD param can never actually reach the
        // reserved namespace, which is exactly the point of prefixing
        // parse artifacts. `_producer` is the slot a sender would want to
        // forge, and the door stamps it from the profile after the strip has
        // already turned any incoming `_producer` into a bare `producer`.
        assert_eq!(event.map["_producer"], "syslog");
    }

    #[test]
    fn service_derivation_priority_and_its_one_repair() {
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let other: IpAddr = "10.0.0.1".parse().unwrap();
        let mut map = HashMap::new();
        map.insert("192.168.1.1".to_string(), "mapped-svc".to_string());

        // The IP map wins over the APP-NAME, verbatim, no repair.
        assert_eq!(
            derive_service(ip, Some("nginx"), &map, "default"),
            ("mapped-svc".to_owned(), None)
        );
        // A valid APP-NAME is used verbatim.
        assert_eq!(
            derive_service(other, Some("nginx"), &map, "default"),
            ("nginx".to_owned(), None)
        );
        // An absent APP-NAME is not a defect: the default, no repair.
        assert_eq!(
            derive_service(other, None, &map, "default"),
            ("default".to_owned(), None)
        );
        // A present but unusable one is: the default, with a repair code
        // rather than a sanitized name nobody sent.
        for bad in ["Living Room AP", "../../escaped", ".hidden", "..", ""] {
            assert_eq!(
                derive_service(other, Some(bad), &HashMap::new(), "syslog"),
                ("syslog".to_owned(), Some(RepairCode::ServiceFromProfile)),
                "APP-NAME {bad:?} must fall to the profile default"
            );
        }
    }

    /// ADR-0009 injectivity: the on-disk name is the service value. Every
    /// path through `derive_service` must yield a name HTTP ingest would
    /// also accept: the two config-sourced ones because config validation
    /// refuses to start otherwise, the APP-NAME because it is checked
    /// against the very same predicate.
    #[test]
    fn derive_service_output_is_always_a_valid_service_name() {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for candidate in [
            "../../escaped",
            "Living Room AP",
            ".hidden",
            "..",
            "/../",
            "",
            &"x".repeat(200),
            "nginx",
        ] {
            let (derived, _) = derive_service(ip, Some(candidate), &HashMap::new(), "syslog");
            assert!(
                pipeline::is_valid_service_name(&derived),
                "derive_service({candidate:?}) yielded invalid name {derived:?}"
            );
        }
    }
}
