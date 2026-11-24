// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query state: the router half of the search URL contract (ADR-0027).

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_location, use_navigate};

pub use crate::query_merge::{Filter, FilterOp, QUICK_RANGES, RangeSpec};
pub use crate::search_url::{Mode, build_search_url, mode_query};

use crate::search_url::{
    Malformed, Reason, Repair, Verdict, admit_search, decode_filters, decode_range, first_value,
    parse_page, plan_repair, read_search, refusal_copy,
};

use fleet_ui::{ToastBus, ToastKind};

/// MUST be called from a component body during initial setup — `use_navigate`
/// internally panics if called outside a `<Router>` context, which includes
/// any deferred callback.
pub fn navigator()
-> impl Fn(&str, usize, Mode, &[Filter], &RangeSpec, bool) -> Result<(), Reason> + Clone + 'static {
    let nav = use_navigate();
    move |query, page, mode, filters, range, replace| {
        let url = build_search_url(query, page, mode, filters, range);
        admit_search(&url)?;
        nav(
            &url,
            NavigateOptions {
                replace,
                ..Default::default()
            },
        );
        Ok(())
    }
}

/// Same `use_navigate` rule as [`navigator`] — call it from a component body.
pub fn replace_navigator() -> impl Fn(&str) -> Result<(), Reason> + Clone + 'static {
    let nav = use_navigate();
    move |url: &str| {
        admit_search(url)?;
        nav(
            url,
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
        Ok(())
    }
}

pub fn report_refusal(bus: ToastBus, outcome: Result<(), Reason>) {
    if let Err(reason) = outcome {
        bus.push(ToastKind::Error, refusal_copy(reason), None);
    }
}

pub struct UrlSignals {
    /// Exact router search identity, including changes with equal decoded values.
    pub raw_search: Memo<String>,
    pub executed_q: Memo<String>,
    pub page: Memo<usize>,
    pub mode: Memo<Mode>,
    pub filters: Memo<Vec<Filter>>,
    pub range: Memo<RangeSpec>,
    pub malformed: Memo<Option<Malformed>>,
    /// Built from the raw query pairs rather than from the memos
    /// above, because those have already fallen back to their defaults:
    /// a link that is wrong in two places must not lose the second one
    /// to a click that repaired the first.
    pub repair: Memo<Option<Repair>>,
}

/// The router's own `use_query_map` cannot replace `use_location().search`
/// here: `ParamsMap::insert` percent-decodes a value `UrlSearchParams` has
/// already decoded.
pub fn url_signals() -> UrlSignals {
    let search = use_location().search;
    let read = Memo::new(move |_| search.with(|raw| read_search(raw)));
    // Empty when the link was refused by length, so every parameter
    // memo below reads as absent without a special case of its own.
    let params = Memo::new(move |_| read.with(|r| r.params().to_vec()));
    let executed_q =
        Memo::new(move |_| params.with(|p| first_value(p, "q").unwrap_or_default().to_owned()));
    let mode = Memo::new(move |_| params.with(|p| Mode::from_url_param(first_value(p, "mode"))));

    let filters_read = Memo::new(move |_| {
        params.with(|p| first_value(p, "f").map_or(Verdict::Absent, decode_filters))
    });
    let range_read = Memo::new(move |_| {
        params.with(|p| first_value(p, "r").map_or(Verdict::Absent, decode_range))
    });
    let page_read = Memo::new(move |_| {
        params.with(|p| first_value(p, "page").map_or(Verdict::Absent, parse_page))
    });

    let filters = Memo::new(move |_| filters_read.get().into_value().unwrap_or_default());
    let range = Memo::new(move |_| range_read.get().into_value().unwrap_or_default());
    let page = Memo::new(move |_| page_read.get().into_value().unwrap_or(0));
    let malformed = Memo::new(move |_| {
        read.with(|r| r.malformed().cloned())
            .or_else(|| filters_read.with(|v| v.malformed().cloned()))
            .or_else(|| range_read.with(|v| v.malformed().cloned()))
            .or_else(|| page_read.with(|v| v.malformed().cloned()))
    });

    let repair = Memo::new(move |_| {
        let param = malformed.with(|m| m.as_ref().map(|m| m.param))?;
        Some(params.with(|p| {
            plan_repair(
                p.iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
                param,
            )
        }))
    });

    UrlSignals {
        raw_search: search,
        executed_q,
        page,
        mode,
        filters,
        range,
        malformed,
        repair,
    }
}
