// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query state: executed DSL + page number synced to URL.
//!
//! Two notions of query text are modeled separately:
//! - `query_text`: what's currently in the editor buffer (changes on every
//!   keystroke, never touches the URL).
//! - `executed_q`: the last query we ran / will run; derived from the URL's
//!   `?q=` param. Only `(executed_q, page)` drives the results resource.
//!
//! This separation keeps the URL stable as the user types and prevents the
//! browser history from filling up with every intermediate edit.

use std::fmt::Write;

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

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

/// Build a `/search?q=...&page=N[&mode=live]` URL with proper component
/// encoding. `page` is elided in live mode since streaming has no pages.
#[must_use]
pub fn build_search_url(query: &str, page: usize, mode: Mode) -> String {
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
    url
}

/// Capture a `Navigator` closure that pushes new `(q, page, mode)` tuples
/// onto the router's history.
///
/// MUST be called from a component body during initial setup — `use_navigate`
/// internally panics if called outside a `<Router>` context, which includes
/// any deferred callback (`CodeMirror` keydown, `EventSource` onmessage,
/// `gloo-timers::Timeout`, etc.). Capture once at component-setup time, then
/// pass the returned closure to whatever callbacks need to navigate.
///
/// `replace = true` is appropriate for pagination clicks (user shouldn't
/// have to hit back 20 times to undo); `false` for explicit submits.
pub fn navigator() -> impl Fn(&str, usize, Mode, bool) + Clone + 'static {
    let nav = use_navigate();
    move |query, page, mode, replace| {
        nav(
            &build_search_url(query, page, mode),
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

/// Hook up URL-driven signals for the executed query, page number, and mode.
///
/// Returns a `(executed_q, page, mode)` triple tracking URL params. Back/
/// forward buttons in the browser just work.
pub fn url_signals() -> (Memo<String>, Memo<usize>, Memo<Mode>) {
    let query_map = use_query_map();
    let executed_q = Memo::new(move |_| query_map.get().get("q").unwrap_or_default());
    let page = Memo::new(move |_| parse_page(query_map.get().get("page")));
    let mode = Memo::new(move |_| Mode::from_url_param(query_map.get().get("mode").as_deref()));
    (executed_q, page, mode)
}

#[cfg(test)]
mod tests {
    // `build_search_url` calls into js_sys so it can't run under plain
    // `cargo test` on native. Coverage is through manual browser QA for now.
    // Pure Rust URL-encoding paths would unblock a native test here; tracked
    // as a potential follow-up if this module grows more logic.
}
