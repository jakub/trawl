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
