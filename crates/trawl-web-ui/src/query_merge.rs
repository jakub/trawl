// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The filter and range merge now lives in `trawl-search-url`; this shim
//! keeps the `crate::query_merge::X` paths the UI already uses.

pub use trawl_search_url::query_merge::*;
