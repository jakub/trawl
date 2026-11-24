// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The cancellation latch a caller hands the executor.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use trawl_core::cancel::Stop;

use crate::error::EngineError;

/// A caller's "stop this work" flag, read at the bind-to-execute boundary
/// (ADR-0024).
#[derive(Clone, Debug, Default)]
pub struct CancelLatch(Stop);

impl CancelLatch {
    #[must_use]
    pub fn never() -> Self {
        Self(Stop::never())
    }

    #[must_use]
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        Self(Stop::from_flag(flag))
    }

    #[must_use]
    pub fn latched(&self) -> bool {
        self.0.requested()
    }

    #[must_use]
    pub fn stop(&self) -> Stop {
        self.0.clone()
    }

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
