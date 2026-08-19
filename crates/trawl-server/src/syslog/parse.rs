// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Syslog message parsing: RFC 3164 (BSD) and RFC 5424.
//!
//! Wraps [`syslog_loose`] to provide lenient parsing suitable for network
//! appliances that often emit slightly non-conformant syslog.

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use syslog_loose::{Message, ProcId, SyslogFacility, Variant};

/// Parse a raw syslog message string.
///
/// Resolves year-less RFC 3164 timestamps to the year nearest the arrival
/// instant. Auto-detects RFC 3164 vs RFC 5424 format.
pub fn parse_syslog(raw: &str) -> Message<&str> {
    syslog_loose::parse_message_with_year(raw, resolve_year, Variant::Either)
}

/// Year resolver for BSD syslog timestamps that lack a year component.
///
/// Chooses the closest valid candidate among the previous, current and next
/// local year. Equal-distance candidates resolve to the past.
fn resolve_year(date: syslog_loose::IncompleteDate) -> i32 {
    resolve_year_at(date, Utc::now(), Local)
}

fn resolve_year_at<Tz: TimeZone + Copy>(
    (month, day, hour, minute, second): syslog_loose::IncompleteDate,
    arrival: DateTime<Utc>,
    timezone: Tz,
) -> i32 {
    let arrival_year = arrival.with_timezone(&timezone).year();

    [arrival_year - 1, arrival_year, arrival_year + 1]
        .into_iter()
        .filter_map(|year| {
            let candidate = timezone
                .with_ymd_and_hms(year, month, day, hour, minute, second)
                .earliest()?
                .with_timezone(&Utc);
            let distance = candidate
                .signed_duration_since(arrival)
                .num_nanoseconds()?
                .unsigned_abs();
            Some((distance, candidate > arrival, year))
        })
        .min_by_key(|&(distance, is_future, _)| (distance, is_future))
        .map_or(arrival_year, |(_, _, year)| year)
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
    use chrono::FixedOffset;
    use syslog_loose::SyslogSeverity;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn rfc3164_year_is_nearest_to_arrival() {
        let local = FixedOffset::west_opt(7 * 60 * 60).unwrap();
        let cases = [
            (
                "late December arriving in January",
                (12, 31, 23, 59, 0),
                utc(2026, 1, 1, 0, 0),
                2025,
            ),
            (
                "early January arriving in December",
                (1, 1, 0, 1, 0),
                utc(2025, 12, 31, 23, 59),
                2026,
            ),
            (
                "mid-year timestamp",
                (6, 14, 12, 0, 0),
                utc(2025, 6, 15, 12, 0),
                2025,
            ),
            (
                "equal distance resolves to the past",
                (1, 1, 0, 0, 0),
                utc(2025, 7, 2, 19, 0),
                2025,
            ),
        ];

        for (name, date, arrival, expected) in cases {
            assert_eq!(resolve_year_at(date, arrival, local), expected, "{name}");
        }
    }

    #[test]
    fn rfc3164_leap_day_skips_invalid_candidate_years() {
        let local = FixedOffset::west_opt(7 * 60 * 60).unwrap();
        let arrival = utc(2025, 3, 1, 0, 0);
        assert_eq!(resolve_year_at((2, 29, 12, 0, 0), arrival, local), 2024);

        let no_valid_candidate = utc(2101, 3, 1, 0, 0);
        let parsed = syslog_loose::parse_message_with_year(
            "<13>Feb 29 12:00:00 host app: message",
            |date| resolve_year_at(date, no_valid_candidate, local),
            Variant::Either,
        );
        assert!(
            parsed.timestamp.is_none(),
            "an invalid date must stay unparseable for the arrival-time fallback"
        );
    }

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
