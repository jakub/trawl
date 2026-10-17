// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ingest and preview work keeps its request's count after the client
//! leaves (ADR-0054's lifetime exception).
//!
//! Each case holds one kind of body work at the state's test seam, after
//! the work has taken the count, then makes the client go away by
//! dropping the HTTP/1.1 connection the request came on, the one way a
//! client leaves an HTTP/1.1-only listener (ADR-0054, amended
//! 2026-10-17). Once the probe shows the held work as the count's only
//! owner, the request future is gone and the count is the work's alone.
//! It must still refuse a second request until the work ends.

use trawl_server::transport::request_limit::body_work::BodyWork;

use crate::common::{self, TestServer};
use crate::transport::{
    Held, Post, assert_fresh_admitted, assert_fresh_refused, h1_send, until_holders,
};

/// One case: hold `work` for `post`, drop the connection the request came
/// on, and require the count to outlive it until the work ends.
async fn keeps_its_count(
    server: &TestServer,
    url: &str,
    work: BodyWork,
    post: impl Fn(&str) -> Post,
) {
    let what = format!("{work:?} after the connection dropped");
    let mut held = Held::next(server, work);
    let first = post("held");

    let stream = h1_send(url, &first).await;
    let probe = held.entered().await;
    assert!(probe.holders() > 1, "{what}: the request holds its count");
    drop(stream);

    // The request future is dropped; only the held work keeps the count.
    until_holders(&probe, 1, &what).await;
    assert_fresh_refused(url, &post("refused")).await;

    held.release();
    until_holders(&probe, 0, &what).await;
    assert_fresh_admitted(url, &post("admitted")).await;
}

/// AC11: with the limit at 1, ingest decode-and-parse and the WAL write
/// and finalize each keep the count after the client drops its
/// connection, and give it back when they end.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_work_keeps_its_count_after_disconnect() {
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = 1, vec![])
        .await;
    for work in [BodyWork::IngestParse, BodyWork::IngestWrite] {
        keeps_its_count(&server, &url, work, |label| Post::ingest(&server, label)).await;
    }
    task.abort();
}

/// AC12: with the limit at 1, the preview report keeps the count after
/// the client drops its connection, and gives it back when it ends.
#[tokio::test(flavor = "multi_thread")]
async fn preview_work_keeps_its_count_after_disconnect() {
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = 1, vec![])
        .await;
    keeps_its_count(&server, &url, BodyWork::Preview, |label| {
        Post::preview(&server, label)
    })
    .await;
    task.abort();
}
