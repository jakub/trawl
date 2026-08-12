// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin engine (ADR-0011 slice B, issue #53): an operator-triggered
//! shadow-generation rewrite of one field's corpus to a new
//! candidate-ladder type, with resurrection of conflict-nulled values
//! from `_raw`, an atomic crash-recoverable cutover, and a persisted
//! one-at-a-time job.
//!
//! Module map: `gate` (compaction interlocks), `marker` (the `data/REPIN`
//! document + sibling staging layout), `plan` (the scan every job runs),
//! `rewrite` (per-file hardlink-or-rewrite), `cutover` (the idempotent
//! per-env swap), `engine` (the job lifecycle), `recover` (the boot
//! decision table).

pub mod cutover;
pub mod engine;
pub mod gate;
pub mod marker;
pub mod plan;
pub mod recover;
pub mod rewrite;

pub use engine::{RepinEngine, StartOutcome};
pub use gate::{RepinCoordinator, RollupPause};
pub use marker::{RepinMarker, RepinPhase, aside_root, marker_path, shadow_root};
