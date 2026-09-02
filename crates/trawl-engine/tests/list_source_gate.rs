// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! List-source resolution and the no-silent-cold-drop gate, across all four
//! read lanes (ADR-0008).
//!
//! `read_parquet` rejects a whole list source when a single element matches
//! nothing, and the server emits one glob per hour in the query's range, so
//! the everyday `service=X last=Nh` shape routinely carries elements pointing
//! at hour directories another service owns. Every lane must answer the same
//! way: resolve the source before the read, and never turn a read that could
//! not reach existing files into an empty success.
//!
//! The three shapes below run against all four entry points, because a
//! per-lane opinion is exactly what makes a query and its export disagree.
//!
//! # The one shape that is not end-to-end here
//!
//! *Matched, then deleted* — a file the source reached at resolution time and
//! not at read time — is exercised on the hot lanes, where the vanishing file
//! can be the hot snapshot (exactly how it happens in production, between
//! compaction's rename and the query). The no-hot lanes have no second file
//! to vanish, and the filesystem cannot fake the disagreement: `DuckDB`'s
//! `glob()` and `read_parquet` agree on every shape probed for it —
//! directories, directories holding only subdirectories, broken symlinks and
//! unreadable (mode 000) directories all glob to zero rows AND read as "No
//! files found". Only a genuine race separates them, so the no-hot cells of
//! that row are pinned on the policy table itself
//! (`cold_action_matrix_is_exhaustive` in `src/executor.rs`).

use std::path::{Path, PathBuf};

use duckdb::Connection;
use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::Executor;

/// `DuckDB`'s "no matching files" error text — the substring
/// `executor::is_no_files_error` tests, mirrored because that predicate is
/// crate-private.
const DUCKDB_NO_FILES_MSG: &str = "No files found that match the pattern";

/// Which entry point a shape is being run through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Query,
    QueryHot,
    Export,
    ExportHot,
}

const LANES: [Lane; 4] = [Lane::Query, Lane::QueryHot, Lane::Export, Lane::ExportHot];

/// The lanes that read the hot buffer — the ones a vanished hot snapshot can
/// reach.
const HOT_LANES: [Lane; 2] = [Lane::QueryHot, Lane::ExportHot];

impl Lane {
    /// Whether this lane unions a hot buffer into the read.
    fn has_hot(self) -> bool {
        match self {
            Lane::QueryHot | Lane::ExportHot => true,
            Lane::Query | Lane::Export => false,
        }
    }
}

/// A parquet corpus laid out the way compaction lays one out: one directory
/// per hour, one file per service inside it.
struct Corpus {
    dir: tempfile::TempDir,
    conn: Connection,
}

impl Corpus {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp dir"),
            conn: Connection::open_in_memory().expect("setup connection"),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Create the hour directory, optionally writing one service's file into
    /// it. A directory with no file of the queried service is the shape that
    /// makes the whole list unreadable.
    fn hour(&self, hour: &str, service_file: Option<&str>) -> PathBuf {
        let hour_dir = self.root().join(hour);
        std::fs::create_dir_all(&hour_dir).expect("hour dir");
        if let Some(name) = service_file {
            self.conn
                .execute_batch(&format!(
                    "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                                  'svc' AS service, 'cold' AS message) \
                     TO '{}' (FORMAT PARQUET)",
                    hour_dir.join(name).display()
                ))
                .expect("write cold parquet");
        }
        hour_dir
    }

    /// The list source the server's planner builds: one glob per hour.
    fn list_source(&self, hours: &[&str]) -> String {
        let quoted: Vec<String> = hours
            .iter()
            .map(|h| format!("'{}/{h}/*.parquet'", self.root().display()))
            .collect();
        format!("[{}]", quoted.join(", "))
    }

    /// Write a one-row hot snapshot and return its path.
    fn hot_snapshot(&self) -> PathBuf {
        let path = self.root().join("hot.ndjson");
        std::fs::write(
            &path,
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\
             \"service\":\"svc\",\"message\":\"hot\"}\n",
        )
        .expect("write hot snapshot");
        path
    }

    /// A hot snapshot path that was never written — the file compaction
    /// renamed away between the plan and the read.
    fn vanished_hot_snapshot(&self) -> PathBuf {
        self.root().join("gone.ndjson")
    }

    fn export_path(&self, lane: Lane) -> PathBuf {
        self.root().join(format!("export-{lane:?}.parquet"))
    }

    fn count_exported(&self, path: &Path) -> usize {
        let n: i64 = self
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    path.display()
                ),
                [],
                |row| row.get(0),
            )
            .expect("read exported parquet");
        usize::try_from(n).expect("row count fits")
    }
}

/// Run `dsl` through one lane, answering with the number of rows it produced.
///
/// The export lanes count the rows they wrote, so every lane answers the same
/// question — "how much of the corpus came back?" — and a shape's expectation
/// can be stated once for all four.
fn run(lane: Lane, corpus: &Corpus, source: &str, hot: &Path) -> Result<usize, EngineError> {
    let exec = Executor::new().expect("executor");
    let pins = FieldTypes::new();
    let out = corpus.export_path(lane);
    match lane {
        Lane::Query => exec
            .run_query("*", source, &pins, usize::MAX, 0)
            .map(|r| r.row_count()),
        Lane::QueryHot => exec
            .run_query_with_hot(
                "*",
                source,
                hot.to_str().expect("utf-8 hot path"),
                &pins,
                &pins,
                usize::MAX,
                0,
            )
            .map(|r| r.row_count()),
        Lane::Export => exec
            .export_parquet("*", source, &pins, &out, 1000)
            .map(|()| corpus.count_exported(&out)),
        Lane::ExportHot => exec
            .export_parquet_with_hot(
                "*",
                source,
                hot.to_str().expect("utf-8 hot path"),
                &pins,
                &pins,
                &out,
                1000,
            )
            .map(|()| corpus.count_exported(&out)),
    }
}

#[test]
fn partial_list_miss_keeps_the_cold_rows_on_every_lane() {
    // Hour 10 holds the queried service's file; hour 11 exists because
    // another service compacted into it. DuckDB rejects the whole list, so
    // without resolution the cold row is silently dropped on the query lanes
    // and 500s on the export ones.
    for lane in LANES {
        let corpus = Corpus::new();
        corpus.hour("10", Some("svc.parquet"));
        corpus.hour("11", None);
        let source = corpus.list_source(&["10", "11"]);
        let hot = corpus.hot_snapshot();

        let rows = run(lane, &corpus, &source, &hot)
            .unwrap_or_else(|e| panic!("{lane:?}: a partial list miss must not fail: {e}"));
        let expected = if lane.has_hot() { 2 } else { 1 };
        assert_eq!(
            rows, expected,
            "{lane:?}: the cold row behind the matching element must survive an \
             empty sibling element"
        );
    }
}

#[test]
fn an_all_missing_list_is_an_empty_window_not_a_failure() {
    // No parquet anywhere: the hour directories exist (another env's
    // services, a retention sweep that just ran) but hold nothing. This is
    // the one shape where an empty answer is the truth.
    for lane in LANES {
        let corpus = Corpus::new();
        corpus.hour("10", None);
        corpus.hour("11", None);
        let source = corpus.list_source(&["10", "11"]);
        let hot = corpus.hot_snapshot();

        let outcome = run(lane, &corpus, &source, &hot);
        match lane {
            // Hot lanes fall back to the hot buffer — the cold start.
            Lane::QueryHot | Lane::ExportHot => {
                let rows = outcome
                    .unwrap_or_else(|e| panic!("{lane:?}: a cold start must read hot-only: {e}"));
                assert_eq!(rows, 1, "{lane:?}: the hot row must come back");
            }
            // A query with no hot buffer answers zero rows, successfully.
            Lane::Query => {
                let rows = outcome
                    .unwrap_or_else(|e| panic!("{lane:?}: an empty window must succeed: {e}"));
                assert_eq!(rows, 0, "{lane:?}: an empty window has no rows");
            }
            // An export with no hot buffer has no empty answer to write, so
            // DuckDB's own "no files" error stands — deliberately loud.
            Lane::Export => {
                let err = outcome.expect_err("an export over an empty window must not succeed");
                // Not merely "some database error": it must be DuckDB's own
                // no-files error, the same thing the in-crate twin
                // `export_without_hot_stays_loud_for_an_empty_window` pins
                // with `is_no_files_error`. That predicate is crate-private,
                // so match its one substring here.
                assert!(
                    matches!(&err, EngineError::Database(e) if e.to_string().contains(DUCKDB_NO_FILES_MSG)),
                    "{lane:?}: the raw no-files error must stand, got {err:?}"
                );
            }
        }
    }
}

#[test]
fn a_file_that_vanishes_between_resolution_and_the_read_is_never_an_empty_answer() {
    // The production race: the source reached files when it was resolved, and
    // by the time the read ran one of them was gone (compaction's rename, a
    // retention sweep, a repin cutover). Provoked here on the hot snapshot,
    // the file that actually moves under a live query. The answer must be an
    // explicit, retryable error — never a 200 that quietly omits the entire
    // cold history.
    for lane in HOT_LANES {
        let corpus = Corpus::new();
        corpus.hour("10", Some("svc.parquet"));
        let source = corpus.list_source(&["10"]);
        let gone = corpus.vanished_hot_snapshot();

        let err = run(lane, &corpus, &source, &gone)
            .err()
            .unwrap_or_else(|| panic!("{lane:?}: a vanished file must not answer successfully"));
        assert!(
            matches!(err, EngineError::ColdDataUnread),
            "{lane:?}: must surface ColdDataUnread, got {err:?}"
        );
    }
}
