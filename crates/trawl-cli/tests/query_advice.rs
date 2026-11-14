// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query advice on the CLI (ADR-0068), observed on the real `trawl`
//! binary: `trawl validate` and `trawl query` print advice on stderr and
//! never change the verdict, the exit code, stdout or an exported file.
//!
//! Every run starts from an empty environment with a temp `HOME` and a
//! config of its own, so no saved key or inherited `TRAWL_*` variable
//! picks the target. The connected case boots a disposable trawld from
//! `tests/common`, so this binary is in the `postgres` nextest group.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::Server;
use common::tui::{DEADLINE, Home, describe, pinned};
use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

/// A valid query with one negated field-shaped term.
const NEGATED: &str = r#""10.244.0.83" -service=coredns"#;
/// The same rows as [`NEGATED`], written so no advice applies.
const NEGATED_SILENCED: &str = r#""10.244.0.83" NOT "service=coredns""#;
/// A valid query with a search-stage `in (…)`.
const SEARCH_IN: &str = "service=trawld event_type in (a, b)";
/// A parse error: an unknown escape in a quoted string.
const UNKNOWN_ESCAPE: &str = r#"* | where m matches "^\s+at ""#;
/// A query that parses and fails the shared semantic checks, with a
/// shape advice would otherwise flag.
const SEMANTIC_ERROR: &str = "service=trawld event_type in (a, b) | stats nosuchfn(x)";

const NEGATED_MESSAGE: &str = "advice: -service=… is a text search, not a field filter";
const NEGATED_HINT: &str = "hint: to filter the field, write service!=…";
const IN_MESSAGE: &str = "advice: in the search stage, in (…) is not a list test";
const IN_HINT: &str = "hint: to match a list, write event_type=a,b";

/// A trawl home with an empty config: local validation and embedded runs.
fn local_home() -> (Home, PathBuf) {
    let home = Home::new();
    let config = home.path().join("empty.toml");
    std::fs::write(&config, "").expect("write the empty config");
    (home, config)
}

/// `trawl --config config <args>` to completion.
async fn trawl(home: &Home, config: &Path, args: &[&str]) -> Output {
    let run = home
        .command("UTC")
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(Stdio::null())
        .output();
    tokio::time::timeout(DEADLINE, run)
        .await
        .unwrap_or_else(|_| panic!("trawl {args:?} did not finish in {DEADLINE:?}"))
        .expect("spawn trawl")
}

/// `trawl` with stdout and stderr written to one file, in the order the
/// process wrote them; returns the status and that text.
async fn trawl_interleaved(home: &Home, config: &Path, args: &[&str]) -> (bool, String) {
    let path = home.path().join("interleaved.txt");
    let file = std::fs::File::create(&path).expect("create the capture");
    let run = home
        .command("UTC")
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(Stdio::null())
        .stdout(file.try_clone().expect("clone the capture"))
        .stderr(file)
        .status();
    let status = tokio::time::timeout(DEADLINE, run)
        .await
        .unwrap_or_else(|_| panic!("trawl {args:?} did not finish in {DEADLINE:?}"))
        .expect("spawn trawl");
    (
        status.success(),
        std::fs::read_to_string(&path).expect("read the capture"),
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A valid verdict: exit 0, exactly `valid\n` on stdout, and every
/// fragment of `expected` on stderr.
fn assert_valid_with_advice(output: &Output, query: &str, expected: &[&str]) {
    assert!(output.status.success(), "{query}: {}", describe(output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "valid\n",
        "{query}: {}",
        describe(output)
    );
    let err = stderr(output);
    for fragment in expected {
        assert!(
            err.contains(fragment),
            "{query}: stderr lacks {fragment:?}:\n{err}"
        );
    }
}

/// `query`'s advice lines in the caret layout: the source line, carets
/// under the span, then the advice and the hint.
fn assert_caret_excerpt(err: &str, source: &str, pad: usize, carets: usize, advice: &str) {
    let expected = format!(
        "\n  {source}\n  {}{}\n  {advice}",
        " ".repeat(pad),
        "~".repeat(carets)
    );
    assert!(
        err.contains(&expected),
        "stderr lacks the caret excerpt {expected:?}:\n{err}"
    );
}

/// Ten negated field shapes: eight findings shown, two counted.
fn many_findings() -> String {
    (0..10)
        .map(|i| format!("-f{i}=v"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Parquet file of five events, four of them carrying the address. In
/// two of those the `service` field and the message text disagree: a
/// coredns event whose text never says `service=coredns`, and a kubelet
/// event whose text does. A text search for `service=coredns` and a
/// filter on the `service` field keep different rows.
fn fixture(dir: &Path) -> String {
    let executor = trawl_engine::executor::Executor::new().expect("an executor");
    let string = |s: &str| Value::String(s.to_owned());
    let source = QueryResult {
        columns: vec![
            Column::new("message", ColumnType::String),
            Column::new("_raw", ColumnType::String),
            Column::new("service", ColumnType::String),
            Column::new("event_type", ColumnType::String),
        ],
        rows: vec![
            vec![
                string("lookup 10.244.0.83 ok"),
                string("lookup 10.244.0.83 ok"),
                string("trawld"),
                string("a"),
            ],
            vec![
                string("lookup 10.244.0.83 via service=coredns"),
                string("lookup 10.244.0.83 via service=coredns"),
                string("coredns"),
                string("b"),
            ],
            vec![
                string("refused 10.244.0.83, \"quoted\", comma"),
                string("refused 10.244.0.83, \"quoted\", comma"),
                string("kubelet"),
                string("c"),
            ],
            vec![
                string("answered 10.244.0.83 upstream"),
                string("answered 10.244.0.83 upstream"),
                string("coredns"),
                string("d"),
            ],
            vec![
                string("probe 10.244.0.83 forwarded to service=coredns"),
                string("probe 10.244.0.83 forwarded to service=coredns"),
                string("kubelet"),
                string("e"),
            ],
        ],
    };
    let path = dir.join("events.parquet");
    executor
        .write_query_result_to_parquet(&source, &path)
        .expect("write the fixture");
    path.to_str().expect("utf-8 fixture path").to_owned()
}

#[tokio::test]
async fn cli_validate_advice_local() {
    let (home, config) = local_home();

    let output = trawl(&home, &config, &["validate", NEGATED]).await;
    assert_valid_with_advice(&output, NEGATED, &[NEGATED_MESSAGE, NEGATED_HINT]);
    let err = stderr(&output);
    // Bytes 14..30 are `-service=coredns`.
    assert_caret_excerpt(&err, NEGATED, 14, 16, NEGATED_MESSAGE);
    assert_eq!(err.matches("advice:").count(), 1, "{err}");
    assert_eq!(err.matches("hint:").count(), 1, "{err}");

    let output = trawl(&home, &config, &["validate", SEARCH_IN]).await;
    assert_valid_with_advice(&output, SEARCH_IN, &[IN_MESSAGE, IN_HINT]);
    // Bytes 15..30 are `event_type in (`… through the group's last byte.
    assert_caret_excerpt(&stderr(&output), SEARCH_IN, 15, 15, IN_MESSAGE);

    // A query with nothing to advise prints nothing on stderr.
    let output = trawl(&home, &config, &["validate", "service=trawld"]).await;
    assert_valid_with_advice(&output, "service=trawld", &[]);
    assert!(output.stderr.is_empty(), "{}", describe(&output));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_validate_advice_connected() {
    let server = Server::start(false).await;
    let home = Home::new();
    let config = home.config(&pinned(&server, &server.agent_token), "UTC");

    for (query, expected) in [
        (NEGATED, [NEGATED_MESSAGE, NEGATED_HINT]),
        (SEARCH_IN, [IN_MESSAGE, IN_HINT]),
    ] {
        let output = trawl(&home, &config, &["validate", query]).await;
        assert_valid_with_advice(&output, query, &expected);
    }
    let output = trawl(&home, &config, &["validate", NEGATED]).await;
    assert_caret_excerpt(&stderr(&output), NEGATED, 14, 16, NEGATED_MESSAGE);

    // The server's refusal stands alone: no advice beside an error.
    let output = trawl(&home, &config, &["validate", UNKNOWN_ESCAPE]).await;
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        !stderr(&output).contains("advice:"),
        "{}",
        describe(&output)
    );

    // A connected run advises before the server answers, and the reply
    // is printed as before.
    let output = trawl(&home, &config, &["query", NEGATED, "--format", "json"]).await;
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        stderr(&output).contains(NEGATED_MESSAGE),
        "{}",
        describe(&output)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("advice"),
        "{}",
        describe(&output)
    );

    server.stop().await;
}

#[tokio::test]
async fn cli_unknown_escape_has_errors_without_advice() {
    let (home, config) = local_home();
    let output = trawl(&home, &config, &["validate", UNKNOWN_ESCAPE]).await;
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
    let err = stderr(&output);
    assert!(err.contains("unknown escape \\s"), "{err}");
    assert!(
        err.contains("hint: write \\\\s to keep the backslash"),
        "{err}"
    );
    // Bytes 22..24 are `\s`.
    assert!(
        err.contains(&format!("\n  {UNKNOWN_ESCAPE}\n  {}~~\n", " ".repeat(22))),
        "{err}"
    );
    assert!(!err.contains("advice:"), "{err}");
}

#[tokio::test]
async fn cli_semantic_error_has_no_advice() {
    let (home, config) = local_home();
    let output = trawl(&home, &config, &["validate", SEMANTIC_ERROR]).await;
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
    assert!(
        !stderr(&output).contains("advice:"),
        "{}",
        describe(&output)
    );

    // `trawl query` leaves the error to the run, and advises nothing.
    let data = fixture(home.path());
    let output = trawl(&home, &config, &["query", SEMANTIC_ERROR, "--data", &data]).await;
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        !stderr(&output).contains("advice:"),
        "{}",
        describe(&output)
    );
}

/// A text that parses with its patterns unbuilt but whose regex fails to
/// build, and that advice would otherwise flag.
const INVALID_REGEX: &str = r"-service=x f=/\p{Nope}/";

/// Daemon-mode `trawl query` refuses advice for a text local validation
/// rejects, as `trawl validate` does, on the table and Parquet paths. The
/// endpoint is unreachable: the advice is printed before any request.
#[tokio::test]
async fn cli_query_advice_skips_locally_invalid_regex() {
    let (home, config) = local_home();
    let token = home.path().join("token");
    std::fs::write(&token, "flt_test_not_real").expect("write the token");
    let token = token.to_str().unwrap();
    let out = home.path().join("never.parquet");
    let daemon = |query: &'static str, parquet: bool| {
        let mut args = vec![
            "query",
            "--url",
            "http://127.0.0.1:1",
            "--token-file",
            token,
        ];
        if parquet {
            args.extend(["--format", "parquet", "-o", out.to_str().unwrap()]);
        }
        args.extend(["--", query]);
        args
    };

    let output = trawl(&home, &config, &["validate", "--", INVALID_REGEX]).await;
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(
        stderr(&output).contains("invalid regex"),
        "{}",
        describe(&output)
    );

    for parquet in [false, true] {
        // The control: the same daemon path advises a valid text.
        let output = trawl(&home, &config, &daemon(NEGATED, parquet)).await;
        assert!(
            stderr(&output).contains(NEGATED_MESSAGE),
            "parquet={parquet}: {}",
            describe(&output)
        );
        let output = trawl(&home, &config, &daemon(INVALID_REGEX, parquet)).await;
        assert!(
            !stderr(&output).contains("advice:"),
            "parquet={parquet}: {}",
            describe(&output)
        );
    }
}

#[tokio::test]
async fn cli_query_advice_precedes_execution() {
    let (home, config) = local_home();
    let data = fixture(home.path());

    // In write order, the advice comes before the first result row.
    let (ok, text) = trawl_interleaved(
        &home,
        &config,
        &["query", NEGATED, "--data", &data, "--format", "json"],
    )
    .await;
    assert!(ok, "{text}");
    let advice = text
        .find(NEGATED_MESSAGE)
        .unwrap_or_else(|| panic!("{text}"));
    let row = text
        .find("lookup 10.244.0.83 ok")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(advice < row, "advice follows the results:\n{text}");
    assert_eq!(text.matches("advice:").count(), 1, "printed once:\n{text}");

    // A run that fails in the engine was advised before it started.
    let broken = home.path().join("broken.parquet");
    std::fs::write(&broken, "not parquet").expect("write the broken file");
    let (ok, text) = trawl_interleaved(
        &home,
        &config,
        &["query", NEGATED, "--data", broken.to_str().unwrap()],
    )
    .await;
    assert!(!ok, "{text}");
    let advice = text
        .find(NEGATED_MESSAGE)
        .unwrap_or_else(|| panic!("{text}"));
    let failure = text.find("trawl:").unwrap_or_else(|| panic!("{text}"));
    assert!(advice < failure, "advice follows the failure:\n{text}");

    // The Parquet export path advises too, once.
    let out = home.path().join("advised.parquet");
    let output = trawl(
        &home,
        &config,
        &[
            "query",
            NEGATED,
            "--data",
            &data,
            "--format",
            "parquet",
            "-o",
            out.to_str().unwrap(),
        ],
    )
    .await;
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        stderr(&output).matches(NEGATED_MESSAGE).count(),
        1,
        "{}",
        describe(&output)
    );
}

/// The rows [`NEGATED`] keeps from [`fixture`]: every event with the
/// address whose text lacks `service=coredns`, whatever its `service`.
const NEGATED_KEEPS: [&str; 3] = [
    "lookup 10.244.0.83 ok",
    "refused 10.244.0.83",
    "answered 10.244.0.83 upstream",
];
/// The rows [`NEGATED`] drops from [`fixture`]: the text says
/// `service=coredns`, whether or not the `service` field does.
const NEGATED_DROPS: [&str; 2] = [
    "lookup 10.244.0.83 via service=coredns",
    "probe 10.244.0.83 forwarded to service=coredns",
];

/// `text` holds every row [`NEGATED`] keeps and none it drops.
fn assert_negated_rows(text: &str, what: &str) {
    for kept in NEGATED_KEEPS {
        assert!(text.contains(kept), "{what} lacks {kept:?}:\n{text}");
    }
    for dropped in NEGATED_DROPS {
        assert!(!text.contains(dropped), "{what} holds {dropped:?}:\n{text}");
    }
}

#[tokio::test]
async fn cli_query_advice_preserves_stdout_and_exports() {
    let (home, config) = local_home();
    let data = fixture(home.path());

    for format in ["json", "csv", "table"] {
        let advised = trawl(
            &home,
            &config,
            &["query", NEGATED, "--data", &data, "--format", format],
        )
        .await;
        let silenced = trawl(
            &home,
            &config,
            &[
                "query",
                NEGATED_SILENCED,
                "--data",
                &data,
                "--format",
                format,
            ],
        )
        .await;
        assert!(advised.status.success(), "{}", describe(&advised));
        assert!(silenced.status.success(), "{}", describe(&silenced));
        assert!(
            stderr(&advised).contains(NEGATED_MESSAGE),
            "{}",
            describe(&advised)
        );
        assert!(
            !stderr(&silenced).contains("advice:"),
            "{}",
            describe(&silenced)
        );
        assert_negated_rows(
            &String::from_utf8_lossy(&advised.stdout),
            &format!("{format} stdout"),
        );
        assert_eq!(advised.stdout, silenced.stdout, "{format} stdout differs");
    }

    for format in ["json", "csv", "parquet"] {
        let advised_path = home.path().join(format!("advised.{format}"));
        let silenced_path = home.path().join(format!("silenced.{format}"));
        for (query, path) in [(NEGATED, &advised_path), (NEGATED_SILENCED, &silenced_path)] {
            let output = trawl(
                &home,
                &config,
                &[
                    "query",
                    query,
                    "--data",
                    &data,
                    "--format",
                    format,
                    "-o",
                    path.to_str().unwrap(),
                ],
            )
            .await;
            assert!(output.status.success(), "{format}: {}", describe(&output));
            assert!(output.stdout.is_empty(), "{format}: {}", describe(&output));
        }
        let advised = std::fs::read(&advised_path).expect("read the advised export");
        let silenced = std::fs::read(&silenced_path).expect("read the silenced export");
        assert!(!advised.is_empty(), "{format} export is empty");
        assert_eq!(advised, silenced, "{format} export differs");
        // The export holds the rows the text search keeps: a Parquet
        // file is read back through the CLI as JSON.
        let rows = if format == "parquet" {
            let output = trawl(
                &home,
                &config,
                &[
                    "query",
                    "*",
                    "--data",
                    advised_path.to_str().unwrap(),
                    "--format",
                    "json",
                ],
            )
            .await;
            assert!(output.status.success(), "{}", describe(&output));
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            String::from_utf8_lossy(&advised).into_owned()
        };
        assert_negated_rows(&rows, &format!("{format} export"));
    }
}

#[tokio::test]
async fn cli_query_advice_reports_omitted_findings() {
    let (home, config) = local_home();
    let query = many_findings();

    // A query that starts with `-` follows `--`, or clap reads a flag.
    let output = trawl(&home, &config, &["validate", "--", &query]).await;
    assert_valid_with_advice(&output, &query, &["and 2 more advice notes not shown"]);
    assert_eq!(
        stderr(&output).matches("advice: ").count(),
        trawl_core::advice::MAX_ADVICE_FINDINGS,
        "{}",
        describe(&output)
    );

    let data = fixture(home.path());
    let output = trawl(
        &home,
        &config,
        &["query", "--data", &data, "--format", "json", "--", &query],
    )
    .await;
    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        stderr(&output).contains("and 2 more advice notes not shown"),
        "{}",
        describe(&output)
    );
}
