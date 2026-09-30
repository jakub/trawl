// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The upstream: `proxy.upstream.trust` and `proxy.upstream.health`,
//! with its `proxy.upstream.health.<key>` rows.
//!
//! Each row reads only its own slot of [`Ctx`]. Until these checks look,
//! each records `not_sampled` with a fixed placeholder.

use trawl_api::doctor::reason;

use super::output::{Row, Text};
use super::{Ctx, Runner, WebCheck};

/// The checks this group runs, in [`WebCheck::ALL`] order.
pub(super) const CHECKS: [WebCheck; 2] = [WebCheck::UpstreamTrust, WebCheck::UpstreamHealth];

/// Run the group's checks, each through the runner's gate.
#[expect(
    clippy::unused_async,
    reason = "the checks that replace the placeholder read files and await"
)]
pub(super) async fn run(runner: &mut Runner, ctx: &mut Ctx) {
    let _ = ctx;
    for check in CHECKS {
        if let Some(gate) = runner.gate(check) {
            let check = gate.check();
            runner.record(
                gate,
                Row::not_sampled(check, reason::UNPROVEN).detail(Text::new("not implemented yet")),
            );
        }
    }
}
