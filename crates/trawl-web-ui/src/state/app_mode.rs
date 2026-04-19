// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Application mode (top-bar tabs): Search / Intel / Jobs / Settings.
//!
//! Distinct from the search-page `Mode` (Snapshot/Live) — that one
//! lives under `state::query` and only governs the search workspace's
//! result rendering. `AppMode` governs which page mounts.

use std::str::FromStr;

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
    pub fn as_param(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Intel => "intel",
            Self::Jobs => "jobs",
            Self::Settings => "settings",
        }
    }
}

impl FromStr for AppMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "search" => Ok(Self::Search),
            "intel" => Ok(Self::Intel),
            "jobs" => Ok(Self::Jobs),
            "settings" => Ok(Self::Settings),
            _ => Err(()),
        }
    }
}

/// `Memo<AppMode>` derived from the URL `?app=` query param. Defaults
/// to `Search` when missing or unrecognized. Reactive — back/forward
/// in the browser updates the memo.
#[must_use]
pub fn from_url() -> Memo<AppMode> {
    let qm = use_query_map();
    Memo::new(move |_| {
        qm.get()
            .get("app")
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(AppMode::Search)
    })
}
