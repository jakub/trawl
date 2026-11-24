// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! List-source resolution and the no-silent-cold-drop gate, across all four
//! read lanes (ADR-0008).
//!
//! *Matched, then deleted* — a file the source reached at resolution time and
//! not at read time — is exercised on the hot lanes. `DuckDB`'s `glob()` and
//! `read_parquet` agree on every shape probed for it, so the no-hot cells of
//! that row are pinned on the policy table itself
//! (`cold_action_matrix_is_exhaustive` in `src/executor.rs`).

use std::path::{Path, PathBuf};

use duckdb::Connection;
use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, hot_columns_of};

/// `DuckDB`'s "no matching files" error text — the substring
/// `executor::is_no_files_error` tests, mirrored because that predicate is
/// crate-private.
const DUCKDB_NO_FILES_MSG: &str = "No files found that match the pattern";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Query,
    QueryHot,
    Export,
    ExportHot,
}

const LANES: [Lane; 4] = [Lane::Query, Lane::QueryHot, Lane::Export, Lane::ExportHot];

const HOT_LANES: [Lane; 2] = [Lane::QueryHot, Lane::ExportHot];

impl Lane {
    fn has_hot(self) -> bool {
        match self {
            Lane::QueryHot | Lane::ExportHot => true,
            Lane::Query | Lane::Export => false,
        }
    }
}

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

    fn list_source(&self, hours: &[&str]) -> String {
        let quoted: Vec<String> = hours
            .iter()
            .map(|h| format!("'{}/{h}/*.parquet'", self.root().display()))
            .collect();
        format!("[{}]", quoted.join(", "))
    }

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

fn run(lane: Lane, corpus: &Corpus, source: &str, hot: &Path) -> Result<usize, EngineError> {
    let exec = Executor::new().expect("executor");
    let pins = FieldTypes::new();
    let out = corpus.export_path(lane);
    match lane {
        Lane::Query => exec
            .run_query("*", source, &pins, usize::MAX)
            .map(|r| r.row_count()),
        Lane::QueryHot => exec
            .run_query_with_hot(
                "*",
                source,
                hot.to_str().expect("utf-8 hot path"),
                &hot_columns_of(hot),
                &pins,
                &pins,
                usize::MAX,
            )
            .map(|r| r.row_count()),
        Lane::Export => exec
            .export_parquet("*", source, &pins, &out, 1000)
            .map(|_| corpus.count_exported(&out)),
        Lane::ExportHot => exec
            .export_parquet_with_hot(
                "*",
                source,
                hot.to_str().expect("utf-8 hot path"),
                &hot_columns_of(hot),
                &pins,
                &pins,
                &out,
                1000,
            )
            .map(|_| corpus.count_exported(&out)),
    }
}

#[test]
fn partial_list_miss_keeps_the_cold_rows_on_every_lane() {
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
    for lane in LANES {
        let corpus = Corpus::new();
        corpus.hour("10", None);
        corpus.hour("11", None);
        let source = corpus.list_source(&["10", "11"]);
        let hot = corpus.hot_snapshot();

        let outcome = run(lane, &corpus, &source, &hot);
        match lane {
            Lane::QueryHot | Lane::ExportHot => {
                let rows = outcome
                    .unwrap_or_else(|e| panic!("{lane:?}: a cold start must read hot-only: {e}"));
                assert_eq!(rows, 1, "{lane:?}: the hot row must come back");
            }
            Lane::Query => {
                let rows = outcome
                    .unwrap_or_else(|e| panic!("{lane:?}: an empty window must succeed: {e}"));
                assert_eq!(rows, 0, "{lane:?}: an empty window has no rows");
            }
            Lane::Export => {
                let err = outcome.expect_err("an export over an empty window must not succeed");
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
