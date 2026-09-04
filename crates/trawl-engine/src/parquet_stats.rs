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

use parquet::basic::Repetition;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;

use trawl_api::value::ParquetColumnStats;

/// Stat-value samples longer than this (in characters) are truncated at
/// `display()`, so a pathological value can't bloat the schema cache.
const MAX_SAMPLE_LEN: usize = 256;

/// Byte-array stat samples are truncated to this many bytes *at decode time*, so
/// a multi-MB min/max blob can't spike memory while it's held in the
/// accumulator. Sized so `display()`'s `MAX_SAMPLE_LEN`-char cap is never
/// starved (UTF-8 is at most 4 bytes/char), keeping the rendered sample intact.
const MAX_SAMPLE_BYTES: usize = MAX_SAMPLE_LEN * 4;

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
    /// A top-level schema element that is not a scalar column: a nested
    /// group, or a repeated primitive. Only [`read_column_names`] raises
    /// this; the stats reader reads whatever `path_in_schema` says.
    #[error(
        "parquet file {} declares non-scalar top-level column {field:?} — \
         trawl writes only flat schemas",
        path.display()
    )]
    NonScalarColumn {
        /// The file carrying the nested element.
        path: PathBuf,
        /// The offending top-level schema element's name.
        field: String,
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
                // Cap the retained sample so a pathological multi-MB stat value
                // can't bloat the accumulator (it's only ever a display sample).
                let end = bytes.len().min(MAX_SAMPLE_BYTES);
                Some(Self::Bytes(bytes[..end].to_vec()))
            }
            // INT96 is a deprecated 96-bit timestamp; skip its min/max sample
            // rather than guess at the legacy nanos-of-day encoding.
            Statistics::Int96(_) => None,
        }
    }

    /// Whether `self` orders strictly before `other`, or `None` when the two are
    /// different kinds (a column's type changed across files) and so can't be
    /// compared.
    ///
    /// `Bytes` are compared as unsigned byte-lexical order, which matches
    /// parquet's modern UNSIGNED sort order for `BYTE_ARRAY` stats. Legacy files
    /// that wrote SIGNED byte-array min/max could in principle mis-order a
    /// high-bit-set value, but these are display-only samples, so the impact is
    /// cosmetic. (Samples are also truncated to `MAX_SAMPLE_BYTES`, so two values
    /// sharing that prefix compare equal — again, sample-only.)
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

/// Read the column names a parquet file declares, from its schema
/// descriptor alone.
///
/// The names come from the schema, not from the row groups, so a file with
/// zero rows still names every column it declares. That is the whole point:
/// pin garbage collection uses this as proof that no standing file carries
/// a field, and an empty-but-schema'd file is a carrier.
///
/// Top-level elements only, and never a leaf path. A column literally named
/// `a.b` and a nested group `a` with a child `b` have the same dotted
/// `path_in_schema` (which is what [`read_file_stats`] reports), so reading
/// leaves would let a nested foreign file answer for a scalar field that
/// does not exist. Rather than guess, a non-scalar top-level element is an
/// error ([`ParquetStatsError::NonScalarColumn`]) and the caller's proof
/// fails closed. Repeated primitives are refused with it: a repeated leaf
/// is a list encoding, not a scalar column.
///
/// Names are returned exactly as the file spells them. Folding to a catalog
/// key is the server's job ([`trawl_core::schema::catalog_key`]) — this
/// crate has no opinion about catalog identity.
///
/// # Errors
/// [`ParquetStatsError::Open`] if the file cannot be opened,
/// [`ParquetStatsError::Footer`] if its footer cannot be parsed, and
/// [`ParquetStatsError::NonScalarColumn`] for a nested or repeated
/// top-level element.
pub fn read_column_names(path: &Path) -> Result<Vec<String>, ParquetStatsError> {
    let file = File::open(path).map_err(|source| ParquetStatsError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = SerializedFileReader::new(file).map_err(|source| ParquetStatsError::Footer {
        path: path.to_path_buf(),
        source,
    })?;
    let root = reader.metadata().file_metadata().schema();

    let mut names = Vec::with_capacity(root.get_fields().len());
    for field in root.get_fields() {
        let repeated = field.get_basic_info().has_repetition()
            && field.get_basic_info().repetition() == Repetition::REPEATED;
        if !field.is_primitive() || repeated {
            return Err(ParquetStatsError::NonScalarColumn {
                path: path.to_path_buf(),
                field: field.name().to_owned(),
            });
        }
        names.push(field.name().to_owned());
    }
    Ok(names)
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
                // Defensive clamp: a corrupt-but-parseable footer could report
                // more nulls than values, underflowing a downstream
                // `total_count - null_count`. Nulls can never exceed values.
                null_count: c.null_count.min(c.num_values),
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
    fn long_byte_array_sample_is_truncated() {
        // A pathologically large byte-array stat must be bounded at decode (so it
        // can't bloat the accumulator) and again at display. Tested directly
        // because DuckDB itself truncates footer min/max stats to a small bound,
        // so a round-tripped file never reaches the decode cap.
        use parquet::data_type::ByteArray;
        use parquet::file::statistics::ValueStatistics;

        let stats = Statistics::ByteArray(ValueStatistics::<ByteArray>::new(
            None, None, None, None, false,
        ));
        let raw = vec![b'x'; 5000];
        let decoded = StatVal::decode(&stats, &raw).unwrap();

        // Decode caps the retained bytes at MAX_SAMPLE_BYTES.
        match &decoded {
            StatVal::Bytes(b) => assert_eq!(b.len(), MAX_SAMPLE_BYTES),
            other => panic!("expected Bytes, got {other:?}"),
        }

        // display() caps at MAX_SAMPLE_LEN chars then appends one ellipsis.
        let sample = decoded.display();
        assert_eq!(sample.chars().count(), MAX_SAMPLE_LEN + 1);
        assert!(sample.ends_with('…'));
    }

    #[test]
    fn corrupt_file_is_err_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.parquet");
        std::fs::write(&path, b"this is definitely not a parquet file").unwrap();
        assert!(read_file_stats(&path).is_err());
    }

    #[test]
    fn column_names_come_from_the_schema_not_the_rows() {
        // A file with no rows has no row groups at all, so anything that
        // read column chunks would call it carrier-less. Its schema still
        // declares the columns, which is exactly the evidence gc needs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        write_parquet(&conn, &path, "SELECT 200 AS status, 'x' AS msg WHERE 1 = 0");

        assert_eq!(read_file_stats(&path).unwrap().num_rows, 0);
        assert_eq!(
            read_column_names(&path).unwrap(),
            vec!["status".to_owned(), "msg".to_owned()]
        );
    }

    #[test]
    fn a_dotted_scalar_name_is_returned_whole() {
        // `a.b` as a column name and `a` as a group with child `b` share one
        // `path_in_schema`. Reading top-level elements keeps them apart: this
        // file has a scalar column whose name contains a dot.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dotted.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        write_parquet(&conn, &path, "SELECT 1 AS \"a.b\"");

        assert_eq!(read_column_names(&path).unwrap(), vec!["a.b".to_owned()]);
    }

    #[test]
    fn a_nested_schema_is_refused_not_flattened() {
        // Foreign parquet trawl never writes. Flattening it to `a.b` would
        // let it answer for a scalar field of that name; the proof refuses.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        write_parquet(&conn, &path, "SELECT {'b': 1} AS a");

        // The stats reader reports the flattened leaf path, which is the
        // conflation this reader exists to avoid.
        let stats = read_file_stats(&path).unwrap();
        assert!(stats.columns.iter().any(|c| c.name == "a.b"));

        match read_column_names(&path) {
            Err(ParquetStatsError::NonScalarColumn { field, .. }) => assert_eq!(field, "a"),
            other => panic!("expected a NonScalarColumn refusal, got {other:?}"),
        }
    }

    #[test]
    fn truncated_and_garbage_footers_are_err_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();

        let garbage = dir.path().join("garbage.parquet");
        std::fs::write(&garbage, b"PAR1 and then nothing that parses").unwrap();
        assert!(matches!(
            read_column_names(&garbage),
            Err(ParquetStatsError::Footer { .. })
        ));

        // A real file cut short: the magic and the footer length survive at
        // the head, the metadata thrift does not.
        let whole = dir.path().join("whole.parquet");
        write_parquet(&conn, &whole, "SELECT 1 AS n");
        let bytes = std::fs::read(&whole).unwrap();
        let truncated = dir.path().join("truncated.parquet");
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        assert!(read_column_names(&truncated).is_err());

        assert!(matches!(
            read_column_names(Path::new("/nonexistent/nope.parquet")),
            Err(ParquetStatsError::Open { .. })
        ));
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
