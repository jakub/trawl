// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cancellation latch a caller hands the executor.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use trawl_core::cancel::Stop;

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
///
/// The latch is a core [`Stop`] over the same flag, so the parse, the
/// pattern builds and every regex search of the lane read the one flag
/// the canceller sets ([`Self::stop`], ADR-0063).
#[derive(Clone, Debug, Default)]
pub struct CancelLatch(Stop);

impl CancelLatch {
    /// A latch nobody can ever set.
    #[must_use]
    pub fn never() -> Self {
        Self(Stop::never())
    }

    /// The latch behind a shared flag the canceller writes.
    #[must_use]
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        Self(Stop::from_flag(flag))
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn latched(&self) -> bool {
        self.0.requested()
    }

    /// The core stop behind this latch: it reads the same flag, so a
    /// cancellation the latch sees, the stop's readers see too.
    #[must_use]
    pub fn stop(&self) -> Stop {
        self.0.clone()
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

    /// The latch and its stop read one flag: a stop taken before the
    /// flag is set sees it, and a request through the stop latches.
    #[test]
    fn the_latch_and_its_stop_agree() {
        let flag = Arc::new(AtomicBool::new(false));
        let latch = CancelLatch::new(Arc::clone(&flag));
        let stop = latch.stop();
        assert!(stop.check().is_ok());
        flag.store(true, Ordering::SeqCst);
        assert!(stop.check().is_err());

        let latch = CancelLatch::new(Arc::new(AtomicBool::new(false)));
        latch.stop().request();
        assert!(latch.latched());

        let never = CancelLatch::never();
        never.stop().request();
        assert!(!never.latched());
    }
}
