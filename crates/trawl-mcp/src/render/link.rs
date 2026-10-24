// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL: a link that opens Search with the executed query and
//! its bounds (ADR-0058, ADR-0027 as amended 2026-10-22).
//!
//! The link is built by the shared codec, never by hand. `q` carries the
//! executed text, which already holds the exact microsecond `earliest=`
//! and `latest=` the capture resolved, so Search's merge of the range can
//! only intersect them and never widen them. `r` carries the whole-second
//! hull of those bounds: the earliest floored and the latest ceiled to
//! the second. `page` is 0; `f` and `mode` are left out. The origin is
//! trawld's `[server] link_origin`, as the capabilities route reports it;
//! nothing a request carries chooses it.
//!
//! There is no link, and [`Link::reason`] says why, when no origin is
//! configured, when the result has no bounds (a stored source), when the
//! codec's own reader would refuse the link, or when the whole link is
//! longer than [`MAX_LINK_BYTES`].

use chrono::{DateTime, DurationRound, SecondsFormat, TimeDelta, Utc};
use trawl_client::CapturedResult;
use trawl_search_url::query_merge::RangeSpec;
use trawl_search_url::search_url::{MAX_SEARCH_BYTES, Mode, admit_search, build_search_url};

use crate::reply::Link;

/// The longest whole link, origin included: the codec's limit on what
/// Search reads.
pub const MAX_LINK_BYTES: usize = MAX_SEARCH_BYTES;

/// The reason when trawld has no `[server] link_origin`.
pub const NO_ORIGIN: &str =
    "trawld has no [server] link_origin configured, so no link to Search is built";
/// The reason when the result has no bounds.
pub const NO_BOUNDS: &str =
    "the result was read from stored report rows (from saved), which have no bounds Search can run";
/// The reason when the result's bounds do not parse as instants.
pub const UNREADABLE_BOUNDS: &str = "the result's bounds did not parse as RFC 3339 instants";
/// The reason when the codec's reader would refuse the link.
pub const REFUSED: &str =
    "Search would refuse the link: its query string is over the 32 KiB or 64-parameter limit";
/// The reason when the whole link is longer than [`MAX_LINK_BYTES`].
pub const TOO_LONG: &str = "the link would be longer than 32 KiB with its origin";

/// The search URL for `result` under `link_origin`, or the reason there is
/// none.
#[must_use]
pub fn search_link(link_origin: Option<&str>, result: &CapturedResult) -> Link {
    let reason = |text: &str| Link {
        url: None,
        reason: Some(text.to_owned()),
    };
    let Some(origin) = link_origin else {
        return reason(NO_ORIGIN);
    };
    let (Some(earliest), Some(latest)) = (result.earliest.as_deref(), result.latest.as_deref())
    else {
        return reason(NO_BOUNDS);
    };
    let Some((from, to)) = hull(earliest, latest) else {
        return reason(UNREADABLE_BOUNDS);
    };
    let path = build_search_url(
        &result.executed_query,
        0,
        Mode::Snapshot,
        &[],
        &RangeSpec::Absolute { from, to },
    );
    let url = format!("{}{path}", origin.trim_end_matches('/'));
    if admit_search(&url).is_err() {
        return reason(REFUSED);
    }
    if url.len() > MAX_LINK_BYTES {
        return reason(TOO_LONG);
    }
    Link {
        url: Some(url),
        reason: None,
    }
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
}
