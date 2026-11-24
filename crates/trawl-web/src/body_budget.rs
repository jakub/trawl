// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's one budget for the request bodies it buffers (ADR-0054).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const PROXY_BODY_BUDGET: usize = 128 * 1024 * 1024;

pub const BODY_BUDGET_NAME: &str = "the proxy body budget";

#[derive(Clone, Debug)]
pub struct BodyBudget(Arc<Semaphore>);

impl BodyBudget {
    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self(Arc::new(Semaphore::new(bytes)))
    }

    /// `None` when fewer than `bytes` are free, and for a body larger
    /// than the whole budget, which no release would ever admit; the
    /// caller refuses the latter as too large before it asks.
    #[must_use]
    pub fn try_reserve(&self, bytes: usize) -> Option<BodyReservation> {
        let bytes = u32::try_from(bytes).ok()?;
        Arc::clone(&self.0)
            .try_acquire_many_owned(bytes)
            .ok()
            .map(|permit| BodyReservation { _permit: permit })
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.0.available_permits()
    }
}

#[derive(Debug)]
#[must_use = "a reservation released at once bounds nothing"]
pub struct BodyReservation {
    _permit: OwnedSemaphorePermit,
}
