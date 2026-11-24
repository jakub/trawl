// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a parsed syslog frame into a payload the one canonicalizer admits.
//!
//! Only the frame-level caps ([`MAX_SD_ELEMENTS`], [`MAX_SD_PARAMS_TOTAL`])
//! live here, because they bound work done before there is a payload at all.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use super::batch::SyslogEvent;
use super::parse::ParsedFrame;
use super::zone::SyslogZones;
use super::{CidrEntry, parse};
use crate::ingest::envelope::{self, EnvelopeContext, RepairCode};
use crate::ingest::pipeline;
use crate::ingest::producer::{
    self, Asserted, Derivation, Producer, ProducerKind, SYSLOG_SEVERITY_FIELD,
    SYSLOG_TIMESTAMP_FIELD,
};

pub const SYSLOG_TIMESTAMP_ZONE_FIELD: &str = "syslog_timestamp_zone";

const MAX_SD_ELEMENTS: usize = 32;
const MAX_SD_PARAMS_TOTAL: usize = 128;

#[derive(Debug)]
pub struct SyslogPayload {
    pub service: String,
    pub host: Option<String>,
    pub message: String,
    pub map: Map<String, Value>,
    pub repairs: Vec<RepairCode>,
}

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

pub fn syslog_to_payload<S: BuildHasher>(
    raw: &str,
    frame: &ParsedFrame<'_>,
    source_ip: IpAddr,
    peer_is_trusted_relay: bool,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> SyslogPayload {
    let msg = &frame.msg;
    let mut map = Map::new();
    let mut repairs = Vec::new();

    let (service, service_repair) =
        derive_service(source_ip, msg.appname, source_service_map, default_service);
    if let Some(code) = service_repair {
        repairs.push(code);
    }

    let host = match msg.hostname {
        Some(hostname) => Some(hostname.to_owned()),
        None if peer_is_trusted_relay => None,
        None => {
            repairs.push(RepairCode::HostFromPeer);
            Some(source_ip.to_string())
        }
    };

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
    // frame named no instant: substituting `Utc::now()` here would make an
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
    if let Some(zone) = frame.read_in {
        map.insert(SYSLOG_TIMESTAMP_ZONE_FIELD.into(), json!(zone.to_string()));
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
    // the frame spelled them: folding them is the door's job.
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

#[derive(Debug)]
pub struct SyslogDoor {
    /// The effective env allowlist (never empty).
    pub envs: Arc<[String]>,
    pub default_env: Arc<str>,
    /// Peers whose address must never be stamped as an event's origin.
    pub trusted_relays: Arc<[CidrEntry]>,
    pub derivation: Arc<Derivation>,
    pub zones: Arc<SyslogZones>,
}

impl SyslogDoor {
    pub fn admit<S: BuildHasher>(
        &self,
        raw: &str,
        source_ip: IpAddr,
        source_service_map: &HashMap<String, String, S>,
        default_service: &str,
        transport: &'static str,
    ) -> Option<SyslogEvent> {
        self.admit_at(
            raw,
            source_ip,
            Utc::now(),
            source_service_map,
            default_service,
            transport,
        )
    }

    fn admit_at<S: BuildHasher>(
        &self,
        raw: &str,
        source_ip: IpAddr,
        arrival_instant: DateTime<Utc>,
        source_service_map: &HashMap<String, String, S>,
        default_service: &str,
        transport: &'static str,
    ) -> Option<SyslogEvent> {
        let zone = self.zones.for_peer(source_ip);
        let parsed = parse::parse_syslog(raw, arrival_instant, zone);
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
            Err(rejection) => {
                producer::count_profile_reject(ProducerKind::Syslog, rejection.reason);
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
            zones: Arc::default(),
        }
    }

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
        let event = admit("<13>test message", "10.0.0.1");
        assert_eq!(event.map["host"], "10.0.0.1");
        assert!(repairs(&event).contains(&"host.from_peer"));
    }

    #[test]
    fn a_hostname_less_frame_behind_a_trusted_relay_keeps_the_event() {
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

        let event = admit("no pri here", "10.0.0.1");
        assert!(!event.map.contains_key(SYSLOG_SEVERITY_FIELD));
        assert!(!event.map.contains_key(trawl_core::schema::SEVERITY));
    }

    #[test]
    fn converted_event_carries_the_envelope() {
        // Year-less timestamps resolve near arrival. A fixed calendar date
        // becomes a future event for part of the year and earns a repair.
        let raw = format!(
            "<134>{} web01 nginx: GET /",
            Utc::now().format("%b %e %H:%M:%S")
        );
        let lab = SyslogDoor {
            default_env: "lab".into(),
            ..door()
        };
        let event = lab
            .admit(
                &raw,
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

        let event = admit("<13>no timestamp here", "10.0.0.1");
        assert!(!event.map.contains_key(SYSLOG_TIMESTAMP_FIELD));
        assert!(repairs(&event).contains(&"time.from_ingest"));
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
            .and_utc()
    }

    fn stamp(instant: DateTime<Utc>) -> String {
        instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    }

    fn zoned_door(syslog: &str, relays: &[&str]) -> SyslogDoor {
        let config: crate::config::SyslogConfig = toml::from_str(syslog).unwrap();
        let peers = super::super::SyslogPeers::resolve(&config).unwrap();
        SyslogDoor {
            trusted_relays: relays
                .iter()
                .map(|cidr| super::super::parse_cidr(cidr).unwrap())
                .collect::<Vec<_>>()
                .into(),
            zones: Arc::new(peers.zones),
            ..door()
        }
    }

    fn admit_from(door: &SyslogDoor, raw: &str, peer: &str, arrival: DateTime<Utc>) -> SyslogEvent {
        let peer = super::super::canonical_peer(peer.parse().unwrap());
        door.admit_at(raw, peer, arrival, &HashMap::new(), "syslog", "udp")
            .expect("the syslog profile must not reject")
    }

    #[test]
    fn header_time_ahead_of_arrival_is_replaced() {
        let arrival = at(2026, 2, 15, 20, 0);
        for raw in [
            "<165>1 2026-02-15T23:00:00Z web01 app 1234 ID47 - boom",
            "<13>Feb 15 23:00:00 host app: x",
        ] {
            let event = admit_from(&door(), raw, "10.0.0.1", arrival);
            assert_eq!(event.map["_time"], stamp(arrival), "{raw}");
            assert!(repairs(&event).contains(&"time.ahead_of_arrival"), "{raw}");
            assert_eq!(
                event.map[SYSLOG_TIMESTAMP_FIELD],
                stamp(at(2026, 2, 15, 23, 0)),
                "the header value stays as its column: {raw}"
            );
        }

        // At the bound the header time is kept.
        let event = admit_from(
            &door(),
            "<13>Feb 15 20:15:00 host app: x",
            "10.0.0.1",
            arrival,
        );
        assert_eq!(event.map["_time"], stamp(at(2026, 2, 15, 20, 15)));
        assert!(!repairs(&event).contains(&"time.ahead_of_arrival"));
    }

    const CHICAGO_PEER: &str = r#"
[sender_timezones]
"192.0.2.10" = "America/Chicago"
"#;

    #[test]
    fn zone_less_3164_reads_in_configured_zone() {
        let door = zoned_door(CHICAGO_PEER, &[]);
        // (frame, arrival, the instant Chicago's wall clock denotes).
        let cases = [
            // Winter: CST, -06:00.
            (
                "<13>Jan 15 12:00:00 host app: x",
                at(2026, 1, 15, 20, 0),
                at(2026, 1, 15, 18, 0),
            ),
            (
                "<13>Jan 15 2026 12:00:00 host app: x",
                at(2026, 1, 15, 20, 0),
                at(2026, 1, 15, 18, 0),
            ),
            // Summer: CDT, -05:00.
            (
                "<13>Jul 15 12:00:00 host app: x",
                at(2026, 7, 15, 20, 0),
                at(2026, 7, 15, 17, 0),
            ),
            (
                "<13>Jul 15 2026 12:00:00 host app: x",
                at(2026, 7, 15, 20, 0),
                at(2026, 7, 15, 17, 0),
            ),
        ];
        for (raw, arrival, expected) in cases {
            let event = admit_from(&door, raw, "192.0.2.10", arrival);
            assert_eq!(event.map[SYSLOG_TIMESTAMP_FIELD], stamp(expected), "{raw}");
            assert_eq!(event.map["_time"], stamp(expected), "{raw}");
            assert_eq!(
                event.map[SYSLOG_TIMESTAMP_ZONE_FIELD], "America/Chicago",
                "{raw}"
            );
        }
    }

    #[test]
    fn sender_timezone_matches_canonical_peer_only() {
        let door = zoned_door(
            r#"
default_timezone = "-03:00"

[sender_timezones]
"192.0.2.10" = "America/Chicago"
"::ffff:192.0.2.11" = "Asia/Kolkata"
"2001:db8::1" = "Europe/Warsaw"
"192.0.2.12" = "+05:30"
"#,
            &["192.0.2.12/32"],
        );
        let arrival = at(2026, 1, 15, 20, 0);
        // (peer, frame hostname, the zone it is read in, the instant).
        let cases = [
            // An IPv4 key.
            (
                "192.0.2.10",
                "host",
                "America/Chicago",
                at(2026, 1, 15, 18, 0),
            ),
            // A `::ffff:` key, reached by the v4 peer and by the mapped
            // spelling a dual-stack listener canonicalizes.
            ("192.0.2.11", "host", "Asia/Kolkata", at(2026, 1, 15, 6, 30)),
            (
                "::ffff:192.0.2.11",
                "host",
                "Asia/Kolkata",
                at(2026, 1, 15, 6, 30),
            ),
            // An IPv6 key: Warsaw is +01:00 in winter.
            (
                "2001:db8::1",
                "host",
                "Europe/Warsaw",
                at(2026, 1, 15, 11, 0),
            ),
            // A hostname naming another mapped peer changes nothing.
            (
                "198.51.100.5",
                "192.0.2.10",
                "-03:00",
                at(2026, 1, 15, 15, 0),
            ),
            // A trusted relay uses its own entry, whatever it forwards.
            ("192.0.2.12", "192.0.2.10", "+05:30", at(2026, 1, 15, 6, 30)),
            // An unlisted peer gets the default.
            ("198.51.100.5", "host", "-03:00", at(2026, 1, 15, 15, 0)),
        ];
        for (peer, hostname, zone, expected) in cases {
            let raw = format!("<13>Jan 15 12:00:00 {hostname} app: x");
            let event = admit_from(&door, &raw, peer, arrival);
            assert_eq!(
                event.map[SYSLOG_TIMESTAMP_ZONE_FIELD], zone,
                "{peer} {hostname}"
            );
            assert_eq!(
                event.map[SYSLOG_TIMESTAMP_FIELD],
                stamp(expected),
                "{peer} {hostname}"
            );
        }
    }

    #[test]
    fn wire_offset_wins_over_sender_zone() {
        let door = zoned_door(CHICAGO_PEER, &[]);
        let arrival = at(2026, 2, 15, 20, 0);
        for (raw, expected) in [
            (
                "<165>1 2026-02-15T12:00:00+05:30 web01 app 1234 ID47 - boom",
                at(2026, 2, 15, 6, 30),
            ),
            (
                "<13>2026-02-15T12:00:00Z web01 app: boom",
                at(2026, 2, 15, 12, 0),
            ),
        ] {
            let event = admit_from(&door, raw, "192.0.2.10", arrival);
            assert_eq!(event.map[SYSLOG_TIMESTAMP_FIELD], stamp(expected), "{raw}");
            assert!(
                !event.map.contains_key(SYSLOG_TIMESTAMP_ZONE_FIELD),
                "{raw}: a wire offset is not read in a zone"
            );
        }
    }

    #[test]
    fn dst_gap_keeps_frame_fields_and_falls_to_arrival() {
        let door = zoned_door(CHICAGO_PEER, &[]);
        let arrival = at(2026, 3, 8, 18, 0);
        let event = admit_from(
            &door,
            "<13>Mar  8 02:30:00 gw01 sshd[42]: x",
            "192.0.2.10",
            arrival,
        );
        assert!(!event.map.contains_key(SYSLOG_TIMESTAMP_FIELD));
        assert_eq!(event.map["_time"], stamp(arrival));
        assert!(repairs(&event).contains(&"time.from_ingest"));
        assert_eq!(event.map[SYSLOG_TIMESTAMP_ZONE_FIELD], "America/Chicago");
        assert_eq!(event.map[SYSLOG_SEVERITY_FIELD], 5);
        assert_eq!(event.map["syslog_facility"], "user");
        assert_eq!(event.map["host"], "gw01");
        assert_eq!(event.service, "sshd");
        assert_eq!(event.map["syslog_pid"], "42");
        assert_eq!(event.map["message"], "x");
    }

    #[test]
    fn syslog_timestamp_zone_is_an_annotation_not_a_repair() {
        let door = zoned_door(
            r#"
[sender_timezones]
"192.0.2.10" = "America/Chicago"
"192.0.2.20" = "+00:00"
"192.0.2.21" = "-04:30"
"#,
            &[],
        );
        let arrival = at(2026, 1, 15, 20, 0);
        for (peer, zone) in [
            ("192.0.2.10", "America/Chicago"),
            ("192.0.2.20", "UTC"),
            ("192.0.2.21", "-04:30"),
            ("198.51.100.5", "UTC"),
        ] {
            let event = admit_from(&door, "<13>Jan 15 12:00:00 host app: x", peer, arrival);
            assert_eq!(event.map[SYSLOG_TIMESTAMP_ZONE_FIELD], zone, "{peer}");
            assert!(
                !event.map.contains_key("_repairs"),
                "{peer}: {:?}",
                event.map.get("_repairs")
            );
        }
    }

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

    #[test]
    fn a_long_message_and_sd_value_are_cut_by_the_doors_value_cap() {
        let over = envelope::MAX_RAW_CHARS + 1;
        let frame = format!(
            r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [x@1 k="{}"] {}"#,
            "v".repeat(over),
            "m".repeat(over)
        );
        let event = admit(&frame, "10.0.0.1");
        assert_eq!(
            event.map["message"],
            "m".repeat(envelope::MAX_RAW_CHARS),
            "the asserted message is cut"
        );
        assert_eq!(
            event.map["sd_x@1_k"],
            "v".repeat(envelope::MAX_RAW_CHARS),
            "a frame value is cut"
        );
        assert_eq!(
            event.map["_raw"].as_str().unwrap().chars().count(),
            envelope::MAX_RAW_CHARS
        );
        assert_eq!(
            repairs(&event)
                .iter()
                .filter(|code| **code == "field.truncated")
                .count(),
            1,
            "{:?}",
            repairs(&event)
        );
    }

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

        assert_eq!(
            derive_service(ip, Some("nginx"), &map, "default"),
            ("mapped-svc".to_owned(), None)
        );
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
