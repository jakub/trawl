// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Application mode (top-bar tabs): Search / Intel / Jobs / Settings.
//!
//! Distinct from the search-page `Mode` (Snapshot/Live) — that one
//! lives under `state::query` and only governs the search workspace's
//! result rendering. `AppMode` governs which page mounts.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Search,
    Intel,
    Jobs,
    Settings,
}

impl AppMode {
    pub const ALL: [Self; 4] = [Self::Search, Self::Intel, Self::Jobs, Self::Settings];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Intel => "Intel",
            Self::Jobs => "Jobs",
            Self::Settings => "Settings",
        }
    }

    #[must_use]
    pub fn default_path(self) -> &'static str {
        match self {
            Self::Search => "/search",
            Self::Intel => "/intel/stories",
            Self::Jobs => "/jobs/nets",
            Self::Settings => "/settings",
        }
    }
}

fn pathname() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_default()
}

fn mode_from_path(path: &str) -> AppMode {
    if path.starts_with("/intel") {
        AppMode::Intel
    } else if path.starts_with("/jobs") {
        AppMode::Jobs
    } else if path.starts_with("/settings") {
        AppMode::Settings
    } else {
        AppMode::Search
    }
}

/// `Memo<AppMode>` derived from the URL pathname. Reactive — the
/// `use_query_map` subscription fires on every navigation (even
/// path-only changes), which triggers a re-read of `window.location`.
#[must_use]
pub fn from_url() -> Memo<AppMode> {
    let qm = use_query_map();
    Memo::new(move |_| {
        let _ = qm.get();
        mode_from_path(&pathname())
    })
}
