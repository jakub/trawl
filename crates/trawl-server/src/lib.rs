// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-server: the trawld daemon.
//!
//! HTTPS server (axum over `tokio-rustls`) that authenticates clients,
//! ingests events, executes queries and streams results, alongside a
//! syslog listener and the compaction, retention and scheduler jobs.

pub mod admission;
pub mod audit;
pub mod bus;
pub mod catalog;
pub mod config;
pub mod deadline;
pub(crate) mod env_dirs;
pub mod epoch;
pub mod error;
pub(crate) mod from_saved;
pub mod handlers;
pub mod hot_buffer;
pub mod ingest;
pub mod metrics;
pub mod monitor;
pub(crate) mod ping;
pub mod policy;
pub mod pool;
pub mod publication;
pub mod query_log;
pub mod rate_limit;
pub mod repin;
pub mod report_window;
pub mod retention;
pub mod scheduler;
pub mod schema_refresh;
pub mod shutdown;
pub(crate) mod source;
pub mod state;
pub mod stats;
pub mod store;
pub mod syslog;
pub mod telemetry;
pub mod tls;
pub mod tracker;
pub mod transport;
