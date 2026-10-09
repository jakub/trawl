// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054).
//!
//! `[server] max_concurrent_requests` is the number of requests in progress
//! on the HTTPS listener, across every route and method. A further four are
//! the control allowance: health, `/metrics`, query listing and query
//! cancellation, which neither use the regular count nor lend to it. A
//! request over either count is refused at once with a 503
//! `request_limit_reached`.

/// Which count a request in progress is charged to.
///
/// Also the `allowance` label on the three request-count metrics, so the
/// set is closed and carries nothing from the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Allowance {
    /// `[server] max_concurrent_requests`: every route not in the control
    /// table.
    Regular,
    /// The fixed control allowance of [`CONTROL_ALLOWANCE`].
    Control,
}

impl Allowance {
    /// Both allowances, for metric registration and tests that enumerate.
    pub const ALL: [Self; 2] = [Self::Regular, Self::Control];

    /// The fixed literal this allowance is labelled with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Control => "control",
        }
    }
}

/// Requests in progress reserved for the control routes, fixed in code
/// (ADR-0054): liveness and readiness probes, one scrape and one operator
/// request.
pub const CONTROL_ALLOWANCE: usize = 4;
