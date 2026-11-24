// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use axum::http::{HeaderName, HeaderValue, header};
use tower_http::set_header::SetResponseHeaderLayer;

pub type HeaderLayer = SetResponseHeaderLayer<HeaderValue>;

/// `wasm-unsafe-eval` is mandatory for `WebAssembly.instantiate`;
/// `unsafe-inline` is required because Trunk bootstraps the wasm via an
/// injected `<script type="module">` block. `style-src 'unsafe-inline'`:
/// inline style attributes are used by some component patterns (codemirror
/// does this).
pub const CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
    style-src 'self' 'unsafe-inline'; \
    font-src 'self'; \
    connect-src 'self'; \
    img-src 'self' data:; \
    frame-ancestors 'none'";

#[must_use]
pub fn layers() -> (
    HeaderLayer,
    HeaderLayer,
    HeaderLayer,
    HeaderLayer,
    HeaderLayer,
) {
    (
        SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ),
        SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ),
        SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
        SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ),
    )
}
