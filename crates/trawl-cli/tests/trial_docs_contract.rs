// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial tutorial and the CLI reference are packaging artifacts: they
//! quote what `trawl trial up` prints and the row the documented query
//! returns. `trial` is a private module, so the literals cannot be imported
//! here. Each one is duplicated below and held against the `insta`
//! snapshot of the summary, which `render.rs` produces from the constants in
//! `sample.rs`. A change on either side fails here before it reaches a
//! reader.

const FIRST_QUERY: &str =
    include_str!("../../../docs/src/content/docs/getting-started/first-query.md");
const QUERY_TUTORIAL: &str = include_str!("../../../docs/src/content/docs/use/query-tutorial.md");
const CLI_REFERENCE: &str = include_str!("../../../docs/src/content/docs/reference/cli.md");
const SUMMARY_SNAPSHOT: &str = include_str!(
    "../src/trial/snapshots/trawl_cli__trial__render__tests__the_summary_of_a_fixture_trial.snap"
);

/// `sample::DOCUMENTED_QUERY`.
const DOCUMENTED_QUERY: &str =
    "service=checkout _severity>=error | stats count() as errors by service";
/// `sample::documented_row()`: the checkout plan's error count.
const DOCUMENTED_SERVICE: &str = "checkout";
const DOCUMENTED_ERRORS: u64 = 20;

/// `sample::TUTORIAL_EVENTS`: the durations of all three, and the message
/// of the one error event, which the query tutorial selects by name.
const TUTORIAL_DURATIONS: [u64; 3] = [12, 1500, 700];
const TUTORIAL_ERROR_MESSAGE: &str = "connection refused";

/// `sample::QUICK_START_QUERIES`.
const QUICK_START_QUERIES: [&str; 4] = [
    "* | head 20",
    "* | stats count() by service",
    "_severity>=error | stats count() as errors by service | sort -errors | head 10",
    "service=web _severity>=warn | timechart span=5m count()",
];

#[test]
fn the_snapshot_prints_the_literals_this_test_duplicates() {
    let command = format!("trawl -p trial query '{DOCUMENTED_QUERY}'");
    assert!(
        SUMMARY_SNAPSHOT.contains(&command),
        "the summary snapshot no longer prints {command:?}; update DOCUMENTED_QUERY here and in the docs"
    );
    let row = format!("service {DOCUMENTED_SERVICE}, errors {DOCUMENTED_ERRORS}.");
    assert!(
        SUMMARY_SNAPSHOT.contains(&row),
        "the summary snapshot no longer prints {row:?}; update the row here and in the docs"
    );
}

#[test]
fn the_tutorial_quotes_the_documented_query_and_its_row() {
    assert!(FIRST_QUERY.contains(DOCUMENTED_QUERY));
    assert!(
        FIRST_QUERY.contains(&format!("trawl -p trial query '{DOCUMENTED_QUERY}'")),
        "the tutorial quotes the command line the summary prints"
    );
    assert!(FIRST_QUERY.contains(&format!("\"errors\":{DOCUMENTED_ERRORS}")));
    assert!(FIRST_QUERY.contains(&format!("\"service\":\"{DOCUMENTED_SERVICE}\"")));
    assert!(FIRST_QUERY.contains(&format!(
        "service {DOCUMENTED_SERVICE}, errors {DOCUMENTED_ERRORS}."
    )));
}

#[test]
fn the_tutorial_quotes_the_summary_verbatim() {
    // The snapshot's fixture paths are `/state/trawl/trial/...`; the page
    // shows the default state home instead. Every other line is verbatim.
    let body = SUMMARY_SNAPSHOT
        .splitn(3, "---\n")
        .nth(2)
        .expect("insta header, then the summary");
    for line in body
        .lines()
        .filter(|line| !line.contains("/state/trawl/trial/"))
    {
        assert!(
            FIRST_QUERY.contains(line),
            "the tutorial lacks the summary line {line:?}"
        );
    }
    for file in ["operator.token", "ingest.token"] {
        assert!(FIRST_QUERY.contains(&format!("~/.local/state/trawl/trial/{file}")));
    }
}

/// The trial publishes the browser UI on 127.0.0.1 only. `localhost` can
/// reach `::1` first, where another local program may listen, so the docs
/// send the browser to the published address.
#[test]
fn the_docs_send_the_browser_to_the_published_address() {
    assert!(SUMMARY_SNAPSHOT.contains("  Browser   http://127.0.0.1:18090\n"));
    assert!(FIRST_QUERY.contains("Open `http://127.0.0.1:18090`"));
    assert!(CLI_REFERENCE.contains("`http://127.0.0.1:<web-port>`"));
    for (page, text) in [("first-query", FIRST_QUERY), ("cli", CLI_REFERENCE)] {
        assert!(
            !text.contains("http://localhost:18090") && !text.contains("http://localhost:<"),
            "{page} sends the browser to localhost"
        );
    }
}

#[test]
fn the_tutorial_is_the_trial_tutorial() {
    assert!(FIRST_QUERY.contains("## From trial to installation"));
    assert!(
        FIRST_QUERY.contains("Linux"),
        "the page states the supported platform"
    );
    for heading in ["## Start the trial", "## Delete the trial"] {
        assert!(FIRST_QUERY.contains(heading), "{heading}");
    }
    for verb in [
        "trawl trial up",
        "trawl trial key",
        "trawl trial status",
        "trawl trial stop",
        "trawl trial down",
    ] {
        assert!(FIRST_QUERY.contains(verb), "{verb}");
    }
    for query in QUICK_START_QUERIES {
        assert!(
            FIRST_QUERY.contains(&format!("trawl -p trial query '{query}'")),
            "{query}"
        );
    }
    // `start/local-parquet.md` links to this anchor.
    assert!(FIRST_QUERY.contains("### Query the export without a server"));
    // The manual walkthrough is gone.
    for gone in [
        "fleet-admin migrate\n",
        "trawl-admin tls generate",
        "docker run",
        "insecure = true",
    ] {
        assert!(
            !FIRST_QUERY.contains(gone),
            "the manual walkthrough is back: {gone:?}"
        );
    }
}

/// The tutorial events sit at the newest sample timestamp, so a relative
/// bound on `service=tutorial` stops matching soon after `up`.
#[test]
fn the_query_tutorial_examples_hold_for_the_trials_life() {
    assert!(QUERY_TUTORIAL.contains(TUTORIAL_ERROR_MESSAGE));
    for duration in TUTORIAL_DURATIONS {
        assert!(QUERY_TUTORIAL.contains(&duration.to_string()), "{duration}");
    }
    let examples: Vec<&str> = QUERY_TUTORIAL
        .lines()
        .filter(|line| line.starts_with("service=tutorial"))
        .collect();
    assert!(
        examples.len() >= 8,
        "the tutorial's examples moved: {examples:?}"
    );
    for example in examples {
        assert!(
            !example.contains("last=") && !example.contains("earliest="),
            "a time bound on the tutorial events ages out: {example:?}"
        );
    }
}

#[test]
fn the_cli_reference_documents_every_trial_flag_and_ca_cert() {
    for text in [
        "## Trial mode",
        "trawl trial <VERB>",
        "`--api-port`",
        "`--web-port`",
        "`--image`",
        "`--no-sample-data`",
        "`--yes`",
        "$XDG_STATE_HOME/trawl/trial",
        "trial.lock",
        "[profiles.trial]",
        "### Pin a CA with `ca_cert`",
    ] {
        assert!(CLI_REFERENCE.contains(text), "{text}");
    }
    for verb in ["`up`", "`status`", "`key`", "`stop`", "`down`"] {
        assert!(CLI_REFERENCE.contains(&format!("| {verb} |")), "{verb}");
    }
}
