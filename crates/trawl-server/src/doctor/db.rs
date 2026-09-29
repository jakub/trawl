// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the Fleet and app-state databases.
//!
//! Connections are bare `PgConnection`s with a bounded connect and query
//! time, in read-only transactions; no pool, no lock, no migration
//! (ADR-0021 ruling 3, ADR-0047). The group fills [`Ctx::app`] and
//! [`Ctx::writer_lock_held`] for the groups after it.

use trawl_api::doctor::reason;

use super::output::Row;
use super::{Ctx, Runner, ServerCheck};

/// This group's checks, in [`ServerCheck::ALL`] order.
pub(super) const CHECKS: [ServerCheck; 5] = [
    ServerCheck::FleetConnect,
    ServerCheck::FleetSchema,
    ServerCheck::AppConnect,
    ServerCheck::AppSchema,
    ServerCheck::AppWriter,
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
