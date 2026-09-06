// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query state: the router half of the search URL contract (ADR-0027).
//!
//! Two notions of query text are modeled separately:
//! - `query_text`: what's currently in the editor buffer (changes on every
//!   keystroke, never touches the URL).
//! - `executed_q`: the last query we ran / will run; derived from the URL's
//!   `?q=` param. The user's raw editor DSL.
//!
//! On top of `executed_q`, the URL also carries structured state that gets
//! folded into the wire query at request time: filters (`?f=`, opaque and
//! versioned) and the range (`?r=15m` or `?r=<from>..<to>`), plus `page`
//! and `mode`.
//!
//! Every encode and decode of that state lives in the pure
//! [`crate::search_url`] module, and the merging rules in
//! [`crate::query_merge`], so native tests cover both. This module is the
//! wasm-only layer over them: the navigator closure and the router memos,
//! including the one memo that says a parameter could not be read at all.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

pub use crate::query_merge::{Filter, FilterOp, QUICK_RANGES, RangeSpec, effective_query};
pub use crate::search_url::{Mode, build_search_url};

use crate::search_url::{Malformed, Verdict, decode_filters, decode_range, parse_page};

/// Capture a `Navigator` closure that pushes new `(q, page, mode, filters,
/// range)` tuples onto the router's history.
///
/// MUST be called from a component body during initial setup — `use_navigate`
/// internally panics if called outside a `<Router>` context, which includes
/// any deferred callback (`CodeMirror` keydown, `EventSource` onmessage,
/// `gloo-timers::Timeout`, etc.). Capture once at component-setup time, then
/// pass the returned closure to whatever callbacks need to navigate.
///
/// `replace = true` is appropriate for pagination clicks (user shouldn't
/// have to hit back 20 times to undo) and for a banner's repair of an
/// unreadable link; `false` for explicit submits.
pub fn navigator() -> impl Fn(&str, usize, Mode, &[Filter], &RangeSpec, bool) + Clone + 'static {
    let nav = use_navigate();
    move |query, page, mode, filters, range, replace| {
        nav(
            &build_search_url(query, page, mode, filters, range),
            NavigateOptions {
                replace,
                ..Default::default()
            },
        );
    }
}

/// URL-driven signals: executed query, page, mode, filters, range, and the
/// one parameter (if any) that could not be read.
///
/// `filters`, `range` and `page` fall back to their defaults for a
/// malformed value so the page still renders; `malformed` is what stops
/// it running. Precedence is `f`, then `r`, then `page` — one banner,
/// naming the first parameter a reader would have to fix.
pub struct UrlSignals {
    pub executed_q: Memo<String>,
    pub page: Memo<usize>,
    pub mode: Memo<Mode>,
    pub filters: Memo<Vec<Filter>>,
    pub range: Memo<RangeSpec>,
    // Read by the search page's gate and its banner, both of which land
    // with the wiring.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub malformed: Memo<Option<Malformed>>,
}

/// Hook up URL-driven signals for everything read back from the URL.
///
/// Back/forward buttons in the browser just work — the router re-fires
/// every memo when the query string changes.
pub fn url_signals() -> UrlSignals {
    let query_map = use_query_map();
    let executed_q = Memo::new(move |_| query_map.get().get("q").unwrap_or_default());
    let mode = Memo::new(move |_| Mode::from_url_param(query_map.get().get("mode").as_deref()));

    let filters_read = Memo::new(move |_| {
        query_map
            .get()
            .get("f")
            .map_or(Verdict::Absent, |raw| decode_filters(&raw))
    });
    let range_read = Memo::new(move |_| {
        query_map
            .get()
            .get("r")
            .map_or(Verdict::Absent, |raw| decode_range(&raw))
    });
    let page_read = Memo::new(move |_| {
        query_map
            .get()
            .get("page")
            .map_or(Verdict::Absent, |raw| parse_page(&raw))
    });

    let filters = Memo::new(move |_| filters_read.get().into_value().unwrap_or_default());
    let range = Memo::new(move |_| range_read.get().into_value().unwrap_or_default());
    let page = Memo::new(move |_| page_read.get().into_value().unwrap_or(0));
    let malformed = Memo::new(move |_| {
        filters_read
            .with(|v| v.malformed().cloned())
            .or_else(|| range_read.with(|v| v.malformed().cloned()))
            .or_else(|| page_read.with(|v| v.malformed().cloned()))
    });

    UrlSignals {
        executed_q,
        page,
        mode,
        filters,
        range,
        malformed,
    }
}
