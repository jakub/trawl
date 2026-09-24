// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native drift guard for the Playwright harness's wire fixtures.
//!
//! `e2e/harness/wire/*.json` are the canned bodies `harness/server.mjs`
//! answers with. The SPA decodes them with the very structs in
//! `trawl-api`, so a fixture that has drifted from the wire type does not
//! fail loudly in the browser: it either renders an empty state or the
//! `Loaded` error arm, and the spec reading it reports something other
//! than what it meant to test.
//!
//! This test decodes each fixture into that struct with `serde_json`. It
//! proves the shape only. Whether the CONTENT is the case a spec wants
//! (a running job, a succeeded twin) is asserted below as well, because a
//! `status` typo would keep the fixture decodable and make the poll spec
//! silently test nothing.
//!
//! When this test fails, fix the FIXTURE. The wire type is the authority.

use trawl_api::{
    CatalogFieldResponse, ListSavedResponse, RepinStatusResponse, SavedQueryResponse,
    ServiceSchemaResponse,
};

const CATALOG_FIELD: &str = include_str!("../e2e/harness/wire/catalog-field.json");
const REPIN_RUNNING: &str = include_str!("../e2e/harness/wire/repin-status-running.json");
const REPIN_SUCCEEDED: &str = include_str!("../e2e/harness/wire/repin-status-succeeded.json");
const SERVICE_SCHEMA: &str = include_str!("../e2e/harness/wire/service-schema.json");
const SERVICE_SCHEMA_POPULATED: &str =
    include_str!("../e2e/harness/wire/service-schema-populated.json");
const SAVED_QUERIES: &str = include_str!("../e2e/harness/wire/saved-queries.json");

/// Decode or fail with the serde message, which names the offending key.
fn decode<T: serde::de::DeserializeOwned>(name: &str, text: &str) -> T {
    match serde_json::from_str::<T>(text) {
        Ok(v) => v,
        Err(e) => panic!(
            "e2e/harness/wire/{name} no longer decodes as its trawl-api type: {e}. \
             The wire type is the authority — fix the fixture, not the struct."
        ),
    }
}

#[test]
fn catalog_field_fixture_decodes() {
    let resp: CatalogFieldResponse = decode("catalog-field.json", CATALOG_FIELD);
    // `data_type` is serde-renamed to `type` on the wire; decoding a
    // non-empty string here is what proves the fixture spells the key
    // the SPA reads rather than the field name.
    assert!(
        !resp.data_type.is_empty(),
        "catalog-field.json carries no `type` — the key is serde-renamed \
         from `data_type`, so a fixture spelling `data_type` decodes as an \
         empty pin instead of failing",
    );
    assert_eq!(resp.name, "duration");
}

#[test]
fn repin_status_fixtures_decode_as_a_running_job_and_its_twin() {
    let running: RepinStatusResponse = decode("repin-status-running.json", REPIN_RUNNING);
    let succeeded: RepinStatusResponse = decode("repin-status-succeeded.json", REPIN_SUCCEEDED);
    let running = running
        .job
        .expect("repin-status-running.json must carry a job, not `null`");
    let succeeded = succeeded
        .job
        .expect("repin-status-succeeded.json must carry a job, not `null`");

    // The case file only starts its poll for `running`, and only raises
    // the "Repin finished" toast for a succeeded job that was not a dry
    // run. Both are what the teardown spec is built on.
    assert_eq!(running.status, "running");
    assert!(running.finished_at.is_none());
    assert!(!running.dry_run);
    assert_eq!(succeeded.status, "succeeded");
    assert!(!succeeded.dry_run);
    assert!(succeeded.finished_at.is_some());

    // One job seen twice, not two jobs: the case file tracks a job BY ID
    // and treats a different id as the one-running slot having moved on
    // (`repin_flow::poll_decide`), which would stop the poll for a reason
    // the spec is not testing.
    assert_eq!(
        running.id, succeeded.id,
        "the succeeded fixture must be the SAME job as the running one",
    );
    assert_eq!(running.field, succeeded.field);
}

#[test]
fn service_schema_fixture_decodes() {
    let resp: ServiceSchemaResponse = decode("service-schema.json", SERVICE_SCHEMA);
    // `cached` has no serde default: the pre-wire fixture omitted it, so
    // every /schema/services read failed to decode and the schema page
    // rendered its error arm.
    assert!(!resp.cached);
    assert!(
        resp.services.is_empty(),
        "the schema specs rely on no service drawer being mountable",
    );
}

/// The `populated` scenario's two bodies. Their CONTENT is pinned, not
/// just their shape: the specs navigate to `?svc=nginx` and `?net=1` by
/// hand (mirrored in `e2e/fixtures.ts`'s `POPULATED`), and a renamed
/// service or a re-numbered net would leave those URLs mounting nothing
/// while every assertion below it waited out its timeout.
#[test]
fn the_populated_scenario_carries_one_service_and_one_net() {
    let services: ServiceSchemaResponse =
        decode("service-schema-populated.json", SERVICE_SCHEMA_POPULATED);
    assert_eq!(services.services.len(), 1);
    assert_eq!(services.services[0].name, "nginx");
    assert!(
        !services.services[0].columns.is_empty(),
        "the service drawer's Fields pane needs at least one column to render",
    );

    let saved: ListSavedResponse = decode("saved-queries.json", SAVED_QUERIES);
    assert_eq!(saved.queries.len(), 1);
    assert_eq!(saved.queries[0].id, 1);
}

// ---- the `corpus` scenario -------------------------------------------------

use trawl_api::{
    HistoryResponse, ListAllRunsResponse, ListReportRunsResponse, QueryResponse, ReportRunResponse,
    RunsStatsResponse,
};

const QUERY_ROWS: &str = include_str!("../e2e/harness/wire/query-rows.json");
const QUERY_CARDINALITY: &str = include_str!("../e2e/harness/wire/query-cardinality.json");
const QUERY_TOP_VALUES: &str = include_str!("../e2e/harness/wire/query-top-values.json");
const QUERY_TIMECHART: &str = include_str!("../e2e/harness/wire/query-timechart.json");
const SERVICE_SCHEMA_CORPUS: &str = include_str!("../e2e/harness/wire/service-schema-corpus.json");
const HISTORY: &str = include_str!("../e2e/harness/wire/history.json");
const NET_RUNS: &str = include_str!("../e2e/harness/wire/net-runs.json");
const RUN_RESULT: &str = include_str!("../e2e/harness/wire/run-result.json");
const RUNS_ALL: &str = include_str!("../e2e/harness/wire/runs-all.json");
const RUNS_STATS: &str = include_str!("../e2e/harness/wire/runs-stats.json");

/// The harness's shape dispatch, and the source it claims to mirror.
const HARNESS_FIXTURES: &str = include_str!("../e2e/harness/fixtures.mjs");
const SEARCH_URL_SRC: &str = include_str!("../src/search_url.rs");
/// The overview histogram's query is written inline here, not in
/// `drawer_query.rs`, so the third shape is pinned by grepping this.
const SERVICE_DRAWER_SRC: &str = include_str!("../src/components/service_drawer.rs");

/// The DSL builders themselves, not a copy of them. `trawl-web-ui` is a
/// binary crate, so an integration test cannot `use` its modules; this
/// path-mod compiles the one file whose output the dispatch keys on.
/// Its own `#[cfg(test)] mod tests` rides along and runs here too, which
/// is duplicated but harmless.
#[path = "../src/drawer_query.rs"]
mod drawer_query;

/// Same trick for the grid the histogram fixture has to land on:
/// `build_histogram` is a private component helper, but the two pure
/// functions it reads its rows through live here and can be called.
#[path = "../src/histogram.rs"]
mod histogram;

/// The rows every `corpus` spec reads. Their CONTENT is the contract: a
/// spec asserts on a host name, counts facet values, and tells one sort
/// order from another by the first row, so a re-shuffled fixture would
/// leave those assertions testing nothing in particular.
#[test]
fn the_corpus_rows_fixture_carries_a_facetable_page() {
    let resp: QueryResponse = decode("query-rows.json", QUERY_ROWS);
    let names: Vec<&str> = resp
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["_time", "host", "status", "message"]);
    assert_eq!(resp.result.rows.len(), 8);
    assert_eq!(resp.pagination.returned, resp.result.rows.len());
    for (i, row) in resp.result.rows.iter().enumerate() {
        assert_eq!(
            row.len(),
            names.len(),
            "row {i} has {} cells for {} columns",
            row.len(),
            names.len(),
        );
    }

    // Six distinct hosts: one more than the five a facet group shows, so
    // the group renders its "+ 1 more" control. Five would silently
    // delete the surface the facet spec is written against.
    let hosts: std::collections::BTreeSet<&str> = resp
        .result
        .rows
        .iter()
        .filter_map(|row| match &row[1] {
            trawl_api::value::Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        hosts.len(),
        6,
        "the `host` column must carry 6 distinct values, saw {hosts:?}",
    );

    // Ascending and descending have to disagree about the first row, or
    // a sort spec passes whichever way the header is wired.
    let mut statuses: Vec<&trawl_api::value::Value> =
        resp.result.rows.iter().map(|row| &row[2]).collect();
    statuses.dedup();
    assert!(
        statuses.len() > 1,
        "the `status` column has one value, so no sort of it is observable",
    );
}

#[test]
fn the_corpus_drawer_fixtures_decode() {
    let card: QueryResponse = decode("query-cardinality.json", QUERY_CARDINALITY);
    // The drawer reads cardinality back BY POSITION, against the aliases
    // the builder minted for the columns of the service it mounted, so
    // these two fixtures are one contract: the cardinality answer has to
    // have exactly one column per column of `service-schema-corpus.json`,
    // named `c0..c{n-1}`, or the decoder refuses the whole response.
    //
    // So the expectation is DERIVED from the builder rather than typed
    // out: ask it what it would send for the corpus service, and hold the
    // fixture to the answer.
    let corpus: ServiceSchemaResponse = decode("service-schema-corpus.json", SERVICE_SCHEMA_CORPUS);
    let columns: Vec<String> = corpus.services[0]
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let q = drawer_query::cardinality_query("nginx", &columns)
        .expect("the corpus service's columns are all nameable");
    let aliases: Vec<String> = (0..q.fields.len()).map(|i| format!("c{i}")).collect();
    let names: Vec<&str> = card
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, aliases);
    assert_eq!(card.result.rows.len(), 1);
    assert_eq!(card.result.rows[0].len(), q.fields.len());

    // A null cell is how the fixture says "no count for this field": the
    // decoder skips it, and `service-schema-corpus.json`'s `duration` row
    // renders as unknown rather than as zero.
    let counts = drawer_query::decode_cardinality(&card, &q.fields);
    assert_eq!(counts.get("_time"), Some(&1200));
    assert_eq!(counts.get("status"), Some(&5));
    assert_eq!(counts.get("duration"), None);

    let top: QueryResponse = decode("query-top-values.json", QUERY_TOP_VALUES);
    let names: Vec<&str> = top.result.columns.iter().map(|c| c.name.as_str()).collect();
    // `value`, not the field's own name: `service_drawer::parse_top_values`
    // accepts either, and `value` is what lets ONE fixture answer the
    // top-values read for whichever field a spec opened.
    assert_eq!(names, ["value", "count"]);
    assert!(!top.result.rows.is_empty());
}

/// The overview histogram's fixture. Its columns are what
/// `service_drawer::build_histogram` looks for by NAME, and its rows have
/// to survive `parse_bucket_ms` and land on the 24-slot hourly grid
/// `align_buckets` lays out. A fixture whose timestamps the parser
/// rejects, or whose buckets fall off the grid, renders an empty chart
/// that looks exactly like a chart with no data.
#[test]
fn the_corpus_timechart_fixture_lands_on_the_drawer_grid() {
    let resp: QueryResponse = decode("query-timechart.json", QUERY_TIMECHART);
    let names: Vec<&str> = resp
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["_time", "count"]);

    // The reader's own column rule, mirrored: the time column is one of
    // four accepted names and the count column is `count` or a name
    // starting with it. Asserting the names above is not enough on its
    // own, and this is what says WHY those two names.
    assert!(matches!(
        names[0],
        "_time" | "time" | "timestamp" | "@timestamp"
    ));
    assert!(names[1].starts_with("count"));

    let rows: Vec<(i64, u64)> = resp
        .result
        .rows
        .iter()
        .map(|row| {
            let trawl_api::value::Value::String(ts) = &row[0] else {
                panic!("the bucket start must be a string the display path can read: {row:?}");
            };
            let ms = histogram::parse_bucket_ms(ts)
                .unwrap_or_else(|| panic!("`{ts}` is not a timestamp the drawer can parse"));
            let trawl_api::value::Value::Integer(count) = &row[1] else {
                panic!("the bucket count must be an integer: {row:?}");
            };
            (
                ms,
                u64::try_from(*count).expect("a bucket count is not negative"),
            )
        })
        .collect();
    assert_eq!(rows.len(), 6);

    // 24 hourly slots ending at the newest row, which is the grid the
    // drawer builds (`INGEST_SLOT_MS` / `INGEST_SLOTS`). Every row has to
    // be inside it, or the chart quietly drops a bar.
    let slots = histogram::align_buckets(&rows, 3_600_000, 24);
    assert_eq!(slots.len(), 24);
    let placed: u64 = slots.iter().map(|s| s.count).sum();
    let offered: u64 = rows.iter().map(|&(_, c)| c).sum();
    assert_eq!(
        placed, offered,
        "some buckets fell outside the 24h grid the drawer lays out",
    );
    assert!(
        slots.iter().any(|s| s.count == 0),
        "an all-populated grid would not show that gaps render as gaps",
    );
}

/// `corpus` serves its OWN services body so it can carry a degraded
/// column; `populated` keeps the one its specs were written against.
/// The badge is rendered from the service's `degraded_fields` list
/// (`service_card_fmt::is_degraded_column`), so a name that is not also
/// a column of that service renders nothing at all.
#[test]
fn the_corpus_service_schema_marks_one_column_degraded() {
    let corpus: ServiceSchemaResponse = decode("service-schema-corpus.json", SERVICE_SCHEMA_CORPUS);
    let populated: ServiceSchemaResponse =
        decode("service-schema-populated.json", SERVICE_SCHEMA_POPULATED);
    assert_eq!(corpus.services.len(), 1);
    let svc = &corpus.services[0];
    assert_eq!(svc.name, populated.services[0].name);
    assert_eq!(svc.degraded_fields, ["duration"]);
    assert!(
        svc.columns.iter().any(|c| c.name == "duration"),
        "the degraded field must be a column of the service, or no row carries the badge",
    );

    // The badge opens `/api/v1/schema/field?name=<field>`, and the stub
    // answers that with `catalog-field.json`. Same field, so the case
    // file is about the column the operator clicked.
    let case: CatalogFieldResponse = decode("catalog-field.json", CATALOG_FIELD);
    assert_eq!(case.name, svc.degraded_fields[0]);

    // `_time`'s sample is what the server writes for compaction's
    // non-UTC microsecond column (issue 238): fixed-width, six fraction
    // digits, no `Z`. `field-presentation.spec.ts` reads these back.
    for schema in [&corpus, &populated] {
        let time = schema.services[0]
            .columns
            .iter()
            .find(|c| c.name == "_time")
            .expect("_time column");
        assert_eq!(
            time.min_value.as_deref(),
            Some("2026-09-01T00:00:00.000000")
        );
        assert_eq!(
            time.max_value.as_deref(),
            Some("2026-09-01T23:59:59.999999")
        );
    }

    // The split is the point: `populated` stays undegraded, so a spec
    // that wants a badge has to say `corpus` and means it.
    assert!(populated.services[0].degraded_fields.is_empty());
}

/// The history fixture's second entry exists to be REFUSED. Its length is
/// the whole point, so it is pinned here rather than trusted: an entry
/// that drifted under the bound would make the refusal spec pass by
/// rerunning the query it was meant to prove nobody reruns.
#[test]
fn the_corpus_history_fixture_carries_an_over_bound_query() {
    let resp: HistoryResponse = decode("history.json", HISTORY);
    assert_eq!(resp.entries.len(), 2);
    assert_eq!(resp.total, 2);
    assert_eq!(
        resp.entries[0].query,
        "service=nginx _severity>=error last=1h"
    );

    let over = &resp.entries[1].query;
    assert_eq!(
        over.len(),
        32_769,
        "the over-bound history entry must be exactly one byte over MAX_SEARCH_BYTES",
    );
    assert!(
        over.is_ascii(),
        "a multi-byte char would make the byte length disagree with the char count a spec reads"
    );

    // `MAX_SEARCH_BYTES` lives in the binary crate, so this test cannot
    // import it. Pinning its SPELLING is the next best thing: change the
    // constant and this fails, naming the fixture that has to follow.
    assert!(
        SEARCH_URL_SRC.contains("pub const MAX_SEARCH_BYTES: usize = 32 * 1024;"),
        "MAX_SEARCH_BYTES is no longer 32 * 1024 — history.json's 32769-byte entry \
         may no longer be over the bound",
    );
}

#[test]
fn the_corpus_run_fixtures_decode() {
    let runs: ListReportRunsResponse = decode("net-runs.json", NET_RUNS);
    assert_eq!(runs.runs.len(), 2);
    assert_eq!(runs.total, 2);
    // One of each outcome: the run rows render a status label and an
    // error cell, and a page of successes would leave failures untested.
    assert_eq!(runs.runs[0].status, "success");
    assert_eq!(runs.runs[1].status, "error");
    assert!(runs.runs[1].error_message.is_some());

    let run: ReportRunResponse = decode("run-result.json", RUN_RESULT);
    assert_eq!(run.summary.id, runs.runs[0].id);
    let result = run
        .result
        .expect("run-result.json must carry a result — the expansion renders it");
    assert!(!result.rows.is_empty());

    let all: ListAllRunsResponse = decode("runs-all.json", RUNS_ALL);
    assert_eq!(all.runs.len(), 2);
    // The runs page's row control links to `?net=<net_id>`, and the net
    // it names has to be the one `saved-queries.json` declares.
    let saved: ListSavedResponse = decode("saved-queries.json", SAVED_QUERIES);
    assert_eq!(all.runs[0].net_id, saved.queries[0].id);
    assert_eq!(all.runs[0].net_name, saved.queries[0].name);
    // A run whose stored result is gone answers 409 with no summary, so
    // the runs page's receipt prints this row instead (`CORPUS
    // .runWithResultRows` and `.runWithResultQuery` in `e2e/fixtures.ts`).
    assert_eq!(all.runs[0].run.id, 501);
    assert_eq!(all.runs[0].run.row_count, Some(3));
    assert_eq!(
        all.runs[0].run.query,
        "_severity>=error last=1h | stats count() by host"
    );

    let stats: RunsStatsResponse = decode("runs-stats.json", RUNS_STATS);
    assert_eq!(stats.total_runs, all.runs.len() as u64);
    assert_eq!(
        stats.success_count + stats.error_count + stats.timeout_count,
        stats.total_runs,
    );

    // The harness serves both files under the one scenario, so the
    // summary has to be the summary OF those runs. The server computes
    // it with `AVG(duration_ms) FILTER (WHERE duration_ms IS NOT NULL)`
    // over every run of the key, whatever its status, then narrows the
    // f64 with `as u64` — which truncates. Two runs of 125 ms and 90 ms
    // average 107.5 and report 107, not 108.
    let timed: Vec<u64> = all.runs.iter().filter_map(|r| r.run.duration_ms).collect();
    let expected = if timed.is_empty() {
        None
    } else {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let avg = (timed.iter().sum::<u64>() as f64 / timed.len() as f64) as u64;
        Some(avg)
    };
    assert_eq!(
        stats.avg_duration_ms, expected,
        "runs-stats.json must summarise runs-all.json: the server truncates \
         the average of every non-null duration_ms, so {timed:?} is {expected:?}",
    );
}

/// The harness dispatches `/api/v1/query` under `corpus` on two DSL
/// substrings. Nothing in the browser notices when one stops matching:
/// the drawer's read would fall through to the pipeline catch-all and
/// the pane would render its error arm, which reads like a broken app
/// rather than a stale fixture.
///
/// So this asserts the substrings against the BUILDERS, by calling them,
/// rather than grepping `drawer_query.rs` for them — `| stats dc(` is
/// assembled from a format string and an expression list and appears in
/// no single line of that file.
#[test]
fn the_query_shapes_match_the_dsl_the_drawer_builds() {
    let top_values = "| top 10 ";
    let cardinality = "| stats dc(";
    let timechart = "| timechart span=1h count()";

    for shape in [top_values, cardinality, timechart] {
        assert!(
            HARNESS_FIXTURES.contains(&format!("'{shape}'")),
            "e2e/harness/fixtures.mjs no longer exports `{shape}` in QUERY_SHAPES",
        );
    }

    let top = drawer_query::top_values_query("nginx", "host")
        .expect("`host` is a nameable field, so the drawer builds a top-values query for it");
    assert!(
        top.dsl.contains(top_values),
        "top_values_query now writes `{}`, which the harness would not recognise",
        top.dsl,
    );

    let card = drawer_query::cardinality_query("nginx", &["status".to_owned()])
        .expect("one nameable field is enough to build a cardinality query");
    assert!(
        card.dsl.contains(cardinality),
        "cardinality_query now writes `{}`, which the harness would not recognise",
        card.dsl,
    );

    // The third shape has no builder to call: the overview pane formats
    // it inline. Pin the literal in that source file instead, which is
    // exact enough that a reworded stage fails here.
    assert!(
        SERVICE_DRAWER_SRC.contains(&format!("last=24h {timechart}")),
        "the overview histogram no longer writes `{timechart}`, so the harness \
         would answer its read with a 500",
    );

    // The collision form is deliberately NOT recognised: it is the shape
    // the harness answers 500 to, and a spec that provoked it must see
    // that rather than a page of rows.
    let collision = drawer_query::top_values_query("nginx", "count")
        .expect("`count` is nameable — it is the field whose output name collides");
    assert!(!collision.dsl.contains(top_values));
    assert!(!collision.dsl.contains(cardinality));
}

fn assert_dashboard_measurement_metadata_is_required(dashboard: &trawl_api::DashboardSnapshot) {
    let dashboard_json = serde_json::to_value(dashboard).unwrap();
    for field in ["wal_measurement", "parquet_measurement"] {
        let mut missing = dashboard_json.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<trawl_api::DashboardSnapshot>(missing).is_err());
        let mut missing_status = dashboard_json.clone();
        missing_status[field]
            .as_object_mut()
            .unwrap()
            .remove("status");
        assert!(serde_json::from_value::<trawl_api::DashboardSnapshot>(missing_status).is_err());
    }
}

#[test]
fn health_page_fixtures_decode_and_exercise_permissions_and_failures() {
    let ok: trawl_api::HealthResponse = decode(
        "health-ok.json",
        include_str!("../e2e/harness/wire/health-ok.json"),
    );
    let failed: trawl_api::HealthResponse = decode(
        "health-unavailable.json",
        include_str!("../e2e/harness/wire/health-unavailable.json"),
    );
    assert_eq!(ok.status, trawl_api::HealthStatus::Ok);
    assert_eq!(failed.status, trawl_api::HealthStatus::Unavailable);
    assert_eq!(failed.checks.as_ref().unwrap()["duckdb"], "error");
    assert_eq!(ok.version, failed.version);
    let stats: trawl_api::StatsResponse = decode(
        "health-stats.json",
        include_str!("../e2e/harness/wire/health-stats.json"),
    );
    assert_eq!(stats.total_queries, 1234);
    assert_eq!(stats.pool_capacity, stats.pool_available + 3);
    assert_eq!(stats.pool_retained, 1);
    let dashboard: trawl_api::DashboardSnapshot = decode(
        "health-dashboard.json",
        include_str!("../e2e/harness/wire/health-dashboard.json"),
    );
    assert_eq!(
        dashboard.wal_measurement.status,
        trawl_api::StorageMeasurementStatus::Complete
    );
    assert_eq!(dashboard.wal_measurement.sample_age_secs, Some(2));
    assert_eq!(
        dashboard.parquet_measurement.status,
        trawl_api::StorageMeasurementStatus::Complete
    );
    assert_eq!(dashboard.parquet_measurement.sample_age_secs, Some(2));
    assert_dashboard_measurement_metadata_is_required(&dashboard);
    assert_eq!(dashboard.hot_buffer_events, 731);
    assert_eq!(dashboard.pool_active, 3);
    assert_eq!(dashboard.pool_retained, stats.pool_retained);
    assert_eq!(dashboard.pool_capacity, stats.pool_capacity);
    let queries: trawl_api::QueriesResponse = decode(
        "health-queries.json",
        include_str!("../e2e/harness/wire/health-queries.json"),
    );
    assert_eq!(queries.active.len(), 2);
    assert_eq!(queries.recent.len(), 2);
    assert_eq!(
        queries
            .active
            .iter()
            .map(|q| (q.snapshot.id, q.own))
            .collect::<Vec<_>>(),
        [(101, true), (102, false)]
    );
    assert_eq!(
        queries
            .recent
            .iter()
            .map(|q| (q.snapshot.id, q.own))
            .collect::<Vec<_>>(),
        [(201, true), (202, false)]
    );
    assert_eq!(
        queries.active[0].snapshot.user,
        queries.active[1].snapshot.user
    );
    assert_eq!(
        queries.recent[0].snapshot.user,
        queries.recent[1].snapshot.user
    );
    assert!(queries.retained.is_empty());
    let accepted: trawl_api::CancelResponse = decode(
        "health-cancel-accepted.json",
        include_str!("../e2e/harness/wire/health-cancel-accepted.json"),
    );
    let finished: trawl_api::CancelResponse = decode(
        "health-cancel-finished.json",
        include_str!("../e2e/harness/wire/health-cancel-finished.json"),
    );
    assert!(accepted.cancelled);
    assert!(!finished.cancelled);
    assert_eq!(accepted.query_id, queries.active[0].snapshot.id);
    assert_eq!(accepted.query_id, finished.query_id);
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../e2e/harness/wire/health-queries.json")).unwrap();
    for lane in ["active", "recent"] {
        for entry in value[lane].as_array().unwrap() {
            assert!(
                entry.get("snapshot").is_none(),
                "wire entries must remain flat"
            );
            assert!(
                entry.get("key_id").is_none(),
                "key ids must not leave the server"
            );
        }
    }
}

#[test]
fn history_export_and_error_fixtures_match_wire_types_and_test_cases() {
    let rows: trawl_api::HistoryResponse = decode(
        "history-export.json",
        include_str!("../e2e/harness/wire/history-export.json"),
    );
    // The harness treats this fixture as the complete corpus, then derives
    // matching totals before pagination from its entries.
    assert_eq!(rows.total, rows.entries.len());
    assert_eq!(
        rows.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        [103, 102, 101]
    );
    assert_eq!(rows.entries[0].query, "=cmd(prod)");
    assert_eq!(rows.entries[1].query, "service=dev");
    assert_eq!(rows.entries[2].query, "prod \"雪,one\"\r\nnext");
    assert!(
        rows.entries
            .windows(2)
            .all(|pair| pair[0].executed_at > pair[1].executed_at)
    );
    let error: trawl_api::ErrorResponse = decode(
        "history-error.json",
        include_str!("../e2e/harness/wire/history-error.json"),
    );
    assert_eq!(error.error.code, trawl_api::ErrorCode::ServiceUnavailable);
}

#[test]
fn pagination_run_fixtures_have_three_matching_rows() {
    let global: trawl_api::ListAllRunsResponse = decode(
        "pagination-runs-all.json",
        include_str!("../e2e/harness/wire/pagination-runs-all.json"),
    );
    let drawer: trawl_api::ListReportRunsResponse = decode(
        "pagination-net-runs.json",
        include_str!("../e2e/harness/wire/pagination-net-runs.json"),
    );
    let stats: RunsStatsResponse = decode(
        "pagination-runs-stats.json",
        include_str!("../e2e/harness/wire/pagination-runs-stats.json"),
    );
    assert_eq!(stats.total_runs, global.total as u64);
    assert_eq!(
        stats.success_count,
        global
            .runs
            .iter()
            .filter(|r| r.run.status == "success")
            .count() as u64
    );
    assert_eq!(
        stats.error_count,
        global
            .runs
            .iter()
            .filter(|r| r.run.status == "error")
            .count() as u64
    );
    assert_eq!(
        stats.timeout_count,
        global
            .runs
            .iter()
            .filter(|r| r.run.status == "timeout")
            .count() as u64
    );
    let durations: Vec<_> = global
        .runs
        .iter()
        .filter_map(|r| r.run.duration_ms)
        .collect();
    assert_eq!(
        stats.avg_duration_ms,
        Some(durations.iter().sum::<u64>() / durations.len() as u64)
    );
    assert_eq!(global.total, 3);
    assert_eq!(drawer.total, 3);
    assert_eq!(global.runs.len(), 3);
    assert_eq!(drawer.runs.len(), 3);
    let saved: ListSavedResponse = decode("saved-queries.json", SAVED_QUERIES);
    let net = saved
        .queries
        .first()
        .expect("pagination's saved net must exist");
    for (global, drawer) in global.runs.iter().zip(&drawer.runs) {
        assert_eq!(global.net_id, net.id);
        assert_eq!(global.net_name, net.name);
        assert_eq!(global.run.id, drawer.id);
    }
}

#[test]
fn created_saved_query_fixture_preserves_the_editor_text() {
    let saved: SavedQueryResponse = decode(
        "saved-created.json",
        include_str!("../e2e/harness/wire/saved-created.json"),
    );
    assert_eq!(saved.id, 164);
    assert_eq!(saved.name, "editor snapshot");
    assert_eq!(saved.query, "  service=apache  | stats count()\n");
    assert_eq!(saved.created_at, "2026-09-09T12:00:00Z");
    assert_eq!(saved.updated_at, saved.created_at);
    assert!(saved.schedule.is_none());
}

// ---- the `schedule` scenario -----------------------------------------------

const SAVED_QUERIES_WINDOWED: &str =
    include_str!("../e2e/harness/wire/saved-queries-windowed.json");
const SCHEDULE_SAVED: &str = include_str!("../e2e/harness/wire/schedule-saved.json");
const SCHEDULE_CONFLICT: &str = include_str!("../e2e/harness/wire/schedule-conflict.json");
const SCHEDULE_NET_RUNS: &str = include_str!("../e2e/harness/wire/schedule-net-runs.json");
const RUN_RESULT_PAGED: &str = include_str!("../e2e/harness/wire/run-result-paged.json");
const RUN_STARTED: &str = include_str!("../e2e/harness/wire/run-started.json");
const RUN_REFUSED: &str = include_str!("../e2e/harness/wire/run-refused.json");

/// The `schedule` scenario's saved list: the unscheduled net and one net
/// per schedule mode, decoded through the SAME type the `populated` list
/// uses.
///
/// Every net is part of the contract. The windowed net is what the form
/// opens showing, and `lag` and `lag_secs` have to agree, because
/// `WindowDraft::from_schedule` reads the SECONDS to decide whether a lag
/// is real and prints the STRING — a fixture where those two disagree
/// would make the form show a value the spec cannot explain. Run now is
/// offered on every scheduled net, whatever its mode, and on no other:
/// the unscheduled net is the control, and the fixed-span and paused
/// query-mode nets are the modes a window-only reading would miss.
#[test]
fn the_schedule_scenario_carries_every_schedule_mode_and_an_unscheduled_net() {
    let saved: ListSavedResponse = decode("saved-queries-windowed.json", SAVED_QUERIES_WINDOWED);
    assert_eq!(saved.queries.len(), 4);

    let plain = &saved.queries[0];
    let windowed = &saved.queries[1];
    let fixed = &saved.queries[2];
    let query_mode = &saved.queries[3];
    assert_eq!(
        saved.queries.iter().map(|q| q.id).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(windowed.name, "tiled error digest");

    // The unscheduled net is the `populated` one verbatim: the specs read
    // it as the case where Run now is NOT offered.
    let populated: ListSavedResponse = decode("saved-queries.json", SAVED_QUERIES);
    assert_eq!(plain.id, populated.queries[0].id);
    assert_eq!(plain.name, populated.queries[0].name);
    assert_eq!(plain.query, populated.queries[0].query);
    assert!(
        plain.schedule.is_none(),
        "the plain net must carry no schedule, or Run now is offered on every net",
    );
    // Its query owns a time clause, which is the half a window conflicts
    // with — the refusal fixture below names that clause.
    assert!(plain.query.contains("last=1h"));

    let schedule = windowed
        .schedule
        .as_ref()
        .expect("the windowed net must carry a schedule, or the form has nothing to open on");
    assert_eq!(schedule.window.as_deref(), Some("since_last"));
    assert_eq!(schedule.lag.as_deref(), Some("5m"));
    assert_eq!(schedule.lag_secs, Some(300));
    assert_eq!(schedule.interval, "1h");
    assert!(
        schedule.covered_through.is_some(),
        "a tiling schedule reports where coverage resumes, and the removal hint quotes it",
    );
    // A window on a query that spells its own interval is what the server
    // refuses, so the windowed net's text must NOT carry one.
    assert!(!windowed.query.contains("last="));

    let fixed_schedule = fixed
        .schedule
        .as_ref()
        .expect("the fixed-span net must carry a schedule");
    assert_eq!(fixed_schedule.window.as_deref(), Some("15m"));
    assert!(!fixed.query.contains("last="));

    // Query mode AND paused: Run now fires a disabled schedule too.
    let query_schedule = query_mode
        .schedule
        .as_ref()
        .expect("the query-mode net must carry a schedule");
    assert!(query_schedule.window.is_none());
    assert!(!query_schedule.enabled);
}

/// What Run now answers: the claimed run and the refusal the stub arms.
///
/// The claimed run is a MANUAL run of the tiling net whose window starts
/// at that net's watermark, which is what a `since_last` manual run covers
/// (ADR-0018 as amended on 2026-09-23). Its text carries the spliced
/// bounds, as the run row stores them. The refusal is a 409's envelope
/// whose message the toast quotes, so the message is the contract.
#[test]
fn the_run_now_fixtures_carry_a_manual_window_and_a_refusal() {
    let saved: ListSavedResponse = decode("saved-queries-windowed.json", SAVED_QUERIES_WINDOWED);
    let watermark = saved.queries[1]
        .schedule
        .as_ref()
        .and_then(|s| s.covered_through.clone())
        .expect("the tiling net reports its watermark");

    let run: trawl_api::ReportRunSummary = decode("run-started.json", RUN_STARTED);
    assert_eq!(run.origin.as_deref(), Some("manual"));
    assert_eq!(run.status, "running");
    assert_eq!(run.window_kind.as_deref(), Some("since_last"));
    assert_eq!(run.window_truncated, Some(false));
    assert_eq!(run.window_start.as_deref(), Some(watermark.as_str()));
    let (start, end) = (
        run.window_start.as_deref().unwrap(),
        run.window_end
            .as_deref()
            .expect("a windowed run carries its end"),
    );
    assert!(
        run.query
            .starts_with(&format!("earliest=\"{start}\" latest=\"{end}\" ")),
        "the stored text is the saved text with the window spliced in front",
    );
    // The spec's toast expectation is written against these two bounds.
    assert_eq!(start, "2026-09-01T09:55:00.000000Z");
    assert_eq!(end, "2026-09-01T11:15:00.000000Z");

    let refusal: trawl_api::ErrorResponse = decode("run-refused.json", RUN_REFUSED);
    assert_eq!(refusal.error.code, trawl_api::ErrorCode::BadRequest);
    assert!(
        refusal
            .error
            .message
            .starts_with("nothing new to read: coverage already reaches "),
        "the refusal mirrors trawl-server's empty-window sentence",
    );

    // Recorded runs carry their origin too: the scheduled history rows
    // say so, and carry no Manual marker.
    let history: ListReportRunsResponse = decode("schedule-net-runs.json", SCHEDULE_NET_RUNS);
    assert!(
        history
            .runs
            .iter()
            .all(|r| r.origin.as_deref() == Some("scheduled"))
    );
}

/// The two schedule PUT answers. Shape only for the success, because no
/// spec reads it: the assertion a schedule spec makes is about the
/// request. The refusal's MESSAGE is the contract, since the form renders
/// it verbatim.
#[test]
fn the_schedule_put_fixtures_decode_as_their_wire_types() {
    let saved: trawl_api::ScheduleResponse = decode("schedule-saved.json", SCHEDULE_SAVED);
    assert_eq!(saved.saved_query_id, 2);

    let refusal: trawl_api::ErrorResponse = decode("schedule-conflict.json", SCHEDULE_CONFLICT);
    assert_eq!(refusal.error.code, trawl_api::ErrorCode::BadRequest);
    // `ServerError::WindowPolicy` answers 400 with an envelope carrying
    // `WindowPolicyError::TimeClause`'s own sentence
    // (`crates/trawl-server/src/report_window.rs`). Mirrored, not
    // invented: the spec asserts the form shows it character for
    // character, which is only worth asserting if it is the real one.
    assert_eq!(
        refusal.error.message,
        "schedule window \"since_last\" conflicts with the saved query's \
         last= time clause; remove one side",
    );
}

/// The long run and the list that offers it.
///
/// `row_count` is the run's own stored count and `rows.len()` is what the
/// response carried. They are EQUAL here on purpose: the preview's cap
/// line appears only when they differ, and the paging spec overrides the
/// count in the browser to produce that case. A fixture that shipped them
/// unequal would leave the uncapped assertion untestable.
#[test]
fn the_paged_run_fixture_carries_more_rows_than_one_preview_page() {
    let runs: ListReportRunsResponse = decode("schedule-net-runs.json", SCHEDULE_NET_RUNS);
    assert_eq!(runs.runs.len(), 3);
    assert_eq!(runs.total, 3);
    // Newest first, and the long run is the newest: a spec expands the
    // first row rather than hunting for an id.
    assert_eq!(runs.runs[0].id, 503);
    assert_eq!(runs.runs[0].row_count, Some(45));
    // The other two are the `corpus` runs verbatim, so the one short
    // preview stays available under this scenario as well.
    let corpus: ListReportRunsResponse = decode("net-runs.json", NET_RUNS);
    assert_eq!(
        runs.runs[1..].iter().map(|r| r.id).collect::<Vec<_>>(),
        corpus.runs.iter().map(|r| r.id).collect::<Vec<_>>(),
    );

    let run: ReportRunResponse = decode("run-result-paged.json", RUN_RESULT_PAGED);
    assert_eq!(run.summary.id, runs.runs[0].id);
    assert_eq!(run.summary.row_count, Some(45));
    let result = run
        .result
        .expect("run-result-paged.json must carry a result — the expansion pages it");
    assert_eq!(
        result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["seq", "message"],
    );
    assert_eq!(result.rows.len(), 45);
    assert_eq!(run.summary.row_count, Some(result.rows.len()));

    // `seq` is the row's own 1-based position, so a spec can name the
    // rows it expects on each page instead of counting them. 45 rows at
    // the preview's 20 per page is three pages, the last one short —
    // which is the case a pager gets wrong.
    let seqs: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match &row[0] {
            trawl_api::value::Value::Integer(n) => *n,
            other => panic!("the `seq` cell must be an integer: {other:?}"),
        })
        .collect();
    assert_eq!(seqs, (1..=45).collect::<Vec<i64>>());
    assert_eq!(
        result.rows[0][1].to_string(),
        "row-01",
        "each row's message names its own index, so a page assertion reads as itself",
    );
    assert_eq!(result.rows[44][1].to_string(), "row-45");

    // The page size the spec's expectations are written against lives in
    // the component. Pin its spelling: a change there has to reach the
    // spec, and this names the file to fix.
    assert!(
        NET_DRAWER_SRC.contains("const PREVIEW_PAGE_SIZE: std::num::NonZeroUsize ="),
        "PREVIEW_PAGE_SIZE moved or was renamed — the paging spec's 20-row pages follow it",
    );
    assert!(NET_DRAWER_SRC.contains("NonZeroUsize::new(20)"));
}

/// The net drawer's own source, for the one constant the paging spec
/// mirrors.
const NET_DRAWER_SRC: &str = include_str!("../src/components/net_drawer.rs");

#[test]
fn result_actions_fixture_has_numeric_and_null_aggregate_cells() {
    use trawl_api::value::Value;
    let resp: QueryResponse = decode(
        "result-actions.json",
        include_str!("../e2e/harness/wire/result-actions.json"),
    );
    assert_eq!(
        resp.result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["host", "count"]
    );
    assert_eq!(resp.pagination.returned, resp.result.rows.len());
    assert_eq!(
        resp.result
            .rows
            .iter()
            .map(|r| r[1].clone())
            .collect::<Vec<_>>(),
        vec![
            Value::Integer(100),
            Value::Integer(20),
            Value::Integer(10),
            Value::Null,
            Value::Float(-1.5)
        ]
    );
}

const QUERY_FIELD_PRESENTATION: &str =
    include_str!("../e2e/harness/wire/query-field-presentation.json");

/// `field-presentation.spec.ts` and `facets.spec.ts` read this page as
/// the cases issue 238 names: one-off `_time` and sender `timestamp`
/// columns, the raw event and its ingest instant, a repeating `level`, a
/// distinct `host` that is not a time, an event with no null field, and
/// a sparse one with seven.
#[test]
fn field_presentation_fixture_carries_one_off_times_and_a_sparse_event() {
    use std::collections::HashSet;
    use trawl_api::value::Value;

    let resp: QueryResponse = decode("query-field-presentation.json", QUERY_FIELD_PRESENTATION);
    let columns: Vec<&str> = resp
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        columns,
        [
            "_time",
            "service",
            "host",
            "level",
            "message",
            "timestamp",
            "user",
            "trace_id",
            "span_id",
            "region",
            "error_code",
            "retry_count",
            "client_ip",
            "_raw",
            "_ingested",
        ]
    );
    let rows = &resp.result.rows;
    assert_eq!(rows.len(), 5);
    let column = |name: &str| -> Vec<&Value> {
        let idx = columns.iter().position(|c| *c == name).expect(name);
        rows.iter().map(|r| &r[idx]).collect()
    };
    let text = |name: &str| -> Vec<String> {
        column(name)
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => panic!("{name} holds {other:?}, not text"),
            })
            .collect()
    };
    let distinct = |values: &[String]| values.iter().collect::<HashSet<_>>().len();

    // Every value distinct and a timestamp the browser parses: the rail
    // drops these.
    for name in ["_time", "timestamp", "_ingested"] {
        let values = text(name);
        assert_eq!(distinct(&values), values.len(), "{name} must be one-off");
        for value in &values {
            assert!(
                fleet_ui::time::parse_timestamp(value).is_some(),
                "{name} value {value} must parse as a timestamp"
            );
        }
    }
    // Distinct but not times: the rail keeps it.
    let hosts = text("host");
    assert_eq!(distinct(&hosts), hosts.len());
    assert!(
        hosts
            .iter()
            .all(|h| fleet_ui::time::parse_timestamp(h).is_none())
    );
    // A repeat, so `level` is a dimension by any reading.
    let levels = text("level");
    assert!(distinct(&levels) < levels.len());
    assert_eq!(distinct(&text("_raw")), rows.len());

    let nulls = |row: &[Value]| row.iter().filter(|v| matches!(v, Value::Null)).count();
    assert_eq!(nulls(&rows[0]), 0, "event 1 has no null field");
    assert_eq!(nulls(&rows[1]), 7, "event 2 is the sparse one");
}

/// Every query fixture the stub serves, decoded as the wire type.
const QUERY_FIXTURES: [(&str, &str); 7] = [
    ("query-rows", QUERY_ROWS),
    ("query-cardinality", QUERY_CARDINALITY),
    ("query-top-values", QUERY_TOP_VALUES),
    ("query-timechart", QUERY_TIMECHART),
    (
        "query-stats-by",
        include_str!("../e2e/harness/wire/query-stats-by.json"),
    ),
    (
        "result-actions",
        include_str!("../e2e/harness/wire/result-actions.json"),
    ),
    ("query-field-presentation", QUERY_FIELD_PRESENTATION),
];

/// Each fixture is a whole result: its `total` is the count the
/// execution produced, and no window was cut from it.
#[test]
fn query_fixtures_report_the_whole_result_they_carry() {
    for (name, body) in QUERY_FIXTURES {
        let response: QueryResponse = decode(name, body);
        assert_eq!(
            response.pagination.offset, 0,
            "{name} is a first window, so its offset is zero"
        );
        assert_eq!(
            response.pagination.returned,
            response.result.rows.len(),
            "{name} must count the rows it carries"
        );
        assert_eq!(
            response.pagination.total,
            response.result.rows.len(),
            "{name} carries its whole result, so `total` is that row count"
        );
    }
}

#[test]
fn query_fixtures_carry_fixed_server_execution_facts() {
    // `result-actions.json` is excluded: it carries no execution facts,
    // which is the synthetic-result shape the wire type also allows.
    for (name, body) in QUERY_FIXTURES
        .into_iter()
        .filter(|(name, _)| *name != "result-actions")
    {
        let response: QueryResponse = decode(name, body);
        let execution = response
            .execution
            .expect("successful fixture has execution facts");
        assert_eq!(execution.started_at, "2026-09-15T12:34:56Z");
        assert_eq!(execution.duration_ms, 125);
    }
}

#[test]
fn global_sort_fixture_has_unique_flat_ids_and_cross_page_comparisons() {
    let raw = include_str!("../e2e/harness/wire/runs-sort.json");
    let response: ListAllRunsResponse = decode("runs-sort", raw);
    assert_eq!(response.total, 43);
    assert_eq!(response.runs.len(), 43);
    let ids: std::collections::BTreeSet<_> = response.runs.iter().map(|r| r.run.id).collect();
    assert_eq!(ids.len(), 43);
    assert_eq!(ids.first(), Some(&501));
    assert_eq!(ids.last(), Some(&543));
    let value: serde_json::Value = serde_json::from_str(raw).unwrap();
    assert!(
        value["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("id").is_some() && r.get("run").is_none())
    );
    for page in response.runs.chunks(20).take(2) {
        assert!(page.iter().any(|r| r.run.duration_ms.is_none()));
        assert!(page.iter().any(|r| r.run.row_count.is_none()));
        assert!(page.iter().any(|r| r.net_name == "ALPHA digest"));
        assert!(page.iter().any(|r| r.net_name == "alpha digest"));
    }
    // Equal timestamp and primary-value groups are deliberate; IDs settle ties.
    let first = response.runs.iter().find(|r| r.run.id == 501).unwrap();
    let second = response.runs.iter().find(|r| r.run.id == 502).unwrap();
    assert_eq!(first.run.started_at, second.run.started_at);
    let durations: std::collections::BTreeSet<_> = response
        .runs
        .iter()
        .filter_map(|r| r.run.duration_ms)
        .collect();
    assert_eq!(durations, [0, 9, 40, 125, 1000].into_iter().collect());
}

#[test]
fn wide_result_capture_fixture_has_full_width_rows_and_local_pages() {
    let response: ReportRunResponse = decode(
        "run-result-wide",
        include_str!("../e2e/harness/wire/run-result-wide.json"),
    );
    assert_eq!(response.summary.id, 501);
    let result = response.result.expect("stored result is available");
    assert_eq!(result.rows.len(), 45);
    assert_eq!(result.columns.len(), 8);
    assert!(
        result
            .rows
            .iter()
            .all(|row| row.len() == result.columns.len())
    );
    assert_eq!(result.columns[6].name, "trace_id");
}

// ---- query errors (ADR-0039) ---------------------------------------------
//
// The query error bodies the notice specs fulfil the query route with.
// Each is the envelope the server writes for the text the spec sends: the
// parse fixtures carry exactly the details the parser reports for that
// effective text (the server's parse arm copies them one for one), and the
// validation fixtures carry the emitter's own message, hint and summary.
// A parser message that moves fails here, not as a spec asserting on a
// message the server no longer writes.

const QUERY_PARSE_ERROR: &str = include_str!("../e2e/harness/wire/query-parse-error.json");
const QUERY_PARSE_ERRORS_TWO: &str =
    include_str!("../e2e/harness/wire/query-parse-errors-two.json");
const QUERY_VALIDATION_HINT: &str = include_str!("../e2e/harness/wire/query-validation-hint.json");
const QUERY_VALIDATION_NO_HINT: &str =
    include_str!("../e2e/harness/wire/query-validation-no-hint.json");
const QUERY_EXECUTION_ERROR: &str = include_str!("../e2e/harness/wire/query-execution-error.json");

/// `ErrorDetail` has no `PartialEq`; compare the wire form instead.
fn json(details: &[trawl_api::ErrorDetail]) -> serde_json::Value {
    serde_json::to_value(details).expect("details serialize")
}

/// The details the server's parse arm writes for `effective`.
fn parse_details(effective: &str) -> Vec<trawl_api::ErrorDetail> {
    trawl_core::parser::parse(effective)
        .expect_err("the fixture's text does not parse")
        .iter()
        .map(|e| trawl_api::ErrorDetail {
            message: e.message.clone(),
            span: Some(trawl_api::ErrorSpan {
                start: e.span.start,
                end: e.span.end,
            }),
            label: e.label.clone(),
            hint: e.hint.clone(),
        })
        .collect()
}

#[test]
fn the_parse_error_fixtures_are_what_the_parser_reports() {
    for (name, text, effective) in [
        (
            "query-parse-error.json",
            QUERY_PARSE_ERROR,
            "last=15m service=kubelet | stats count( by host",
        ),
        (
            "query-parse-errors-two.json",
            QUERY_PARSE_ERRORS_TWO,
            "last=15m f=#a,#b",
        ),
    ] {
        let body: trawl_api::ErrorResponse = decode(name, text);
        let expected = parse_details(effective);
        assert_eq!(body.error.code, trawl_api::ErrorCode::ParseError, "{name}");
        assert_eq!(body.error.message, expected[0].message, "{name}");
        assert_eq!(json(&body.error.details), json(&expected), "{name}");
    }
}

#[test]
fn the_validation_fixtures_are_what_the_emitter_reports() {
    use trawl_core::emitter::EmitError;
    for (name, text, error) in [
        (
            "query-validation-hint.json",
            QUERY_VALIDATION_HINT,
            EmitError::UnknownFunction {
                name: "countt".into(),
                suggestion: Some("count".into()),
            },
        ),
        (
            "query-validation-no-hint.json",
            QUERY_VALIDATION_NO_HINT,
            EmitError::UnknownFunction {
                name: "nosuchfunc".into(),
                suggestion: None,
            },
        ),
    ] {
        let body: trawl_api::ErrorResponse = decode(name, text);
        assert_eq!(
            body.error.code,
            trawl_api::ErrorCode::ValidationError,
            "{name}"
        );
        assert_eq!(body.error.message, error.to_string(), "{name}");
        assert_eq!(
            json(&body.error.details),
            json(&[trawl_api::ErrorDetail {
                message: error.message(),
                span: None,
                label: None,
                hint: error.hint(),
            }]),
            "{name}"
        );
    }
}

#[test]
fn the_execution_error_fixture_is_not_a_query_error() {
    let body: trawl_api::ErrorResponse =
        decode("query-execution-error.json", QUERY_EXECUTION_ERROR);
    assert_eq!(body.error.code, trawl_api::ErrorCode::ExecutionError);
    assert!(body.error.details.is_empty());
}
