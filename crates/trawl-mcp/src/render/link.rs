// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL: a link that opens Search with the executed query and
//! its bounds (ADR-0058, ADR-0027 as amended 2026-10-22).
//!
//! The path is built by the shared codec's `result_search_path`, never by
//! hand: `q` carries the executed text, which already holds the exact
//! microsecond `earliest=` and `latest=` the capture resolved, and `r` the
//! whole-second hull of those bounds. This module adds the origin, which
//! is trawld's `[server] link_origin`, as the capabilities route reports it;
//! nothing a request carries chooses it.
//!
//! There is no link, and [`Link::reason`] says why, when no origin is
//! configured, when the result has no bounds (a stored source), when the
//! codec's own reader would refuse the link, or when the whole link is
//! longer than [`MAX_LINK_BYTES`].

use trawl_client::CapturedResult;
use trawl_search_url::result_path::{ResultPathRefusal, result_search_path};
use trawl_search_url::search_url::MAX_SEARCH_BYTES;

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
    let path = match result_search_path(&result.executed_query, earliest, latest) {
        Ok(path) => path,
        Err(ResultPathRefusal::UnreadableBounds) => return reason(UNREADABLE_BOUNDS),
        Err(ResultPathRefusal::Refused(_)) => return reason(REFUSED),
    };
    let url = format!("{}{path}", origin.trim_end_matches('/'));
    if url.len() > MAX_LINK_BYTES {
        return reason(TOO_LONG);
    }
    Link {
        url: Some(url),
        reason: None,
    }
}
