// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request shape as a 400, judged after authority (ADR-0059).
//!
//! The rule for every authenticated handler is authority first, shape
//! second, lookup third. A key without the grant must not learn whether its
//! request parsed, and a bare `Path`, `Query` or `Json` extractor makes axum
//! answer its plain-text 400 before the handler runs. So a handler takes
//! each of those as `Result<_, …Rejection>`, checks its authority, and only
//! then passes the rejection through [`shaped`].
//!
//! `tests/route_shape_guard.rs` refuses a bare extractor on an
//! authenticated handler.

use crate::error::ServerError;

/// An extracted path, query string or body, or a 400 when it did not read:
/// the extractor's reason, which quotes only what the caller sent. Call it
/// only after the route's authority check.
pub(crate) fn shaped<T, E: std::fmt::Display>(
    what: &str,
    extracted: Result<T, E>,
) -> Result<T, ServerError> {
    extracted.map_err(|e| ServerError::BadRequest(format!("invalid {what}: {e}")))
}
