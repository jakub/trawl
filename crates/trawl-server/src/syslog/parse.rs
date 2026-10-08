// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Syslog message parsing: RFC 3164 (BSD) and RFC 5424.
//!
//! Wraps [`syslog_loose`] to provide lenient parsing suitable for network
//! appliances that often emit slightly non-conformant syslog.
//!
//! A zone-less RFC 3164 timestamp is read in the peer's configured zone,
//! never the host's (ADR-0050). `syslog_loose` is always handed UTC, so the
//! wall-clock fields come back unshifted, and this module reads them in
//! the zone. Whether a timestamp was zone-less is decided from the form
//! that parsed, never from the parsed offset: a wire `Z` and a supplied
//! UTC look identical once parsed.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};
use syslog_loose::{Message, ProcId, Protocol, SyslogFacility, Variant};

use super::zone::{Resolved, SyslogZone};

/// A parsed frame and the zone its timestamp was read in.
#[derive(Debug)]
pub struct ParsedFrame<'a> {
    /// The parsed message. A zone-less timestamp has been read in the zone
    /// already; one in a spring-forward gap is `None`.
    pub msg: Message<&'a str>,
    /// The zone a zone-less timestamp was read in, gap included. `None`
    /// when the timestamp carried its own offset or did not parse.
    pub read_in: Option<SyslogZone>,
}

/// Parse a raw syslog message string, reading a zone-less timestamp in
/// `zone`. Auto-detects RFC 3164 vs RFC 5424 format.
///
/// A year-less RFC 3164 timestamp takes the year nearest `arrival` on the
/// calendar of `zone` ([`resolve_year`]). A wall time in a fall-back
/// overlap takes the instant nearer `arrival`, and one in a spring-forward
/// gap leaves the timestamp `None` while every other field stays parsed.
pub fn parse_syslog(raw: &str, arrival: DateTime<Utc>, zone: SyslogZone) -> ParsedFrame<'_> {
    let mut msg = syslog_loose::parse_message_with_year_tz(
        raw,
        |date| resolve_year(date, arrival, zone),
        Some(Utc),
        Variant::Either,
    );
    let read_in = match msg.timestamp {
        Some(ts) if msg.protocol == Protocol::RFC3164 && zone_less_3164(raw) => {
            // Parsed in UTC, so the instant's UTC fields are the wire's
            // wall clock.
            msg.timestamp = match zone.instant(ts.naive_utc(), arrival) {
                Resolved::Unique(instant) => Some(instant.fixed_offset()),
                Resolved::Gap => None,
            };
            Some(zone)
        }
        _ => None,
    };
    ParsedFrame { msg, read_in }
}

/// Whether a frame whose RFC 3164 timestamp parsed carries it in a
/// zone-less form: year-less `MMM DD HH:MM:SS` or `MMM DD YYYY HH:MM:SS`.
/// Both start with a month name, and the RFC 3339 form starts with a digit.
///
/// This re-reads only the prefix `syslog_loose`'s 3164 grammar puts before
/// the timestamp: `str::trim`, an optional `<digits>` PRI, then spaces and
/// tabs. A timestamp that parsed means a leading `<` was a whole PRI.
fn zone_less_3164(raw: &str) -> bool {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut rest = raw.trim();
    if let Some(after) = rest.strip_prefix('<') {
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && let Some(after) = after[digits..].strip_prefix('>')
        {
            rest = after;
        }
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    rest.get(..3)
        .is_some_and(|word| MONTHS.iter().any(|month| month.eq_ignore_ascii_case(word)))
}

/// The year of a year-less timestamp: the previous, current or next year
/// of `arrival` as it reads on `zone`'s wall clock, whichever puts the
/// wall time nearest that wall clock on the calendar. Only a date the
/// calendar rejects (Feb 29 outside a leap year) drops out; a DST gap does
/// not. An exact tie goes to the past. With no valid candidate the
/// arrival year comes back and the timestamp stays unparsed.
fn resolve_year(
    (month, day, hour, minute, second): syslog_loose::IncompleteDate,
    arrival: DateTime<Utc>,
    zone: SyslogZone,
) -> i32 {
    let arrival_wall = zone.wall_at(arrival);
    let arrival_year = arrival_wall.year();

    [arrival_year - 1, arrival_year, arrival_year + 1]
        .into_iter()
        .filter_map(|year| {
            let candidate: NaiveDateTime =
                NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second)?;
            let distance = (candidate - arrival_wall).num_nanoseconds()?.unsigned_abs();
            Some((distance, candidate > arrival_wall, year))
        })
        .min_by_key(|&(distance, is_future, _)| (distance, is_future))
        .map_or(arrival_year, |(_, _, year)| year)
}

/// Map a syslog facility to its conventional name (`kern`, `local0`, …).
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
    use chrono::TimeZone;
    use syslog_loose::SyslogSeverity;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn zone(name: &str) -> SyslogZone {
        name.parse().unwrap()
    }

    fn instant(frame: &ParsedFrame<'_>) -> Option<DateTime<Utc>> {
        frame.msg.timestamp.map(|ts| ts.with_timezone(&Utc))
    }

    #[test]
    fn rfc3164_year_is_nearest_to_arrival() {
        let local = zone("-07:00");
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
            assert_eq!(resolve_year(date, arrival, local), expected, "{name}");
        }
    }

    #[test]
    fn rfc3164_leap_day_skips_invalid_candidate_years() {
        let local = zone("-07:00");
        let arrival = utc(2025, 3, 1, 0, 0);
        assert_eq!(resolve_year((2, 29, 12, 0, 0), arrival, local), 2024);

        let no_valid_candidate = utc(2101, 3, 1, 0, 0);
        let parsed = parse_syslog(
            "<13>Feb 29 12:00:00 host app: message",
            no_valid_candidate,
            local,
        );
        assert!(
            parsed.msg.timestamp.is_none(),
            "an invalid date must stay unparseable for the arrival-time fallback"
        );
        assert_eq!(parsed.read_in, None, "nothing was read in a zone");
    }

    /// The year is chosen on the calendar, by wall-clock distance from
    /// arrival as arrival reads in the peer's zone (ADR-0050), not by the
    /// distance between candidate instants. Here the two disagree.
    #[test]
    fn year_ranks_by_calendar_wall_time() {
        let chicago = zone("America/Chicago");
        // 12:30 CDT. On the calendar, 2026-01-01 00:00 is 182d 11h30m
        // ahead and 2025-01-01 00:00 is 182d 12h30m behind. As instants
        // (CST, -06:00), the past candidate is the nearer one.
        let arrival = utc(2025, 7, 2, 17, 30);
        assert_eq!(resolve_year((1, 1, 0, 0, 0), arrival, chicago), 2026);

        let parsed = parse_syslog("<13>Jan  1 00:00:00 host app: x", arrival, chicago);
        assert_eq!(instant(&parsed), Some(utc(2026, 1, 1, 6, 0)));
        assert_eq!(parsed.read_in, Some(chicago));
    }

    /// Under the old instant rule a gap wall time dropped its own year:
    /// `Mar  8 02:30` arriving on 2026-03-08 in Chicago resolved to 2027.
    #[test]
    fn gap_wall_time_does_not_skip_a_year() {
        let chicago = zone("America/Chicago");
        let arrival = utc(2026, 3, 8, 18, 0);
        assert_eq!(resolve_year((3, 8, 2, 30, 0), arrival, chicago), 2026);

        let parsed = parse_syslog("<13>Mar  8 02:30:00 host app: x", arrival, chicago);
        assert_eq!(
            parsed.msg.timestamp, None,
            "a gap wall time names no instant in 2026 or any other year"
        );
        assert_eq!(parsed.read_in, Some(chicago));
        assert_eq!(parsed.msg.hostname, Some("host"));
        assert_eq!(parsed.msg.appname, Some("app"));
        assert_eq!(parsed.msg.severity, Some(SyslogSeverity::SEV_NOTICE));
        assert_eq!(parsed.msg.msg, "x");
    }

    /// With a zone supplied, `syslog_loose` reads the `MMM DD YYYY` form's
    /// wall clock as UTC. trawld supplies UTC and re-reads the wall clock
    /// in the peer's zone, so the defect cannot shift the instant.
    #[test]
    fn with_year_3164_is_not_shifted() {
        let arrival = utc(2026, 1, 15, 20, 0);
        for (name, expected) in [
            ("America/Chicago", utc(2026, 1, 15, 18, 0)),
            ("+05:30", utc(2026, 1, 15, 6, 30)),
            ("UTC", utc(2026, 1, 15, 12, 0)),
        ] {
            let parsed = parse_syslog("<13>Jan 15 2026 12:00:00 host app: x", arrival, zone(name));
            assert_eq!(instant(&parsed), Some(expected), "{name}");
            assert_eq!(parsed.read_in, Some(zone(name)), "{name}");
            assert_eq!(parsed.msg.hostname, Some("host"), "{name}");
        }
        // Summer in Chicago is CDT, -05:00.
        let parsed = parse_syslog(
            "<13>Jul 15 2026 12:00:00 host app: x",
            arrival,
            zone("America/Chicago"),
        );
        assert_eq!(instant(&parsed), Some(utc(2026, 7, 15, 17, 0)));
    }

    /// RFC 5424 requires an offset. A frame without one fails the RFC 3339
    /// parse and loses its whole header, as before ADR-0050. That salvage
    /// is a separate design; this pins the behavior so a change is
    /// deliberate.
    #[test]
    fn offsetless_rfc5424_remains_unsalvaged() {
        let raw = "<165>1 2026-03-12T10:00:00 host app - - - msg";
        let parsed = parse_syslog(raw, utc(2026, 3, 12, 10, 0), zone("America/Chicago"));
        assert_eq!(parsed.msg.timestamp, None);
        assert_eq!(parsed.msg.hostname, None);
        assert_eq!(parsed.msg.appname, None);
        assert_eq!(parsed.msg.severity, None);
        assert_eq!(parsed.msg.msg, raw);
        assert_eq!(parsed.read_in, None);
    }

    /// The zone-less decision comes from the form that parsed, through the
    /// real parser: every 3164 form that starts with a month name takes the
    /// zone, and every form carrying an offset keeps it. A `syslog_loose`
    /// grammar change that breaks the prefix scan fails here.
    #[test]
    fn zone_less_is_decided_by_the_timestamp_form() {
        let india = zone("+05:30");
        let arrival = utc(2026, 10, 2, 12, 0);
        let read_in_india = Some(utc(2026, 10, 2, 8, 30));
        let cases = [
            ("<13>Oct  2 14:00:00 host app: x", read_in_india, true),
            ("<13>Oct 2 14:00:00 host app: x", read_in_india, true),
            ("<13>Oct  2 2026 14:00:00 host app: x", read_in_india, true),
            ("Oct  2 14:00:00 host app: x", read_in_india, true),
            ("  <13>Oct  2 14:00:00 host app: x  ", read_in_india, true),
            ("<13> \tOct  2 14:00:00 host app: x", read_in_india, true),
            ("<13>OCT  2 14:00:00 host app: x", read_in_india, true),
            ("<13>oct  2 14:00:00 host app: x", read_in_india, true),
            // An RFC 3339 timestamp inside 3164 carries its own offset,
            // `Z` included, which looks exactly like a supplied UTC.
            (
                "<13>2026-10-02T14:00:00Z host app: x",
                Some(utc(2026, 10, 2, 14, 0)),
                false,
            ),
            (
                "<13>2026-10-02T14:00:00+05:30 host app: x",
                Some(utc(2026, 10, 2, 8, 30)),
                false,
            ),
            (
                "<13>2026-10-02T14:00:00-05:00 host app: x",
                Some(utc(2026, 10, 2, 19, 0)),
                false,
            ),
            (
                "<165>1 2026-10-02T14:00:00Z host app - - - x",
                Some(utc(2026, 10, 2, 14, 0)),
                false,
            ),
            (
                "<165>1 2026-10-02T14:00:00+05:30 host app - - - x",
                Some(utc(2026, 10, 2, 8, 30)),
                false,
            ),
            ("<13>no timestamp here", None, false),
            ("<165>1 2026-10-02T14:00:00 host app - - - x", None, false),
        ];
        for (raw, expected, zone_less) in cases {
            let parsed = parse_syslog(raw, arrival, india);
            assert_eq!(instant(&parsed), expected, "{raw:?}");
            assert_eq!(parsed.read_in, zone_less.then_some(india), "{raw:?}");
        }
    }

    #[test]
    fn rfc3164_parser_uses_supplied_arrival() {
        let parsed = parse_syslog(
            "<13>Dec 31 23:59:00 host app: message",
            utc(2026, 1, 1, 0, 0),
            SyslogZone::UTC,
        );

        assert_eq!(parsed.msg.timestamp.unwrap().year(), 2025);
    }

    #[test]
    fn parse_rfc3164_unifi_style() {
        let msg = "<134>Mar 12 10:00:00 UGW kernel: [UFW BLOCK] IN=eth0 SRC=192.168.1.100";
        let parsed = parse_syslog(msg, utc(2026, 3, 12, 10, 0), SyslogZone::UTC).msg;

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
        let parsed = parse_syslog(msg, utc(2026, 3, 12, 10, 0), SyslogZone::UTC).msg;

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
        let parsed = parse_syslog(msg, utc(2026, 3, 12, 10, 0), SyslogZone::UTC).msg;
        assert!(parsed.msg.contains("test message"));
    }
}
