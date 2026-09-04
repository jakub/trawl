// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin engine (ADR-0011): an operator-triggered shadow-generation
//! rewrite of one field's corpus to a new candidate-ladder type, with
//! resurrection of conflict-nulled values from `_raw`, an atomic
//! crash-recoverable cutover, and a persisted one-at-a-time job.
//!
//! Module map: `gate` (compaction interlocks), `marker` (the `data/REPIN`
//! document + sibling staging layout), `plan` (the scan every job runs),
//! `rewrite` (per-file hardlink-or-rewrite), `cutover` (the idempotent
//! per-env swap), `engine` (the job lifecycle), `cancel` (the cooperative
//! cancel registry and its point-of-no-return latch), `recover` (the boot
//! decision table).

/// How recently the field must have been observed for a repin's report to
/// call it live (ADR-0013 ruling 10).
///
/// A repin translates history. If something is still writing the field, the
/// live half keeps arriving in the ingest-time reading, so a syslog-dialect
/// rewrite leaves a discontinuity at the cutover instant — and the fix for
/// that half is `[ingest] severity_from`, not another repin. The operator
/// has to be told, so the report carries the fact and the CLI writes the
/// warning.
///
/// 24 hours, deliberately not `retention.max_age_days`: that window says
/// how much corpus a schema listing should describe, which is a different
/// question with a different answer (90 days of history says nothing about
/// whether a sender is still connected). It matches the degraded-pin
/// analyzer's minimum span for the same reason — a day is the shortest
/// window over which "still writing" is a fact rather than a coincidence.
pub const LIVENESS_WINDOW: std::time::Duration = std::time::Duration::from_hours(24);

pub mod cancel;
pub mod cutover;
pub mod engine;
pub mod gate;
pub mod marker;
pub mod plan;
pub mod recover;
pub mod rewrite;

pub(crate) use engine::force_refusal;

pub use cancel::{
    CANCEL_LATENCY_CONTRACT, CancelActor, CancelHandle, CancelRegistry, CancelVerdict,
};
pub use engine::{RepinEngine, StartOutcome};
pub use gate::{RepinCoordinator, RollupPause};
pub use marker::{RepinMarker, RepinPhase, aside_root, marker_path, shadow_root};
