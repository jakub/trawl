// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` end to end (ADR-0047).
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
