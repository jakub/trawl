// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's one budget for the request bodies it buffers (ADR-0054,
//! amended 2026-10-17).
//!
//! The forwarder buffers each request body whole, up to `[server]
//! max_request_body_bytes`, before it calls trawld. The request count
//! (`middleware::request_count`) bounds how many requests are in progress
//! but not how many bytes they hold: at an 8 MiB limit, 64 bodies are
//! 512 MiB, the sidecar's whole memory ceiling, and a bearer token that
//! only trawld can refuse is enough to send them. So the bodies share one
//! budget of [`PROXY_BODY_BUDGET`] bytes, fixed, and a request reserves
//! its body's worst case from it before it reads a byte: the length the
//! body declares, or the whole limit when it declares none. The buffer
//! is allocated at the reservation and never grows past it, so the sum
//! of the reservations is the sum of the buffers. A request with no body
//! reserves nothing and is never refused by the budget.
//!
//! The reservation is taken with a try-acquire and never waited for: a
//! body the budget cannot cover is refused at once with the same 503
//! `request_limit_reached` the count gives, and trawld is not contacted.
//! It is held until the response head, like the count, because the
//! buffered body is alive until trawld has read it.
//!
//! At the default 128 KiB limit the count fills first: 64 × 128 KiB is
//! 8 MiB, a sixteenth of the budget, so the defaults behave as before.
//! At an 8 MiB limit the budget admits 16 bodies of the full limit
//! while the count's other 48 places serve requests without bodies. A
//! declared body is charged its own length, so small bodies are refused
//! only when fewer bytes than they need are left.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The most request-body bytes the proxy buffers at once, across every
/// request in progress.
pub const PROXY_BODY_BUDGET: usize = 128 * 1024 * 1024;

/// The name the budget's 413 carries when a body is larger than the
/// budget could ever cover, where trawld's names a setting.
pub const BODY_BUDGET_NAME: &str = "the proxy body budget";

/// The budget, shared by every request the forwarder buffers a body for.
#[derive(Clone, Debug)]
pub struct BodyBudget(Arc<Semaphore>);

impl BodyBudget {
    /// A budget of `bytes`.
    ///
    /// # Panics
    /// When `bytes` is more than tokio's semaphore holds, which
    /// [`PROXY_BODY_BUDGET`] and any test budget are far under.
    #[must_use]
    pub fn new(bytes: usize) -> Self {
        Self(Arc::new(Semaphore::new(bytes)))
    }

    /// Reserve `bytes` of the budget for one body, or refuse at once.
    ///
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

    /// How many bytes the budget still covers. For tests that need to
    /// know what a request reserved and that it gave it back.
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.0.available_permits()
    }
}

/// One body's reservation. Dropping it gives the bytes back.
#[derive(Debug)]
#[must_use = "a reservation released at once bounds nothing"]
pub struct BodyReservation {
    _permit: OwnedSemaphorePermit,
}
