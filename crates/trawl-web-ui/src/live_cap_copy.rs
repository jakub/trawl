// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The copy of `<LiveCapNotice/>`: what Search says above a live
//! aggregate whose snapshot reports `capped` (ADR-0001).
//!
//! Each cap gets one sentence, with the limit the snapshot carried, so
//! the copy never hard-codes a bound the server owns. Ungated so the
//! sentences are tested natively; the component itself is wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::stream::{SnapshotCap, SnapshotCapKind};

use crate::service_card_fmt::format_exact;

/// The notice title.
pub const LIVE_CAP_TITLE: &str = "Live aggregate is partial";

/// The sentence after the caps: what the reader can do about it.
pub const LIVE_CAP_ADVICE: &str = "Narrow the query, or switch to Snapshot.";

/// One cap's sentence, naming the limit the snapshot reported.
#[must_use]
pub fn cap_sentence(cap: &SnapshotCap) -> String {
    let limit = format_exact(cap.limit);
    match cap.cap {
        SnapshotCapKind::Groups => format!("New groups past {limit} are not counted."),
        SnapshotCapKind::DistinctValues => {
            format!("dc() and values() stop at {limit} distinct values per group.")
        }
        SnapshotCapKind::ExactValues => {
            format!("median and percentiles use the first {limit} values per group.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cap_names_the_limit_it_was_sent() {
        let sentence = |cap, limit| cap_sentence(&SnapshotCap { cap, limit });
        assert_eq!(
            sentence(SnapshotCapKind::Groups, 10_000),
            "New groups past 10,000 are not counted."
        );
        assert_eq!(
            sentence(SnapshotCapKind::DistinctValues, 10_000),
            "dc() and values() stop at 10,000 distinct values per group."
        );
        assert_eq!(
            sentence(SnapshotCapKind::ExactValues, 100_000),
            "median and percentiles use the first 100,000 values per group."
        );
        // The limit is the wire's, not a constant of the copy.
        assert_eq!(
            sentence(SnapshotCapKind::Groups, 250),
            "New groups past 250 are not counted."
        );
    }
}
