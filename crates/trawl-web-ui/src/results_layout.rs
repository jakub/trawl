// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure layout decisions for the snapshot results table's reading modes.
//!
//! At the crate root rather than under `components/`, which is wasm-gated:
//! these answers are asked on every render of the results table, and
//! wrong ones address the wrong event, so their tests run on native
//! `cargo nextest run -p trawl-web-ui`.
//!
//! On native, only the tests consume these items — `#[allow(dead_code)]`
//! at module scope silences the bin-crate dead-code warning. Matches the
//! `facets.rs` / `query_merge.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// The row index the docked inspector describes, if any.
///
/// A selection is a `(generation, original row index)` pair, and this is
/// the arbitration between the two halves. The index alone is not enough:
/// a new response, a page turn or a new effective query bumps the
/// generation, and index 2 of the page that just arrived is a different
/// event from index 2 of the page that was on screen when the reader
/// clicked. A stale generation reads as "nothing selected", which closes
/// the inspector rather than repointing it.
#[must_use]
pub fn inspector_selection(generation: u64, selected: Option<(u64, usize)>) -> Option<usize> {
    selected
        .filter(|&(generation_id, _)| generation_id == generation)
        .map(|(_, idx)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspector_selection_is_none_without_a_selection() {
        assert_eq!(inspector_selection(7, None), None);
    }

    #[test]
    fn inspector_selection_keeps_the_index_on_a_matching_generation() {
        assert_eq!(inspector_selection(7, Some((7, 3))), Some(3));
        assert_eq!(inspector_selection(0, Some((0, 0))), Some(0));
    }

    #[test]
    fn inspector_selection_drops_a_stale_generation() {
        // The row the reader clicked is gone; index 3 of the new page is
        // a different event, so the inspector closes instead of moving.
        assert_eq!(inspector_selection(8, Some((7, 3))), None);
        assert_eq!(inspector_selection(7, Some((8, 3))), None);
    }
}
