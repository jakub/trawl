// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Index arithmetic for roving tabindex, shared by the composite
//! widgets that keep exactly one tab stop (ADR-0028).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nav {
    Next,
    Prev,
    First,
    Last,
}

/// `None` means there is nothing to move to: an empty widget, or a current
/// index that is no longer in range (the item list can shrink under a held
/// focus).
pub(crate) fn next_index(current: usize, len: usize, nav: Nav) -> Option<usize> {
    if len == 0 || current >= len {
        return None;
    }
    Some(match nav {
        Nav::Next => (current + 1) % len,
        Nav::Prev => (current + len - 1) % len,
        Nav::First => 0,
        Nav::Last => len - 1,
    })
}

/// An id matching no tab would leave every tab at `tabindex="-1"` — a
/// tablist a keyboard cannot enter. Those ids are client text (trawl's
/// `?ntab=` and `?stab=` come straight out of the URL), so an unmatched id
/// selects the first tab, which is what the pane already shows.
pub(crate) fn resolve_selected(active: &str, ids: &[&str]) -> Option<usize> {
    if ids.is_empty() {
        return None;
    }
    Some(ids.iter().position(|id| *id == active).unwrap_or(0))
}

pub(crate) fn vertical_nav(key: &str) -> Option<Nav> {
    match key {
        "ArrowDown" => Some(Nav::Next),
        "ArrowUp" => Some(Nav::Prev),
        "Home" => Some(Nav::First),
        "End" => Some(Nav::Last),
        _ => None,
    }
}

pub(crate) fn horizontal_nav(key: &str) -> Option<Nav> {
    match key {
        "ArrowRight" => Some(Nav::Next),
        "ArrowLeft" => Some(Nav::Prev),
        "Home" => Some(Nav::First),
        "End" => Some(Nav::Last),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Nav, horizontal_nav, next_index, resolve_selected, vertical_nav};

    #[test]
    fn next_and_prev_wrap_at_both_ends() {
        assert_eq!(next_index(0, 3, Nav::Next), Some(1));
        assert_eq!(next_index(2, 3, Nav::Next), Some(0), "past the end wraps");
        assert_eq!(next_index(2, 3, Nav::Prev), Some(1));
        assert_eq!(
            next_index(0, 3, Nav::Prev),
            Some(2),
            "before the start wraps"
        );
    }

    #[test]
    fn first_and_last_reach_the_endpoints_from_anywhere() {
        for current in 0..4 {
            assert_eq!(next_index(current, 4, Nav::First), Some(0));
            assert_eq!(next_index(current, 4, Nav::Last), Some(3));
        }
    }

    #[test]
    fn a_single_item_navigates_to_itself() {
        for nav in [Nav::Next, Nav::Prev, Nav::First, Nav::Last] {
            assert_eq!(next_index(0, 1, nav), Some(0), "{nav:?} on a singleton");
        }
    }

    #[test]
    fn an_empty_or_out_of_range_widget_has_nowhere_to_go() {
        for nav in [Nav::Next, Nav::Prev, Nav::First, Nav::Last] {
            assert_eq!(next_index(0, 0, nav), None, "{nav:?} with no items");
            assert_eq!(
                next_index(5, 3, nav),
                None,
                "{nav:?} from an index the list no longer has"
            );
        }
    }

    #[test]
    fn a_known_tab_id_selects_its_own_tab() {
        let ids = ["query", "runs"];
        assert_eq!(resolve_selected("query", &ids), Some(0));
        assert_eq!(resolve_selected("runs", &ids), Some(1));
    }

    #[test]
    fn an_id_matching_no_tab_selects_the_first_one() {
        let ids = ["query", "runs"];
        for active in ["bogus", "", "Query", "runs "] {
            assert_eq!(
                resolve_selected(active, &ids),
                Some(0),
                "`{active}` matches no tab and must fall back to the first"
            );
        }
    }

    #[test]
    fn a_strip_with_no_tabs_selects_nothing() {
        assert_eq!(resolve_selected("query", &[]), None);
        assert_eq!(resolve_selected("", &[]), None);
    }

    #[test]
    fn vertical_keys_map_to_their_navigations_and_nothing_else() {
        assert_eq!(vertical_nav("ArrowDown"), Some(Nav::Next));
        assert_eq!(vertical_nav("ArrowUp"), Some(Nav::Prev));
        assert_eq!(vertical_nav("Home"), Some(Nav::First));
        assert_eq!(vertical_nav("End"), Some(Nav::Last));
        for key in [
            "ArrowLeft",
            "ArrowRight",
            "Escape",
            "Tab",
            "Enter",
            " ",
            "a",
            "arrowdown",
        ] {
            assert_eq!(
                vertical_nav(key),
                None,
                "`{key}` must not be read as a menu navigation"
            );
        }
    }
    #[test]
    fn horizontal_keys_map_to_their_navigations_and_nothing_else() {
        assert_eq!(horizontal_nav("ArrowRight"), Some(Nav::Next));
        assert_eq!(horizontal_nav("ArrowLeft"), Some(Nav::Prev));
        assert_eq!(horizontal_nav("Home"), Some(Nav::First));
        assert_eq!(horizontal_nav("End"), Some(Nav::Last));
        for key in [
            "ArrowDown",
            "ArrowUp",
            "Escape",
            "Tab",
            "Enter",
            " ",
            "a",
            "arrowright",
        ] {
            assert_eq!(
                horizontal_nav(key),
                None,
                "`{key}` must not be read as a tab-strip navigation"
            );
        }
    }

    #[test]
    fn the_two_key_maps_share_only_the_endpoints() {
        for key in ["Home", "End"] {
            assert_eq!(vertical_nav(key), horizontal_nav(key));
        }
        for key in ["ArrowDown", "ArrowUp"] {
            assert!(vertical_nav(key).is_some() && horizontal_nav(key).is_none());
        }
        for key in ["ArrowRight", "ArrowLeft"] {
            assert!(horizontal_nav(key).is_some() && vertical_nav(key).is_none());
        }
    }
}
