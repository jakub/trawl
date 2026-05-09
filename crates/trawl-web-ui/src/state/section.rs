// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-mode rail section. The valid set depends on which `AppMode` is
//! active; the active section is derived from the current pathname.

use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::state::app_mode::AppMode;

/// Single item rendered in the left rail.
#[derive(Debug, Clone, Copy)]
pub struct RailItem {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: RailIcon,
    pub path: &'static str,
}

/// Icons we currently render in the rail. Kept as an enum so the SVG
/// path stays in one place rather than scattered across templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RailIcon {
    Search,
    Clock,
    Database,
    News,
    Alert,
    Link,
    Zap,
    Check,
    Grid,
    User,
    Chart,
    Question,
}

#[must_use]
pub fn items_for(mode: AppMode) -> &'static [RailItem] {
    use RailIcon::{
        Alert, Chart, Clock, Database, Grid, Link, News, Search as SearchIcon, User, Zap,
    };
    match mode {
        AppMode::Search => &[
            RailItem {
                id: "search",
                label: "Search",
                icon: SearchIcon,
                path: "/search",
            },
            RailItem {
                id: "history",
                label: "History",
                icon: Clock,
                path: "/search/history",
            },
            RailItem {
                id: "schema",
                label: "Schema",
                icon: Database,
                path: "/search/schema",
            },
        ],
        AppMode::Intel => &[
            RailItem {
                id: "stories",
                label: "Stories",
                icon: News,
                path: "/intel/stories",
            },
            RailItem {
                id: "queue",
                label: "Queue",
                icon: Alert,
                path: "/intel/queue",
            },
            RailItem {
                id: "entities",
                label: "Entities",
                icon: Link,
                path: "/intel/entities",
            },
            RailItem {
                id: "sources",
                label: "Sources",
                icon: Zap,
                path: "/intel/sources",
            },
            RailItem {
                id: "derivations",
                label: "Derivations",
                icon: Link,
                path: "/intel/derivations",
            },
        ],
        AppMode::Jobs => &[
            RailItem {
                id: "nets",
                label: "Nets",
                icon: Database,
                path: "/jobs/nets",
            },
            RailItem {
                id: "runs",
                label: "Runs",
                icon: Chart,
                path: "/jobs/runs",
            },
        ],
        AppMode::Settings => &[
            RailItem {
                id: "sources",
                label: "Sources",
                icon: Database,
                path: "/settings",
            },
            RailItem {
                id: "schema",
                label: "Schema",
                icon: Grid,
                path: "/settings",
            },
            RailItem {
                id: "users",
                label: "Users & API",
                icon: User,
                path: "/settings",
            },
            RailItem {
                id: "retention",
                label: "Retention",
                icon: Clock,
                path: "/settings",
            },
            RailItem {
                id: "health",
                label: "Health",
                icon: Chart,
                path: "/settings",
            },
        ],
    }
}

#[must_use]
pub fn default_for(mode: AppMode) -> &'static str {
    items_for(mode)[0].id
}

/// `Memo<String>` for the current section, derived from the URL
/// pathname matched against the active mode's rail items. Prefers
/// exact matches, then longest-prefix match, to avoid `/search`
/// shadowing `/search/history`.
#[must_use]
pub fn from_url(mode: Memo<AppMode>) -> Memo<String> {
    let location = use_location();
    Memo::new(move |_| {
        let m = mode.get();
        let path = location.pathname.get();
        let valid: &[RailItem] = items_for(m);

        // Exact match first.
        if let Some(item) = valid.iter().find(|item| path == item.path) {
            return item.id.to_string();
        }
        // Longest prefix match — sort by path length descending so
        // /search/history beats /search for /search/history/... paths.
        let mut by_len: Vec<&RailItem> = valid.iter().collect();
        by_len.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        if let Some(item) = by_len
            .iter()
            .find(|item| path.starts_with(&format!("{}/", item.path)))
        {
            return item.id.to_string();
        }
        default_for(m).to_string()
    })
}
