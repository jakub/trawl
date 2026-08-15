// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Convert parsed syslog messages to trawl event maps.
//!
//! Produces the declared ADR-0009 envelope (`_time`, `_ingested`,
//! `_raw`, `env`, `service`, `host`, `_severity`,
//! `message`) so that queries work identically regardless of ingestion
//! path. The syslog listener is an input-mapping ingestor: appnames are
//! mapped *into* the service charset before validation, syslog numeric
//! severities are inverted onto the `OTel` ladder, and `_raw` carries the
//! pre-parse wire line verbatim.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::IpAddr;

use chrono::{DateTime, FixedOffset, Utc};
use serde_json::{Map, Value, json};
use syslog_loose::Message;

use super::parse;
use crate::ingest::pipeline;

/// Maximum number of RFC 5424 structured data elements to extract.
const MAX_SD_ELEMENTS: usize = 32;
/// Maximum total structured data params across all elements.
const MAX_SD_PARAMS_TOTAL: usize = 128;
/// Maximum length of a structured data field key (`sd_{id}_{param}`).
///
/// Exactly the catalog's own field-name bound: the syslog listener writes
/// straight into the pipeline (it does not route through
/// `envelope::canonicalize`), so a key admitted here becomes a column with
/// no further gate — one byte over the catalog bound and the field could
/// never be pinned.
const MAX_SD_KEY_LEN: usize = trawl_core::schema::MAX_FIELD_NAME_BYTES;

/// Last-resort service name when every candidate sanitizes away.
const FALLBACK_SERVICE: &str = "syslog";

/// Map a candidate into the service charset: drop invalid characters,
/// drop leading dots, truncate to the length cap. Returns `None` when
/// nothing usable survives.
///
/// The result always satisfies [`pipeline::is_valid_service_name`] — the
/// syslog listener is an input-mapping ingestor, so it maps rather than
/// rejects, but it may never emit a name HTTP ingest would refuse
/// (ADR-0009: the name reaches WAL filenames verbatim).
fn sanitize_service(raw: &str) -> Option<String> {
    let filtered: String = raw
        .bytes()
        .filter(|b| pipeline::is_valid_service_char(*b))
        .map(char::from)
        .collect();

    // Dot-leading names are `.`, `..` (path escape) or dotfiles; strip the
    // leading run rather than rejecting the whole candidate.
    let trimmed = filtered.trim_start_matches('.');
    let capped = &trimmed[..trimmed.len().min(pipeline::MAX_SERVICE_NAME_LEN)];

    if capped.is_empty() {
        None
    } else {
        Some(capped.to_owned())
    }
}

/// Derive the service name for a syslog event.
///
/// Priority (each candidate sanitized, falling through when it maps to
/// nothing):
/// 1. `source_service_map` lookup by source IP (explicit user config)
/// 2. APP-NAME / tag from the syslog message
/// 3. `default_service` from config
/// 4. [`FALLBACK_SERVICE`]
///
/// The return value always satisfies [`pipeline::is_valid_service_name`].
pub fn derive_service<S: BuildHasher>(
    source_ip: IpAddr,
    appname: Option<&str>,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> String {
    let ip_str = source_ip.to_string();
    source_service_map
        .get(&ip_str)
        .map(String::as_str)
        .and_then(sanitize_service)
        .or_else(|| appname.and_then(sanitize_service))
        .or_else(|| sanitize_service(default_service))
        .unwrap_or_else(|| FALLBACK_SERVICE.to_owned())
}

/// Format a syslog timestamp to RFC 3339 UTC at microsecond precision
/// (the envelope's canonical `_time` spelling).
fn format_timestamp(ts: Option<DateTime<FixedOffset>>) -> String {
    match ts {
        Some(dt) => dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        None => Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    }
}

/// The syslog severity keyword and its `OTel` `SeverityNumber` (via the
/// ADR-0009 inversion table — syslog counts down from Emergency 0, `OTel`
/// counts up).
fn severity_mapping(sev: syslog_loose::SyslogSeverity) -> (&'static str, Option<u8>) {
    use syslog_loose::SyslogSeverity as S;
    let (keyword, numeral) = match sev {
        S::SEV_EMERG => ("emerg", 0),
        S::SEV_ALERT => ("alert", 1),
        S::SEV_CRIT => ("crit", 2),
        S::SEV_ERR => ("err", 3),
        S::SEV_WARNING => ("warning", 4),
        S::SEV_NOTICE => ("notice", 5),
        S::SEV_INFO => ("info", 6),
        S::SEV_DEBUG => ("debug", 7),
    };
    (keyword, trawl_core::severity::from_syslog(numeral))
}

/// Convert a parsed syslog message into a declared-envelope event map.
///
/// `raw` is the pre-parse wire line — the most original form available —
/// and lands in `_raw` verbatim. `default_env` stamps `env`.
pub fn syslog_to_event<S: BuildHasher>(
    raw: &str,
    msg: &Message<&str>,
    source_ip: IpAddr,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
    default_env: &str,
) -> (String, Map<String, Value>) {
    let mut map = Map::new();

    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let service = derive_service(source_ip, msg.appname, source_service_map, default_service);
    map.insert("service".into(), json!(service));
    map.insert("env".into(), json!(default_env));
    map.insert("_time".into(), json!(format_timestamp(msg.timestamp)));
    map.insert("_ingested".into(), json!(now));
    map.insert("_raw".into(), json!(raw));
    map.insert(
        "host".into(),
        json!(msg.hostname.unwrap_or(&source_ip.to_string())),
    );
    // Severity: the listener KNOWS its input is syslog, so it is the one
    // place trawl may invert the numeral onto the OTel ladder (ADR-0013
    // §4) — and it therefore writes `_severity` itself rather than
    // proposing a source for the generic derivation, which reads
    // numerics strictly as OTel. The keyword and the numeral both stay
    // findable in `_raw`, which is the provenance that proves the
    // dialect. Absent severity is omitted, never guessed.
    if let Some(sev) = msg.severity
        && let (_, Some(n)) = severity_mapping(sev)
    {
        map.insert(trawl_core::schema::SEVERITY.into(), json!(n));
    }
    map.insert("message".into(), json!(msg.msg));

    // Syslog-specific metadata (prefixed to avoid collisions)
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

    // Flatten RFC 5424 structured data elements (bounded to prevent
    // memory exhaustion from malicious messages with huge SD payloads).
    //
    // Keys are ASCII-lowercased at construction — the fold-at-the-door
    // rule (ADR-0009): this path writes straight into the pipeline without
    // routing through `envelope::canonicalize`, and SD-IDs/param names are
    // conventionally mixed-case (`exampleSDID@32473`, `eventID`), so an
    // unfolded key here would reach the WAL and hot buffer under a
    // spelling the (folded) catalog pin never matches. Two params folding
    // to one key keep the FIRST value, mirroring the canonicalizer's
    // collision rule.
    let mut sd_param_count: usize = 0;
    'outer: for element in msg.structured_data.iter().take(MAX_SD_ELEMENTS) {
        for (param_name, param_value) in &element.params {
            if sd_param_count >= MAX_SD_PARAMS_TOTAL {
                break 'outer;
            }
            let key = format!("sd_{}_{}", element.id, param_name).to_ascii_lowercase();
            if key.len() > MAX_SD_KEY_LEN {
                continue;
            }
            map.entry(key).or_insert_with(|| json!(param_value));
            sd_param_count += 1;
        }
    }

    (service, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syslog::parse::parse_syslog;

    #[test]
    fn convert_rfc3164_unifi() {
        let raw = "<134>Mar 12 10:00:00 UGW kernel: [UFW BLOCK] IN=eth0 SRC=192.168.1.100";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "192.168.1.1".parse().unwrap();

        let mut service_map = HashMap::new();
        service_map.insert("192.168.1.1".to_string(), "unifi-gateway".to_string());

        let (service, map) =
            syslog_to_event(raw, &parsed, source_ip, &service_map, "syslog", "prod");

        assert_eq!(service, "unifi-gateway");
        assert_eq!(map["service"], "unifi-gateway");
        assert_eq!(map["env"], "prod");
        assert_eq!(map["host"], "UGW");
        assert_eq!(
            map[trawl_core::schema::SEVERITY],
            9,
            "syslog 6 (info) inverts to OTel 9"
        );
        assert!(map["_time"].is_string());
        assert!(map["_ingested"].is_string());
        assert_eq!(map["_raw"], raw, "the pre-parse wire line lands in _raw");
        assert!(map["message"].as_str().unwrap().contains("[UFW BLOCK]"));
        assert_eq!(map["syslog_facility"], "local0");
        assert_eq!(map["syslog_source_ip"], "192.168.1.1");
    }

    #[test]
    fn convert_uses_appname_when_no_ip_mapping() {
        let raw = "<13>Mar 12 10:00:00 myhost sshd[1234]: Accepted password";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.50".parse().unwrap();

        let (service, map) =
            syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "prod");

        assert_eq!(service, "sshd");
        assert_eq!(map["service"], "sshd");
        assert_eq!(map["host"], "myhost");
    }

    #[test]
    fn convert_falls_back_to_default_service() {
        let raw = "<13>test message only";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();

        let (service, _map) =
            syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "prod");

        assert_eq!(service, "syslog");
    }

    #[test]
    fn convert_host_falls_back_to_source_ip() {
        // Message without hostname
        let raw = "<13>test message";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();

        let (_service, map) =
            syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "prod");

        // If hostname is None, should use source IP
        let host = map["host"].as_str().unwrap();
        // hostname might be parsed from the message; if not, should be source IP
        assert!(!host.is_empty());
    }

    /// Acceptance criterion (ADR-0009): syslog numerics are INVERTED —
    /// 0 (emerg) maps into the FATAL band, 7 (debug) into DEBUG. A naive
    /// passthrough would invert every severity in the corpus.
    #[test]
    fn syslog_numerics_invert_onto_the_otel_ladder() {
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();
        // PRI 0*8+0 = <0> emerg; PRI 0*8+7 = <7> debug (facility kern).
        let emerg_raw = "<0>Mar 12 10:00:00 h app: the world is on fire";
        let parsed = parse_syslog(emerg_raw);
        let (_s, map) = syslog_to_event(
            emerg_raw,
            &parsed,
            source_ip,
            &HashMap::new(),
            "syslog",
            "prod",
        );
        assert_eq!(
            map[trawl_core::schema::SEVERITY],
            24,
            "syslog 0 (emerg) is OTel 24 (FATAL band)"
        );
        // The keyword and the numeral both stay findable in `_raw`, the
        // provenance that proves the dialect (ADR-0013 §4).
        assert!(map["_raw"].as_str().unwrap().contains("<0>"));

        let debug_raw = "<7>Mar 12 10:00:00 h app: noisy detail";
        let parsed = parse_syslog(debug_raw);
        let (_s, map) = syslog_to_event(
            debug_raw,
            &parsed,
            source_ip,
            &HashMap::new(),
            "syslog",
            "prod",
        );
        assert_eq!(
            map[trawl_core::schema::SEVERITY],
            5,
            "syslog 7 (debug) is OTel 5 (DEBUG band)"
        );
        assert!(map["_raw"].as_str().unwrap().contains("<7>"));
    }

    /// The full envelope is present on every converted event.
    #[test]
    fn converted_event_carries_the_envelope() {
        let raw = "<134>Mar 12 10:00:00 web01 nginx: GET /";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let (_s, map) = syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "lab");
        for key in [
            "_time",
            "_ingested",
            "_raw",
            "env",
            "service",
            "host",
            "message",
        ] {
            assert!(map.contains_key(key), "envelope key {key} missing");
        }
        assert_eq!(map["env"], "lab");
        assert!(!map.contains_key("level"), "level is never stored");
        assert!(!map.contains_key("timestamp"), "timestamp is never stored");
    }

    /// RFC 5424 SD-IDs and param names are conventionally mixed-case
    /// (`exampleSDID@32473`, `eventID`), and this path does NOT route
    /// through `envelope::canonicalize` — so the fold happens at key
    /// construction, or the key would reach the WAL and hot buffer under a
    /// spelling the folded catalog pin never matches.
    #[test]
    fn sd_keys_are_ascii_folded_at_construction() {
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [exampleSDID@32473 eventID="1011" eventSource="Application"] boom"#;
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let (_s, map) = syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "prod");

        assert_eq!(map["sd_examplesdid@32473_eventid"], "1011");
        assert_eq!(map["sd_examplesdid@32473_eventsource"], "Application");
        assert!(
            map.keys()
                .all(|k| !k.bytes().any(|b| b.is_ascii_uppercase())),
            "no key may carry ASCII uppercase: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }

    /// Two params folding to one key keep the FIRST value — the same
    /// deterministic collision rule as the canonicalizer's.
    #[test]
    fn sd_key_collision_after_folding_keeps_the_first_value() {
        let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [x@1 eventID="first" EVENTID="second"] boom"#;
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let (_s, map) = syslog_to_event(raw, &parsed, source_ip, &HashMap::new(), "syslog", "prod");
        assert_eq!(
            map["sd_x@1_eventid"], "first",
            "the first spelling in document order wins"
        );
    }

    /// The SD key bound IS the catalog's field-name bound: one byte over
    /// and the key could be admitted here but never pinned.
    #[test]
    fn sd_key_bound_matches_the_catalog_field_name_bound() {
        assert_eq!(MAX_SD_KEY_LEN, trawl_core::schema::MAX_FIELD_NAME_BYTES);
    }

    #[test]
    fn sanitize_service_strips_invalid_chars() {
        assert_eq!(
            sanitize_service("nginx/error"),
            Some("nginxerror".to_string())
        );
        assert_eq!(
            sanitize_service("my-app_v2.0"),
            Some("my-app_v2.0".to_string())
        );
        assert_eq!(sanitize_service(""), None);
        assert_eq!(sanitize_service("///"), None);
        // Dot-leading names would become dotfile parquets; `..` is a path
        // escape. Both lose their leading dot run.
        assert_eq!(sanitize_service(".foo"), Some("foo".to_string()));
        assert_eq!(sanitize_service(".."), None);
        assert_eq!(
            sanitize_service("../../escaped"),
            Some("escaped".to_string())
        );
    }

    /// ADR-0009 injectivity: the on-disk name IS the service value, so
    /// every `derive_service` path — including the two unvalidated config
    /// fields — must yield a name HTTP ingest would also accept. Before
    /// this was enforced, a mapped service of `../../escaped` wrote WAL
    /// files outside the WAL root (silent data loss) and `Living Room AP`
    /// produced a parquet glob the emitter refuses.
    #[test]
    fn derive_service_output_is_always_a_valid_service_name() {
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let other: IpAddr = "10.0.0.1".parse().unwrap();

        let mut map = HashMap::new();
        map.insert("192.168.1.1".to_string(), "../../escaped".to_string());
        assert_eq!(derive_service(ip, None, &map, "syslog"), "escaped");

        let mut map = HashMap::new();
        map.insert("192.168.1.1".to_string(), "Living Room AP".to_string());
        assert_eq!(derive_service(ip, None, &map, "syslog"), "LivingRoomAP");

        // A map value that sanitizes to nothing falls through to appname.
        let mut map = HashMap::new();
        map.insert("192.168.1.1".to_string(), "..".to_string());
        assert_eq!(derive_service(ip, Some("sshd"), &map, "syslog"), "sshd");

        // Dot-leading appnames never become dotfiles.
        assert_eq!(
            derive_service(other, Some(".hidden"), &HashMap::new(), "syslog"),
            "hidden"
        );

        // An unusable default_service still yields a valid name.
        assert_eq!(
            derive_service(other, None, &HashMap::new(), "/../"),
            "syslog"
        );

        for candidate in [
            "../../escaped",
            "Living Room AP",
            ".hidden",
            "..",
            "/../",
            "",
            &"x".repeat(200),
        ] {
            let derived = derive_service(other, Some(candidate), &HashMap::new(), candidate);
            assert!(
                pipeline::is_valid_service_name(&derived),
                "derive_service({candidate:?}) yielded invalid name {derived:?}"
            );
        }
    }

    #[test]
    fn derive_service_priority() {
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let mut map = HashMap::new();
        map.insert("192.168.1.1".to_string(), "mapped-svc".to_string());

        // IP mapping takes priority over appname
        assert_eq!(
            derive_service(ip, Some("nginx"), &map, "default"),
            "mapped-svc"
        );

        // Without IP mapping, use appname
        let ip2: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(derive_service(ip2, Some("nginx"), &map, "default"), "nginx");

        // Without appname, use default
        assert_eq!(derive_service(ip2, None, &map, "default"), "default");
    }
}
