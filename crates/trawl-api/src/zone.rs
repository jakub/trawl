// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display zone: how a native client shows the UTC instants trawld
//! answers with.
//!
//! The zone is presentation only. The server always answers in UTC, and a
//! client converts each instant as it prints it. `local` is the zone of the
//! machine that runs the client, taken at each instant, so a client follows
//! daylight saving time: the two 02:30s of a fall-back night print with
//! different offsets.
//!
//! The wire spelling of a timestamp is RFC 3339 with six fraction digits,
//! `Z` at offset zero and `±HH:MM` otherwise. `DuckDB` holds years from
//! 290309 BC to 294247 AD, so a year outside 0000-9999 is written in
//! ISO 8601's expanded form, a sign and at least four digits
//! (`+10000-01-01T00:00:00.000000Z`). `DuckDB`'s infinite timestamps spell
//! `infinity` and `-infinity`.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Local, Offset, TimeZone, Utc};

use crate::value::{ColumnType, QueryResult, Value};

/// `DuckDB`'s infinite timestamps as microsecond counts: the two extremes
/// of its count. They are not instants and spell `infinity` and
/// `-infinity` on the wire.
pub const TIMESTAMP_INFINITY: i64 = i64::MAX;
/// See [`TIMESTAMP_INFINITY`].
pub const TIMESTAMP_NEG_INFINITY: i64 = -i64::MAX;

const MICROS_PER_SEC: i128 = 1_000_000;
const MICROS_PER_DAY: i128 = 86_400 * MICROS_PER_SEC;
/// One 400-year Gregorian cycle, exactly 146,097 days, in microseconds.
const CYCLE_MICROS: i128 = 146_097 * MICROS_PER_DAY;

/// Where a native client shows instants: `ui.timezone`, parsed once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayZone {
    /// Offset zero.
    Utc,
    /// The zone of the machine that runs the client, at each instant.
    Local,
    /// One offset for every instant.
    Fixed(FixedOffset),
}

/// A `ui.timezone` value that is not a display zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRefusal {
    value: String,
}

impl fmt::Display for ZoneRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a display zone; use UTC, local, or a fixed offset such as +05:30.",
            self.value
        )
    }
}

impl std::error::Error for ZoneRefusal {}

impl FromStr for DisplayZone {
    type Err = ZoneRefusal;

    /// `UTC` or `local` in any case, or a fixed offset spelled exactly
    /// `±HH:MM` in ASCII digits, hours 00-23 and minutes 00-59. Whitespace
    /// around the value is ignored. Zone names such as `Europe/Berlin` are
    /// refused: `local` already follows the machine's zone rules.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.trim();
        if value.eq_ignore_ascii_case("utc") {
            return Ok(Self::Utc);
        }
        if value.eq_ignore_ascii_case("local") {
            return Ok(Self::Local);
        }
        fixed_offset(value)
            .map(Self::Fixed)
            .ok_or_else(|| ZoneRefusal {
                value: s.to_owned(),
            })
    }
}

/// A strict `±HH:MM`: six ASCII bytes, nothing else.
fn fixed_offset(value: &str) -> Option<FixedOffset> {
    let &[sign, h1, h2, b':', m1, m2] = value.as_bytes() else {
        return None;
    };
    let sign = match sign {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digit = |b: u8| b.is_ascii_digit().then(|| i32::from(b - b'0'));
    let hours = digit(h1)? * 10 + digit(h2)?;
    let minutes = digit(m1)? * 10 + digit(m2)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3_600 + minutes * 60))
}

impl fmt::Display for DisplayZone {
    /// The canonical spelling: `UTC`, `local`, or `±HH:MM`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utc => f.write_str("UTC"),
            Self::Local => f.write_str("local"),
            Self::Fixed(offset) => write!(f, "{offset}"),
        }
    }
}

impl DisplayZone {
    /// The zone's offset at `at`. For `local`, the machine's offset at that
    /// instant, in whole minutes: the wire spells no seconds, and a
    /// historical offset such as a local mean time can carry them.
    #[must_use]
    pub fn offset_at(self, at: DateTime<Utc>) -> FixedOffset {
        match self {
            Self::Utc => utc(),
            Self::Fixed(offset) => offset,
            Self::Local => {
                let secs = Local
                    .offset_from_utc_datetime(&at.naive_utc())
                    .local_minus_utc();
                let minutes = (secs + 30).div_euclid(60);
                FixedOffset::east_opt(minutes * 60).unwrap_or_else(utc)
            }
        }
    }

    /// The offset for an instant `micros` after the epoch, or `None` when
    /// `local` cannot place it: chrono's calendar, which `Local` needs,
    /// ends at ±262,143 years, short of `DuckDB`'s range.
    fn offset_for(self, micros: i64) -> Option<FixedOffset> {
        match self {
            Self::Utc => Some(utc()),
            Self::Fixed(offset) => Some(offset),
            // An infinity is no instant, so no zone has anything to place.
            Self::Local if matches!(micros, TIMESTAMP_INFINITY | TIMESTAMP_NEG_INFINITY) => {
                Some(utc())
            }
            Self::Local => DateTime::from_timestamp_micros(micros).map(|at| self.offset_at(at)),
        }
    }
}

fn utc() -> FixedOffset {
    Utc.fix()
}

/// An instant in the wire's spelling, at `offset`.
///
/// Byte for byte chrono's `to_rfc3339_opts(SecondsFormat::Micros, true)`
/// at a whole-minute offset where chrono's calendar reaches, and the same form for every other
/// count `DuckDB` holds; [`TIMESTAMP_INFINITY`] and
/// [`TIMESTAMP_NEG_INFINITY`] spell `infinity` and `-infinity`. The offset
/// is applied and written in whole minutes, the precision the wire spells.
#[must_use]
pub fn wire_timestamp(micros: i64, offset: FixedOffset) -> String {
    match micros {
        TIMESTAMP_INFINITY => return "infinity".to_owned(),
        TIMESTAMP_NEG_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }
    let civil = Civil::new(micros, offset);
    let offset = if civil.offset_minutes == 0 {
        "Z".to_owned()
    } else {
        civil.offset_text()
    };
    format!(
        "{}T{:02}:{:02}:{:02}.{:06}{offset}",
        civil.date_text(),
        civil.hour,
        civil.minute,
        civil.second,
        civil.micro
    )
}

/// An instant as a terminal table shows it: `2026-10-25 02:30:00 +02:00`,
/// the fraction without its trailing zeros, and the offset always written,
/// `+00:00` included, so each row says which clock it reads.
fn human_timestamp(micros: i64, offset: FixedOffset) -> String {
    match micros {
        TIMESTAMP_INFINITY => return "infinity".to_owned(),
        TIMESTAMP_NEG_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }
    let civil = Civil::new(micros, offset);
    let fraction = format!("{:06}", civil.micro);
    let fraction = fraction.trim_end_matches('0');
    let dot = if fraction.is_empty() { "" } else { "." };
    format!(
        "{} {:02}:{:02}:{:02}{dot}{fraction} {}",
        civil.date_text(),
        civil.hour,
        civil.minute,
        civil.second,
        civil.offset_text()
    )
}

/// An instant's civil reading at an offset, by integer arithmetic over the
/// whole `i64` count, so no count or offset can overflow.
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    micro: u32,
    offset_minutes: i32,
}

impl Civil {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn new(micros: i64, offset: FixedOffset) -> Self {
        let offset_minutes = (offset.local_minus_utc() + 30).div_euclid(60);
        let local = i128::from(micros) + i128::from(offset_minutes) * 60 * MICROS_PER_SEC;
        // |local| < 2^64, so the day count fits an i64 and the time of day
        // a u64.
        let days = local.div_euclid(MICROS_PER_DAY) as i64;
        let of_day = local.rem_euclid(MICROS_PER_DAY) as u64;
        let (year, month, day) = civil_from_days(days);
        let secs = of_day / 1_000_000;
        Self {
            year,
            month,
            day,
            hour: (secs / 3_600) as u32,
            minute: (secs / 60 % 60) as u32,
            second: (secs % 60) as u32,
            micro: (of_day % 1_000_000) as u32,
            offset_minutes,
        }
    }

    /// chrono's year spelling: four digits inside 0000-9999, a sign and at
    /// least four digits outside it.
    fn date_text(&self) -> String {
        let year = if (0..=9_999).contains(&self.year) {
            format!("{:04}", self.year)
        } else {
            format!("{:+05}", self.year)
        };
        format!("{year}-{:02}-{:02}", self.month, self.day)
    }

    fn offset_text(&self) -> String {
        let sign = if self.offset_minutes < 0 { '-' } else { '+' };
        let minutes = self.offset_minutes.unsigned_abs();
        format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day), by
/// Howard Hinnant's `civil_from_days`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = i64::from(yoe) + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The microsecond count a wire timestamp names, or `None` for text that
/// is not one.
///
/// Reads RFC 3339 as chrono does, at any offset and fraction length, and
/// the expanded-year form over it: the year is moved by whole 400-year
/// cycles into chrono's calendar, where the leap rule and every date fall
/// the same, and moved back after. `infinity` and `-infinity` read as
/// [`TIMESTAMP_INFINITY`] and [`TIMESTAMP_NEG_INFINITY`]; a finite text
/// that lands on either count, or past `i64`, is refused.
#[must_use]
pub fn wire_micros(text: &str) -> Option<i64> {
    match text {
        "infinity" => return Some(TIMESTAMP_INFINITY),
        "-infinity" => return Some(TIMESTAMP_NEG_INFINITY),
        _ => {}
    }
    let (sign, unsigned) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => (0, text),
    };
    let (digits, rest) = unsigned.split_once('-')?;
    let well_formed = if sign == 0 {
        digits.len() == 4
    } else {
        // Seven digits reach past `DuckDB`'s range and stay well inside
        // i64.
        (4..=7).contains(&digits.len())
    };
    if !well_formed || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.parse::<i64>().ok()?;
    let year = if sign < 0 { -magnitude } else { magnitude };
    let proxy = 2_000 + year.rem_euclid(400);
    let at = DateTime::parse_from_rfc3339(&format!("{proxy}-{rest}")).ok()?;
    let cycles = i128::from((year - proxy) / 400);
    let micros = i64::try_from(i128::from(at.timestamp_micros()) + cycles * CYCLE_MICROS).ok()?;
    (micros != TIMESTAMP_INFINITY && micros != TIMESTAMP_NEG_INFINITY).then_some(micros)
}

/// A wire timestamp in a display zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presented {
    /// The instant at the zone's offset, or an infinity as it stands.
    Converted(String),
    /// The instant in UTC: `local` cannot place it.
    NotConverted(String),
}

impl Presented {
    /// The text, whichever way it came.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Converted(text) | Self::NotConverted(text) => text,
        }
    }
}

/// What a human surface writes after an instant shown in UTC because the
/// display zone could not place it.
pub const NOT_CONVERTED: &str = "(not converted)";

/// `wire` re-spelled in `zone`'s offset at that instant, or `None` when
/// `wire` is not a wire timestamp ([`wire_micros`]).
#[must_use]
pub fn present(wire: &str, zone: DisplayZone) -> Option<Presented> {
    let micros = wire_micros(wire)?;
    Some(match zone.offset_for(micros) {
        Some(offset) => Presented::Converted(wire_timestamp(micros, offset)),
        None => Presented::NotConverted(wire_timestamp(micros, utc())),
    })
}

/// `wire` as a terminal table shows it in `zone`
/// (`2026-10-25 02:30:00 +02:00`), each instant with its own offset. An
/// instant the zone cannot place shows in UTC followed by
/// [`NOT_CONVERTED`]. `None` when `wire` is not a wire timestamp.
#[must_use]
pub fn present_human(wire: &str, zone: DisplayZone) -> Option<String> {
    let micros = wire_micros(wire)?;
    Some(match zone.offset_for(micros) {
        Some(offset) => human_timestamp(micros, offset),
        None => format!("{} {NOT_CONVERTED}", human_timestamp(micros, utc())),
    })
}

/// A `timestamp` cell, or a list of them at any depth, as a terminal table
/// shows it in `zone`: each instant through [`present_human`], a list the
/// way `Value`'s `Display` writes one. `None` for any other cell.
#[must_use]
pub fn present_cell(ty: &ColumnType, value: &Value, zone: DisplayZone) -> Option<String> {
    match (ty, value) {
        (ColumnType::Timestamp, Value::String(_)) => Some(cell_text(ty, value, zone)),
        (ColumnType::List { items }, Value::Array(_)) if holds_timestamps(items) => {
            Some(cell_text(ty, value, zone))
        }
        _ => None,
    }
}

fn holds_timestamps(ty: &ColumnType) -> bool {
    match ty {
        ColumnType::Timestamp => true,
        ColumnType::List { items } => holds_timestamps(items),
        _ => false,
    }
}

fn cell_text(ty: &ColumnType, value: &Value, zone: DisplayZone) -> String {
    match (ty, value) {
        (ColumnType::Timestamp, Value::String(text)) => {
            present_human(text, zone).unwrap_or_else(|| text.clone())
        }
        (ColumnType::List { items }, Value::Array(elements)) => {
            let shown: Vec<String> = elements
                .iter()
                .map(|element| cell_text(items, element, zone))
                .collect();
            format!("[{}]", shown.join(", "))
        }
        _ => value.to_string(),
    }
}

/// Re-spell every `timestamp` cell of `result`, in lists at any depth too,
/// in `zone`, for machine output such as JSON and CSV.
///
/// Only columns typed `timestamp` change: text that merely looks like a
/// time stays as sent. An instant the zone cannot place keeps its UTC text,
/// which says so with its `Z`; machine output carries no marker a parser
/// would trip on. A cell that is not a wire timestamp stays as it came.
pub fn apply_zone(result: &mut QueryResult, zone: DisplayZone) {
    let QueryResult { columns, rows } = result;
    for row in rows {
        for (column, cell) in columns.iter().zip(row.iter_mut()) {
            apply_to_cell(&column.ty, cell, zone);
        }
    }
}

fn apply_to_cell(ty: &ColumnType, cell: &mut Value, zone: DisplayZone) {
    match (ty, cell) {
        (ColumnType::Timestamp, Value::String(text)) => {
            if let Some(presented) = present(text, zone) {
                *text = match presented {
                    Presented::Converted(text) | Presented::NotConverted(text) => text,
                };
            }
        }
        (ColumnType::List { items }, Value::Array(elements)) => {
            for element in elements {
                apply_to_cell(items, element, zone);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod display_zone_grammar {
    use super::*;

    fn fixed(secs: i32) -> DisplayZone {
        DisplayZone::Fixed(FixedOffset::east_opt(secs).unwrap())
    }

    #[test]
    fn accepts_utc_local_and_strict_offsets() {
        for (text, zone) in [
            ("UTC", DisplayZone::Utc),
            ("utc", DisplayZone::Utc),
            ("Utc", DisplayZone::Utc),
            ("  UTC\t", DisplayZone::Utc),
            ("local", DisplayZone::Local),
            (" local ", DisplayZone::Local),
            ("LOCAL", DisplayZone::Local),
            ("+05:30", fixed(19_800)),
            ("-08:00", fixed(-28_800)),
            ("-00:00", fixed(0)),
            ("+00:00", fixed(0)),
            ("+23:59", fixed(86_340)),
            ("-23:59", fixed(-86_340)),
            (" +05:30 ", fixed(19_800)),
        ] {
            assert_eq!(text.parse::<DisplayZone>(), Ok(zone), "{text:?}");
        }
    }

    #[test]
    fn refuses_everything_else_in_one_sentence_naming_the_value() {
        for text in [
            "+5:30",
            "+05:3",
            "+05:30:00",
            "++05:30",
            "+-5:30",
            "+24:00",
            "+05:60",
            "05:30",
            "+0530",
            "+05-30",
            " +05:3 0",
            "+\u{ff10}5:30",        // fullwidth zero
            "+\u{0660}\u{0665}:30", // Arabic-Indic digits
            "-\u{0660}\u{0665}:\u{0663}\u{0660}",
            "Mars/Olympus",
            "Europe/Berlin",
            "GMT",
            "Z",
            "",
            "   ",
            "\"quoted\"\nnewline",
        ] {
            let refusal = text.parse::<DisplayZone>().unwrap_err();
            let sentence = refusal.to_string();
            assert!(
                sentence.starts_with(&format!("{text:?} ")),
                "{sentence} names {text:?}"
            );
            assert_eq!(
                sentence.matches(". ").count(),
                0,
                "{sentence} is one sentence"
            );
            assert!(sentence.ends_with('.'), "{sentence}");
            assert!(!sentence.contains('\n'), "{sentence} escapes the value");
        }
    }

    #[test]
    fn displays_its_canonical_spelling() {
        for (text, shown) in [
            ("utc", "UTC"),
            (" Local", "local"),
            ("+05:30", "+05:30"),
            ("-00:00", "+00:00"),
            ("-08:00", "-08:00"),
        ] {
            assert_eq!(text.parse::<DisplayZone>().unwrap().to_string(), shown);
        }
    }

    /// The canonical spelling parses back to the same zone.
    #[test]
    fn canonical_spelling_round_trips() {
        for minutes in -(23 * 60 + 59)..=(23 * 60 + 59) {
            let zone = fixed(minutes * 60);
            assert_eq!(zone.to_string().parse::<DisplayZone>(), Ok(zone));
        }
    }
}

#[cfg(test)]
mod typed_timestamp_presentation_domain {
    use super::*;
    use crate::value::Column;

    fn offset(secs: i32) -> FixedOffset {
        FixedOffset::east_opt(secs).unwrap()
    }

    fn fixed(secs: i32) -> DisplayZone {
        DisplayZone::Fixed(offset(secs))
    }

    /// `DuckDB`'s finite extremes, from `test_all_types()`.
    const DUCKDB_MIN: i64 = -9_223_372_022_400_000_000;
    const DUCKDB_MAX: i64 = 9_223_372_036_854_775_806;

    /// The wire spelling the engine has always written, case for case.
    #[test]
    fn wire_spelling_matches_the_engine_cases() {
        let at = 1_705_312_800_000_000; // 2024-01-15T10:00:00Z
        let pre_epoch = -500_000; // 1969-12-31T23:59:59.5Z
        for (micros, secs, expected) in [
            (at, 0, "2024-01-15T10:00:00.000000Z"),
            (at, 19_800, "2024-01-15T15:30:00.000000+05:30"),
            (at, -18_000, "2024-01-15T05:00:00.000000-05:00"),
            (at + 120_450, 0, "2024-01-15T10:00:00.120450Z"),
            (pre_epoch, 0, "1969-12-31T23:59:59.500000Z"),
            (pre_epoch, 3_600, "1970-01-01T00:59:59.500000+01:00"),
            (0, 0, "1970-01-01T00:00:00.000000Z"),
            (1_705_312_800_123_000, 0, "2024-01-15T10:00:00.123000Z"),
            (1_705_312_800_123_456, 0, "2024-01-15T10:00:00.123456Z"),
            (253_402_300_800_000_000, 0, "+10000-01-01T00:00:00.000000Z"),
            (DUCKDB_MIN, 0, "-290308-12-22T00:00:00.000000Z"),
            (DUCKDB_MAX, 0, "+294247-01-10T04:00:54.775806Z"),
            (DUCKDB_MAX, -3_600, "+294247-01-10T03:00:54.775806-01:00"),
        ] {
            assert_eq!(wire_timestamp(micros, offset(secs)), expected, "{micros}");
        }
        for secs in [0, 19_800, -28_800] {
            assert_eq!(wire_timestamp(TIMESTAMP_INFINITY, offset(secs)), "infinity");
            assert_eq!(
                wire_timestamp(TIMESTAMP_NEG_INFINITY, offset(secs)),
                "-infinity"
            );
        }
    }

    /// Wherever chrono's calendar reaches, the spelling is chrono's own.
    #[test]
    fn wire_spelling_is_chronos_inside_its_calendar() {
        let min = DateTime::<Utc>::MIN_UTC.timestamp_micros();
        let max = DateTime::<Utc>::MAX_UTC.timestamp_micros();
        let span = i128::from(max) - i128::from(min);
        let samples =
            (0..=4_000).map(|i| i64::try_from(i128::from(min) + span * i / 4_000).unwrap());
        let edges = [
            min,
            max,
            0,
            -1,
            1,
            -62_167_219_200_000_001, // -0001-12-31T23:59:59.999999Z
            -62_167_219_200_000_000, // 0000-01-01
            253_402_300_799_999_999, // 9999-12-31T23:59:59.999999Z
            253_402_300_800_000_000,
            951_782_400_000_000, // 2000-02-29
        ];
        for micros in samples.chain(edges) {
            let at = DateTime::from_timestamp_micros(micros).unwrap();
            for secs in [0, 19_800, -28_800, 86_340, -86_340] {
                // chrono cannot spell a reading past its own calendar.
                if at
                    .naive_utc()
                    .checked_add_signed(chrono::TimeDelta::seconds(i64::from(secs)))
                    .is_none()
                {
                    continue;
                }
                let local = at.with_timezone(&offset(secs));
                assert_eq!(
                    wire_timestamp(micros, offset(secs)),
                    local.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                    "{micros} at {secs}"
                );
            }
        }
    }

    /// Every count, at every offset, reads back to itself, the `i64`
    /// extremes and the offsets that carry a reading past them included.
    #[test]
    fn wire_spelling_round_trips_across_the_domain() {
        let counts = [
            i64::MIN,
            i64::MIN + 1,
            TIMESTAMP_NEG_INFINITY,
            DUCKDB_MIN,
            -5_000_000_000_000_000_000,
            DateTime::<Utc>::MIN_UTC.timestamp_micros() - 1,
            DateTime::<Utc>::MIN_UTC.timestamp_micros(),
            -62_167_219_200_000_001,
            -500_000,
            0,
            1_705_312_800_123_456,
            253_402_300_800_000_000,
            DateTime::<Utc>::MAX_UTC.timestamp_micros(),
            DateTime::<Utc>::MAX_UTC.timestamp_micros() + 1,
            5_000_000_000_000_000_000,
            DUCKDB_MAX,
            TIMESTAMP_INFINITY - 1,
            TIMESTAMP_INFINITY,
        ];
        for micros in counts {
            for secs in [0, 60, -60, 19_800, -28_800, 86_340, -86_340] {
                let text = wire_timestamp(micros, offset(secs));
                assert_eq!(wire_micros(&text), Some(micros), "{text}");
            }
        }
        // The other wire, without a fraction or with a short one, reads too.
        assert_eq!(
            wire_micros("2026-02-18T12:00:00Z"),
            wire_micros("2026-02-18T12:00:00.000000Z")
        );
        assert_eq!(
            wire_micros("2026-02-18T17:30:00.5+05:30"),
            wire_micros("2026-02-18T12:00:00.500000Z")
        );
    }

    #[test]
    fn wire_micros_refuses_what_is_not_a_wire_timestamp() {
        for text in [
            "",
            "Infinity",
            "+infinity",
            "2024-01-15 10:00:00",
            "24-01-15T10:00:00Z",
            "+999-01-01T00:00:00Z",
            "+10000000-01-01T00:00:00Z",
            "+\u{0661}0000-01-01T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "+10001-02-29T00:00:00Z",         // 10001 is no leap year
            "+300000-01-01T00:00:00Z",        // past i64
            "+294247-01-10T04:00:54.775807Z", // the infinity's count
            "-294247-01-10T04:00:54.775807Z",
            "hello",
        ] {
            assert_eq!(wire_micros(text), None, "{text:?}");
        }
        assert!(wire_micros("+10000-02-29T00:00:00.000000Z").is_some());
    }

    /// A fixed offset converts every timestamp cell, lists at any depth
    /// included, and keeps microseconds, the infinities and expanded
    /// years; a `string` column that looks like time stays as sent.
    #[test]
    fn apply_zone_converts_typed_timestamps_only() {
        let timestamp_list = ColumnType::List {
            items: Box::new(ColumnType::Timestamp),
        };
        let nested = ColumnType::List {
            items: Box::new(timestamp_list.clone()),
        };
        let mut result = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("note", ColumnType::String),
                Column::new("seen", timestamp_list),
                Column::new("nested", nested),
            ],
            rows: vec![
                vec![
                    Value::String("2024-01-15T10:00:00.120450Z".into()),
                    Value::String("2024-01-15T10:00:00.000000Z".into()),
                    Value::Array(vec![
                        Value::String("infinity".into()),
                        Value::String("-infinity".into()),
                        Value::Null,
                    ]),
                    Value::Array(vec![Value::Array(vec![Value::String(
                        "+294247-01-10T04:00:54.775806Z".into(),
                    )])]),
                ],
                vec![
                    Value::String("+10000-01-01T00:00:00.000001Z".into()),
                    Value::String("not a time".into()),
                    Value::Array(vec![]),
                    Value::Null,
                ],
                vec![
                    Value::String("-290308-12-22T00:00:00.000000Z".into()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            ],
        };
        let mut at_minus_eight = result.clone();
        apply_zone(&mut result, fixed(19_800));
        let s = |t: &str| Value::String(t.into());
        assert_eq!(
            result.rows,
            vec![
                vec![
                    s("2024-01-15T15:30:00.120450+05:30"),
                    s("2024-01-15T10:00:00.000000Z"),
                    Value::Array(vec![s("infinity"), s("-infinity"), Value::Null]),
                    Value::Array(vec![Value::Array(vec![s(
                        "+294247-01-10T09:30:54.775806+05:30"
                    )])]),
                ],
                vec![
                    s("+10000-01-01T05:30:00.000001+05:30"),
                    s("not a time"),
                    Value::Array(vec![]),
                    Value::Null,
                ],
                vec![
                    s("-290308-12-22T05:30:00.000000+05:30"),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            ]
        );
        apply_zone(&mut at_minus_eight, fixed(-28_800));
        assert_eq!(
            at_minus_eight.rows[0][0],
            s("2024-01-15T02:00:00.120450-08:00")
        );
        assert_eq!(
            at_minus_eight.rows[1][0],
            s("9999-12-31T16:00:00.000001-08:00")
        );
        assert_eq!(
            at_minus_eight.rows[2][0],
            s("-290308-12-21T16:00:00.000000-08:00")
        );
        assert_eq!(at_minus_eight.rows[0][1], s("2024-01-15T10:00:00.000000Z"));
    }

    /// The human form: each instant with its own offset, the fraction's
    /// trailing zeros gone, the infinities as they stand.
    #[test]
    fn human_text_carries_each_offset() {
        let zone = fixed(19_800);
        for (wire, shown) in [
            ("2024-01-15T10:00:00.000000Z", "2024-01-15 15:30:00 +05:30"),
            (
                "2024-01-15T10:00:00.120450Z",
                "2024-01-15 15:30:00.12045 +05:30",
            ),
            (
                "+294247-01-10T04:00:54.775806Z",
                "+294247-01-10 09:30:54.775806 +05:30",
            ),
            ("infinity", "infinity"),
            ("-infinity", "-infinity"),
        ] {
            assert_eq!(present_human(wire, zone).as_deref(), Some(shown), "{wire}");
        }
        assert_eq!(
            present_human("2024-01-15T15:30:00.000000+05:30", DisplayZone::Utc).as_deref(),
            Some("2024-01-15 10:00:00 +00:00")
        );
        assert_eq!(present_human("2024-01-15 10:00:00", zone), None);
        let list = ColumnType::List {
            items: Box::new(ColumnType::Timestamp),
        };
        assert_eq!(
            present_cell(
                &list,
                &Value::Array(vec![
                    Value::String("2024-01-15T10:00:00.000000Z".into()),
                    Value::Null,
                    Value::String("infinity".into()),
                ]),
                fixed(-28_800),
            )
            .as_deref(),
            // A null element writes nothing, as `Value`'s `Display` has it.
            Some("[2024-01-15 02:00:00 -08:00, , infinity]")
        );
        assert_eq!(
            present_cell(
                &ColumnType::String,
                &Value::String("2024-01-15T10:00:00.000000Z".into()),
                zone
            ),
            None
        );
    }

    /// `local` places every instant inside chrono's calendar at the
    /// machine's offset for that instant; past it, the instant stays UTC,
    /// marked in human text and plain in machine output. No assumption
    /// about the machine's zone.
    #[test]
    fn local_beyond_chronos_calendar_is_not_converted() {
        let beyond = "+294247-01-10T04:00:54.775806Z";
        assert_eq!(
            present(beyond, DisplayZone::Local),
            Some(Presented::NotConverted(beyond.to_owned()))
        );
        assert_eq!(
            present_human(beyond, DisplayZone::Local).as_deref(),
            Some("+294247-01-10 04:00:54.775806 +00:00 (not converted)")
        );
        let mut result = QueryResult {
            columns: vec![Column::new("_time", ColumnType::Timestamp)],
            rows: vec![vec![Value::String(beyond.into())]],
        };
        apply_zone(&mut result, DisplayZone::Local);
        assert_eq!(result.rows[0][0], Value::String(beyond.into()));

        for micros in [
            DateTime::<Utc>::MIN_UTC.timestamp_micros(),
            -62_167_219_200_000_001,
            0,
            1_761_352_200_000_000, // 2026-10-25T00:30:00Z
            DateTime::<Utc>::MAX_UTC.timestamp_micros(),
        ] {
            let at = DateTime::from_timestamp_micros(micros).unwrap();
            let expected = wire_timestamp(micros, DisplayZone::Local.offset_at(at));
            let wire = wire_timestamp(micros, utc());
            assert_eq!(
                present(&wire, DisplayZone::Local),
                Some(Presented::Converted(expected.clone())),
                "{wire}"
            );
            assert_eq!(wire_micros(&expected), Some(micros), "{expected}");
        }
        // Two instants an hour apart, across a fall-back night where the
        // machine has one, stay two readings: each carries its own offset.
        let first = present_human("2026-10-25T00:30:00.000000Z", DisplayZone::Local);
        let second = present_human("2026-10-25T01:30:00.000000Z", DisplayZone::Local);
        assert_ne!(first, second);
        assert_eq!(
            present("infinity", DisplayZone::Local),
            Some(Presented::Converted("infinity".to_owned()))
        );
    }
}
