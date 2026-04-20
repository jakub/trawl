// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-mode rail section. URL-synced via `?section=`. The valid set
//! depends on which `AppMode` is active; an unknown section falls
//! back to the mode's default.

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::state::app_mode::AppMode;

/// Single item rendered in the left rail.
#[derive(Debug, Clone, Copy)]
pub struct RailItem {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: RailIcon,
}

/// Icons we currently render in the rail. Kept as an enum so the SVG
/// path stays in one place rather than scattered across templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    use RailIcon::{Alert, Chart, Clock, Database, Grid, Link, News, Search as SearchIcon, User};
    match mode {
        AppMode::Search => &[
            RailItem {
                id: "search",
                label: "Search",
                icon: SearchIcon,
            },
            RailItem {
                id: "history",
                label: "History",
                icon: Clock,
            },
            RailItem {
                id: "schema",
                label: "Schema",
                icon: Database,
            },
        ],
        AppMode::Intel => &[
            RailItem {
                id: "news",
                label: "News",
                icon: News,
            },
            RailItem {
                id: "iocs",
                label: "IoCs",
                icon: Alert,
            },
            RailItem {
                id: "feeds",
                label: "Feeds",
                icon: Link,
            },
        ],
        AppMode::Jobs => &[
            RailItem {
                id: "nets",
                label: "Nets",
                icon: Database,
            },
            RailItem {
                id: "runs",
                label: "Runs",
                icon: Chart,
            },
        ],
        AppMode::Settings => &[
            RailItem {
                id: "sources",
                label: "Sources",
                icon: Database,
            },
            RailItem {
                id: "schema",
                label: "Schema",
                icon: Grid,
            },
            RailItem {
                id: "users",
                label: "Users & API",
                icon: User,
            },
            RailItem {
                id: "retention",
                label: "Retention",
                icon: Clock,
            },
            RailItem {
                id: "health",
                label: "Health",
                icon: Chart,
            },
        ],
    }
}

#[must_use]
pub fn default_for(mode: AppMode) -> &'static str {
    items_for(mode)[0].id
}

/// `Memo<String>` for the current section, derived from URL `?section=`
/// and validated against the active mode's item list. Falls back to the
/// mode's default if missing or invalid.
#[must_use]
pub fn from_url(mode: Memo<AppMode>) -> Memo<String> {
    let qm = use_query_map();
    Memo::new(move |_| {
        let m = mode.get();
        let valid: &[RailItem] = items_for(m);
        match qm.get().get("section") {
            Some(s) if valid.iter().any(|it| it.id == s) => s,
            _ => default_for(m).to_string(),
        }
    })
}
