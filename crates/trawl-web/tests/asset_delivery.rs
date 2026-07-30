// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;

use axum::body::to_bytes;
use axum::http::{HeaderMap, StatusCode, header};
use tempfile::tempdir;

#[tokio::test]
async fn wasm_prefers_brotli_and_preserves_wasm_metadata() {
    let dir = tempdir().unwrap();
    let asset = "app-deadbeef_bg.wasm";
    fs::write(dir.path().join(asset), b"raw wasm").unwrap();
    fs::write(dir.path().join(format!("{asset}.gz")), b"gzip wasm").unwrap();
    fs::write(dir.path().join(format!("{asset}.br")), b"brotli wasm").unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, "gzip, br".parse().unwrap());
    let response =
        trawl_web::assets::serve_from_dir(dir.path(), &format!("/{asset}"), &headers, false).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
    assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(response.headers().contains_key(header::ETAG));
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "brotli wasm"
    );
}

#[tokio::test]
async fn gzip_fallback_and_conditional_request_work() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("app-deadbeef.wasm"), b"raw wasm").unwrap();
    fs::write(dir.path().join("app-deadbeef.wasm.gz"), b"gzip wasm").unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, "br, gzip".parse().unwrap());
    let response =
        trawl_web::assets::serve_from_dir(dir.path(), "/app-deadbeef.wasm", &headers, false).await;
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    let etag = response.headers()[header::ETAG].clone();

    headers.insert(header::IF_NONE_MATCH, etag);
    let response =
        trawl_web::assets::serve_from_dir(dir.path(), "/app-deadbeef.wasm", &headers, false).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn spa_deep_link_uses_compressed_no_cache_index() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("index.html"), b"raw index").unwrap();
    fs::write(dir.path().join("index.html.br"), b"brotli index").unwrap();

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_ENCODING, "br".parse().unwrap());
    let response =
        trawl_web::assets::serve_from_dir(dir.path(), "/search/history", &headers, false).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        "brotli index"
    );
}
