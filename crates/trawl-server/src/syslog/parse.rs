// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Syslog message parsing: RFC 3164 (BSD) and RFC 5424.
//!
//! Wraps [`syslog_loose`] to provide lenient parsing suitable for network
//! appliances that often emit slightly non-conformant syslog.

use chrono::{Datelike, Utc};
use syslog_loose::{Message, ProcId, SyslogFacility, Variant};

/// Parse a raw syslog message string.
///
/// Uses the current year for RFC 3164 messages that lack a year in
/// their timestamp. Auto-detects RFC 3164 vs RFC 5424 format.
pub fn parse_syslog(raw: &str) -> Message<&str> {
    syslog_loose::parse_message_with_year(raw, resolve_year, Variant::Either)
}

/// Year resolver for BSD syslog timestamps that lack a year component.
///
/// Simply returns the current UTC year — good enough for real-time
/// ingestion. Logs from late December parsed in early January could
/// get the wrong year, but this is acceptable for a homelab tool.
fn resolve_year(_: syslog_loose::IncompleteDate) -> i32 {
    Utc::now().year()
}

/// Map syslog severity to trawl's level vocabulary.
///
/// Map syslog facility to a human-readable string.
pub fn facility_to_str(facility: Option<SyslogFacility>) -> Option<&'static str> {
    Some(match facility? {
        SyslogFacility::LOG_KERN => "kern",
        SyslogFacility::LOG_USER => "user",
        SyslogFacility::LOG_MAIL => "mail",
        SyslogFacility::LOG_DAEMON => "daemon",
        SyslogFacility::LOG_AUTH => "auth",
        SyslogFacility::LOG_SYSLOG => "syslog",
        SyslogFacility::LOG_LPR => "lpr",
        SyslogFacility::LOG_NEWS => "news",
        SyslogFacility::LOG_UUCP => "uucp",
        SyslogFacility::LOG_CRON => "cron",
        SyslogFacility::LOG_AUTHPRIV => "authpriv",
        SyslogFacility::LOG_FTP => "ftp",
        SyslogFacility::LOG_NTP => "ntp",
        SyslogFacility::LOG_AUDIT => "audit",
        SyslogFacility::LOG_ALERT => "alert",
        SyslogFacility::LOG_CLOCKD => "clockd",
        SyslogFacility::LOG_LOCAL0 => "local0",
        SyslogFacility::LOG_LOCAL1 => "local1",
        SyslogFacility::LOG_LOCAL2 => "local2",
        SyslogFacility::LOG_LOCAL3 => "local3",
        SyslogFacility::LOG_LOCAL4 => "local4",
        SyslogFacility::LOG_LOCAL5 => "local5",
        SyslogFacility::LOG_LOCAL6 => "local6",
        SyslogFacility::LOG_LOCAL7 => "local7",
    })
}

/// Extract process ID as a string from syslog `ProcId`.
pub fn procid_to_string(procid: &Option<ProcId<&str>>) -> Option<String> {
    match procid {
        Some(ProcId::PID(pid)) => Some(pid.to_string()),
        Some(ProcId::Name(name)) => Some((*name).to_string()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syslog_loose::SyslogSeverity;

    #[test]
    fn parse_rfc3164_unifi_style() {
        let msg = "<134>Mar 12 10:00:00 UGW kernel: [UFW BLOCK] IN=eth0 SRC=192.168.1.100";
        let parsed = parse_syslog(msg);

        assert_eq!(parsed.hostname, Some("UGW"));
        assert_eq!(parsed.appname, Some("kernel"));
        assert!(parsed.msg.contains("[UFW BLOCK]"));
        assert_eq!(
            parsed.severity,
            Some(SyslogSeverity::SEV_INFO) // 134 = facility 16 (local0) * 8 + severity 6 (info)
        );
        assert_eq!(parsed.facility, Some(SyslogFacility::LOG_LOCAL0));
    }

    #[test]
    fn parse_rfc5424() {
        let msg = "<165>1 2026-03-12T10:00:00.000Z router1 nginx 1234 - - GET /api/v1/health";
        let parsed = parse_syslog(msg);

        assert_eq!(parsed.hostname, Some("router1"));
        assert_eq!(parsed.appname, Some("nginx"));
        assert!(parsed.msg.contains("GET /api/v1/health"));
        assert_eq!(parsed.severity, Some(SyslogSeverity::SEV_NOTICE));
    }

    #[test]
    fn facility_mapping() {
        assert_eq!(
            facility_to_str(Some(SyslogFacility::LOG_KERN)),
            Some("kern")
        );
        assert_eq!(
            facility_to_str(Some(SyslogFacility::LOG_LOCAL0)),
            Some("local0")
        );
        assert_eq!(facility_to_str(None), None);
    }

    #[test]
    fn parse_minimal_bsd() {
        // Some appliances send very minimal syslog
        let msg = "<13>test message without hostname";
        let parsed = parse_syslog(msg);
        assert!(parsed.msg.contains("test message"));
    }
}
