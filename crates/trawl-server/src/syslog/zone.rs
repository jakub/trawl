// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The zone a zone-less syslog timestamp is read in (ADR-0050).
//!
//! An RFC 3164 frame carries wall-clock time and no zone. The operator
//! names the zone per peer, and this module turns that wall clock into an
//! instant. Nothing here reads the host's zone: the IANA rules are
//! compiled in through chrono-tz, so every host gives the same answer.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// A configured syslog zone: a fixed offset or an IANA zone.
///
/// `UTC` and a zero offset are both `Fixed(+00:00)`, so they compare equal
/// and print as `UTC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyslogZone {
    /// `UTC` or a `±HH:MM` offset that holds all year.
    Fixed(FixedOffset),
    /// An IANA zone such as `Europe/Warsaw`, with its DST rules.
    Iana(Tz),
}

/// What reading a wall-clock time in a zone gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The wall time names one instant. In a fall-back overlap, this is the
    /// candidate nearer arrival.
    Unique(DateTime<Utc>),
    /// The wall time falls in a spring-forward gap and names no instant.
    Gap,
}

/// A zone setting that is not `UTC`, an IANA zone name or a `±HH:MM`
/// offset. It carries nothing, so no error text can echo the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneRefused;

impl fmt::Display for ZoneRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(ZoneRefused::REASON)
    }
}

impl std::error::Error for ZoneRefused {}

impl ZoneRefused {
    /// The one reason every refused zone gives.
    pub const REASON: &'static str = "not UTC, an IANA zone name, or a ±HH:MM offset";
}

impl SyslogZone {
    /// UTC, the zone an unset `default_timezone` means.
    pub const UTC: Self = match FixedOffset::east_opt(0) {
        Some(offset) => Self::Fixed(offset),
        None => unreachable!(),
    };

    /// `instant` as it reads on a wall clock in this zone.
    #[must_use]
    pub fn wall_at(self, instant: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Self::Fixed(offset) => instant.with_timezone(&offset).naive_local(),
            Self::Iana(tz) => instant.with_timezone(&tz).naive_local(),
        }
    }

    /// The instant `wall` names in this zone.
    ///
    /// A wall time in a fall-back overlap names two instants; the one nearer
    /// `arrival` wins, and an exact tie goes to the earlier one. A wall time
    /// in a spring-forward gap names none.
    #[must_use]
    pub fn instant(self, wall: NaiveDateTime, arrival: DateTime<Utc>) -> Resolved {
        match self {
            Self::Fixed(offset) => resolve_in(&offset, wall, arrival),
            Self::Iana(tz) => resolve_in(&tz, wall, arrival),
        }
    }
}

/// The zone each syslog peer's zone-less timestamps are read in: the
/// peer's `sender_timezones` entry, else `default_timezone`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyslogZones {
    default: SyslogZone,
    /// Keyed by the canonical peer address's spelling, folded like
    /// `source_service_map`.
    by_peer: HashMap<String, SyslogZone>,
}

impl SyslogZones {
    /// `by_peer` must already be folded to canonical peer spellings.
    #[must_use]
    pub fn new(default: SyslogZone, by_peer: HashMap<String, SyslogZone>) -> Self {
        Self { default, by_peer }
    }

    /// The zone for a transport peer, canonicalized by the listener. The
    /// frame's hostname never takes part, and neither does
    /// `trusted_relays`: a relay's entry covers everything it forwards.
    #[must_use]
    pub fn for_peer(&self, peer: IpAddr) -> SyslogZone {
        self.by_peer
            .get(&peer.to_string())
            .copied()
            .unwrap_or(self.default)
    }
}

impl Default for SyslogZones {
    /// Every peer in UTC: the unset configuration.
    fn default() -> Self {
        Self::new(SyslogZone::UTC, HashMap::new())
    }
}

fn resolve_in<T: TimeZone>(zone: &T, wall: NaiveDateTime, arrival: DateTime<Utc>) -> Resolved {
    match zone.from_local_datetime(&wall) {
        LocalResult::Single(t) => Resolved::Unique(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, b) => {
            // Order the candidates here rather than trust the zone's order.
            let (a, b) = (a.with_timezone(&Utc), b.with_timezone(&Utc));
            let (early, late) = (a.min(b), a.max(b));
            if (late - arrival).abs() < (early - arrival).abs() {
                Resolved::Unique(late)
            } else {
                Resolved::Unique(early)
            }
        }
        LocalResult::None => Resolved::Gap,
    }
}

impl FromStr for SyslogZone {
    type Err = ZoneRefused;

    /// Accept exactly `UTC`, a `±HH:MM` offset (hours 00-23, minutes
    /// 00-59), or an IANA name containing `/`, spelled as the zone database
    /// spells it. The `/` rule refuses the database's single-word legacy
    /// names (`EST`, `CET`, `Zulu`, `GMT`, `EST5EDT`), which read as
    /// abbreviations. `local` and every other spelling are refused.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "UTC" {
            return Ok(Self::UTC);
        }
        if let Some(offset) = parse_offset(s) {
            return Ok(Self::Fixed(offset));
        }
        if s.contains('/') {
            return Tz::from_str(s).map(Self::Iana).map_err(|_| ZoneRefused);
        }
        Err(ZoneRefused)
    }
}

/// `±HH:MM`, exactly six bytes.
fn parse_offset(s: &str) -> Option<FixedOffset> {
    let &[sign, h1, h2, b':', m1, m2] = s.as_bytes() else {
        return None;
    };
    let digit = |b: u8| b.is_ascii_digit().then(|| i32::from(b - b'0'));
    let hours = digit(h1)? * 10 + digit(h2)?;
    let minutes = digit(m1)? * 10 + digit(m2)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let seconds = hours * 3600 + minutes * 60;
    match sign {
        b'+' => FixedOffset::east_opt(seconds),
        b'-' => FixedOffset::west_opt(seconds),
        _ => None,
    }
}

impl fmt::Display for SyslogZone {
    /// The normalized spelling: `UTC` for a zero offset, `±HH:MM` for any
    /// other offset, and the IANA name as configured.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(offset) => {
                let seconds = offset.local_minus_utc();
                if seconds == 0 {
                    return f.write_str("UTC");
                }
                let sign = if seconds < 0 { '-' } else { '+' };
                let minutes = seconds.unsigned_abs() / 60;
                write!(f, "{sign}{:02}:{:02}", minutes / 60, minutes % 60)
            }
            Self::Iana(tz) => f.write_str(tz.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn zone(s: &str) -> SyslogZone {
        s.parse().unwrap_or_else(|_| panic!("{s} must parse"))
    }

    fn wall(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        wall(y, mo, d, h, mi).and_utc()
    }

    #[test]
    fn zone_settings_parse_to_one_normalized_spelling() {
        for (given, shown) in [
            ("UTC", "UTC"),
            ("+00:00", "UTC"),
            ("-00:00", "UTC"),
            ("+05:30", "+05:30"),
            ("-03:00", "-03:00"),
            ("+23:59", "+23:59"),
            ("Europe/Warsaw", "Europe/Warsaw"),
            ("America/Chicago", "America/Chicago"),
            ("Australia/Lord_Howe", "Australia/Lord_Howe"),
            // An IANA link stays as written; it is not canonicalized.
            ("US/Central", "US/Central"),
            // `Etc/` names are IANA names, inverted sign and all.
            ("Etc/GMT-5", "Etc/GMT-5"),
        ] {
            assert_eq!(zone(given).to_string(), shown, "{given}");
        }
        assert_eq!(zone("+00:00"), SyslogZone::UTC);
        assert_eq!(zone("-00:00"), zone("UTC"));
    }

    #[test]
    fn zone_settings_refuse_host_zones_abbreviations_and_unknown_names() {
        for refused in [
            "",
            "local",
            "Local",
            "utc",
            "Utc",
            "EST",
            "CET",
            "GMT",
            "Zulu",
            "Japan",
            "EST5EDT",
            "CST6CDT,M3.2.0/2,M11.1.0/2",
            "europe/warsaw",
            "Not/AZone",
            "+5:30",
            "+05:3",
            "+0530",
            "05:30",
            "+24:00",
            "+05:60",
            " UTC",
            "UTC ",
            "＋05:30",
        ] {
            assert_eq!(
                refused.parse::<SyslogZone>(),
                Err(ZoneRefused),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn dst_overlap_picks_nearest_arrival() {
        // (zone, overlap wall time, earlier instant, later instant).
        let cases = [
            // America/Chicago falls back from CDT (-05:00) to CST (-06:00).
            (
                "America/Chicago",
                wall(2026, 11, 1, 1, 30),
                utc(2026, 11, 1, 6, 30),
                utc(2026, 11, 1, 7, 30),
            ),
            // Southern hemisphere: AEDT (+11:00) back to AEST (+10:00).
            (
                "Australia/Sydney",
                wall(2026, 4, 5, 2, 30),
                utc(2026, 4, 4, 15, 30),
                utc(2026, 4, 4, 16, 30),
            ),
            // A 30-minute shift: +11:00 back to +10:30.
            (
                "Australia/Lord_Howe",
                wall(2026, 4, 5, 1, 45),
                utc(2026, 4, 4, 14, 45),
                utc(2026, 4, 4, 15, 15),
            ),
        ];
        for (name, at, early, late) in cases {
            let z = zone(name);
            let midpoint = early + (late - early) / 2;
            let minute = chrono::TimeDelta::minutes(1);
            assert_eq!(
                z.instant(at, midpoint),
                Resolved::Unique(early),
                "{name}: an exact tie takes the earlier instant"
            );
            assert_eq!(
                z.instant(at, midpoint - minute),
                Resolved::Unique(early),
                "{name}: arrival nearer the earlier instant"
            );
            assert_eq!(
                z.instant(at, midpoint + minute),
                Resolved::Unique(late),
                "{name}: arrival nearer the later instant"
            );
            assert_eq!(
                z.instant(at, late + chrono::TimeDelta::days(30)),
                Resolved::Unique(late),
                "{name}: a distant later arrival"
            );
            assert_eq!(
                z.instant(at, early - chrono::TimeDelta::days(30)),
                Resolved::Unique(early),
                "{name}: a distant earlier arrival"
            );
            // Both instants read back as the same wall time.
            assert_eq!(z.wall_at(early), at, "{name}");
            assert_eq!(z.wall_at(late), at, "{name}");
        }
    }

    #[test]
    fn dst_gap_names_no_instant() {
        let arrival = utc(2026, 3, 8, 18, 0);
        assert_eq!(
            zone("America/Chicago").instant(wall(2026, 3, 8, 2, 30), arrival),
            Resolved::Gap
        );
        // Lord Howe springs forward 30 minutes, 02:00 to 02:30.
        assert_eq!(
            zone("Australia/Lord_Howe").instant(wall(2026, 10, 4, 2, 15), arrival),
            Resolved::Gap
        );
        // Either side of the gap is an ordinary instant.
        assert_eq!(
            zone("America/Chicago").instant(wall(2026, 3, 8, 1, 59), arrival),
            Resolved::Unique(utc(2026, 3, 8, 7, 59))
        );
        assert_eq!(
            zone("America/Chicago").instant(wall(2026, 3, 8, 3, 0), arrival),
            Resolved::Unique(utc(2026, 3, 8, 8, 0))
        );
    }

    #[test]
    fn a_fixed_offset_reads_every_wall_time_once() {
        let z = zone("+05:30");
        let arrival = utc(2026, 1, 15, 0, 0);
        assert_eq!(
            z.instant(wall(2026, 1, 15, 12, 0), arrival),
            Resolved::Unique(utc(2026, 1, 15, 6, 30))
        );
        assert_eq!(z.wall_at(utc(2026, 1, 15, 6, 30)), wall(2026, 1, 15, 12, 0));
        assert_eq!(
            SyslogZone::UTC.instant(wall(2026, 3, 8, 2, 30), arrival),
            Resolved::Unique(utc(2026, 3, 8, 2, 30))
        );
    }
}
