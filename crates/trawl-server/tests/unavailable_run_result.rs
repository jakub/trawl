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

use common::{seed_data_root, setup, setup_in_dir_with_data};
use std::time::Duration;
use trawl_client::{ClientError, HttpClient};

mod unavailable_run_result {
    use super::common::{TestServer, app_pool};
    use super::{ClientError, Duration, HttpClient, seed_data_root, setup, setup_in_dir_with_data};
    use trawl_server::config::RateLimitConfig;
    use trawl_server::store::{RunClaim, RunStatus};

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
    /// A `data_dir` the emitter's glob validator refuses.
    ///
    /// `[data] path` is operator configuration, and the config layer keeps a
    /// directory with spaces verbatim
    /// (`daemon_data_directory_is_preserved_and_drives_wal_location`). A
    /// single selected run reads through a source the SERVER escapes, so the
    /// glob validator's narrow alphabet never applies to it.
    ///
    /// Returns the server and its data root. The run is PLANTED rather than
    /// triggered: an ordinary event query cannot run under this `data_dir`
    /// at all (see the commit body's parked finding), so a real report run
    /// here would record zero rows and no parquet file. What is under test
    /// is the read of a stored result, and that is real: a real parquet file
    /// at a real odd path, read by a real `DuckDB` through the real handler.
    async fn server_on_odd_data_dir(leaf: &str) -> (TestServer, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let seeded = seed_data_root(&root);
        let data_dir = root.join(leaf);
        std::fs::rename(&seeded, &data_dir).expect("rename the seeded data root");
        let server = setup_in_dir_with_data(
            &root,
            data_dir.to_str().expect("utf-8 path").to_owned(),
            RateLimitConfig::default(),
        )
        .await;
        std::mem::forget(tmp); // outlives the server; the OS cleans up
        (server, data_dir)
    }

    /// Plant a successful run carrying a real three-row parquet file, and
    /// return its id and that file's absolute path.
    async fn plant_run(
        server: &TestServer,
        data_dir: &std::path::Path,
        saved_id: i64,
        query: &str,
    ) -> (i64, std::path::PathBuf) {
        let pool = app_pool(&server.app_db_url).await;
        let schedule_id: i64 =
            sqlx::query_scalar("SELECT id FROM schedules WHERE saved_query_id = $1")
                .bind(saved_id)
                .fetch_one(&pool)
                .await
                .expect("the schedule the API just attached");

        let store = &server.state.storage.schedule;
        let run_id = match store
            .claim_run(schedule_id, saved_id, query, None, None)
            .await
            .expect("claim a run")
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };

        let relative = format!("scheduled/run_{run_id}.parquet");
        let full = data_dir.join(&relative);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("mkdir scheduled");
        duckdb::Connection::open_in_memory()
            .expect("duckdb")
            .execute_batch(&format!(
                "COPY (SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) t(n, msg)) \
                 TO '{}' (FORMAT PARQUET)",
                full.display().to_string().replace('\'', "''")
            ))
            .expect("write the run's parquet result");

        store
            .finish_run(
                run_id,
                RunStatus::Success,
                5,
                Some(3),
                None,
                None,
                Some(&relative),
            )
            .await
            .expect("finish the run");
        (run_id, full)
    }

    /// The whole read path over an odd `data_dir`: the stored run reads, and
    /// once its file is gone both single-run selectors answer the 409 —
    /// which, unlike the 400 a rejected path produced, carries no path.
    async fn reads_under_odd_data_dir(leaf: &str, net: &str) {
        let (server, data_dir) = server_on_odd_data_dir(leaf).await;
        let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let saved = client
            .create_saved(net, "service=nginx | table service")
            .await
            .expect("create the net");
        client
            .set_schedule(saved.id, "1h", None, true, None, None)
            .await
            .expect("attach a schedule");
        let (run_id, file) = plant_run(&server, &data_dir, saved.id, "service=nginx").await;

        for dsl in [
            format!("| from saved {net} run={run_id} | stats count()"),
            format!("| from saved {net} run=latest | stats count()"),
        ] {
            let counted = client
                .query_paginated(&dsl, None, None)
                .await
                .unwrap_or_else(|e| panic!("{dsl} over {} failed: {e}", data_dir.display()));
            assert_eq!(
                counted.result.rows[0][0].to_string(),
                "3",
                "{dsl}: {:?}",
                counted.result
            );
        }
        let listed = client
            .query_paginated(
                &format!("| from saved {net} run={run_id} | table msg"),
                None,
                None,
            )
            .await
            .expect("the rows themselves read too");
        assert_eq!(listed.result.rows.len(), 3, "{:?}", listed.result);

        std::fs::remove_file(&file).expect("delete the stored result");

        for dsl in [
            format!("| from saved {net} run={run_id} | stats count()"),
            format!("| from saved {net} run=latest | stats count()"),
        ] {
            assert_unavailable(
                refusal(client.query_paginated(&dsl, None, None).await),
                run_id,
            );
        }
        assert_unavailable(
            refusal(client.get_report_run(saved.id, run_id).await),
            run_id,
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_run_reads_under_a_spaced_data_dir() {
        reads_under_odd_data_dir("data with spaces", "spaced_net").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_run_reads_under_a_unicode_data_dir() {
        reads_under_odd_data_dir("données", "unicode_net").await;
    }
}
