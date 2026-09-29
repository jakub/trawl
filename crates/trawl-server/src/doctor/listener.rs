// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the certificate and trawld's own listener.
//!
//! The material is read through [`super::fsread`] and never generated. The
//! probe dials the configured address, with a wildcard mapped to loopback,
//! and accepts only the leaf on disk. The group fills [`Ctx::tls`] and
//! [`Ctx::listener_refused`].

use trawl_api::doctor::reason;

use super::output::Row;
use super::{Ctx, Runner, ServerCheck};

/// This group's checks, in [`ServerCheck::ALL`] order.
pub(super) const CHECKS: [ServerCheck; 3] = [
    ServerCheck::TlsMaterial,
    ServerCheck::ListenerIdentity,
    ServerCheck::ListenerHealth,
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
