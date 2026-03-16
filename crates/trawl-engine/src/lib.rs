// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-engine: `DuckDB` integration and query execution.
//!
//! This crate owns the database connection lifecycle, query execution,
//! and result extraction. It takes SQL + parameters from trawl-core's
//! emitter and executes them against `DuckDB`.

pub mod error;
pub mod executor;
pub mod post_process;
pub mod timezone;

// Value types live in trawl-api (the wire types crate); re-exported here
// so that existing `trawl_engine::value::*` imports across the workspace
// continue to work without changes.
pub use trawl_api::value;
