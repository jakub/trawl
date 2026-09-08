// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cancellation latch a caller hands the executor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::EngineError;

/// A caller's "stop this work" flag, read at the bind-to-execute boundary.
///
/// `DuckDB`'s interrupt handle does not reliably stop a statement that is
/// still binding, and a long bind is exactly where a cancellation is most
/// likely to land (ADR-0024). The latch is the recovery, not a second
/// interrupt: whoever cancels sets it, and the executor reads it once
/// binding is over and before it asks `DuckDB` to execute anything. It
/// promises nothing about work already inside a `DuckDB` API call.
///
/// [`CancelLatch::never`] is the answer for every caller with nothing to
/// cancel it — embedded `--data`, tests — and it is what the plain
/// [`Executor`](crate::executor::Executor) entry points pass. A latch
/// arrives only through
/// [`Executor::cancellable`](crate::executor::Executor::cancellable), so
/// no lane can pick one up by accident or drop one silently.
#[derive(Clone, Debug, Default)]
pub struct CancelLatch(Option<Arc<AtomicBool>>);

impl CancelLatch {
    /// A latch nobody can ever set.
    #[must_use]
    pub fn never() -> Self {
        Self(None)
    }

    /// The latch behind a shared flag the canceller writes.
    #[must_use]
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        Self(Some(flag))
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn latched(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    /// [`EngineError::Cancelled`] once the latch is set.
    ///
    /// Every read of the latch inside the executor goes through here, so
    /// the answer to "was this cancelled" has one spelling.
    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.latched() {
            return Err(EngineError::Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::CancelLatch;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_absent_latch_is_never_cancelled() {
        let latch = CancelLatch::never();
        assert!(!latch.latched());
        assert!(latch.check().is_ok());
        assert!(!CancelLatch::default().latched());
    }

    #[test]
    fn a_shared_flag_reaches_every_clone() {
        let flag = Arc::new(AtomicBool::new(false));
        let latch = CancelLatch::new(Arc::clone(&flag));
        let clone = latch.clone();
        assert!(latch.check().is_ok());
        flag.store(true, Ordering::SeqCst);
        assert!(latch.latched());
        assert!(matches!(
            clone.check(),
            Err(crate::error::EngineError::Cancelled)
        ));
    }
}
