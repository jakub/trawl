// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Search-page state: URL-as-canonical-state, query signals, result resource.
//!
//! The URL query string (`?q=...&page=N`) is the single source of truth for
//! executed-query state. The editor's in-progress text lives in a separate
//! signal so typing doesn't spam the URL or trigger refetches.

pub mod query;
pub mod search_session;
