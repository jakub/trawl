// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web`: browser-facing session proxy.
//!
//! Serves the `trawl-web-ui` SPA and translates cookie-based browser sessions
//! into bearer-token requests against `trawld`. See the v1 plan for the full
//! route table; at this commit the binary only exposes `GET /healthz`.

use axum::{Router, http::StatusCode, routing::get};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let app = Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8090").await?;
    tracing::info!(addr = %listener.local_addr()?, "trawl-web listening");
    axum::serve(listener, app).await?;
    Ok(())
}
