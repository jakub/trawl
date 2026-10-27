// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Search path of a captured result (ADR-0058, ADR-0027 as amended
//! 2026-10-22).
//!
//! A capture resolves its bounds once, on the server, and records the text
//! that ran with exact microsecond `earliest=` and `latest=` clauses
//! spliced in. [`result_search_path`] turns that text and those bounds into
//! the Search path that reopens the result. `q` carries the executed text
//! unchanged, so Search's merge of the range can only intersect the exact
//! bounds and never widen them. `r` carries the whole-second hull of the
//! bounds: the earliest floored and the latest ceiled to the second, in
//! the canonical form the codec reads. `page` is 0; `f` and `mode` are
//! left out.
//!
//! The origin and any policy on the whole link's length belong to the
//! caller; this module builds the path and asks the codec's own reader
//! whether it would be read back.

use chrono::{DateTime, DurationRound, SecondsFormat, TimeDelta, Utc};

use crate::query_merge::RangeSpec;
use crate::search_url::{Mode, Reason, admit_search, build_search_url};

/// Why a captured result has no Search path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultPathRefusal {
    /// A bound did not parse as an RFC 3339 instant, or its hull does not
    /// fit the calendar.
    UnreadableBounds,
    /// The codec's own reader would refuse the path.
    Refused(Reason),
}

/// The Search path (`/search?...`) that reopens a captured result: the
/// executed text in `q`, the whole-second hull of `[earliest, latest)` in
/// `r`, page zero.
///
/// # Errors
/// [`ResultPathRefusal::UnreadableBounds`] when a bound is not an RFC 3339
/// instant; [`ResultPathRefusal::Refused`] when the codec's reader would
/// refuse the path (a query over its size limit).
pub fn result_search_path(
    executed_query: &str,
    earliest: &str,
    latest: &str,
) -> Result<String, ResultPathRefusal> {
    let (from, to) = hull(earliest, latest).ok_or(ResultPathRefusal::UnreadableBounds)?;
    let path = build_search_url(
        executed_query,
        0,
        Mode::Snapshot,
        &[],
        &RangeSpec::Absolute { from, to },
    );
    admit_search(&path).map_err(ResultPathRefusal::Refused)?;
    Ok(path)
}

/// The whole-second hull of `[earliest, latest)`: the earliest floored and
/// the latest ceiled to the second, in the canonical UTC form `r` carries.
#[must_use]
pub fn hull(earliest: &str, latest: &str) -> Option<(String, String)> {
    let second = TimeDelta::seconds(1);
    let earliest = instant(earliest)?;
    let latest = instant(latest)?;
    let floor = earliest.duration_trunc(second).ok()?;
    let truncated = latest.duration_trunc(second).ok()?;
    let ceil = if truncated == latest {
        latest
    } else {
        truncated.checked_add_signed(second)?
    };
    Some((canonical(floor), canonical(ceil)))
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

fn canonical(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_url::{MAX_SEARCH_BYTES, SearchRead, Verdict, decode_range, read_search};

    #[test]
    fn the_hull_floors_the_start_and_ceils_the_end() {
        assert_eq!(
            hull("2026-03-14T03:00:00.250000Z", "2026-03-14T04:00:00.750000Z"),
            Some((
                "2026-03-14T03:00:00Z".to_owned(),
                "2026-03-14T04:00:01Z".to_owned()
            ))
        );
    }

    #[test]
    fn whole_seconds_stay_as_they_are() {
        assert_eq!(
            hull("2026-03-14T03:00:00.000000Z", "2026-03-14T04:00:00Z"),
            Some((
                "2026-03-14T03:00:00Z".to_owned(),
                "2026-03-14T04:00:00Z".to_owned()
            ))
        );
    }

    #[test]
    fn an_offset_is_brought_to_utc() {
        assert_eq!(
            hull("2026-03-14T05:00:00.5+02:00", "2026-03-14T06:00:00+02:00"),
            Some((
                "2026-03-14T03:00:00Z".to_owned(),
                "2026-03-14T04:00:00Z".to_owned()
            ))
        );
        assert_eq!(hull("not a time", "2026-03-14T04:00:00Z"), None);
    }

    #[test]
    fn the_path_carries_the_text_in_q_and_the_hull_in_r() {
        let executed = "earliest=\"2026-03-14T03:00:00.250000Z\" latest=\"2026-03-14T04:00:00.750000Z\" host=\"a b\" 100%";
        let path = result_search_path(
            executed,
            "2026-03-14T03:00:00.250000Z",
            "2026-03-14T04:00:00.750000Z",
        )
        .expect("a path");
        assert!(path.starts_with("/search?q="), "{path}");
        let SearchRead::Params(params) = read_search(path.split_once('?').unwrap().1) else {
            panic!("the reader refused the path");
        };
        let value = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("q"), Some(executed));
        assert_eq!(value("page"), Some("0"));
        assert_eq!(value("mode"), None);
        assert_eq!(value("f"), None);
        let Verdict::Valid(RangeSpec::Absolute { from, to }) = decode_range(value("r").expect("r"))
        else {
            panic!("r did not decode");
        };
        assert_eq!(from, "2026-03-14T03:00:00Z");
        assert_eq!(to, "2026-03-14T04:00:01Z");
    }

    #[test]
    fn unreadable_bounds_have_no_path() {
        assert_eq!(
            result_search_path("host=a", "yesterday", "2026-03-14T04:00:00Z"),
            Err(ResultPathRefusal::UnreadableBounds)
        );
        assert_eq!(
            result_search_path("host=a", "2026-03-14T04:00:00Z", ""),
            Err(ResultPathRefusal::UnreadableBounds)
        );
    }

    #[test]
    fn an_overlong_query_is_refused_by_the_reader() {
        let executed = "x".repeat(MAX_SEARCH_BYTES);
        assert_eq!(
            result_search_path(&executed, "2026-03-14T03:00:00Z", "2026-03-14T04:00:00Z"),
            Err(ResultPathRefusal::Refused(Reason::LinkTooLong))
        );
    }
}
