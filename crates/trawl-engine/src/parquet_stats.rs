// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-Rust parquet footer statistics reader.
//!
//! Reads per-column aggregate statistics (value/null counts, min/max, compressed
//! size) and the row count straight from parquet file footers via the `parquet`
//! crate — deliberately NOT `DuckDB`'s `parquet_metadata()` table function, which
//! can `SIGSEGV` on some files (a null cast-function pointer while materializing a
//! stats `Value`). Parsing footers in safe Rust turns a poisoned file into a
//! catchable [`ParquetStatsError`] instead of an uncatchable crash that takes the
//! whole daemon down.
//!
//! Only the footer (thrift metadata) is read — never the data pages — so this is
//! footer-cheap and needs no decompression codec, even for compressed files.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;

use trawl_api::value::ParquetColumnStats;

/// Stat-value samples longer than this are truncated, so a pathological value
/// can't bloat the schema cache.
const MAX_SAMPLE_LEN: usize = 256;

/// Error reading a parquet file's footer. Carries the offending path so callers
/// can quarantine and log exactly which file failed.
#[derive(Debug, thiserror::Error)]
pub enum ParquetStatsError {
    /// The file could not be opened.
    #[error("opening parquet file {}: {source}", path.display())]
    Open {
        /// The file that failed to open.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The footer could not be parsed (corrupt, truncated, or not parquet).
    #[error("reading parquet footer from {}: {source}", path.display())]
    Footer {
        /// The file whose footer failed to parse.
        path: PathBuf,
        /// The underlying parquet error.
        #[source]
        source: parquet::errors::ParquetError,
    },
}

/// Footer-derived statistics for a single parquet file.
#[derive(Debug)]
pub struct FileStats {
    /// Rows in the file (`FileMetaData::num_rows`).
    pub num_rows: u64,
    /// Per-column stats, summed across the file's row groups.
    pub columns: Vec<FileColumnStat>,
}

/// One column's footer stats within a single file.
#[derive(Debug)]
pub struct FileColumnStat {
    /// Dotted column path (the parquet `path_in_schema`).
    pub name: String,
    /// Total values across the file's row groups.
    pub num_values: u64,
    /// Null values across the file's row groups (0 if stats are absent).
    pub null_count: u64,
    /// Total compressed size of this column's chunks, in bytes.
    pub compressed_bytes: u64,
    /// Minimum value sample, typed (`None` if stats are absent).
    pub min: Option<StatVal>,
    /// Maximum value sample, typed (`None` if stats are absent).
    pub max: Option<StatVal>,
}

/// A typed min/max sample. Typed rather than raw bytes so cross-row-group and
/// cross-file merges order correctly — comparing the raw little-endian stat
/// bytes would mis-order integers and floats.
#[derive(Debug, Clone)]
pub enum StatVal {
    /// A boolean stat.
    Bool(bool),
    /// An integer stat (`INT32`/`INT64`, widened to `i64`).
    Int(i64),
    /// A floating-point stat (`FLOAT`/`DOUBLE`, widened to `f64`).
    Float(f64),
    /// A byte-array stat (string/binary), kept as raw bytes until display.
    Bytes(Vec<u8>),
}

impl StatVal {
    /// Decode a stat value from its raw little-endian footer bytes, using the
    /// [`Statistics`] variant to pick the physical type.
    fn decode(stats: &Statistics, bytes: &[u8]) -> Option<Self> {
        match stats {
            Statistics::Boolean(_) => Some(Self::Bool(bytes.first().copied().unwrap_or(0) != 0)),
            Statistics::Int32(_) => {
                take::<4>(bytes).map(|a| Self::Int(i64::from(i32::from_le_bytes(a))))
            }
            Statistics::Int64(_) => take::<8>(bytes).map(|a| Self::Int(i64::from_le_bytes(a))),
            Statistics::Float(_) => {
                take::<4>(bytes).map(|a| Self::Float(f64::from(f32::from_le_bytes(a))))
            }
            Statistics::Double(_) => take::<8>(bytes).map(|a| Self::Float(f64::from_le_bytes(a))),
            Statistics::ByteArray(_) | Statistics::FixedLenByteArray(_) => {
                Some(Self::Bytes(bytes.to_vec()))
            }
            // INT96 is a deprecated 96-bit timestamp; skip its min/max sample
            // rather than guess at the legacy nanos-of-day encoding.
            Statistics::Int96(_) => None,
        }
    }

    /// Whether `self` orders strictly before `other`, or `None` when the two are
    /// different kinds (a column's type changed across files) and so can't be
    /// compared.
    fn precedes(&self, other: &Self) -> Option<bool> {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => Some(a < b),
            (Self::Int(a), Self::Int(b)) => Some(a < b),
            (Self::Float(a), Self::Float(b)) => Some(a.total_cmp(b).is_lt()),
            (Self::Bytes(a), Self::Bytes(b)) => Some(a < b),
            _ => None,
        }
    }

    /// Render the sample as a display string (truncated to [`MAX_SAMPLE_LEN`]).
    fn display(&self) -> String {
        match self {
            Self::Bool(v) => v.to_string(),
            Self::Int(v) => v.to_string(),
            Self::Float(v) => v.to_string(),
            Self::Bytes(b) => {
                let s = String::from_utf8_lossy(b);
                if s.chars().count() > MAX_SAMPLE_LEN {
                    let mut t: String = s.chars().take(MAX_SAMPLE_LEN).collect();
                    t.push('…');
                    t
                } else {
                    s.into_owned()
                }
            }
        }
    }
}

/// Read the first `N` bytes of `bytes` as a fixed array, or `None` if short.
fn take<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.get(..N)?.try_into().ok()
}

/// Fold a candidate into a running min (`is_min = true`) or max accumulator.
/// Different-kind values (a column's type changed across row groups/files) are
/// dropped so ordering stays well-defined.
fn merge(acc: &mut Option<StatVal>, candidate: Option<StatVal>, is_min: bool) {
    let Some(cand) = candidate else { return };
    match acc {
        None => *acc = Some(cand),
        Some(cur) => match cand.precedes(cur) {
            // candidate is a tighter min, or (for max) is >= the current bound
            Some(true) if is_min => *acc = Some(cand),
            Some(false) if !is_min => *acc = Some(cand),
            // candidate doesn't extend the bound, or kinds differ → keep current
            _ => {}
        },
    }
}

/// Read a single parquet file's footer statistics. Pure Rust — never invokes
/// `DuckDB`. A corrupt, truncated, or non-parquet file yields an [`Err`], not a
/// crash.
///
/// # Errors
/// Returns [`ParquetStatsError::Open`] if the file cannot be opened and
/// [`ParquetStatsError::Footer`] if its footer cannot be parsed.
pub fn read_file_stats(path: &Path) -> Result<FileStats, ParquetStatsError> {
    let file = File::open(path).map_err(|source| ParquetStatsError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = SerializedFileReader::new(file).map_err(|source| ParquetStatsError::Footer {
        path: path.to_path_buf(),
        source,
    })?;
    let meta = reader.metadata();
    let num_rows = u64::try_from(meta.file_metadata().num_rows()).unwrap_or(0);

    // Sum each column's chunks across the file's row groups, keyed by name.
    let mut cols: BTreeMap<String, FileColumnStat> = BTreeMap::new();
    for rg in meta.row_groups() {
        for cc in rg.columns() {
            let name = cc.column_path().string();
            let entry = cols.entry(name.clone()).or_insert_with(|| FileColumnStat {
                name,
                num_values: 0,
                null_count: 0,
                compressed_bytes: 0,
                min: None,
                max: None,
            });
            entry.num_values += u64::try_from(cc.num_values()).unwrap_or(0);
            entry.compressed_bytes += u64::try_from(cc.compressed_size()).unwrap_or(0);
            if let Some(stats) = cc.statistics() {
                if let Some(nc) = stats.null_count_opt() {
                    entry.null_count += nc;
                }
                if let Some(bytes) = stats.min_bytes_opt() {
                    merge(&mut entry.min, StatVal::decode(stats, bytes), true);
                }
                if let Some(bytes) = stats.max_bytes_opt() {
                    merge(&mut entry.max, StatVal::decode(stats, bytes), false);
                }
            }
        }
    }

    Ok(FileStats {
        num_rows,
        columns: cols.into_values().collect(),
    })
}

/// Accumulates [`FileStats`] from many files into per-column
/// [`ParquetColumnStats`] plus a total row count — the footer-based replacement
/// for `DuckDB`'s `parquet_metadata()` aggregation.
#[derive(Debug, Default)]
pub struct StatsAccumulator {
    total_rows: u64,
    cols: BTreeMap<String, ColAccum>,
}

#[derive(Debug, Default)]
struct ColAccum {
    num_values: u64,
    null_count: u64,
    compressed_bytes: u64,
    min: Option<StatVal>,
    max: Option<StatVal>,
}

impl StatsAccumulator {
    /// Fold one file's footer stats into the running totals.
    pub fn add_file(&mut self, file: FileStats) {
        self.total_rows += file.num_rows;
        for c in file.columns {
            let e = self.cols.entry(c.name).or_default();
            e.num_values += c.num_values;
            e.null_count += c.null_count;
            e.compressed_bytes += c.compressed_bytes;
            merge(&mut e.min, c.min, true);
            merge(&mut e.max, c.max, false);
        }
    }

    /// Total rows seen across all added files.
    pub fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// Finalize into per-column stats, sorted by column name.
    pub fn finish(self) -> Vec<ParquetColumnStats> {
        self.cols
            .into_iter()
            .map(|(column_name, c)| ParquetColumnStats {
                column_name,
                total_count: c.num_values,
                null_count: c.null_count,
                min_value: c.min.as_ref().map(StatVal::display),
                max_value: c.max.as_ref().map(StatVal::display),
                compressed_bytes: c.compressed_bytes,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_parquet(conn: &duckdb::Connection, path: &Path, select: &str) {
        let sql = format!("COPY ({select}) TO '{}' (FORMAT PARQUET)", path.display());
        conn.execute_batch(&sql).unwrap();
    }

    #[test]
    fn reads_counts_and_typed_minmax_from_duckdb_parquet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        // 3 rows; one null in `msg`; integer `status`; string min/max.
        write_parquet(
            &conn,
            &path,
            "SELECT * FROM (VALUES (200, 'alpha'), (500, NULL), (404, 'zulu')) t(status, msg)",
        );

        let file_stats = read_file_stats(&path).unwrap();
        assert_eq!(file_stats.num_rows, 3);

        let mut acc = StatsAccumulator::default();
        acc.add_file(file_stats);
        assert_eq!(acc.total_rows(), 3);
        let cols = acc.finish();

        let status = cols.iter().find(|c| c.column_name == "status").unwrap();
        assert_eq!(status.total_count, 3);
        assert_eq!(status.null_count, 0);
        assert_eq!(status.min_value.as_deref(), Some("200"));
        assert_eq!(status.max_value.as_deref(), Some("500"));
        assert!(status.compressed_bytes > 0);

        let msg = cols.iter().find(|c| c.column_name == "msg").unwrap();
        assert_eq!(msg.total_count, 3);
        assert_eq!(msg.null_count, 1);
        assert_eq!(msg.min_value.as_deref(), Some("alpha"));
        assert_eq!(msg.max_value.as_deref(), Some("zulu"));
    }

    #[test]
    fn merges_minmax_across_files_in_typed_space() {
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let p1 = dir.path().join("a.parquet");
        let p2 = dir.path().join("b.parquet");
        write_parquet(&conn, &p1, "SELECT * FROM (VALUES (10), (40)) t(n)");
        write_parquet(&conn, &p2, "SELECT * FROM (VALUES (5), (99)) t(n)");

        let mut acc = StatsAccumulator::default();
        acc.add_file(read_file_stats(&p1).unwrap());
        acc.add_file(read_file_stats(&p2).unwrap());
        let cols = acc.finish();

        let n = cols.iter().find(|c| c.column_name == "n").unwrap();
        assert_eq!(n.total_count, 4);
        // Typed merge: min is 5, not "10" (which a lexical string compare gives).
        assert_eq!(n.min_value.as_deref(), Some("5"));
        assert_eq!(n.max_value.as_deref(), Some("99"));
    }

    #[test]
    fn corrupt_file_is_err_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.parquet");
        std::fs::write(&path, b"this is definitely not a parquet file").unwrap();
        assert!(read_file_stats(&path).is_err());
    }

    #[test]
    fn missing_file_is_open_error() {
        let path = Path::new("/nonexistent/does-not-exist.parquet");
        assert!(matches!(
            read_file_stats(path),
            Err(ParquetStatsError::Open { .. })
        ));
    }
}
