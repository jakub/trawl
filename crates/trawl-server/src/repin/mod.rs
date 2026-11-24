// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin engine (ADR-0011): an operator-triggered shadow-generation
//! rewrite of one field's corpus to a new candidate-ladder type, with
//! resurrection of conflict-nulled values from `_raw`, an atomic
//! crash-recoverable cutover, and a persisted one-at-a-time job.

/// How recently the field must have been observed for a repin's report to
/// call it live (ADR-0013 ruling 10).
pub const LIVENESS_WINDOW: std::time::Duration = std::time::Duration::from_hours(24);

pub mod cancel;
pub mod ceiling;
pub mod cutover;
pub mod engine;
pub mod gate;
#[cfg(any(test, feature = "test-support"))]
pub mod holds;
pub mod jobs;
pub mod marker;
pub mod plan;
pub mod recover;
pub mod rewrite;

pub(crate) use engine::force_refusal;

pub use cancel::{
    CANCEL_LATENCY_CONTRACT, CancelActor, CancelDecision, CancelHandle, CancelRegistry,
    CancelVerdict, Settlement,
};
pub use engine::{BoundedStart, RepinEngine, StartOutcome};
pub use gate::{RepinCoordinator, RollupPause};
#[cfg(any(test, feature = "test-support"))]
pub use holds::{JobHold, JobHolds};
pub use jobs::{JobGeneration, JobReading};
pub use marker::{
    RepinMarker, RepinPhase, aside_root, in_flight_evidence, marker_path, shadow_root,
};
