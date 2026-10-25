// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request shape as a 400, judged after authority (ADR-0059).
//!
//! The rule for every authenticated handler is authority first, shape
//! second, lookup third. A key without the grant must not learn whether its
//! request parsed, and a bare `Path`, `Query`, `Json` or `Bytes` extractor
//! makes axum answer its plain-text refusal before the handler runs. So a
//! handler takes each of those as `Result<_, …Rejection>`, checks its
//! authority, and only then passes the rejection through [`shaped`]. A
//! handler that reads a body checks its authority with
//! [`crate::policy::Authorized`], an extractor bound ahead of the body, so
//! the body of a key without the grant is never read.
//!
//! The body limit is part of the shape: a body past it fails the
//! extractor's read ([`crate::transport::body_limit`]), and [`shaped`]
//! answers that as the 413. Only a key past the authority extractor gets
//! that far, so a key without the grant does not learn a body's size
//! either.
//!
//! `tests/route_shape_guard.rs` refuses a bare extractor on an
//! authenticated handler, and a body parameter that no authority extractor
//! precedes.

use crate::error::ServerError;
use crate::transport::body_limit::OverLimit;

/// An extracted path, query string or body, or the refusal when it did not
/// read: 413 `request_too_large` naming the router's setting for a body
/// past the limit, otherwise a 400 with the extractor's reason, which
/// quotes only what the caller sent. Call it only after the route's
/// authority check.
pub(crate) fn shaped<T, E: std::error::Error + 'static>(
    what: &str,
    extracted: Result<T, E>,
) -> Result<T, ServerError> {
    extracted.map_err(|e| match OverLimit::find(&e) {
        Some(over) => over.into(),
        None => ServerError::BadRequest(format!("invalid {what}: {e}")),
    })
}
