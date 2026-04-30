// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-mode rail section. The valid set depends on which `AppMode` is
//! active; the active section is derived from the current pathname.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

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

fn pathname() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_default()
}

/// `Memo<String>` for the current section, derived from the URL
/// pathname matched against the active mode's rail items.
#[must_use]
pub fn from_url(mode: Memo<AppMode>) -> Memo<String> {
    let qm = use_query_map();
    Memo::new(move |_| {
        let _ = qm.get();
        let m = mode.get();
        let path = pathname();
        let valid: &[RailItem] = items_for(m);
        valid
            .iter()
            .find(|item| path == item.path || path.starts_with(&format!("{}/", item.path)))
            .map_or_else(|| default_for(m).to_string(), |item| item.id.to_string())
    })
}
