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
    CatalogFieldResponse, ListSavedResponse, RepinStatusResponse, ServiceSchemaResponse,
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
    // The drawer reads cardinality back BY COLUMN NAME against the
    // columns of the service it mounted, so these fixtures are one
    // contract: a column here that the service does not declare is a
    // number nothing displays. The other direction is allowed and is how
    // `service-schema-corpus.json`'s `duration` behaves: a column with no
    // count is skipped, not rendered as zero.
    let names: Vec<&str> = card
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["_time", "status"]);
    assert_eq!(card.result.rows.len(), 1);

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
    // One of each outcome: the run rows render a status tone and an
    // error cell, and a page of successes would leave both untested.
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

    let stats: RunsStatsResponse = decode("runs-stats.json", RUNS_STATS);
    assert_eq!(stats.total_runs, all.runs.len() as u64);
    assert_eq!(
        stats.success_count + stats.error_count + stats.timeout_count,
        stats.total_runs,
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
        card.contains(cardinality),
        "cardinality_query now writes `{card}`, which the harness would not recognise",
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
