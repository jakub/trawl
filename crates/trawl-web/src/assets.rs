// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SPA asset delivery with precompressed representation negotiation.
//!
//! `rust-embed` bakes `../trawl-web-ui/dist/` into the production binary.
//! Release builds add `.br` and `.gz` sidecars with `cargo xtask
//! compress-web`; this module chooses the best representation accepted by
//! the browser without compressing on the request path. The
//! `TRAWL_WEB_SPA_DIR` development override uses the same code and headers.

use std::cmp::Reverse;
use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../trawl-web-ui/dist/"]
struct SpaAssets;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

impl Encoding {
    const fn preference(self) -> u8 {
        match self {
            Self::Brotli => 2,
            Self::Gzip => 1,
            Self::Identity => 0,
        }
    }

    const fn name(self) -> Option<&'static str> {
        match self {
            Self::Brotli => Some("br"),
            Self::Gzip => Some("gzip"),
            Self::Identity => None,
        }
    }

    const fn suffix(self) -> &'static str {
        match self {
            Self::Brotli => ".br",
            Self::Gzip => ".gz",
            Self::Identity => "",
        }
    }
}

/// Serve an embedded SPA path, falling back to `index.html` for deep links.
#[must_use]
pub fn serve_embedded(
    request_path: &str,
    request_headers: &HeaderMap,
    head_only: bool,
) -> Response {
    if accepted_encodings(request_headers).is_empty() {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let Some(path) = safe_relative_path(request_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Some(response) = embedded_response(&path, request_headers, head_only) {
        return response;
    }
    embedded_response(Path::new("index.html"), request_headers, head_only).unwrap_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "SPA not built — run `cargo xtask build-web`",
        )
            .into_response()
    })
}

/// Serve a disk-backed SPA path with the same negotiation as embedded assets.
///
/// Used by `TRAWL_WEB_SPA_DIR` and exposed so integration tests can verify the
/// production HTTP contract without compiling generated `dist/` files into
/// the test binary.
pub async fn serve_from_dir(
    root: &Path,
    request_path: &str,
    request_headers: &HeaderMap,
    head_only: bool,
) -> Response {
    if accepted_encodings(request_headers).is_empty() {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let Some(path) = safe_relative_path(request_path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if let Some(response) = disk_response(root, &path, request_headers, head_only).await {
        return response;
    }
    disk_response(root, Path::new("index.html"), request_headers, head_only)
        .await
        .unwrap_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "SPA not built — run `cargo xtask build-web`",
            )
                .into_response()
        })
}

fn embedded_response(
    logical_path: &Path,
    headers: &HeaderMap,
    head_only: bool,
) -> Option<Response> {
    let logical = logical_path.to_str()?;
    for encoding in accepted_encodings(headers) {
        let candidate = format!("{logical}{}", encoding.suffix());
        if let Some(file) = SpaAssets::get(&candidate) {
            return Some(asset_response(
                logical_path,
                file.data.into_owned(),
                encoding,
                headers,
                head_only,
            ));
        }
    }
    None
}

async fn disk_response(
    root: &Path,
    logical_path: &Path,
    headers: &HeaderMap,
    head_only: bool,
) -> Option<Response> {
    for encoding in accepted_encodings(headers) {
        let candidate = append_suffix(logical_path, encoding.suffix());
        match tokio::fs::read(root.join(candidate)).await {
            Ok(bytes) => {
                return Some(asset_response(
                    logical_path,
                    bytes,
                    encoding,
                    headers,
                    head_only,
                ));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
                ) => {}
            Err(e) => {
                tracing::warn!(
                    path = %root.join(logical_path).display(),
                    error = %e,
                    "failed to read SPA asset"
                );
                return Some(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        }
    }
    None
}

fn asset_response(
    logical_path: &Path,
    bytes: Vec<u8>,
    encoding: Encoding,
    request_headers: &HeaderMap,
    head_only: bool,
) -> Response {
    let etag = format!("\"{}\"", blake3::hash(&bytes).to_hex());
    let mime = mime_guess::from_path(logical_path).first_or_octet_stream();
    let cache_control = if is_hashed_asset(logical_path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    let not_modified = request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|candidate| {
            let candidate = candidate.trim();
            candidate == etag || candidate == "*"
        });

    let mut builder = Response::builder()
        .status(if not_modified {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::VARY, "Accept-Encoding")
        .header(header::ETAG, &etag);
    if let Some(name) = encoding.name() {
        builder = builder.header(header::CONTENT_ENCODING, name);
    }

    if !not_modified {
        builder = builder.header(header::CONTENT_LENGTH, bytes.len());
    }
    let body = if not_modified || head_only {
        Body::empty()
    } else {
        Body::from(bytes)
    };
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn accepted_encodings(headers: &HeaderMap) -> Vec<Encoding> {
    let mut encodings = [Encoding::Brotli, Encoding::Gzip, Encoding::Identity]
        .into_iter()
        .filter_map(|encoding| {
            let quality = quality(headers, encoding)?;
            (quality > 0).then_some((encoding, quality))
        })
        .collect::<Vec<_>>();
    encodings
        .sort_by_key(|(encoding, quality)| (Reverse(*quality), Reverse(encoding.preference())));
    encodings
        .into_iter()
        .map(|(encoding, _quality)| encoding)
        .collect()
}

fn quality(headers: &HeaderMap, encoding: Encoding) -> Option<u16> {
    let values = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return (encoding == Encoding::Identity).then_some(1000);
    }

    let wanted = encoding.name().unwrap_or("identity");
    let mut exact = None;
    let mut wildcard = None;
    for value in values {
        for item in value.split(',') {
            let mut parts = item.trim().split(';');
            let name = parts.next().unwrap_or_default().trim();
            let q = parts
                .find_map(|part| part.trim().strip_prefix("q="))
                .map_or(1000, |value| parse_quality(value).unwrap_or(0));
            if name.eq_ignore_ascii_case(wanted) {
                exact = Some(exact.map_or(q, |current: u16| current.max(q)));
            } else if name == "*" {
                wildcard = Some(wildcard.map_or(q, |current: u16| current.max(q)));
            }
        }
    }

    exact.or_else(|| match encoding {
        Encoding::Identity => {
            if wildcard == Some(0) {
                Some(0)
            } else {
                Some(1000)
            }
        }
        Encoding::Brotli | Encoding::Gzip => wildcard.or(Some(0)),
    })
}

fn parse_quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let mut digits = fraction.bytes();
    let hundreds = u16::from(digits.next().unwrap_or(b'0').checked_sub(b'0')?);
    let tens = u16::from(digits.next().unwrap_or(b'0').checked_sub(b'0')?);
    let ones = u16::from(digits.next().unwrap_or(b'0').checked_sub(b'0')?);
    if digits.next().is_some() || !fraction.bytes().all(|digit| digit.is_ascii_digit()) {
        return None;
    }
    let fraction = hundreds * 100 + tens * 10 + ones;
    match whole {
        "0" => Some(fraction),
        "1" if fraction == 0 => Some(1000),
        _ => None,
    }
}

fn safe_relative_path(request_path: &str) -> Option<PathBuf> {
    let trimmed = request_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(PathBuf::from("index.html"));
    }
    let path = Path::new(trimmed);
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then(|| path.to_path_buf())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        return path.to_path_buf();
    }
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

/// Detect Trunk's `name-<8+ hex>.ext` and
/// `name-<8+ hex>_<suffix>.ext` content-hashed filenames.
fn is_hashed_asset(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let Some((_prefix, suffix)) = stem.rsplit_once('-') else {
        return false;
    };
    let hash = suffix.split_once('_').map_or(suffix, |(hash, _)| hash);
    hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_by_quality_then_server_preference() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "gzip, br".parse().unwrap());
        assert_eq!(
            accepted_encodings(&headers),
            [Encoding::Brotli, Encoding::Gzip, Encoding::Identity]
        );

        headers.insert(
            header::ACCEPT_ENCODING,
            "br;q=0.5, gzip;q=1".parse().unwrap(),
        );
        assert_eq!(
            accepted_encodings(&headers),
            [Encoding::Gzip, Encoding::Identity, Encoding::Brotli]
        );
    }

    #[test]
    fn absent_header_allows_only_identity() {
        assert_eq!(accepted_encodings(&HeaderMap::new()), [Encoding::Identity]);
    }

    #[test]
    fn quality_parser_accepts_http_grammar() {
        assert_eq!(parse_quality("0"), Some(0));
        assert_eq!(parse_quality("0.5"), Some(500));
        assert_eq!(parse_quality("0.125"), Some(125));
        assert_eq!(parse_quality("1.000"), Some(1000));
        assert_eq!(parse_quality("1.1"), None);
        assert_eq!(parse_quality("0.1234"), None);
    }

    #[test]
    fn hashed_asset_detection() {
        assert!(is_hashed_asset(Path::new("index-abcd1234.js")));
        assert!(is_hashed_asset(Path::new("assets/index-abcd1234ef.js")));
        assert!(is_hashed_asset(Path::new("wasm-0123456789abcdef.wasm")));
        assert!(is_hashed_asset(Path::new(
            "trawl-web-ui-585f03470f9bb52a_bg.wasm"
        )));
        assert!(!is_hashed_asset(Path::new("index.html")));
        assert!(!is_hashed_asset(Path::new("index.js")));
        assert!(!is_hashed_asset(Path::new("style.css")));
        assert!(!is_hashed_asset(Path::new("index-abc.js")));
        assert!(!is_hashed_asset(Path::new("index-xyz12345.js")));
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(safe_relative_path("/assets/app.wasm").is_some());
        assert_eq!(
            safe_relative_path("/").as_deref(),
            Some(Path::new("index.html"))
        );
        assert!(safe_relative_path("/../secret").is_none());
        assert!(safe_relative_path("/assets/../secret").is_none());
    }
}
