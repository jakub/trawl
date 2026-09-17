// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Browser evidence server using the production asset handlers and CSP.
//! Build in release mode after building the SPA so rust-embed captures that
//! distribution. Run distinct processes for embedded and disk-backed evidence.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::routing::get;
use clap::{Parser, ValueEnum};
use trawl_web::{assets, middleware::security_headers};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Embedded,
    Disk,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    mode: Mode,
    #[arg(long, default_value = "127.0.0.1:8136")]
    bind: SocketAddr,
    #[arg(long)]
    dist: Option<PathBuf>,
}

#[derive(Clone)]
struct AssetSource(Option<PathBuf>);

async fn asset(State(source): State<AssetSource>, request: Request) -> Response {
    let head = request.method() == Method::HEAD;
    match source.0 {
        Some(root) => {
            assets::serve_from_dir(&root, request.uri().path(), request.headers(), head).await
        }
        None => assets::serve_embedded(request.uri().path(), request.headers(), head),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !args.bind.ip().is_loopback() {
        return Err("the evidence server must bind a loopback address".into());
    }
    let source = match (args.mode, args.dist) {
        (Mode::Embedded, None) => AssetSource(None),
        (Mode::Disk, Some(path)) if path.join("index.html").is_file() => AssetSource(Some(path)),
        _ => {
            return Err(
                "embedded takes no --dist; disk requires --dist with a built index.html".into(),
            );
        }
    };
    let (csp, hsts, xcto, refp, xfo) = security_headers::layers();
    let router = Router::new()
        .route("/__theme_health", get(|| async { StatusCode::OK }))
        .fallback(asset)
        .with_state(source)
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
