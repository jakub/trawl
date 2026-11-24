// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL: a link that opens Search with the executed query and
//! its bounds (ADR-0058, ADR-0027).

use trawl_client::CapturedResult;
use trawl_search_url::result_path::{ResultPathRefusal, result_search_path};
use trawl_search_url::search_url::MAX_SEARCH_BYTES;

use crate::reply::Link;

pub const MAX_LINK_BYTES: usize = MAX_SEARCH_BYTES;

pub const NO_ORIGIN: &str =
    "trawld has no [server] link_origin configured, so no link to Search is built";
pub const NO_BOUNDS: &str =
    "the result was read from stored report rows (from saved), which have no bounds Search can run";
pub const UNREADABLE_BOUNDS: &str = "the result's bounds did not parse as RFC 3339 instants";
pub const REFUSED: &str =
    "Search would refuse the link: its query string is over the 32 KiB or 64-parameter limit";
pub const TOO_LONG: &str = "the link would be longer than 32 KiB with its origin";

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
