// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The sender proof recipes in the Vector guide and the syslog section of
//! the ingestion guide are runnable blocks, each named by a
//! `<!-- proof:NAME -->` comment on the line before its fence. This test
//! holds the inventory of those names exactly, and parses every proof query
//! with the DSL parser after putting sample values in for its shell
//! variables. A query that stops parsing, or loses the identity, arrival, or
//! time-window predicate that makes it a proof, fails here.

use trawl_core::ast::{
    Expr, FieldFilter, FilterOp, FilterValue, PipeStage, Query, SearchToken, TimeUnit,
};

const VECTOR_GUIDE: &str =
    include_str!("../../../docs/src/content/docs/getting-started/vector-integration.md");
const INGESTION_GUIDE: &str = include_str!("../../../docs/src/content/docs/operate/ingestion.md");

const VECTOR_MARKERS: &[&str] = &[
    "key-write",
    "vector-start",
    "vars",
    "journald-send",
    "journald-check",
    "nginx-send",
    "nginx-confirm",
    "nginx-check",
    "docker-send",
    "docker-check",
    "docker-cleanup",
    "ufw-vars",
    "ufw-rule",
    "ufw-send",
    "ufw-check",
    "history-finder",
];
const INGESTION_MARKERS: &[&str] = &[
    "syslog-config",
    "syslog-vars",
    "syslog-firewall-allow",
    "syslog-check",
];

// Sample values for the shell variables the proof queries read.
const SENDER_ENV: &str = "prod";
const HOST: &str = "collector1.example.com";
const MARKER: &str = "trawl-check-3f6c1a2e-9b7d-4e1f-8a2b-5c4d3e2f1a0b";
const T0: &str = "2026-09-28T10:00:00Z";
const VECTOR_START: &str = "2026-09-28T09:00:00Z";
const PEER: &str = "192.0.2.20";
const PORT: &str = "45123";
const DEVICE: &str = "192.0.2.1";
const BOOT_DAYS: &str = "3";

const MARKER_PREFIX: &str = "<!-- proof:";

/// Every proof marker on a page, in order, with the index of its line.
fn markers(page: &str) -> Vec<(&str, usize)> {
    page.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            line.strip_prefix(MARKER_PREFIX).map(|rest| {
                let name = rest
                    .strip_suffix(" -->")
                    .unwrap_or_else(|| panic!("malformed proof marker {line:?}"));
                (name, index)
            })
        })
        .collect()
}

fn assert_inventory(page: &str, page_name: &str, expected: &[&str]) {
    let found = markers(page);
    let lines: Vec<&str> = page.lines().collect();
    for (name, index) in &found {
        let next = lines.get(index + 1).map_or("", |line| line.trim_start());
        assert!(
            next.starts_with("```"),
            "{page_name}: proof marker {name:?} is not on the line immediately before a fence"
        );
    }
    let mut names: Vec<&str> = found.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(
        names, want,
        "{page_name}: the proof markers differ from the inventory (each name exactly once)"
    );
    assert_eq!(
        page.matches("proof:").count(),
        expected.len(),
        "{page_name}: a proof marker is spelled in a form this test does not read"
    );
}

/// The body of the fenced block that follows the named marker, with the
/// list indentation removed.
fn block<'a>(page: &'a str, name: &str) -> Vec<&'a str> {
    let (_, index) = markers(page)
        .into_iter()
        .find(|(found, _)| *found == name)
        .unwrap_or_else(|| panic!("no proof marker {name:?}"));
    page.lines()
        .skip(index + 2)
        .map(str::trim)
        .take_while(|line| !line.starts_with("```"))
        .collect()
}

/// The query argument of the one `trawl -p prod query "…"` line in a
/// block, with the shell escapes undone and the sample values put in.
fn proof_query(page: &str, name: &str) -> String {
    const COMMAND: &str = "trawl -p prod query \"";
    let lines: Vec<&str> = block(page, name)
        .into_iter()
        .filter(|line| line.starts_with(COMMAND))
        .collect();
    assert_eq!(lines.len(), 1, "{name}: expected one `{COMMAND}…\"` line");
    let argument = lines[0][COMMAND.len()..]
        .strip_suffix('"')
        .unwrap_or_else(|| panic!("{name}: the query argument does not end the line"));
    let mut query = argument.replace("\\\"", "\"");
    let samples = [
        ("VECTOR_START", VECTOR_START),
        ("SENDER_ENV", SENDER_ENV),
        ("BOOT_DAYS", BOOT_DAYS),
        ("MARKER", MARKER),
        ("DEVICE", DEVICE),
        ("HOST", HOST),
        ("PEER", PEER),
        ("PORT", PORT),
        ("T0", T0),
    ];
    for (variable, value) in samples {
        query = query
            .replace(&format!("${{{variable}}}"), value)
            .replace(&format!("${variable}"), value);
    }
    assert!(
        !query.contains('$') && !query.contains('\\'),
        "{name}: the query reads a shell variable or escape this test has no sample for: {query}"
    );
    query
}

fn parse(page: &str, name: &str) -> Query {
    let query = proof_query(page, name);
    trawl_core::parser::parse(&query)
        .unwrap_or_else(|errors| panic!("{name}: {query:?} does not parse: {errors:?}"))
}

/// The search stage's top-level tokens. A proof query is one AND group.
fn tokens(query: &Query, name: &str) -> Vec<SearchToken> {
    assert_eq!(query.search.groups.len(), 1, "{name}: one AND group");
    query.search.groups[0]
        .iter()
        .map(|token| token.node.clone())
        .collect()
}

fn filter(field: &str, op: FilterOp, value: &str) -> SearchToken {
    SearchToken::FieldFilter(FieldFilter {
        field: field.to_owned(),
        op,
        value: FilterValue::Literal(value.to_owned()),
    })
}

fn phrase(text: &str) -> SearchToken {
    SearchToken::QuotedSearch(trawl_core::ast::QuotedSearch {
        phrase: text.to_owned(),
    })
}

/// Assert the predicates that make a query a proof: the named identity
/// filters, any phrase, `_ingested>=` the given start, and a `last=` window
/// of the given number of days.
fn assert_proof(query: &Query, name: &str, expected: &[SearchToken], ingested: &str, days: u64) {
    let found = tokens(query, name);
    let mut want = expected.to_vec();
    want.push(filter("_ingested", FilterOp::Gte, ingested));
    for token in &want {
        assert!(
            found.contains(token),
            "{name}: missing predicate {token:?} in {found:?}"
        );
    }
    let window = query
        .search
        .time_filter
        .as_ref()
        .unwrap_or_else(|| panic!("{name}: no `last=` window, so the scan reads every date"))
        .node
        .duration;
    assert_eq!(
        (window.quantity, window.unit),
        (days, TimeUnit::Days),
        "{name}: the `last=` window"
    );
    assert!(
        query.search.earliest.is_none() && query.search.latest.is_none(),
        "{name}: an absolute bound does not prune date directories"
    );
}

/// Assert `| head 20 | table …` naming the given columns in order.
fn assert_table(query: &Query, name: &str, columns: &[&str]) {
    let stages: Vec<&PipeStage> = query.pipeline.iter().map(|stage| &stage.node).collect();
    match stages.as_slice() {
        [PipeStage::Limit(limit), PipeStage::Table(table)] => {
            assert_eq!(
                (limit.count, limit.keyword),
                (20, "head"),
                "{name}: head 20"
            );
            assert_eq!(table.fields, columns, "{name}: table columns");
        }
        other => panic!("{name}: expected `| head 20 | table …`, got {other:?}"),
    }
}

fn columns(extra: &[&'static str]) -> Vec<&'static str> {
    let mut all = vec!["_time", "_ingested", "_producer", "env", "service", "host"];
    all.extend_from_slice(extra);
    all
}

#[test]
fn every_proof_block_is_marked_once() {
    assert_inventory(VECTOR_GUIDE, "vector-integration.md", VECTOR_MARKERS);
    assert_inventory(INGESTION_GUIDE, "ingestion.md", INGESTION_MARKERS);
}

#[test]
fn neither_page_checks_arrival_with_a_bare_short_window() {
    for (page_name, page) in [
        ("vector-integration.md", VECTOR_GUIDE),
        ("ingestion.md", INGESTION_GUIDE),
    ] {
        assert!(!page.contains("last=15m"), "{page_name} uses last=15m");
    }
}

#[test]
fn the_variables_capture_a_uuid_marker_and_a_utc_start() {
    let utc_now = "T0=\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"";
    let vars = block(VECTOR_GUIDE, "vars");
    assert!(vars.contains(&"MARKER=\"trawl-check-$(cat /proc/sys/kernel/random/uuid)\""));
    assert!(vars.contains(&utc_now));
    assert!(vars.contains(&"HOST=\"$(hostname)\""));
    assert!(vars.iter().any(|line| line.starts_with("SENDER_ENV=")));
    assert!(
        block(VECTOR_GUIDE, "vector-start")
            .contains(&"VECTOR_START=\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"")
    );
    assert!(block(INGESTION_GUIDE, "syslog-vars").contains(&utc_now));
    let ufw = block(VECTOR_GUIDE, "ufw-vars");
    for variable in ["PEER=", "COLLECTOR=", "PORT="] {
        assert!(
            ufw.iter().any(|line| line.starts_with(variable)),
            "{variable}"
        );
    }
}

#[test]
fn the_key_file_is_restricted_before_the_key_is_written() {
    let lines = block(VECTOR_GUIDE, "key-write");
    let restrict = lines
        .iter()
        .position(|line| {
            line.contains("install -m 0600")
                && line
                    .trim_end_matches("&&")
                    .trim_end()
                    .ends_with("/etc/default/vector")
        })
        .expect("key-write creates /etc/default/vector with mode 0600");
    let write = lines
        .iter()
        .position(|line| line.contains("TRAWL_INGEST_TOKEN="))
        .expect("key-write writes TRAWL_INGEST_TOKEN");
    assert!(restrict < write, "the mode is set after the key lands");
    assert!(
        !lines[write].contains("flt_"),
        "the key is read from a file, never typed on a command line"
    );
    let remove = lines
        .iter()
        .position(|line| line.trim() == "rm vector.token")
        .expect("key-write deletes the key file copy");
    assert!(
        lines[restrict].ends_with("&&") && lines[write].ends_with("&&") && write < remove,
        "the key file copy is deleted only after the restricted write succeeds"
    );
    assert!(
        lines[write].contains("| sudo tee -a /etc/default/vector"),
        "the key reaches the file through sudo tee's standard input"
    );
    for line in &lines[restrict..=remove] {
        assert!(
            !line.contains("||") && !line.contains(';'),
            "nothing in the key-write chain tolerates a failed step: {line}"
        );
    }
}

#[test]
fn journald_check_matches_the_unit_and_marker() {
    let query = parse(VECTOR_GUIDE, "journald-check");
    assert_proof(
        &query,
        "journald-check",
        &[
            filter("env", FilterOp::Eq, SENDER_ENV),
            filter("service", FilterOp::Eq, MARKER),
            filter("host", FilterOp::Eq, HOST),
            phrase(MARKER),
        ],
        T0,
        1,
    );
    assert_table(&query, "journald-check", &columns(&["message"]));
}

#[test]
fn nginx_check_matches_the_service_and_marker() {
    let query = parse(VECTOR_GUIDE, "nginx-check");
    assert_proof(
        &query,
        "nginx-check",
        &[
            filter("env", FilterOp::Eq, SENDER_ENV),
            filter("service", FilterOp::Eq, "nginx"),
            filter("host", FilterOp::Eq, HOST),
            phrase(MARKER),
        ],
        T0,
        1,
    );
    assert_table(
        &query,
        "nginx-check",
        &columns(&["uri", "status", "message"]),
    );
}

#[test]
fn docker_check_matches_the_container_identity_and_marker() {
    let query = parse(VECTOR_GUIDE, "docker-check");
    assert_proof(
        &query,
        "docker-check",
        &[
            filter("env", FilterOp::Eq, SENDER_ENV),
            filter("service", FilterOp::Eq, MARKER),
            filter("host", FilterOp::Eq, MARKER),
            phrase(MARKER),
        ],
        T0,
        1,
    );
    assert_table(
        &query,
        "docker-check",
        &columns(&["container_name", "image", "message"]),
    );
}

#[test]
fn ufw_check_matches_the_packet_source_and_port() {
    let query = parse(VECTOR_GUIDE, "ufw-check");
    assert_proof(
        &query,
        "ufw-check",
        &[
            filter("env", FilterOp::Eq, SENDER_ENV),
            filter("service", FilterOp::Eq, "ufw"),
            filter("host", FilterOp::Eq, HOST),
            filter("src_ip", FilterOp::Eq, PEER),
            filter("dst_port", FilterOp::Eq, PORT),
        ],
        T0,
        1,
    );
    assert_table(
        &query,
        "ufw-check",
        &columns(&["src_ip", "dst_port", "protocol", "message"]),
    );
}

#[test]
fn syslog_check_matches_the_device_address() {
    let query = parse(INGESTION_GUIDE, "syslog-check");
    assert_proof(
        &query,
        "syslog-check",
        &[
            filter("service", FilterOp::Eq, "firewall"),
            filter("_producer", FilterOp::Eq, "syslog"),
            filter("syslog_source_ip", FilterOp::Eq, DEVICE),
        ],
        T0,
        1,
    );
    assert_table(
        &query,
        "syslog-check",
        &columns(&[
            "syslog_source_ip",
            "syslog_severity",
            "syslog_timestamp",
            "syslog_facility",
            "message",
        ]),
    );
}

#[test]
fn history_finder_bounds_time_and_groups_by_service() {
    let query = parse(VECTOR_GUIDE, "history-finder");
    assert_proof(
        &query,
        "history-finder",
        &[
            filter("env", FilterOp::Eq, SENDER_ENV),
            filter("host", FilterOp::Eq, HOST),
        ],
        VECTOR_START,
        BOOT_DAYS.parse().expect("sample day count"),
    );
    let stages: Vec<&PipeStage> = query.pipeline.iter().map(|stage| &stage.node).collect();
    let [PipeStage::Stats(stats)] = stages.as_slice() else {
        panic!("history-finder: expected one `stats` stage, got {stages:?}");
    };
    assert_eq!(stats.group_by, ["service"]);
    let aggregations: Vec<(String, Vec<Expr>)> = stats
        .aggregations
        .iter()
        .map(|agg| {
            (
                agg.function.clone(),
                agg.args.iter().map(|arg| arg.node.clone()).collect(),
            )
        })
        .collect();
    let time = Expr::FieldRef("_time".to_owned());
    assert_eq!(
        aggregations,
        [
            ("count".to_owned(), vec![]),
            ("min".to_owned(), vec![time.clone()]),
            ("max".to_owned(), vec![time]),
        ]
    );
}
