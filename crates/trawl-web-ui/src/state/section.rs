// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sidebar's destinations, grouped. Every group ships at once (the
//! sidebar lists them all); `AppMode` stays the grouping key, and the
//! active section is derived from the current pathname.
//!
//! `RailItem`, `SidebarGroupSpec`, `groups`, `items_for` and
//! `default_for` are pure `&'static` data and build on every target so
//! they can be unit-tested natively; `from_url` (the reactive `Memo`)
//! is wasm32-only.

// On native, only the tests consume `items_for` / `default_for`.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use fleet_ui::Icon;
#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos_router::hooks::use_location;

use crate::state::app_mode::AppMode;

/// Single destination in the sidebar — a `&'static` descriptor; the
/// `AuthShell` maps these onto owned `fleet_ui::RailItem`s at the Shell
/// boundary. Icons are `fleet_ui::Icon` (ADR-0030: apps never inline
/// raw SVG for chrome).
#[derive(Debug, Clone, Copy)]
pub struct RailItem {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: Icon,
    pub path: &'static str,
}

/// One labelled run of destinations. `label: None` renders with no
/// heading; `mode` is the `AppMode` the run belongs to, which is what
/// `items_for` matches on.
#[derive(Debug, Clone, Copy)]
pub struct SidebarGroupSpec {
    pub label: Option<&'static str>,
    pub mode: AppMode,
    pub items: &'static [RailItem],
}

/// Every sidebar group, in render order. Schema is listed once, under
/// Search: the Settings group carries Health alone.
const GROUPS: &[SidebarGroupSpec] = &[
    SidebarGroupSpec {
        label: None,
        mode: AppMode::Search,
        items: &[
            RailItem {
                id: "search",
                label: "Search",
                icon: Icon::Search,
                path: "/search",
            },
            RailItem {
                id: "history",
                label: "History",
                icon: Icon::Clock,
                path: "/search/history",
            },
            RailItem {
                id: "schema",
                label: "Schema",
                icon: Icon::Database,
                path: "/search/schema",
            },
        ],
    },
    SidebarGroupSpec {
        label: Some("Scheduled work"),
        mode: AppMode::Jobs,
        items: &[
            RailItem {
                id: "nets",
                label: "Nets",
                icon: Icon::Database,
                path: "/jobs/nets",
            },
            RailItem {
                id: "runs",
                label: "Runs",
                icon: Icon::Chart,
                path: "/jobs/runs",
            },
        ],
    },
    SidebarGroupSpec {
        label: Some("Operations"),
        mode: AppMode::Settings,
        items: &[RailItem {
            id: "health",
            label: "Health",
            icon: Icon::Chart,
            path: "/settings/health",
        }],
    },
];

#[must_use]
pub fn groups() -> &'static [SidebarGroupSpec] {
    GROUPS
}

/// The destinations of the group a mode owns. Every mode owns exactly
/// one group, so the empty fallback is unreachable — and pinned so by
/// `every_mode_has_a_non_empty_rail_with_a_valid_default`.
#[must_use]
pub fn items_for(mode: AppMode) -> &'static [RailItem] {
    groups()
        .iter()
        .find(|group| group.mode == mode)
        .map_or(&[][..], |group| group.items)
}

#[must_use]
pub fn default_for(mode: AppMode) -> &'static str {
    items_for(mode)[0].id
}

/// `Memo<String>` for the current section, derived from the URL
/// pathname matched against the active mode's rail items. Prefers
/// exact matches, then longest-prefix match, to avoid `/search`
/// shadowing `/search/history`.
#[cfg(target_arch = "wasm32")]
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
        by_len.sort_by_key(|item| std::cmp::Reverse(item.path.len()));
        if let Some(item) = by_len
            .iter()
            .find(|item| path.starts_with(&format!("{}/", item.path)))
        {
            return item.id.to_string();
        }
        default_for(m).to_string()
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn groups_carry_the_three_modes_in_order_and_schema_once() {
        assert_eq!(
            groups().iter().map(|g| g.label).collect::<Vec<_>>(),
            vec![None, Some("Scheduled work"), Some("Operations")]
        );
        assert_eq!(
            groups().iter().map(|g| g.mode).collect::<Vec<_>>(),
            AppMode::ALL.to_vec()
        );
        // Schema is one destination, under Search: the Settings group
        // used to list it a second time.
        let schema = groups()
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|item| item.path == "/search/schema")
            .count();
        assert_eq!(schema, 1, "/search/schema must appear in exactly one group");
    }

    #[test]
    fn every_mode_has_a_non_empty_rail_with_a_valid_default() {
        for mode in AppMode::ALL {
            let items = items_for(mode);
            assert!(!items.is_empty(), "{mode:?} has no rail items");

            // `default_for` indexes `items_for(mode)[0]` — assert it holds so
            // the `from_url` fallback branch can never panic.
            assert_eq!(
                default_for(mode),
                items[0].id,
                "{mode:?} default should be its first item's id",
            );

            // Each section has one id and one destination within its mode.
            let mut ids = HashSet::new();
            let mut paths = HashSet::new();
            for item in items {
                assert!(
                    paths.insert(item.path),
                    "{mode:?} has a duplicate rail path: {}",
                    item.path,
                );
                assert!(
                    ids.insert(item.id),
                    "{mode:?} has a duplicate rail item id: {}",
                    item.id,
                );
            }
        }
    }
}
