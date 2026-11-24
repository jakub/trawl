// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wide filter rail's hand choice (ADR-0044).

use leptos::prelude::*;

/// `None` is automatic mode. `Some(open)` is what the reader last left
/// the rail as.
#[derive(Clone, Copy, Debug)]
pub struct FilterRailChoice(RwSignal<Option<bool>>);

impl FilterRailChoice {
    pub fn new() -> Self {
        Self(RwSignal::new(None))
    }

    pub fn held(self) -> Option<bool> {
        self.0.get()
    }

    /// Only the rail's `<summary>` press calls this, never the `<details>`
    /// `toggle` event: `toggle` also fires when the rail opens by itself.
    pub fn choose(self, open: bool) {
        self.0.set(Some(open));
    }
}
