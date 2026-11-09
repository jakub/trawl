// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The turn a component gives each of its reads, for reads that have no
//! generation of their own: a `LocalResource` and its Retry.
//!
//! A sign-in redirect may follow only the newest read of a component that
//! is still mounted (ADR-0066). A `LocalResource` holds at most one read
//! in flight and runs a refetch only after it lands, so a Retry clicked
//! meanwhile has to retire the old read itself: [`ReadTurn::supersede`].
//! The turn lives in the component's arena, so a disposed component reads
//! as no turn at all.

use crate::api::ApiError;
use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct ReadTurn(StoredValue<u64>);

impl Default for ReadTurn {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTurn {
    /// A counter owned by the calling component.
    pub fn new() -> Self {
        Self(StoredValue::new(0))
    }

    /// Start a read, retiring every earlier one, and return its turn.
    /// Zero, which is never current, once the owner is gone.
    pub fn begin(self) -> u64 {
        self.0
            .try_update_value(|turn| {
                *turn = turn.wrapping_add(1).max(1);
                *turn
            })
            .unwrap_or(0)
    }

    /// Retire the read in flight before the one that replaces it starts.
    pub fn supersede(self) {
        self.begin();
    }

    /// Whether `turn` is still the newest read of a mounted owner.
    pub fn is_current(self, turn: u64) -> bool {
        turn != 0 && self.0.try_get_value() == Some(turn)
    }

    /// The shared sign-in handler for the answer to read `turn`, asked only
    /// while that read is current: whether the page left for sign-in.
    pub fn leaves_for_sign_in<T>(self, turn: u64, answer: &Result<T, ApiError>) -> bool {
        self.is_current(turn) && crate::auth_return::leaves_for_sign_in(answer)
    }
}
