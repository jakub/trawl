// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The accept loop through the production TLS listener.

use crate::common;
use crate::support::{capture_logs, field, stdout_lines};

/// An `accept()` error ends nothing: it is logged under the pre-auth
/// transport target, and the next connection is still served.
///
/// The fault is EMFILE, the error a connection flood against a process
/// out of file descriptors produces. It is answered by the second
/// server's first `accept()`, before any connection exists, so the
/// readiness poll inside `spawn_server_with` is already a connection
/// served after it.
#[tokio::test(flavor = "multi_thread")]
async fn accept_error_does_not_end_the_serve_loop() {
    capture_logs();
    let server = common::setup().await;
    let emfile = std::io::Error::from(nix::errno::Errno::EMFILE);
    let expected = emfile.to_string();
    let (url, task) = server.spawn_server_with(|_| {}, vec![emfile]).await;

    let failures: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| field(line, "event_type").as_deref() == Some("accept_failed"))
        .collect();
    assert_eq!(failures.len(), 1, "{failures:#?}");
    let line = &failures[0];
    assert!(
        line.contains(trawl_server::telemetry::PREAUTH_TRANSPORT_TARGET),
        "{line}"
    );
    assert!(line.contains(" WARN "), "{line}");
    // `Display` fields are written bare, so the error is the line's tail.
    assert!(line.ends_with(&format!(" error={expected}")), "{line}");

    // A fresh connection, after the one the readiness poll used.
    let client = common::harness_client_builder().build().unwrap();
    let response = client
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .expect("the next connection is served");
    assert!(response.status().is_success(), "{}", response.status());
    assert!(!task.is_finished(), "the serve loop ended");
    task.abort();
}
