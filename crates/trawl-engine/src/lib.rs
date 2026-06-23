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
pub mod parquet_stats;
pub mod post_process;
pub mod timezone;

// Value types live in trawl-api (the wire types crate); re-exported here
// so that existing `trawl_engine::value::*` imports across the workspace
// continue to work without changes.
pub use trawl_api::value;

// The union-type-conflict classifier is shared with trawl-server's
// compaction path so both lanes agree on which `DuckDB` errors warrant a
// cast-to-`VARCHAR` fallback (vs quarantine/abort).
pub use executor::is_union_type_conflict;

// The complex-type classifier is likewise shared with compaction's write-time
// coercion, so the schema describe and the compaction writer agree on exactly
// which `DuckDB` types get folded to `VARCHAR`.
pub use executor::is_complex_type;
