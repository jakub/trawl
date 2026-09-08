// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Accessible names for sort-header buttons (ADR-0029).
//!
//! The div tables (schema services, nets, the service drawer's field
//! list) get no ARIA table roles, so `aria-sort` is unavailable there
//! and the direction has to live in the button's name instead. Pure and
//! ungated so its table test runs under native `cargo test`; the
//! rendering half is `components::sort_th`, which is wasm-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The button's accessible name. `direction` is `None` when the column
/// is not the sorted one, else `Some(desc)` matching the sort tuple's
/// descending flag.
pub fn sort_button_name(label: &str, direction: Option<bool>) -> String {
    match direction {
        None => format!("Sort by {label}"),
        Some(true) => format!("Sort by {label}, descending"),
        Some(false) => format!("Sort by {label}, ascending"),
    }
}

/// The glyph beside the label on the sorted column, `None` elsewhere.
/// Hidden from assistive technology: [`sort_button_name`] already says
/// the direction in words.
pub fn sort_arrow(direction: Option<bool>) -> Option<&'static str> {
    direction.map(|desc| if desc { "↓" } else { "↑" })
}

#[cfg(test)]
mod tests {
    use super::{sort_arrow, sort_button_name};

    #[test]
    fn name_carries_the_direction_only_when_sorted() {
        assert_eq!(sort_button_name("Service", None), "Sort by Service");
        assert_eq!(
            sort_button_name("Service", Some(false)),
            "Sort by Service, ascending"
        );
        assert_eq!(
            sort_button_name("Service", Some(true)),
            "Sort by Service, descending"
        );
    }

    #[test]
    fn every_direction_gets_a_distinct_name() {
        let names = ["Cardinality", "Storage"]
            .map(|label| [None, Some(false), Some(true)].map(|dir| sort_button_name(label, dir)));
        let mut flat: Vec<&str> = names.iter().flatten().map(String::as_str).collect();
        flat.sort_unstable();
        let before = flat.len();
        flat.dedup();
        assert_eq!(
            flat.len(),
            before,
            "two sort states share an accessible name"
        );
    }

    #[test]
    fn arrow_points_the_way_the_name_says() {
        assert_eq!(sort_arrow(None), None);
        assert_eq!(sort_arrow(Some(false)), Some("↑"));
        assert_eq!(sort_arrow(Some(true)), Some("↓"));
    }
}
