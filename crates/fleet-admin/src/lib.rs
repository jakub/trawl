// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library surface of the `fleet-admin` binary — exposes the command
//! handlers and error type so integration tests can exercise them
//! directly instead of spawning a subprocess.

pub mod commands;
pub mod error;
