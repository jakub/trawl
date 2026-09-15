// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Application mode: the sidebar group key (Search / Jobs / Settings).
//!
//! Distinct from the search-page `Mode` (Snapshot/Live) — that one
//! lives under `state::query` and only governs the search workspace's
//! result rendering. `AppMode` governs which page mounts.
//!
//! The enum + `mode_from_path` are pure and build on every target so
//! `section`'s native tests can name modes; `from_url` (the reactive
//! `Memo`) is wasm32-only.

// `ALL` and `mode_from_path` exist for `section`'s native tests and for
// `from_url`; neither target reaches every item, so the module carries
// the allowance rather than sprinkling it per item.
#![allow(dead_code)]

#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos_router::hooks::use_location;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Search,
    Jobs,
    Settings,
}

impl AppMode {
    pub const ALL: [Self; 3] = [Self::Search, Self::Jobs, Self::Settings];
}

fn mode_from_path(path: &str) -> AppMode {
    if path.starts_with("/jobs") {
        AppMode::Jobs
    } else if path.starts_with("/settings") {
        AppMode::Settings
    } else {
        AppMode::Search
    }
}

/// `Memo<AppMode>` derived from the URL pathname. Reactive — uses the
/// router's `use_location().pathname` which fires on every navigation.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn from_url() -> Memo<AppMode> {
    let location = use_location();
    Memo::new(move |_| mode_from_path(&location.pathname.get()))
}
