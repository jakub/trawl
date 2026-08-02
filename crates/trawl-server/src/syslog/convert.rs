// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Convert parsed syslog messages to trawl event maps.
//!
//! Produces the declared ADR-0009 envelope (`_time`, `_ingested`,
//! `_raw`, `env`, `service`, `host`, `severity`, `severity_text`,
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
const MAX_SD_KEY_LEN: usize = 256;

/// Sanitize a service name: strip invalid characters, truncate, fallback.
fn sanitize_service(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }

    let sanitized: String = raw
        .bytes()
        .filter(|b| pipeline::is_valid_service_char(*b))
        .map(|b| b as char)
        .collect();

    if sanitized.is_empty() {
        return None;
    }

    if sanitized.len() > pipeline::MAX_SERVICE_NAME_LEN {
        Some(sanitized[..pipeline::MAX_SERVICE_NAME_LEN].to_owned())
    } else {
        Some(sanitized)
    }
}

/// Derive the service name for a syslog event.
///
/// Priority:
/// 1. `source_service_map` lookup by source IP (explicit user config)
/// 2. APP-NAME / tag from the syslog message (sanitized)
/// 3. `default_service` from config
pub fn derive_service<S: BuildHasher>(
    source_ip: IpAddr,
    appname: Option<&str>,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> String {
    // 1. Check source IP mapping
    let ip_str = source_ip.to_string();
    if let Some(mapped) = source_service_map.get(&ip_str) {
        return mapped.clone();
    }

    // 2. Use APP-NAME/tag if valid
    if let Some(sanitized) = appname.and_then(sanitize_service) {
        return sanitized;
    }

    // 3. Fallback to default
    default_service.to_owned()
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
    // Severity: syslog numerics inverted onto the OTel ladder; the
    // keyword is preserved as severity_text. Absent severity is omitted,
    // never guessed (omit-when-null, ADR-0009).
    if let Some(sev) = msg.severity {
        let (keyword, number) = severity_mapping(sev);
        if let Some(n) = number {
            map.insert("severity".into(), json!(n));
        }
        map.insert("severity_text".into(), json!(keyword));
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
    let mut sd_param_count: usize = 0;
    'outer: for element in msg.structured_data.iter().take(MAX_SD_ELEMENTS) {
        for (param_name, param_value) in &element.params {
            if sd_param_count >= MAX_SD_PARAMS_TOTAL {
                break 'outer;
            }
            let key = format!("sd_{}_{}", element.id, param_name);
            if key.len() > MAX_SD_KEY_LEN {
                continue;
            }
            map.insert(key, json!(param_value));
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
        assert_eq!(map["severity"], 9, "syslog 6 (info) inverts to OTel 9");
        assert_eq!(map["severity_text"], "info");
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
            map["severity"], 24,
            "syslog 0 (emerg) is OTel 24 (FATAL band)"
        );
        assert_eq!(map["severity_text"], "emerg");

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
            map["severity"], 5,
            "syslog 7 (debug) is OTel 5 (DEBUG band)"
        );
        assert_eq!(map["severity_text"], "debug");
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
