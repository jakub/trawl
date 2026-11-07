// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fault and barrier hooks for one audit task, for tests only.
//!
//! The hooks live on the task they were handed to, so concurrent tasks in
//! one process never see each other's faults or holds.

use std::collections::HashMap;
use std::sync::Arc;

use fleet_auth::AuthError;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::Phase;

/// Where in a fetch or a wait the task can be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Point {
    /// Before `list_keys`, the first read of a fetch.
    BeforeListKeys,
    /// After `list_keys` succeeded, before `list_roles`.
    BeforeListRoles,
    /// Before the interval sleep.
    BeforeWait,
}

/// A keystore read that can be made to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Step {
    ListKeys,
    ListRoles,
}

/// Text every injected failure carries. A log that contains it leaked the
/// raw error.
pub const INJECTED_ERROR_TEXT: &str = "INJECTED-RAW-DB-TEXT";

/// Hooks for one audit task. Clones share state: the test keeps one and
/// hands another to [`super::spawn_audit_task_with_hooks`].
#[derive(Debug, Clone, Default)]
pub struct AuditHooks {
    inner: Arc<Mutex<HookState>>,
}

#[derive(Debug, Default)]
struct HookState {
    holds: HashMap<(Phase, Point), Armed>,
    failures: HashMap<(Phase, Step), u32>,
}

/// The task's half of an [`AuditHold`].
#[derive(Debug)]
struct Armed {
    parked: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    resumed: oneshot::Sender<()>,
}

/// The test's half of a hold. `parked` resolves once the task waits at the
/// point. Sending on `release`, or dropping it, lets the task go on.
/// `resumed` resolves once the task has taken the release, in the same poll
/// that runs on to whatever follows the point, so a shutdown sent after it
/// lands on that next await, never on the hold.
///
/// The task waits inside its shutdown race, so a shutdown still ends a held
/// task.
#[derive(Debug)]
pub struct AuditHold {
    pub parked: oneshot::Receiver<()>,
    pub release: oneshot::Sender<()>,
    pub resumed: oneshot::Receiver<()>,
}

impl AuditHooks {
    /// Hold the task the next time it reaches `point` in `phase`. One hold
    /// stops the task once; arm another for the next visit.
    pub fn hold(&self, phase: Phase, point: Point) -> AuditHold {
        let (parked_tx, parked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (resumed_tx, resumed_rx) = oneshot::channel();
        self.inner.lock().holds.insert(
            (phase, point),
            Armed {
                parked: parked_tx,
                release: release_rx,
                resumed: resumed_tx,
            },
        );
        AuditHold {
            parked: parked_rx,
            release: release_tx,
            resumed: resumed_rx,
        }
    }

    /// Fail the next `times` runs of `step` in `phase` with a real
    /// [`AuthError`] whose text holds [`INJECTED_ERROR_TEXT`]. The error
    /// takes the same path out of the fetch as a database failure.
    pub fn fail(&self, phase: Phase, step: Step, times: u32) {
        *self.inner.lock().failures.entry((phase, step)).or_default() += times;
    }

    pub(super) async fn reach(&self, phase: Phase, point: Point) {
        // The guard drops at the end of this statement, before any await.
        let armed = self.inner.lock().holds.remove(&(phase, point));
        if let Some(armed) = armed {
            let _ = armed.parked.send(());
            // Err: the test dropped its sender, which releases too.
            let _ = armed.release.await;
            let _ = armed.resumed.send(());
        }
    }

    pub(super) fn take_failure(&self, phase: Phase, step: Step) -> Option<AuthError> {
        let mut state = self.inner.lock();
        let left = state.failures.get_mut(&(phase, step))?;
        *left = left.checked_sub(1)?;
        Some(AuthError::Database(sqlx::Error::Protocol(format!(
            "{INJECTED_ERROR_TEXT}: injected {step:?} failure in {phase:?}"
        ))))
    }
}
