// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Captured results (ADR-0057): a search executed once and kept as an
//! immutable, typed, expiring file that later reads page without
//! re-running anything.
//!
//! [`bounds`] decides what a capture executes: the one window it covers,
//! the text with that window spliced in, and whether the result is
//! event-shaped.

pub mod bounds;
