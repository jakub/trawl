// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A query's cooperative stop (ADR-0063).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A flag that work reads between its steps. Clones share it, so a
/// request through any clone reaches every reader.
#[derive(Debug, Clone, Default)]
pub struct Stop(Option<Arc<AtomicBool>>);

impl Stop {
    /// A stop nobody can request, for work with no one to cancel it.
    #[must_use]
    pub fn never() -> Self {
        Self(None)
    }

    /// A stop of its own, not yet requested. Unlike the default, a
    /// request reaches it.
    #[must_use]
    pub fn new() -> Self {
        Self::from_flag(Arc::new(AtomicBool::new(false)))
    }

    /// The stop behind a flag the canceller already shares, such as an
    /// executor's cancellation latch.
    #[must_use]
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(Some(flag))
    }

    /// Ask the work to stop at its next check. A [`Stop::never`] ignores
    /// it. A request is never withdrawn.
    pub fn request(&self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
    }

    #[must_use]
    pub fn requested(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    pub fn check(&self) -> Result<(), Cancelled> {
        if self.requested() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Work that ended at a cancellation point because its stop was requested.
/// It says nothing about the query: the work that owned it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the query was cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_never_stop_is_never_requested() {
        for stop in [Stop::never(), Stop::default()] {
            stop.request();
            assert!(!stop.requested());
            assert_eq!(stop.check(), Ok(()));
        }
    }

    #[test]
    fn a_request_reaches_every_clone() {
        let stop = Stop::new();
        let clone = stop.clone();
        assert_eq!(stop.check(), Ok(()));
        clone.request();
        assert!(stop.requested());
        assert_eq!(stop.check(), Err(Cancelled));

        let flag = Arc::new(AtomicBool::new(false));
        let shared = Stop::from_flag(Arc::clone(&flag));
        assert!(!shared.requested());
        flag.store(true, Ordering::SeqCst);
        assert_eq!(shared.check(), Err(Cancelled));
    }
}
