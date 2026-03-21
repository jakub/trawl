// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Key event handlers, split by focus/context.
//!
//! Each submodule adds methods to `App` via split `impl` blocks.

mod editor;
mod mouse;
mod palette;
mod panels;
mod popup;
mod results;
mod schema_tree;
