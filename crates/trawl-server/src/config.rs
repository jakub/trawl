// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Server configuration.
//!
//! Types are defined in the `trawl-config` crate so proxy-only consumers
//! (e.g. `trawl-web`) don't transitively pull in the `DuckDB` engine chain.

pub use trawl_config::*;
