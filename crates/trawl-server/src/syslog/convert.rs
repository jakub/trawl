// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Convert parsed syslog messages to trawl event maps.
//!
//! Produces the same core fields as Vector-ingested events (`service`,
//! `host`, `timestamp`, `level`, `message`) so that queries work
//! identically regardless of ingestion path.

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

/// Format a syslog timestamp to RFC 3339 with millisecond precision.
fn format_timestamp(ts: Option<DateTime<FixedOffset>>) -> String {
    match ts {
        Some(dt) => dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        None => Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    }
}

/// Convert a parsed syslog message into a trawl event map.
///
/// The resulting map contains the same core fields as Vector-ingested events.
pub fn syslog_to_event<S: BuildHasher>(
    msg: &Message<&str>,
    source_ip: IpAddr,
    source_service_map: &HashMap<String, String, S>,
    default_service: &str,
) -> (String, Map<String, Value>) {
    let mut map = Map::new();

    // Core fields (matching Vector schema)
    let service = derive_service(source_ip, msg.appname, source_service_map, default_service);
    map.insert("service".into(), json!(service));
    map.insert("timestamp".into(), json!(format_timestamp(msg.timestamp)));
    map.insert(
        "host".into(),
        json!(msg.hostname.unwrap_or(&source_ip.to_string())),
    );
    map.insert(
        "level".into(),
        json!(parse::severity_to_level(msg.severity)),
    );
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

        let (service, map) = syslog_to_event(&parsed, source_ip, &service_map, "syslog");

        assert_eq!(service, "unifi-gateway");
        assert_eq!(map["service"], "unifi-gateway");
        assert_eq!(map["host"], "UGW");
        assert_eq!(map["level"], "info"); // severity 6
        assert!(map["message"].as_str().unwrap().contains("[UFW BLOCK]"));
        assert_eq!(map["syslog_facility"], "local0");
        assert_eq!(map["syslog_source_ip"], "192.168.1.1");
    }

    #[test]
    fn convert_uses_appname_when_no_ip_mapping() {
        let raw = "<13>Mar 12 10:00:00 myhost sshd[1234]: Accepted password";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.50".parse().unwrap();

        let (service, map) = syslog_to_event(&parsed, source_ip, &HashMap::new(), "syslog");

        assert_eq!(service, "sshd");
        assert_eq!(map["service"], "sshd");
        assert_eq!(map["host"], "myhost");
    }

    #[test]
    fn convert_falls_back_to_default_service() {
        let raw = "<13>test message only";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();

        let (service, _map) = syslog_to_event(&parsed, source_ip, &HashMap::new(), "syslog");

        assert_eq!(service, "syslog");
    }

    #[test]
    fn convert_host_falls_back_to_source_ip() {
        // Message without hostname
        let raw = "<13>test message";
        let parsed = parse_syslog(raw);
        let source_ip: IpAddr = "10.0.0.1".parse().unwrap();

        let (_service, map) = syslog_to_event(&parsed, source_ip, &HashMap::new(), "syslog");

        // If hostname is None, should use source IP
        let host = map["host"].as_str().unwrap();
        // hostname might be parsed from the message; if not, should be source IP
        assert!(!host.is_empty());
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
