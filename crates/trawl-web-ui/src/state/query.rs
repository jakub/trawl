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
//! and `mode`. How much of it folds in depends on the mode: a snapshot
//! takes the range, a live stream never does, and [`mode_query`] is the
//! one place that decides (ADR-0027 as amended 2026-09-21).
//!
//! Every encode and decode of that state lives in the pure
//! [`crate::search_url`] module, and the merging rules in
//! [`crate::query_merge`], so native tests cover both. This module is the
//! wasm-only layer over them: the navigator closure and the router memos,
//! including the one memo that says a parameter could not be read at all.
//! The reading itself starts from the router's RAW query string, because
//! the decode is the pure module's job and doing it twice changes what a
//! link means (see [`url_signals`]).

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
///
/// The URL is admitted before it is pushed. A link past
/// `MAX_SEARCH_BYTES` is one this crate's own reader refuses whole, so
/// navigating to it would replace the page with a banner whose only
/// action is "Start over", and, since the editor follows the executed
/// query, would take the text that caused it down too. On a refusal
/// nothing moves: no history entry, no address bar change, no editor
/// change. The caller says so out loud with [`report_refusal`].
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

/// Navigate to a URL this crate already built, replacing the current
/// history entry.
///
/// The banner's repair is the only caller: it edits one parameter of the
/// link as it stands (`search_url::repair_url`) rather than rebuilding
/// the URL out of state, so it needs a door that takes a URL and not a
/// `(q, page, mode, filters, range)` tuple. Same `use_navigate` rule as
/// [`navigator`] — call it from a component body.
///
/// Admitted like [`navigator`]. A repair carries every other parameter
/// through and percent-encodes `q` on the way, so a link inside the
/// bound can be rewritten into one past it — which is why
/// `search_url::plan_repair` asks the same question when it decides
/// which button to render. This check stays because a navigator that
/// trusts its caller is how the two drift apart.
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

/// Say a refused navigation out loud, in one sentence, and do nothing
/// when there was nothing to refuse.
///
/// Every page that navigates into `/search` goes through this, so the
/// copy for "the link this would have built is one we cannot read back"
/// lives in [`refusal_copy`] beside the filter refusals rather than
/// being written per caller.
pub fn report_refusal(bus: ToastBus, outcome: Result<(), Reason>) {
    if let Err(reason) = outcome {
        bus.push(ToastKind::Error, refusal_copy(reason), None);
    }
}

/// URL-driven signals: executed query, page, mode, filters, range, the
/// one parameter (if any) that could not be read, and the repair its
/// banner offers.
///
/// `filters`, `range` and `page` fall back to their defaults for a
/// malformed value so the page still renders; `malformed` is what stops
/// it running. Precedence is the whole link, then `f`, then `r`, then
/// `page` — one banner, naming the first thing a reader would have to
/// fix. The link comes first because a link over `MAX_SEARCH_BYTES` was
/// never parsed: there are no parameter verdicts underneath it.
pub struct UrlSignals {
    /// Exact router search identity, including changes with equal decoded values.
    pub raw_search: Memo<String>,
    pub executed_q: Memo<String>,
    pub page: Memo<usize>,
    pub mode: Memo<Mode>,
    pub filters: Memo<Vec<Filter>>,
    pub range: Memo<RangeSpec>,
    pub malformed: Memo<Option<Malformed>>,
    /// The repair on offer: where the button goes, and which repair it
    /// is (which is its label, and whether the editor buffer is cleared
    /// with it). `None` while the link reads.
    ///
    /// Built from the raw query pairs rather than from the memos
    /// above, because those have already fallen back to their defaults:
    /// a link that is wrong in two places must not lose the second one
    /// to a click that repaired the first. Admitted here too, so a
    /// candidate the producer's own door would refuse becomes "Start
    /// over" in the banner instead of a button that fails when clicked.
    pub repair: Memo<Option<Repair>>,
}

/// Hook up URL-driven signals for everything read back from the URL.
///
/// Back/forward buttons in the browser just work — the router re-fires
/// every memo when the query string changes.
///
/// Every memo below hangs off ONE reading of the raw query string
/// (`use_location().search`, which is the URL's own text minus the `?`),
/// bounded and decoded once by [`read_search`]. The router's own
/// `use_query_map` cannot be that reading: `ParamsMap::insert`
/// percent-decodes a value `UrlSearchParams` has already decoded, so
/// `?q=message%3D%2F100%2541%2F` reached this function as
/// `message=/100A/` and the page ran a query its own address bar
/// disagreed with.
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
