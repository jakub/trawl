// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` end to end (ADR-0047 and its 2026-09-28 amendment,
//! #269).
//!
//! Every test runs the real `trawld` binary as a child process through
//! [`support::run_doctor`], with an empty environment plus what the test
//! names, and applies [`support::assert_no_values`] to both output streams.
//! The tests are grouped the way the doctor's checks are:
//!
//! - [`seal`]: the entry, the seal, and the command line.
//! - [`db`]: the Fleet and app-state databases.
//! - [`storage`]: the data root and its recovery markers.
//! - [`listener`]: the certificate and trawld's own listener.
//! - [`proof`]: whole installations, and the proof that nothing is written.
//!
//! The database, listener and proof tests run against real Postgres and a
//! real trawld through the shared fixtures in `common`, so this binary is in
//! the ADR-0021 `postgres` nextest group.

mod common;

#[path = "trawld_doctor/support.rs"]
mod support;

#[path = "trawld_doctor/seal.rs"]
mod seal;

#[path = "trawld_doctor/db.rs"]
mod db;

#[path = "trawld_doctor/storage.rs"]
mod storage;

#[path = "trawld_doctor/listener.rs"]
mod listener;

#[path = "trawld_doctor/proof.rs"]
mod proof;
