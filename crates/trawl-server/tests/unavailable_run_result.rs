// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A successful run whose stored result file is gone, over the real
//! server: real postgres, real parquet files on disk, real reads.
//!
//! `report_runs.result_path` is relative to `data_dir`, so an operator who
//! repoints `data_dir` (an epoch bump) leaves every recorded path naming a
//! file under the abandoned root. ADR-0035's rule for that: a committed row
//! naming a missing file is a NAMED unavailable state, never an empty
//! result and never a success with a null body. These tests delete the file
//! under a live server and read every surface that can be asked about it.

mod common;

use common::setup;
use std::time::Duration;
use trawl_client::{ClientError, HttpClient};

mod unavailable_run_result {
    use super::common::{TestServer, app_pool};
    use super::{ClientError, Duration, HttpClient, setup};

    /// Finished runs for a saved query, newest first.
    async fn finished_runs(
        client: &HttpClient,
        saved_id: i64,
        n: usize,
    ) -> Vec<trawl_api::ReportRunSummary> {
        for _ in 0..100 {
            let runs = client
                .list_report_runs(saved_id, None, None)
                .await
                .expect("the runs list answers")
                .runs;
            if runs.len() >= n && runs.iter().all(|r| r.status != "running") {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{n} finished runs never appeared for saved query {saved_id}");
    }

    /// The absolute path a run's stored result sits at, under this
    /// server's data root.
    fn stored_path(server: &TestServer, run: &trawl_api::ReportRunSummary) -> String {
        let relative = run
            .result_path
            .as_deref()
            .unwrap_or_else(|| panic!("run {} must have a parquet result: {run:?}", run.id));
        format!(
            "{}/{relative}",
            server.state.query.pool.base_dir().trim_end_matches('/')
        )
    }

    /// The status and message of a refusal, or a panic naming what came
    /// back instead.
    fn refusal<T: std::fmt::Debug>(outcome: Result<T, ClientError>) -> (u16, String) {
        match outcome {
            Err(ClientError::Server { status, error }) => (status, error.message),
            other => panic!("expected a refusal, got: {other:?}"),
        }
    }

    /// Assert a refusal is THE unavailable answer for `run_id`: 409, the
    /// run named, and nothing about where the file was.
    fn assert_unavailable(outcome: (u16, String), run_id: i64) {
        let (status, message) = outcome;
        assert_eq!(status, 409, "{message}");
        assert!(
            message.contains(&format!("run {run_id}")),
            "the refusal must name run {run_id}: {message}"
        );
        assert!(
            message.contains("no older run was substituted"),
            "the refusal must say nothing older was read instead: {message}"
        );
        assert!(
            !message.contains('/'),
            "the refusal must carry no filesystem path: {message}"
        );
    }

    /// Trigger one run and return once the server has accepted it.
    async fn record_run(client: &HttpClient, saved_id: i64) {
        client
            .trigger_run(saved_id)
            .await
            .expect("the run is accepted");
    }

    /// A saved query with a schedule, ready to be triggered.
    async fn net(client: &HttpClient, name: &str, query: &str) -> i64 {
        let saved = client
            .create_saved(name, query)
            .await
            .expect("the saved query is created");
        client
            .set_schedule(saved.id, "1h", None, true, None, None)
            .await
            .expect("the schedule is attached");
        saved.id
    }

    /// `run=N` names the run it was asked for and refuses.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_id_refuses_naming_run() {
        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(&client, "gone_by_id", "service=nginx | table service").await;
        record_run(&client, saved_id).await;
        let runs = finished_runs(&client, saved_id, 1).await;
        let run = &runs[0];
        assert_eq!(run.status, "success");
        assert!(run.row_count.unwrap_or(0) > 0, "{run:?}");

        // It reads before the delete, so the refusal afterwards is about
        // the file and not about the query.
        let dsl = format!("| from saved gone_by_id run={} | table service", run.id);
        let before = client
            .query_paginated(&dsl, None, None)
            .await
            .expect("the stored run reads while its file is there");
        assert_eq!(before.result.rows.len(), 2, "{:?}", before.result);

        std::fs::remove_file(stored_path(&server, run)).expect("delete the stored result");

        assert_unavailable(
            refusal(client.query_paginated(&dsl, None, None).await),
            run.id,
        );
        // An aggregate cannot come back as a 0 either: "zero rows" and
        // "no rows to count" are the two answers this must never give.
        assert_unavailable(
            refusal(
                client
                    .query_paginated(
                        &format!("| from saved gone_by_id run={} | stats count()", run.id),
                        None,
                        None,
                    )
                    .await,
            ),
            run.id,
        );
    }

    /// `run=latest` refuses naming the NEWEST run and never answers from
    /// the older one behind it (ADR-0018 ruling 13).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_latest_refuses_and_never_falls_back() {
        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(&client, "gone_latest", "service=nginx | table service").await;
        record_run(&client, saved_id).await;
        finished_runs(&client, saved_id, 1).await;
        record_run(&client, saved_id).await;
        let runs = finished_runs(&client, saved_id, 2).await;
        let newer = &runs[0];
        let older = &runs[1];
        assert_ne!(newer.id, older.id);
        assert!(older.result_path.is_some(), "{older:?}");

        // Only the newer file goes. The older one is still readable, which
        // is what makes "never falls back" an assertion rather than a hope.
        std::fs::remove_file(stored_path(&server, newer)).expect("delete the newer result");

        assert_unavailable(
            refusal(
                client
                    .query_paginated(
                        "| from saved gone_latest run=latest | stats count()",
                        None,
                        None,
                    )
                    .await,
            ),
            newer.id,
        );

        // The older run's rows are still there to be wrongly served.
        let by_id = client
            .query_paginated(
                &format!("| from saved gone_latest run={} | stats count()", older.id),
                None,
                None,
            )
            .await
            .expect("the older run still reads");
        assert_eq!(by_id.result.rows[0][0].to_string(), "2");
    }

    /// The direct run endpoint refuses rather than answering 200 with a
    /// null result beside a non-zero row count.
    #[tokio::test(flavor = "multi_thread")]
    async fn get_report_run_never_answers_null_as_success() {
        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(&client, "gone_endpoint", "service=nginx | table service").await;
        record_run(&client, saved_id).await;
        let runs = finished_runs(&client, saved_id, 1).await;
        let run = &runs[0];

        let answered = client
            .get_report_run(saved_id, run.id)
            .await
            .expect("the run reads while its file is there");
        assert_eq!(answered.result.expect("rows").rows.len(), 2);

        std::fs::remove_file(stored_path(&server, run)).expect("delete the stored result");

        assert_unavailable(
            refusal(client.get_report_run(saved_id, run.id).await),
            run.id,
        );
    }

    /// `run=all` fails naming the missing member. Never a partial union,
    /// never zero rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_all_fails_naming_missing_member() {
        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(&client, "gone_member", "service=nginx | table service").await;
        for n in 1..=3 {
            record_run(&client, saved_id).await;
            finished_runs(&client, saved_id, n).await;
        }
        let runs = finished_runs(&client, saved_id, 3).await;
        // Newest first, so the middle run is the middle entry either way.
        let middle = &runs[1];

        let before = client
            .query_paginated(
                "| from saved gone_member run=all | stats count()",
                None,
                None,
            )
            .await
            .expect("the union reads while every file is there");
        assert_eq!(before.result.rows[0][0].to_string(), "6");

        std::fs::remove_file(stored_path(&server, middle)).expect("delete the middle result");

        assert_unavailable(
            refusal(
                client
                    .query_paginated(
                        "| from saved gone_member run=all | stats count()",
                        None,
                        None,
                    )
                    .await,
            ),
            middle.id,
        );
        // The shape that would hide it: four rows from the two survivors.
        assert_unavailable(
            refusal(
                client
                    .query_paginated(
                        "| from saved gone_member run=all | table service",
                        None,
                        None,
                    )
                    .await,
            ),
            middle.id,
        );
    }

    /// The two neighbouring representations keep their own answers: a
    /// genuine zero-row success is still an empty typed relation, and a
    /// blob-only nonempty run still refuses with its own message.
    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_from_empty_and_blob_only() {
        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(
            &client,
            "still_empty",
            "service=nginx host=no_such_host | table service, host",
        )
        .await;
        record_run(&client, saved_id).await;
        let runs = finished_runs(&client, saved_id, 1).await;
        let empty = &runs[0];
        assert_eq!(empty.row_count, Some(0), "{empty:?}");
        assert!(empty.result_path.is_none(), "{empty:?}");

        let counted = client
            .query_paginated(
                "| from saved still_empty run=latest | stats count()",
                None,
                None,
            )
            .await
            .expect("a zero-row success is not unavailable");
        assert_eq!(counted.result.rows[0][0].to_string(), "0");
        let answered = client
            .get_report_run(saved_id, empty.id)
            .await
            .expect("and the endpoint serves its columns");
        let answered = answered.result.expect("the blob carries the columns");
        assert!(
            answered.rows.is_empty() && !answered.columns.is_empty(),
            "{answered:?}"
        );

        // A blob-only NONEMPTY run: the rows exist, but no file does. Its
        // refusal predates this work and keeps its own wording.
        let blob_id = net(&client, "blob_only", "service=nginx | table service").await;
        record_run(&client, blob_id).await;
        let blob_runs = finished_runs(&client, blob_id, 1).await;
        let blob_run = &blob_runs[0];
        let stored = stored_path(&server, blob_run);
        // Move the rows into the blob, exactly as a failed parquet write
        // would have left them, and drop the path.
        let result = trawl_api::value::QueryResult {
            columns: vec![trawl_api::value::Column {
                name: "service".to_owned(),
            }],
            rows: vec![vec![trawl_api::value::Value::String("nginx".to_owned())]],
        };
        let blob = zstd::encode_all(serde_json::to_vec(&result).unwrap().as_slice(), 3).unwrap();
        let pool = app_pool(&server.app_db_url).await;
        sqlx::query("UPDATE report_runs SET result_path = NULL, result_data = $1 WHERE id = $2")
            .bind(blob)
            .bind(blob_run.id)
            .execute(&pool)
            .await
            .expect("rewrite the run as blob-backed");
        std::fs::remove_file(&stored).expect("the file is not what this run reads any more");

        let (status, message) = refusal(
            client
                .query_paginated("| from saved blob_only run=latest", None, None)
                .await,
        );
        assert_eq!(status, 409, "{message}");
        assert!(
            message.contains("no parquet result"),
            "the blob-only refusal keeps its own message: {message}"
        );
        assert!(
            !message.contains("stored result is unavailable"),
            "a blob-backed run is not a missing file: {message}"
        );
    }

    /// Only `NotFound` becomes the unavailable state. A file that is there
    /// but unreadable is a read failure, reported as one.
    #[tokio::test(flavor = "multi_thread")]
    #[cfg(unix)]
    async fn permission_error_is_not_relabelled() {
        use std::os::unix::fs::PermissionsExt as _;

        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:").map(str::to_owned))
            })
            .and_then(|line| line.split_whitespace().next().map(str::to_owned));
        assert_ne!(
            uid.as_deref(),
            Some("0"),
            "root reads a mode-000 file anyway, so this test cannot run as root"
        );

        let server = setup().await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved_id = net(&client, "unreadable", "service=nginx | table service").await;
        record_run(&client, saved_id).await;
        let runs = finished_runs(&client, saved_id, 1).await;
        let run = &runs[0];
        let path = stored_path(&server, run);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("make the file unreadable");

        let (_, query_message) = refusal(
            client
                .query_paginated(
                    &format!("| from saved unreadable run={} | table service", run.id),
                    None,
                    None,
                )
                .await,
        );
        assert!(
            !query_message.contains("stored result is unavailable"),
            "an unreadable file is not a missing one: {query_message}"
        );
        let (_, endpoint_message) = refusal(client.get_report_run(saved_id, run.id).await);
        assert!(
            !endpoint_message.contains("stored result is unavailable"),
            "an unreadable file is not a missing one: {endpoint_message}"
        );

        // Restore, so the tempdir teardown is not fighting the mode.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
