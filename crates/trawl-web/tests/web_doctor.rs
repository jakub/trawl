// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web --doctor` end to end (ADR-0047 and its 2026-09-28
//! amendment, #271).
//!
//! Every test runs the real `trawl-web` binary as a child process through
//! [`support::run_web_doctor`], with an empty environment plus what the
//! test names. That helper applies the leak check to both output streams
//! on every run, so no test can skip it. The tests are grouped the way the
//! doctor's checks are:
//!
//! - [`settings`]: the public origins, the cookie settings and the cookie
//!   key.
//! - [`upstream`]: the upstream's trust and its health, against real
//!   rustls upstreams from [`test_support`].
//! - [`proof`]: the command line, malformed input, and the proof that
//!   nothing is bound or written.

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
