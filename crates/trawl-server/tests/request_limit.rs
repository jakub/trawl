// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawld's one count of requests in progress (ADR-0054, #293).
//!
//! The tests are grouped by where they reach the count:
//!
//! - [`edge`]: test routes under the production edge layers.
//! - [`listener`]: the accept loop behind the production TLS listener.
//! - [`body_work`]: ingest and preview work that keeps its count after
//!   the client leaves.
//! - [`transport`]: connections and HTTP/2 streams through the production
//!   TLS listener.
//! - [`routes`]: the production router.
//!
//! The listener, body-work, transport and route tests run against real
//! Postgres through the shared fixtures in `common`, so this binary is in
//! the ADR-0021 `postgres` nextest group.

mod common;

#[path = "request_limit/support.rs"]
mod support;

#[path = "request_limit/edge.rs"]
mod edge;

#[path = "request_limit/listener.rs"]
mod listener;

#[path = "request_limit/body_work.rs"]
mod body_work;

#[path = "request_limit/transport.rs"]
mod transport;

#[path = "request_limit/routes.rs"]
mod routes;
