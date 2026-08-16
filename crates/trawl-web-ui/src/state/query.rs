// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query state: executed DSL + page number + filters + range, synced to URL.
//!
//! Two notions of query text are modeled separately:
//! - `query_text`: what's currently in the editor buffer (changes on every
//!   keystroke, never touches the URL).
//! - `executed_q`: the last query we ran / will run; derived from the URL's
//!   `?q=` param. The user's raw editor DSL.
//!
//! On top of `executed_q`, the URL also carries structured state that gets
//! folded into the wire query at request time:
//! - filters (`?f=+host=web-01,-source=auth.log`) — include/exclude clauses
//!   driven by the facet sidebar and detail-row tag clicks.
//! - range (`?r=15m` or `?r=abs:<from>:<to>`) — time window from the
//!   date-range popover.
//!
//! The merging rules live in `crate::query_merge::effective_query` — pure
//! Rust so native tests cover it. This module layers URL encoding + signal
//! plumbing (wasm-only) on top.

use std::fmt::Write;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

pub use crate::query_merge::{Filter, FilterOp, QUICK_RANGES, RangeSpec, effective_query};

/// Display mode for the search page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Paginated snapshot of a one-shot query.
    Snapshot,
    /// SSE-streamed raw events (or aggregation snapshots, depending on
    /// the query shape — resolved downstream).
    Live,
}

impl Mode {
    #[must_use]
    pub fn from_url_param(raw: Option<&str>) -> Self {
        match raw {
            Some("live") => Self::Live,
            _ => Self::Snapshot,
        }
    }

    fn as_param(self) -> Option<&'static str> {
        match self {
            Self::Snapshot => None,
            Self::Live => Some("live"),
        }
    }
}

/// Build a `/search?q=...` URL with proper component encoding. Omits
/// elidable params (default mode, zero page in live mode, default range,
/// no filters) so the common case stays readable.
#[must_use]
pub fn build_search_url(
    query: &str,
    page: usize,
    mode: Mode,
    filters: &[Filter],
    range: &RangeSpec,
) -> String {
    let encoded = js_sys::encode_uri_component(query)
        .as_string()
        .unwrap_or_default();
    let mut url = format!("/search?q={encoded}");
    if mode == Mode::Snapshot {
        let _ = write!(url, "&page={page}");
    }
    if let Some(m) = mode.as_param() {
        let _ = write!(url, "&mode={m}");
    }
    if !filters.is_empty() {
        // The OUTER layer: one percent-encode of the whole payload, which
        // the router undoes exactly once before `decode_filters` sees it.
        // Without it a `&` or a literal `+` in a component would end the
        // parameter (or decode as a space) before the inner codec ever
        // ran.
        let enc = js_sys::encode_uri_component(&encode_filters(filters))
            .as_string()
            .unwrap_or_default();
        let _ = write!(url, "&f={enc}");
    }
    if *range != RangeSpec::default() {
        let enc = encode_range(range);
        let _ = write!(url, "&r={enc}");
    }
    url
}

/// The `f=` payload, ready to be percent-encoded ONCE into the URL.
///
/// Structure is escaped by [`crate::filter_codec`] in an alphabet
/// percent-decoding does not touch, because the router hands this module
/// back an already-decoded string — see that module for the pipeline.
fn encode_filters(filters: &[Filter]) -> String {
    crate::filter_codec::encode_payload(
        filters
            .iter()
            .map(|f| (f.op.prefix(), f.field.as_str(), f.value.as_str())),
    )
}

/// Parse the payload `use_query_map` hands back — ALREADY percent-decoded
/// by the browser, which is why the codec's escapes are not percent
/// escapes.
fn decode_filters(raw: &str) -> Vec<Filter> {
    crate::filter_codec::decode_payload(raw)
        .into_iter()
        .map(|(op, field, value)| Filter {
            field,
            value,
            op: if op == '-' {
                FilterOp::Exclude
            } else {
                FilterOp::Include
            },
        })
        .collect()
}

fn encode_range(range: &RangeSpec) -> String {
    match range {
        RangeSpec::Quick(q) => (*q).to_string(),
        RangeSpec::Absolute { from, to } => {
            let f = js_sys::encode_uri_component(from)
                .as_string()
                .unwrap_or_default();
            let t = js_sys::encode_uri_component(to)
                .as_string()
                .unwrap_or_default();
            format!("abs:{f}:{t}")
        }
    }
}

fn decode_range(raw: &str) -> RangeSpec {
    if let Some(rest) = raw.strip_prefix("abs:")
        && let Some((f_raw, t_raw)) = rest.split_once(':')
    {
        let from = js_sys::decode_uri_component(f_raw)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| f_raw.to_string());
        let to = js_sys::decode_uri_component(t_raw)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| t_raw.to_string());
        return RangeSpec::Absolute { from, to };
    }
    // Quick range — only accept known labels so stale URLs don't poison
    // the pill strip.
    if let Some(q) = QUICK_RANGES.iter().find(|q| **q == raw) {
        return RangeSpec::Quick(q);
    }
    RangeSpec::default()
}

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
/// have to hit back 20 times to undo); `false` for explicit submits.
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

/// Parse page number from URL query string, defaulting to 0 on missing/invalid.
fn parse_page(s: Option<String>) -> usize {
    s.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0)
}

/// URL-driven signals: executed query, page, mode, filters, range.
pub struct UrlSignals {
    pub executed_q: Memo<String>,
    pub page: Memo<usize>,
    pub mode: Memo<Mode>,
    pub filters: Memo<Vec<Filter>>,
    pub range: Memo<RangeSpec>,
}

/// Hook up URL-driven signals for everything read back from the URL.
///
/// Back/forward buttons in the browser just work — the router re-fires
/// every memo when the query string changes.
pub fn url_signals() -> UrlSignals {
    let query_map = use_query_map();
    let executed_q = Memo::new(move |_| query_map.get().get("q").unwrap_or_default());
    let page = Memo::new(move |_| parse_page(query_map.get().get("page")));
    let mode = Memo::new(move |_| Mode::from_url_param(query_map.get().get("mode").as_deref()));
    let filters = Memo::new(move |_| {
        query_map
            .get()
            .get("f")
            .map(|raw| decode_filters(&raw))
            .unwrap_or_default()
    });
    let range = Memo::new(move |_| {
        query_map
            .get()
            .get("r")
            .map(|raw| decode_range(&raw))
            .unwrap_or_default()
    });
    UrlSignals {
        executed_q,
        page,
        mode,
        filters,
        range,
    }
}
