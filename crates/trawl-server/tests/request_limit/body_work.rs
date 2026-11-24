// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ingest and preview work keeps its request's count after the client
//! leaves (ADR-0054's lifetime exception).

use trawl_server::transport::request_limit::body_work::BodyWork;

use crate::common::{self, TestServer};
use crate::transport::{
    Held, Post, assert_fresh_admitted, assert_fresh_refused, h1_send, until_holders,
};

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

    until_holders(&probe, 1, &what).await;
    assert_fresh_refused(url, &post("refused")).await;

    held.release();
    until_holders(&probe, 0, &what).await;
    assert_fresh_admitted(url, &post("admitted")).await;
}

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
