// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-engine: `DuckDB` integration and query execution.
//!
//! This crate owns the database connection lifecycle, query execution,
//! and result extraction. It takes SQL + parameters from trawl-core's
//! emitter and executes them against `DuckDB`.

pub mod cancel;
pub mod error;
pub mod executor;
pub mod parquet_stats;
pub mod post_process;
pub mod timezone;

pub use trawl_api::value;

// Nothing on the read path classifies conversion errors; the classifier
// exists for the ADR-evidence tests, which reach it as
// `executor::is_conversion_error`. See its own doc comment for why the
// classification is unusable in production.
pub use executor::is_conversion_error;
