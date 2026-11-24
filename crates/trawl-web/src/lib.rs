// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// trawld's `DEFAULT_LOG_FILTER` names no `trawl_web` target, so the sidecar
/// must never inherit it: a target-only filter silences every target it fails
/// to name.
pub const DEFAULT_LOG_FILTER: &str = "trawl_web=info,fleet_auth=info";

pub mod assets;
pub mod body_budget;
pub mod config;
pub mod doctor;
pub mod error;
pub mod listener;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod upstream;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "../../trawl-server/src/test_timing.rs"]
mod test_timing;
