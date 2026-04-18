// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embedded SPA assets served as the production fallback.
//!
//! `rust-embed` bakes the contents of `../trawl-web-ui/dist/` into the
//! binary at compile time. `cargo xtask build-web --release` is the
//! canonical way to populate that directory; a `build.rs` in this
//! crate creates the dir if missing so fresh clones build cleanly
//! with an empty asset set.
//!
//! The dev mode override `TRAWL_WEB_SPA_DIR` takes precedence over
//! embedded assets (see `routes::mod::build`), so iterating on the SPA
//! without rebuilding `trawl-web` stays fast.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../trawl-web-ui/dist/"]
struct SpaAssets;

/// Serve `path` from the embedded SPA bundle, falling back to
/// `index.html` for SPA deep-links (routes the wasm router handles).
///
/// Strips a leading `/` so the paths passed in (e.g. `/search` or
/// `/assets/index-<hash>.js`) resolve against the embed's flat layout.
/// Returns 404 only when the fallback `index.html` is also missing
/// — i.e. the binary was built with an empty `dist/` directory.
pub fn serve(path: &str) -> Response {
    let trimmed = path.trim_start_matches('/');
    if let Some(resp) = try_serve(trimmed) {
        return resp;
    }
    // SPA fallback: every unknown path becomes /index.html so the
    // client-side router can interpret it.
    try_serve("index.html").unwrap_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "SPA not built — run `cargo xtask build-web`",
        )
            .into_response()
    })
}

fn try_serve(path: &str) -> Option<Response> {
    let file = SpaAssets::get(path)?;
    let mime = file.metadata.mimetype();

    // Hashed filenames produced by Trunk contain a `-<hex>.` segment
    // (e.g. `index-abcd1234.js`) — cache those immutably. Everything
    // else (index.html, non-hashed CSS) stays no-cache so we can ship
    // updates without a version bump.
    let cache_control = if is_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    Some(
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::from(file.data.into_owned()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

/// Heuristic for "is this a Trunk-produced hashed filename" — a `-`
/// followed by 8+ hex chars, followed by `.`. Good enough to avoid
/// caching index.html while still getting long-lived caching on the
/// wasm bundle and code-split JS.
fn is_hashed_asset(path: &str) -> bool {
    let Some(name) = path.rsplit('/').next() else {
        return false;
    };
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let Some((_prefix, suffix)) = stem.rsplit_once('-') else {
        return false;
    };
    suffix.len() >= 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_asset_detection() {
        assert!(is_hashed_asset("index-abcd1234.js"));
        assert!(is_hashed_asset("assets/index-abcd1234ef.js"));
        assert!(is_hashed_asset("wasm-0123456789abcdef.wasm"));
        assert!(!is_hashed_asset("index.html"));
        assert!(!is_hashed_asset("index.js"));
        assert!(!is_hashed_asset("style.css"));
        // Too short to be a content hash.
        assert!(!is_hashed_asset("index-abc.js"));
        // Non-hex chars.
        assert!(!is_hashed_asset("index-xyz12345.js"));
    }
}
