// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-engine: `DuckDB` integration and query execution.

pub mod cancel;
pub mod column_type;
pub mod error;
pub mod executor;
pub mod parquet_stats;
pub mod post_process;
pub mod scratch;
pub mod timing;

pub use trawl_api::value;

pub use executor::is_conversion_error;
