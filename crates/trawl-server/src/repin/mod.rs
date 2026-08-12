// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin engine (ADR-0011 slice B, issue #53): an operator-triggered
//! shadow-generation rewrite of one field's corpus to a new
//! candidate-ladder type, with resurrection of conflict-nulled values
//! from `_raw`, an atomic crash-recoverable cutover, and a persisted
//! one-at-a-time job.

pub mod gate;

pub use gate::{RepinCoordinator, RollupPause};
