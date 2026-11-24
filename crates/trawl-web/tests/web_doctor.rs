// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web --doctor` end to end (ADR-0047).
//!
//! Every test runs the real `trawl-web` binary through
//! [`support::run_web_doctor`]. That helper applies the leak check to both
//! output streams on every run, so no test can skip it.

#[path = "../src/test_support.rs"]
mod test_support;

#[path = "web_doctor/support.rs"]
mod support;

#[path = "web_doctor/settings.rs"]
mod settings;

#[path = "web_doctor/upstream.rs"]
mod upstream;

#[path = "web_doctor/proof.rs"]
mod proof;
