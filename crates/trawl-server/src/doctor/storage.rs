// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the data root and its recovery markers.
//!
//! Every marker is read through [`super::fsread`]; nothing under the data
//! root is created, renamed, or removed. The group reads [`Ctx::app`],
//! which the database group filled.

use trawl_api::doctor::reason;

use super::output::Row;
use super::{Ctx, Runner, ServerCheck};

/// This group's checks, in [`ServerCheck::ALL`] order.
pub(super) const CHECKS: [ServerCheck; 6] = [
    ServerCheck::DataRoot,
    ServerCheck::DataEpoch,
    ServerCheck::DataIdentity,
    ServerCheck::DataConformance,
    ServerCheck::RecoveryRepin,
    ServerCheck::RecoveryPublication,
];

/// Run this group's checks through `runner`.
#[expect(
    clippy::unused_async,
    reason = "the group's checks are not built yet; they await their probes"
)]
pub(super) async fn run(ctx: &mut Ctx, runner: &mut Runner) {
    let _ = ctx;
    for check in CHECKS {
        if let Some(gate) = runner.gate(check) {
            runner.record(gate, Row::not_sampled(check, reason::NOT_IMPLEMENTED));
        }
    }
}
