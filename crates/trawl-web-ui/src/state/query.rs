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

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

/// Build a `/search?q=...&page=N` URL with proper component encoding.
#[must_use]
pub fn build_search_url(query: &str, page: usize) -> String {
    let encoded = js_sys::encode_uri_component(query)
        .as_string()
        .unwrap_or_default();
    format!("/search?q={encoded}&page={page}")
}

/// Navigate to a new `(q, page)` pair, replacing the current URL entry when
/// `replace` is true (e.g. pagination — user shouldn't need to hit back 20
/// times to undo page clicks).
pub fn go_to(query: &str, page: usize, replace: bool) {
    let nav = use_navigate();
    nav(
        &build_search_url(query, page),
        NavigateOptions {
            replace,
            ..Default::default()
        },
    );
}

/// Parse page number from URL query string, defaulting to 0 on missing/invalid.
fn parse_page(s: Option<String>) -> usize {
    s.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0)
}

/// Hook up URL-driven signals for the executed query + page number.
///
/// Returns a `(executed_q, page)` tuple of read signals that track the URL's
/// `?q=` and `?page=` params. Back/forward buttons in the browser just work.
pub fn url_signals() -> (Memo<String>, Memo<usize>) {
    let query_map = use_query_map();
    let executed_q = Memo::new(move |_| query_map.get().get("q").unwrap_or_default());
    let page = Memo::new(move |_| parse_page(query_map.get().get("page")));
    (executed_q, page)
}

#[cfg(test)]
mod tests {
    // `build_search_url` calls into js_sys so it can't run under plain
    // `cargo test` on native. Coverage is through manual browser QA for now.
    // Pure Rust URL-encoding paths would unblock a native test here; tracked
    // as a potential follow-up if this module grows more logic.
}
