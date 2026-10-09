// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054, #293).
//!
//! The tests are grouped by where they reach the count:
//!
//! - [`edge`]: test routes under the production edge layers.

#[path = "request_limit/support.rs"]
mod support;

#[path = "request_limit/edge.rs"]
mod edge;
