// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The wide filter rail's hand choice (ADR-0044).
//!
//! At 900px and wider the rail opens for a countable page and closes for
//! any other settled answer, until the reader presses its `<summary>`.
//! That press is a hand choice, and it holds for the rest of the browser
//! session in both directions: across route changes, until a reload or
//! sign-out. `AuthShell` provides it, so it outlives the Search route and
//! drops with the shell, because `/login` sits outside it.
//!
//! It is neither search URL state (ADR-0027) nor a `UiPrefs` preference
//! (ADR-0032): a stored open or closed value cannot say "follow the
//! result". No control resets it. The narrow disclosure keeps its own
//! component-local state, and never reads or writes this one.

use leptos::prelude::*;

/// The wide filter rail's hand choice for this browser session (ADR-0044).
///
/// `None` is automatic mode. `Some(open)` is what the reader last left
/// the rail as.
#[derive(Clone, Copy, Debug)]
pub struct FilterRailChoice(RwSignal<Option<bool>>);

impl FilterRailChoice {
    pub fn new() -> Self {
        Self(RwSignal::new(None))
    }

    /// The hand choice, if the reader has made one. A tracked read.
    pub fn held(self) -> Option<bool> {
        self.0.get()
    }

    /// Record a hand choice. Only the rail's `<summary>` press calls
    /// this, never the `<details>` `toggle` event: `toggle` also fires
    /// when the rail opens or closes by itself, and a choice read from it
    /// would end automatic behaviour on the first automatic open.
    pub fn choose(self, open: bool) {
        self.0.set(Some(open));
    }
}
