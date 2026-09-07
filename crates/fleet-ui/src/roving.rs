// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Index arithmetic for roving tabindex, shared by the composite
//! widgets that keep exactly one tab stop (ADR-0028).
//!
//! A roving widget answers two questions per keypress: which navigation
//! a key asks for, and which index that navigation lands on. Both are
//! pure, so both are native `nextest` facts rather than browser-only
//! behaviour. The DOM half — reading the elements and calling `.focus()`
//! — stays in the wasm components.
//!
//! Walking indexes, not element siblings, is the point. The sibling
//! walk this replaced hopped whatever element came next, so a header or
//! a separator inside the container could take focus; indexing a
//! queried `[role="menuitem"]` list can only ever land on an item.
//!
//! ## The one asymmetry
//!
//! A menu's roving index follows FOCUS: arrows move focus and nothing
//! else, so the menu tracks where focus went in its own signal. A tab
//! strip's roving index follows SELECTION: `aria-selected` and
//! `tabindex` are one predicate over the active tab, so arrows move
//! focus without changing which tab is tabbable, and only activation
//! (Enter or Space) moves the tab stop. That is a deliberate deviation
//! from the APG's focus-following strip: trawl's tabs write `?ntab=` to
//! the URL, and a tab stop that followed focus would disagree with the
//! selected pane the moment a user arrowed away without activating.

/// One navigation step a key asks for. Direction is orientation-free:
/// the key maps (vertical for menus, horizontal for tab strips) decide
/// which physical key means [`Nav::Next`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nav {
    /// The following item, wrapping past the end to the first.
    Next,
    /// The preceding item, wrapping past the start to the last.
    Prev,
    /// The first item.
    First,
    /// The last item.
    Last,
}

/// Where a navigation lands, given the current index and how many items
/// there are. `None` means there is nothing to move to: an empty widget,
/// or a current index that is no longer in range (the item list can
/// shrink under a held focus).
///
/// `Next` and `Prev` wrap, so a single item navigates to itself and the
/// caller re-focuses it rather than letting focus escape the widget.
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

/// The key map for a vertically-arranged widget (both menus): Arrow
/// Down/Up plus Home/End. Every other key returns `None` and the
/// component leaves the event alone — Escape belongs to the overlay
/// stack's window listener, and Enter/Space are a native button's own
/// click.
pub(crate) fn vertical_nav(key: &str) -> Option<Nav> {
    match key {
        "ArrowDown" => Some(Nav::Next),
        "ArrowUp" => Some(Nav::Prev),
        "Home" => Some(Nav::First),
        "End" => Some(Nav::Last),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Nav, next_index, vertical_nav};

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
        // Focus must stay inside the widget: with one item every
        // navigation is that item, and the caller re-focuses it.
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
}
