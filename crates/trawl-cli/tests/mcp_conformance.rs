// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl mcp` against a real trawld, in both protocol eras (ADR-0058).
//!
//! The server is trawld's own stack, booted in-process by `common` with a
//! seeded corpus: two services in two environments over two hosts,
//! ingested through `/api/v1/ingest` and compacted, so searches have rows,
//! facets, a dominant service, a lagging sender and long cells. The MCP
//! server is the real `trawl mcp --profile agent` child, started from an
//! empty environment (only the dynamic loader's search path survives, see
//! `loader_path`) with a temp `HOME` whose config names the profile:
//! the server's URL, its pinned certificate and the agent key in the
//! profile's own table.
//!
//! Each child is driven over its pipes with hand-written JSON-RPC lines,
//! once per era:
//!
//! - 2025-11-25: `initialize`, `notifications/initialized`, then
//!   requests that carry no per-request metadata;
//! - 2026-07-28: `server/discover`, then requests whose `_meta` carries the
//!   protocol version and client capabilities the revision requires.
//!
//! Both eras list the same tools, in the same order, with the same
//! schemas, and every tool is called against the seeded server. Every
//! result is one text block, and a successful one carries a
//! `structuredContent` that validates against the tool's listed
//! `outputSchema`. Every stdout line is a JSON-RPC message, and the child
//! exits 0 once its stdin closes.
//!
//! A cancelled search drops its trawld request: the test holds the
//! corpus's publication write guard so the capture waits, in flight and
//! listed by `/api/v1/queries`, then sends `notifications/cancelled` and
//! sees the query recorded as abandoned and no response for its id.
//!
//! An expired capture reads as trawld's 410 `result_gone`: the server's
//! results clock is a fixed one the test moves past the capture's
//! deadline, so expiry is deterministic, and neither `read` nor `fields`
//! runs or captures anything to replace it.
//!
//! Without `DATABASE_URL` these tests fail rather than skip. The binary is
//! in the ADR-0021 `postgres` nextest group.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use common::mcp::{DEADLINE, Era, Mcp, home};
use common::{Options, Server};
use serde_json::{Value, json};
use trawl_server::results::ResultsClock;

/// The origin the link test's server builds search URLs under.
const LINK_ORIGIN: &str = "https://trawl.example";

/// The `api` events: newest first, half a second apart, so every one is
/// under [`trawl_mcp::hints::LAG_THRESHOLD_SECS`] old when ingested.
const API_EVENTS: usize = 340;

/// The `worker` events, twenty minutes behind their arrival.
const WORKER_EVENTS: usize = 60;

/// Every this many `api` events, one carries a message long enough to be
/// abbreviated.
const LONG_EVERY: usize = 25;

/// How far behind its arrival a `worker` event's `_time` is.
const WORKER_LAG: TimeDelta = TimeDelta::minutes(20);

/// RFC 3339 with microseconds, in UTC.
fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A server that accepts the corpus's two environments.
fn options() -> Options {
    Options {
        envs: vec!["prod".to_owned(), "staging".to_owned()],
        ..Options::default()
    }
}

/// Ingest the corpus, compact it into Parquet and the catalog, and fill
/// the per-service schema cache from it.
///
/// - [`API_EVENTS`] of `api` in `prod`, newest at `now`, alternating hosts
///   `web-1` and `web-2`; every [`LONG_EVERY`]th carries a 1500-byte
///   message, so a page abbreviates it.
/// - [`WORKER_EVENTS`] of `worker` in `staging` on `web-2`, each
///   [`WORKER_LAG`] behind its arrival: the lagging sender.
///
/// `api` holds 85 % of the corpus: the dominant service.
async fn seed(server: &mut Server) -> DateTime<Utc> {
    let now = Utc::now();
    let mut events = Vec::with_capacity(API_EVENTS + WORKER_EVENTS);
    for i in 0..API_EVENTS {
        let at = now - TimeDelta::milliseconds(500 * i64::try_from(i).unwrap());
        let message = if i % LONG_EVERY == 0 {
            format!("request {i} payload {}", "x".repeat(1500))
        } else {
            format!("request {i} ok")
        };
        events.push(json!({
            "_time": rfc3339(at),
            "service": "api",
            "env": "prod",
            "host": if i % 2 == 0 { "web-1" } else { "web-2" },
            "level": "info",
            "message": message,
        }));
    }
    for i in 0..WORKER_EVENTS {
        let at = now - WORKER_LAG - TimeDelta::seconds(i64::try_from(i).unwrap());
        events.push(json!({
            "_time": rfc3339(at),
            "service": "worker",
            "env": "staging",
            "host": "web-2",
            "level": "warn",
            "message": format!("job {i} done"),
        }));
    }
    for batch in events.chunks(100) {
        server.ingest(batch).await;
    }
    server.compact().await;
    server.refresh_schema(2).await;
    now
}

/// The lines of `text` after its `rows from captured row N:` line.
fn row_lines(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|line| !line.starts_with("rows from captured row "))
        .skip(1)
        .collect()
}

/// The hint lines of `text`, without their `hint: ` prefix.
fn hints(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("hint: "))
        .collect()
}

/// The proposed `read` call of `structured` with `action`.
fn next_action<'a>(structured: &'a Value, action: &str) -> Option<&'a Value> {
    structured["next_actions"]
        .as_array()?
        .iter()
        .find(|next| next["tool"] == "read" && next["arguments"]["action"] == action)
}

/// An RFC 3339 instant.
fn instant(text: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text.as_str().unwrap_or_else(|| panic!("not text: {text}")))
        .unwrap_or_else(|e| panic!("not RFC 3339 ({e}): {text}"))
        .with_timezone(&Utc)
}

/// What `/api/v1/queries` and the agent's result list hold: a read that
/// re-ran a query, or captured again, changes one of them.
async fn executions(server: &Server) -> (Vec<u64>, Vec<u64>, Vec<String>) {
    let queries = server
        .client(&server.admin_token)
        .queries()
        .await
        .expect("list the queries");
    let results = server
        .client(&server.agent_token)
        .list_results()
        .await
        .expect("list the agent's results");
    (
        queries.active.iter().map(|q| q.snapshot.id).collect(),
        queries.recent.iter().map(|q| q.snapshot.id).collect(),
        results.results.into_iter().map(|r| r.id).collect(),
    )
}

/// Drive every tool through `era` against `server`, whose `link_origin`
/// is [`LINK_ORIGIN`]; returns the listed tools.
#[allow(clippy::too_many_lines)] // one session, each call asserted in order
async fn every_tool(server: &Server, home: &Path, era: Era, now: DateTime<Utc>) -> Vec<Value> {
    let case = format!("era {}", era.version());
    let mut mcp = Mcp::spawn(home, era);
    mcp.open().await;
    let tools = mcp.list_tools().await;

    let resources = mcp.request("resources/list", json!({})).await;
    let uris: Vec<&str> = resources["result"]["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("{case}: no resources: {resources}"))
        .iter()
        .filter_map(|r| r["uri"].as_str())
        .collect();
    assert!(
        uris.contains(&"trawl://dsl/search-stage"),
        "{case}: {uris:?}"
    );

    // ping is a session request: 2026-07-28 removed it from the inline
    // lifecycle.
    let ping_id = mcp.start("ping", json!({})).await;
    let ping = mcp.response(ping_id).await;
    match era {
        Era::Initialize => assert!(ping["result"].is_object(), "{case}: {ping}"),
        Era::Discover => assert_eq!(ping["error"]["code"], -32601, "{case}: {ping}"),
    }

    // orient: the installation, in UTC, naming what was seeded.
    let orient = mcp.call("orient", json!({})).await;
    assert!(!orient.is_error, "{case}: {}", orient.text);
    let mirror = orient.structured.as_ref().unwrap();
    assert_eq!(
        mirror["display_zone"], "UTC, the mcp host's setting",
        "{case}: {}",
        orient.text
    );
    assert!(
        mirror.get("degraded_note").is_none(),
        "{case}: the schema routes answered: {}",
        orient.text
    );
    let services: Vec<&str> = mirror["inventory"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["service"].as_str())
        .collect();
    for service in ["api", "worker"] {
        assert!(services.contains(&service), "{case}: {}", orient.text);
    }
    assert_eq!(
        mirror["key"]["permissions"],
        json!(["query", "schema_read", "validate"]),
        "{case}: {}",
        orient.text
    );

    // fields of a service, from the catalog the compaction filled.
    let fields = mcp
        .call("fields", json!({"source": "service", "service": "api"}))
        .await;
    assert!(!fields.is_error, "{case}: {}", fields.text);
    let names: Vec<&str> = fields.structured.as_ref().unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"host"), "{case}: {}", fields.text);
    assert!(names.contains(&"message"), "{case}: {}", fields.text);

    // validate: a valid query, and an invalid one that is still a
    // successful call, with a caret under the error.
    let valid = mcp
        .call(
            "search",
            json!({"mode": "validate", "query": "service=api last=1h"}),
        )
        .await;
    assert!(!valid.is_error, "{case}: {}", valid.text);
    assert_eq!(
        valid.structured.as_ref().unwrap()["validation"]["valid"],
        true,
        "{case}: {}",
        valid.text
    );
    let invalid = mcp
        .call(
            "search",
            json!({"mode": "validate", "query": "service=api last=1h | stats cout("}),
        )
        .await;
    assert!(
        !invalid.is_error,
        "{case}: a failed validation is a successful call: {}",
        invalid.text
    );
    let verdict = &invalid.structured.as_ref().unwrap()["validation"];
    assert_eq!(verdict["valid"], false, "{case}: {}", invalid.text);
    let excerpt = verdict["errors"][0]["excerpt"]
        .as_str()
        .unwrap_or_else(|| panic!("{case}: no excerpt: {}", invalid.text));
    assert!(excerpt.contains('^'), "{case}: {excerpt}");
    assert!(invalid.text.contains('^'), "{case}: {}", invalid.text);

    // execute over everything: rows, facets, the search URL, and the
    // dominant service named over the captured rows.
    let all = mcp
        .call("search", json!({"mode": "execute", "query": "last=1h"}))
        .await;
    assert!(!all.is_error, "{case}: {}", all.text);
    let reply = all.structured.as_ref().unwrap();
    let view = &reply["result"];
    let total = API_EVENTS + WORKER_EVENTS;
    assert_eq!(view["rows_captured"], total, "{case}: {}", all.text);
    // The response read the default rows; the ones it printed and the
    // ones it dropped to fit the default budget make them up.
    let printed = view["rows"].as_array().unwrap().len();
    assert!(printed > 0, "{case}: {}", all.text);
    assert_eq!(row_lines(&all.text).len(), printed, "{case}: {}", all.text);
    assert_eq!(
        printed as u64
            + view["presentation"]["rows_dropped_to_fit"]
                .as_u64()
                .unwrap(),
        u64::from(trawl_mcp::tools::schema::DEFAULT_ROWS),
        "{case}: {}",
        all.text
    );
    assert!(
        view["facets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|facet| facet["field"] == "service"),
        "{case}: {}",
        all.text
    );
    let url = view["link"]["url"]
        .as_str()
        .unwrap_or_else(|| panic!("{case}: no search URL: {}", all.text));
    assert!(url.starts_with(&format!("{LINK_ORIGIN}/")), "{case}: {url}");
    assert!(
        all.text.contains(&format!("search URL: {url}")),
        "{case}: {}",
        all.text
    );
    let dominance = format!(
        "dominance: \"service\" \"api\" holds {API_EVENTS} of the {total} captured rows (85%)"
    );
    assert!(
        hints(&all.text).iter().any(|h| h.starts_with(&dominance)),
        "{case}: expected {dominance:?} in {}",
        all.text
    );
    assert!(
        !view["abbreviated"].as_array().unwrap().is_empty(),
        "{case}: the long messages are abbreviated: {}",
        all.text
    );

    // The executed bounds are the capture's.
    let result_id = view["result_id"].as_str().unwrap().to_owned();
    let listed = server
        .client(&server.agent_token)
        .list_results()
        .await
        .expect("list the agent's results");
    let captured = listed
        .results
        .iter()
        .find(|r| r.id == result_id)
        .unwrap_or_else(|| panic!("{case}: {result_id} is not listed"));
    assert_eq!(
        instant(&view["bounds"]["earliest"]),
        instant(&json!(
            captured.earliest.as_deref().expect("captured earliest")
        )),
        "{case}"
    );
    assert_eq!(
        instant(&view["bounds"]["latest"]),
        instant(&json!(captured.latest.as_deref().expect("captured latest"))),
        "{case}"
    );

    // The lagging sender, over the rows the response read.
    let worker = mcp
        .call(
            "search",
            json!({"mode": "execute", "query": "service=worker last=1h"}),
        )
        .await;
    assert!(!worker.is_error, "{case}: {}", worker.text);
    assert!(
        hints(&worker.text)
            .iter()
            .any(|h| h.starts_with("lag: service \"worker\" on host \"web-2\" sent an event ")),
        "{case}: {}",
        worker.text
    );

    // No rows in the bounds: the nearest bounds holding data for the same
    // search.
    let empty_query = format!(
        "service=api earliest=\"{}\" latest=\"{}\"",
        rfc3339(now - TimeDelta::hours(3)),
        rfc3339(now - TimeDelta::hours(2))
    );
    let empty = mcp
        .call("search", json!({"mode": "execute", "query": empty_query}))
        .await;
    assert!(!empty.is_error, "{case}: {}", empty.text);
    assert_eq!(
        empty.structured.as_ref().unwrap()["result"]["rows_produced"],
        0,
        "{case}: {}",
        empty.text
    );
    assert!(
        hints(&empty.text).iter().any(|h| h
            .starts_with("nearest bounds over events matching the same search stage from ")
            && h.contains("the first event after them is at ")),
        "{case}: {}",
        empty.text
    );

    // Wide rows under a small budget: the renderer drops rows, and the
    // page continuation starts after the rows the text printed.
    let tight = mcp
        .call(
            "search",
            json!({"mode": "execute", "query": "service=api last=1h", "rows": 200, "max_bytes": 4096}),
        )
        .await;
    assert!(!tight.is_error, "{case}: {}", tight.text);
    let tight_view = &tight.structured.as_ref().unwrap()["result"];
    assert!(
        tight_view["presentation"]["rows_dropped_to_fit"]
            .as_u64()
            .unwrap()
            > 0,
        "{case}: {}",
        tight.text
    );
    let printed = row_lines(&tight.text).len() as u64;
    let continuation = next_action(tight.structured.as_ref().unwrap(), "page")
        .unwrap_or_else(|| panic!("{case}: no page continuation: {}", tight.text));
    assert_eq!(
        continuation["arguments"]["offset"].as_u64().unwrap(),
        tight_view["offset"].as_u64().unwrap() + printed,
        "{case}: {}",
        tight.text
    );

    // read never runs the query again.
    let before = executions(server).await;
    let page = mcp
        .call(
            "read",
            json!({"action": "page", "result_id": result_id, "offset": 50, "rows": 10}),
        )
        .await;
    assert!(!page.is_error, "{case}: {}", page.text);
    let page_view = &page.structured.as_ref().unwrap()["result"];
    assert_eq!(page_view["offset"], 50, "{case}: {}", page.text);
    assert_eq!(
        page_view["rows"].as_array().unwrap().len(),
        10,
        "{case}: {}",
        page.text
    );
    assert_eq!(row_lines(&page.text).len(), 10, "{case}: {}", page.text);

    let abbreviated = view["abbreviated"][0].clone();
    let cell = mcp
        .call(
            "read",
            json!({
                "action": "cell",
                "result_id": result_id,
                "row": abbreviated["row"],
                "column": abbreviated["column"],
            }),
        )
        .await;
    assert!(!cell.is_error, "{case}: {}", cell.text);
    let cell_view = &cell.structured.as_ref().unwrap()["cell"];
    assert_eq!(
        cell_view["total_bytes"], abbreviated["total_bytes"],
        "{case}: {}",
        cell.text
    );
    assert_eq!(cell_view["offset"], 0, "{case}: {}", cell.text);
    assert_eq!(
        cell_view["length"], abbreviated["total_bytes"],
        "{case}: the whole cell fits the default budget: {}",
        cell.text
    );

    let columns = mcp
        .call(
            "fields",
            json!({"source": "result", "result_id": result_id}),
        )
        .await;
    assert!(!columns.is_error, "{case}: {}", columns.text);
    assert_eq!(
        columns.structured.as_ref().unwrap()["fields"]
            .as_array()
            .unwrap()
            .len(),
        view["columns"].as_array().unwrap().len()
            + usize::try_from(view["presentation"]["columns_omitted"].as_u64().unwrap()).unwrap(),
        "{case}: {}",
        columns.text
    );
    assert_eq!(
        executions(server).await,
        before,
        "{case}: read and fields ran nothing and captured nothing"
    );

    // A DSL reference section.
    let section = mcp
        .request("resources/read", json!({"uri": "trawl://dsl/search-stage"}))
        .await;
    let contents = &section["result"]["contents"][0];
    assert_eq!(contents["mimeType"], "text/markdown", "{case}: {section}");
    assert!(
        contents["text"]
            .as_str()
            .unwrap()
            .starts_with("## Search stage"),
        "{case}: {section}"
    );

    // investigation: the agent key holds no investigation permission, so
    // trawld's 403 is a tool error naming the permission the action needs.
    let investigations = mcp.call("investigation", json!({"action": "list"})).await;
    assert!(investigations.is_error, "{case}: {}", investigations.raw);
    assert!(
        investigations.text.contains(
            "investigation action=list needs the query and investigation_read permissions"
        ),
        "{case}: {}",
        investigations.text
    );

    // A well-formed id that names no result is the server's 404, as a
    // tool error.
    let gone = mcp
        .call(
            "read",
            json!({"action": "page", "result_id": "0123456789abcdef0123456789abcdef"}),
        )
        .await;
    assert!(gone.is_error, "{case}: {}", gone.raw);
    assert!(gone.text.contains("HTTP 404"), "{case}: {}", gone.text);

    let finished = mcp.finish().await;
    finished.assert_clean(&case);
    // rmcp logs a refused request at WARN, to stderr: the 2026-07-28 ping
    // is the only one, and nothing else is logged.
    let logged: Vec<&str> = finished.stderr.lines().collect();
    match era {
        Era::Initialize => assert!(logged.is_empty(), "{case}: {}", finished.stderr),
        Era::Discover => assert!(
            logged.len() == 1
                && logged[0].contains(&format!("WARN rmcp::service: response error id={ping_id} "))
                && logged[0].contains("ErrorCode(-32601)"),
            "{case}: {}",
            finished.stderr
        ),
    }
    tools
}

/// Both eras list the same tools, in the same order, with the same
/// schemas, and call every tool against a seeded trawld.
#[tokio::test(flavor = "multi_thread")]
async fn every_tool_answers_in_both_protocol_eras() {
    let mut server = Server::start_with(Options {
        link_origin: Some(LINK_ORIGIN.to_owned()),
        ..options()
    })
    .await;
    let now = seed(&mut server).await;
    let home = home(&server);
    let mut listed = Vec::new();
    for era in Era::ALL {
        listed.push(every_tool(&server, home.path(), era, now).await);
    }
    assert_eq!(
        listed[0], listed[1],
        "tools/list differs between 2025-11-25 and 2026-07-28"
    );
    server.stop().await;
}

/// The id of the query `/api/v1/queries` lists active whose text holds
/// `marker`, if any.
async fn active_capture(server: &Server, marker: &str) -> Option<u64> {
    let queries = server
        .client(&server.admin_token)
        .queries()
        .await
        .expect("list the queries");
    queries
        .active
        .iter()
        .find(|q| q.snapshot.query.contains(marker))
        .map(|q| q.snapshot.id)
}

/// Poll `probe` until it answers `Some`, failing after [`DEADLINE`].
async fn until<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Without `link_origin` the search says why it has no URL; a cancelled
/// search drops its trawld request, which the server records as
/// abandoned, and the child sends no response for it. In both eras.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_search_drops_its_server_query_in_both_eras() {
    let mut server = Server::start_with(options()).await;
    seed(&mut server).await;
    let home = home(&server);
    for era in Era::ALL {
        let case = format!("era {}", era.version());
        let mut mcp = Mcp::spawn(home.path(), era);
        mcp.open().await;
        mcp.list_tools().await;

        let plain = mcp
            .call(
                "search",
                json!({"mode": "execute", "query": "service=api last=1h"}),
            )
            .await;
        assert!(!plain.is_error, "{case}: {}", plain.text);
        let link = &plain.structured.as_ref().unwrap()["result"]["link"];
        assert!(link.get("url").is_none(), "{case}: {link}");
        let reason = link["reason"]
            .as_str()
            .unwrap_or_else(|| panic!("{case}: no reason: {link}"));
        assert!(reason.contains("link_origin"), "{case}: {reason}");
        assert!(
            plain.text.contains(&format!("no search URL: {reason}")),
            "{case}: {}",
            plain.text
        );

        // Hold the corpus: a capture over a window waits for the
        // publication read guard, tracked and in flight.
        let gate = server.state.query.pool.publication();
        let held = gate.write().await;
        let marker = format!("cancelme{}", era.version().replace('-', ""));
        let query = format!("service=api {marker} last=1h");
        let id = mcp
            .start(
                "tools/call",
                json!({"name": "search", "arguments": {"mode": "execute", "query": query}}),
            )
            .await;
        let query_id = until("the capture in flight", || active_capture(&server, &marker)).await;

        mcp.notify(
            "notifications/cancelled",
            json!({"requestId": id, "reason": "the test cancels it"}),
        )
        .await;
        until("the capture recorded as abandoned", || async {
            let queries = server
                .client(&server.admin_token)
                .queries()
                .await
                .expect("list the queries");
            let gone = !queries.active.iter().any(|q| q.snapshot.id == query_id);
            let recent = queries.recent.iter().find(|q| q.snapshot.id == query_id)?;
            gone.then(|| {
                assert_eq!(
                    recent.snapshot.error.as_deref(),
                    Some(trawl_server::tracker::ABANDONED),
                    "{case}: {recent:?}"
                );
                assert_eq!(recent.snapshot.rows, None, "{case}: {recent:?}");
            })
        })
        .await;
        drop(held);

        // The session goes on; the cancelled request is never answered.
        mcp.list_tools().await;
        let finished = mcp.finish().await;
        finished.assert_clean(&case);
        // rmcp logs the dropped call's error at WARN, to stderr; it is the
        // only line there.
        let logged: Vec<&str> = finished.stderr.lines().collect();
        assert!(
            logged.len() == 1
                && logged[0].contains(&format!("WARN rmcp::service: response error id={id} "))
                && logged[0].contains("the request was cancelled"),
            "{case}: {}",
            finished.stderr
        );
        assert!(
            !finished.answered(id),
            "{case}: a response arrived for the cancelled request {id}: {:?}",
            finished.lines
        );
    }
    server.stop().await;
}

/// A capture past its deadline is trawld's 410 `result_gone` to `read`
/// and to `fields`, as a tool error, and neither runs a query or captures
/// a replacement. In both eras.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_capture_reads_as_result_gone_in_both_eras() {
    let clock = ResultsClock::fixed(Utc::now());
    let mut server = Server::start_with(Options {
        results_clock: clock.clone(),
        ..options()
    })
    .await;
    seed(&mut server).await;
    let home = home(&server);
    for era in Era::ALL {
        let case = format!("era {}", era.version());
        // The capture's bounds resolve against the results clock: now,
        // after the corpus was seeded.
        clock.set(Utc::now());
        let mut mcp = Mcp::spawn(home.path(), era);
        mcp.open().await;
        mcp.list_tools().await;

        let search = mcp
            .call(
                "search",
                json!({"mode": "execute", "query": "service=api last=1h"}),
            )
            .await;
        assert!(!search.is_error, "{case}: {}", search.text);
        let view = &search.structured.as_ref().unwrap()["result"];
        assert!(
            view["rows_captured"].as_u64().unwrap() > 0,
            "{case}: {view}"
        );
        let result_id = view["result_id"].as_str().unwrap().to_owned();
        let deadline = instant(&view["expires_at"]);
        let before = executions(&server).await;
        assert!(before.2.contains(&result_id), "{case}: {before:?}");

        clock.set(deadline + TimeDelta::seconds(1));

        let page = mcp
            .call("read", json!({"action": "page", "result_id": result_id}))
            .await;
        assert!(page.is_error, "{case}: {}", page.raw);
        assert!(page.structured.is_none(), "{case}: {}", page.raw);
        assert!(
            page.text
                .starts_with("read was refused: result_gone (HTTP 410)."),
            "{case}: {}",
            page.text
        );

        let fields = mcp
            .call(
                "fields",
                json!({"source": "result", "result_id": result_id}),
            )
            .await;
        assert!(fields.is_error, "{case}: {}", fields.raw);
        assert!(fields.structured.is_none(), "{case}: {}", fields.raw);
        assert!(
            fields.text.contains("result_gone (HTTP 410)"),
            "{case}: {}",
            fields.text
        );

        let after = executions(&server).await;
        assert_eq!(
            (&after.0, &after.1),
            (&before.0, &before.1),
            "{case}: the reads of an expired capture ran no query"
        );
        assert!(
            after.2.iter().all(|id| before.2.contains(id)),
            "{case}: no capture replaced the expired one: {before:?} then {after:?}"
        );

        let finished = mcp.finish().await;
        finished.assert_clean(&case);
        assert!(finished.stderr.is_empty(), "{case}: {}", finished.stderr);
    }
    server.stop().await;
}
