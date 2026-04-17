// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Security response headers applied to every response.
//!
//! These are baseline hardening controls, not defense against a
//! determined attacker. The CSP in particular is tuned for a wasm SPA —
//! `wasm-unsafe-eval` is mandatory for `WebAssembly.instantiate` to
//! work, and `connect-src 'self'` restricts browser-initiated requests
//! to the same origin (which is the only thing the SPA ever needs).

use axum::http::{HeaderName, HeaderValue, header};
use tower_http::set_header::SetResponseHeaderLayer;

/// Alias for the concrete `SetResponseHeaderLayer` type we use throughout.
pub type HeaderLayer = SetResponseHeaderLayer<HeaderValue>;

/// Content Security Policy string applied to every response.
///
/// - `default-src 'self'`: only same-origin resources by default
/// - `script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'`: load scripts
///   from same origin; `wasm-unsafe-eval` is mandatory for
///   `WebAssembly.instantiate`; `unsafe-inline` is required because
///   Trunk bootstraps the wasm via an injected `<script type="module">`
///   block, and so does the codemirror/uplot `<link data-trunk>` glue
///   when it ends up inline. A future hardening pass can switch to a
///   nonce-per-response scheme (Trunk supports `data-integrity`), but
///   for v1 behind same-origin auth the trade-off is acceptable.
/// - `style-src 'self' 'unsafe-inline'`: inline style attributes are used
///   by some component patterns (codemirror does this); kept permissive
///   because the alternative is shipping style nonces
/// - `connect-src 'self'`: fetch/XHR/SSE only to same origin
/// - `img-src 'self' data:`: allow data URLs for inline SVGs
/// - `frame-ancestors 'none'`: disallow embedding in iframes (belt and
///   suspenders with `X-Frame-Options: DENY`)
pub const CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
    style-src 'self' 'unsafe-inline'; \
    connect-src 'self'; \
    img-src 'self' data:; \
    frame-ancestors 'none'";

/// Build a `tower_http::Layer` chain that injects security headers.
///
/// Returns a tuple of layers so the caller composes them onto its router
/// however it likes.
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
