// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trawl-owned local development controller.
//!
//! The crate intentionally supports exactly the Trawl and Coastwatch
//! development stacks. It is not a general plugin runner.

pub mod cli;
pub mod command;
pub mod config;
pub mod controller;
pub mod credentials;
pub mod database;
pub mod environment;
pub mod error;
pub mod fleet;
pub mod plan;
pub mod resolver;
pub mod runtime;
pub mod selection;
pub mod session_key;
pub mod state;
pub mod tailscale;
pub mod topology;

pub use error::{Error, Result};
