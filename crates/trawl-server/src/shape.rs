// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Request shape as a 400 or 413, judged after authority (ADR-0059).
//!
//! A bare `Path`, `Query`, `Json` or `Bytes` extractor makes axum answer its
//! plain-text refusal before the handler runs. So a handler takes each of
//! those as `Result<_, …Rejection>`, checks its authority, and only then
//! passes the rejection through [`shaped`].

use crate::error::ServerError;
use crate::transport::body_limit::OverLimit;

pub(crate) fn shaped<T, E: std::error::Error + 'static>(
    what: &str,
    extracted: Result<T, E>,
) -> Result<T, ServerError> {
    extracted.map_err(|e| match OverLimit::find(&e) {
        Some(over) => over.into(),
        None => ServerError::BadRequest(format!("invalid {what}: {e}")),
    })
}
