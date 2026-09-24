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

use parquet::basic::{ConvertedType, LogicalType, Repetition, TimeUnit, Type as PhysicalType};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;
use parquet::schema::types::ColumnDescriptor;

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
    /// Min/max value samples, folded across the file's row groups.
    pub samples: SampleBounds,
}

/// The unit of a parquet `TIMESTAMP` logical type: how many of the stored
/// INT64's ticks make one second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampUnit {
    /// Milliseconds since the epoch.
    Millis,
    /// Microseconds since the epoch.
    Micros,
    /// Nanoseconds since the epoch.
    Nanos,
}

impl TimestampUnit {
    /// Ticks per second.
    const fn per_second(self) -> i64 {
        match self {
            Self::Millis => 1_000,
            Self::Micros => 1_000_000,
            Self::Nanos => 1_000_000_000,
        }
    }

    /// Nanoseconds per tick.
    const fn nanos(self) -> i64 {
        1_000_000_000 / self.per_second()
    }
}

/// What a column's min/max samples are, read from its parquet column
/// descriptor rather than from the statistics, so a file whose chunks
/// carry no min/max still declares its kind.
///
/// Two samples compare only within one kind. The timestamp units share a
/// kind (they are all instants, ordered in nanoseconds), but a
/// UTC-adjusted timestamp and a local one do not: the same INT64 names a
/// different instant in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    Bool,
    Int,
    Float,
    Bytes,
    /// Deprecated 96-bit timestamps: typed, but never sampled.
    Int96,
    Timestamp {
        unit: TimestampUnit,
        utc: bool,
    },
}

impl SampleKind {
    /// The kind a column descriptor declares.
    ///
    /// An `INT64` is a timestamp when its logical type says so, or, for a
    /// file written without logical types, when its legacy converted type
    /// is `TIMESTAMP_MILLIS`/`TIMESTAMP_MICROS` — which the parquet spec
    /// defines as UTC-adjusted. A logical type that is present and not a
    /// timestamp wins over the converted type. `DATE`, `TIME` and every
    /// other annotation keep their physical kind.
    fn of(descr: &ColumnDescriptor) -> Self {
        match descr.physical_type() {
            PhysicalType::BOOLEAN => Self::Bool,
            PhysicalType::INT32 => Self::Int,
            PhysicalType::INT64 => Self::timestamp(descr).unwrap_or(Self::Int),
            PhysicalType::INT96 => Self::Int96,
            PhysicalType::FLOAT | PhysicalType::DOUBLE => Self::Float,
            PhysicalType::BYTE_ARRAY | PhysicalType::FIXED_LEN_BYTE_ARRAY => Self::Bytes,
        }
    }

    fn timestamp(descr: &ColumnDescriptor) -> Option<Self> {
        let (unit, utc) = match descr.logical_type_ref() {
            Some(LogicalType::Timestamp(ts)) => {
                let unit = match ts.unit {
                    TimeUnit::MILLIS => TimestampUnit::Millis,
                    TimeUnit::MICROS => TimestampUnit::Micros,
                    TimeUnit::NANOS => TimestampUnit::Nanos,
                };
                (unit, ts.is_adjusted_to_u_t_c)
            }
            Some(_) => return None,
            None => match descr.converted_type() {
                ConvertedType::TIMESTAMP_MILLIS => (TimestampUnit::Millis, true),
                ConvertedType::TIMESTAMP_MICROS => (TimestampUnit::Micros, true),
                _ => return None,
            },
        };
        Some(Self::Timestamp { unit, utc })
    }

    /// Whether samples of the two kinds order against each other.
    fn comparable(self, other: Self) -> bool {
        match (self, other) {
            (Self::Timestamp { utc: a, .. }, Self::Timestamp { utc: b, .. }) => a == b,
            (a, b) => a == b,
        }
    }
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
    /// An `INT64` the column declares a timestamp: `raw` ticks of `unit`
    /// since the epoch, UTC-adjusted or local as the file says.
    Timestamp {
        /// The stored integer.
        raw: i64,
        /// What one tick of `raw` is.
        unit: TimestampUnit,
        /// The file marks the column adjusted to UTC.
        utc: bool,
    },
}

impl StatVal {
    /// Decode a stat value from its raw little-endian footer bytes. The
    /// [`Statistics`] variant picks the physical type; the column's
    /// [`SampleKind`] says whether an `INT64` is a timestamp.
    fn decode(kind: SampleKind, stats: &Statistics, bytes: &[u8]) -> Option<Self> {
        match stats {
            Statistics::Int64(_) => take::<8>(bytes).map(|a| {
                let raw = i64::from_le_bytes(a);
                match kind {
                    SampleKind::Timestamp { unit, utc } => Self::Timestamp { raw, unit, utc },
                    _ => Self::Int(raw),
                }
            }),
            Statistics::Boolean(_) => Some(Self::Bool(bytes.first().copied().unwrap_or(0) != 0)),
            Statistics::Int32(_) => {
                take::<4>(bytes).map(|a| Self::Int(i64::from(i32::from_le_bytes(a))))
            }
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
    /// different kinds and so can't be compared. Timestamps order as instants
    /// across units; a UTC-adjusted and a local timestamp are different kinds.
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
            (
                Self::Timestamp {
                    raw: a,
                    unit: ua,
                    utc: za,
                },
                Self::Timestamp {
                    raw: b,
                    unit: ub,
                    utc: zb,
                },
            ) if za == zb => {
                // i128 holds every i64 tick count scaled to nanoseconds.
                let a = i128::from(*a) * i128::from(ua.nanos());
                let b = i128::from(*b) * i128::from(ub.nanos());
                Some(a < b)
            }
            _ => None,
        }
    }

    /// Render the sample as a display string (truncated to [`MAX_SAMPLE_LEN`]).
    fn display(&self) -> String {
        match self {
            Self::Bool(v) => v.to_string(),
            Self::Int(v) => v.to_string(),
            Self::Float(v) => v.to_string(),
            Self::Timestamp { raw, unit, utc } => render_timestamp(*raw, *unit, *utc),
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

/// Render a timestamp sample as fixed-width text,
/// `YYYY-MM-DDTHH:MM:SS.ffffff`, with a trailing `Z` only when the file marks
/// the column UTC-adjusted. Nanoseconds floor to microseconds, and ticks
/// before the epoch count backwards from it (`-500` milliseconds is
/// `1969-12-31T23:59:59.500000`).
///
/// The conversion is checked. A value no calendar date in `0000..=9999`
/// expresses renders as its stored integer, never as a guessed date — the
/// same text an unannotated `INT64` sample shows.
fn render_timestamp(raw: i64, unit: TimestampUnit, utc: bool) -> String {
    use chrono::{Datelike as _, Timelike as _};

    let per_second = unit.per_second();
    // Euclidean division keeps the sub-second part non-negative, so a
    // pre-epoch value lands on the right second.
    let secs = raw.div_euclid(per_second);
    let Ok(nanos) = u32::try_from(raw.rem_euclid(per_second) * unit.nanos()) else {
        return raw.to_string();
    };
    let Some(at) = chrono::DateTime::from_timestamp(secs, nanos) else {
        return raw.to_string();
    };
    if !(0..=9999).contains(&at.year()) {
        return raw.to_string();
    }
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}{zone}",
        year = at.year(),
        month = at.month(),
        day = at.day(),
        hour = at.hour(),
        minute = at.minute(),
        second = at.second(),
        micros = at.nanosecond() / 1_000,
        zone = if utc { "Z" } else { "" },
    )
}

/// Read the first `N` bytes of `bytes` as a fixed array, or `None` if short.
fn take<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.get(..N)?.try_into().ok()
}

/// A column's running min/max samples, folded across row groups and files.
///
/// The fold tracks the column's [`SampleKind`] as well as its bounds, and
/// the kind comes from each file's column descriptor, so a file or row
/// group without min/max still takes part. Once two observations disagree
/// on kind (a BIGINT file beside a TIMESTAMP file during a repin, say) the
/// column is mixed for good: both bounds are gone for the rest of the fold,
/// whatever comes after, so the sample never depends on file order. A
/// missing min or max is otherwise neutral.
#[derive(Debug, Clone, Default)]
pub struct SampleBounds(Bounds);

#[derive(Debug, Clone, Default)]
enum Bounds {
    #[default]
    Unseen,
    Known {
        kind: SampleKind,
        min: Option<StatVal>,
        max: Option<StatVal>,
    },
    Mixed,
}

impl SampleBounds {
    /// The minimum sample, or `None` when no observation carried one or the
    /// column's kinds disagree.
    #[must_use]
    pub fn min(&self) -> Option<&StatVal> {
        match &self.0 {
            Bounds::Known { min, .. } => min.as_ref(),
            Bounds::Unseen | Bounds::Mixed => None,
        }
    }

    /// The maximum sample, under the same rules as [`Self::min`].
    #[must_use]
    pub fn max(&self) -> Option<&StatVal> {
        match &self.0 {
            Bounds::Known { max, .. } => max.as_ref(),
            Bounds::Unseen | Bounds::Mixed => None,
        }
    }

    /// Fold in one observation of a column of `kind`, with the min/max it
    /// carries, if any.
    fn observe(&mut self, kind: SampleKind, min: Option<StatVal>, max: Option<StatVal>) {
        match &mut self.0 {
            Bounds::Mixed => {}
            Bounds::Unseen => self.0 = Bounds::Known { kind, min, max },
            Bounds::Known {
                kind: seen,
                min: cur_min,
                max: cur_max,
            } => {
                if !seen.comparable(kind)
                    || !merge(cur_min, min, true)
                    || !merge(cur_max, max, false)
                {
                    self.0 = Bounds::Mixed;
                }
            }
        }
    }

    /// Fold another fold (one file's) into this one.
    fn absorb(&mut self, other: Self) {
        match other.0 {
            Bounds::Unseen => {}
            Bounds::Known { kind, min, max } => self.observe(kind, min, max),
            Bounds::Mixed => self.0 = Bounds::Mixed,
        }
    }
}

/// Fold a candidate into a running min (`is_min = true`) or max bound of the
/// same kind. Returns `false` when the two values cannot be ordered, which
/// the caller treats as a kind disagreement.
fn merge(acc: &mut Option<StatVal>, candidate: Option<StatVal>, is_min: bool) -> bool {
    let Some(cand) = candidate else { return true };
    match acc {
        None => *acc = Some(cand),
        Some(cur) => match cand.precedes(cur) {
            // candidate is a tighter min, or (for max) is >= the current bound
            Some(true) if is_min => *acc = Some(cand),
            Some(false) if !is_min => *acc = Some(cand),
            Some(_) => {}
            None => return false,
        },
    }
    true
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
                samples: SampleBounds::default(),
            });
            entry.num_values += u64::try_from(cc.num_values()).unwrap_or(0);
            entry.compressed_bytes += u64::try_from(cc.compressed_size()).unwrap_or(0);
            // The kind is observed even when this chunk has no min/max.
            let kind = SampleKind::of(cc.column_descr());
            let (mut min, mut max) = (None, None);
            if let Some(stats) = cc.statistics() {
                if let Some(nc) = stats.null_count_opt() {
                    entry.null_count += nc;
                }
                min = stats
                    .min_bytes_opt()
                    .and_then(|b| StatVal::decode(kind, stats, b));
                max = stats
                    .max_bytes_opt()
                    .and_then(|b| StatVal::decode(kind, stats, b));
            }
            entry.samples.observe(kind, min, max);
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
    samples: SampleBounds,
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
            e.samples.absorb(c.samples);
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
                min_value: c.samples.min().map(StatVal::display),
                max_value: c.samples.max().map(StatVal::display),
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

    /// What the pinned `DuckDB` writes for each of its timestamp types. The
    /// timestamp sample tests below rest on these annotations: every naive
    /// type is written NOT adjusted to UTC (so its sample has no `Z`), only
    /// `TIMESTAMPTZ` is adjusted, `TIMESTAMP_S` is widened to microseconds,
    /// and there is no UTC-adjusted milli or nano type to write.
    #[test]
    fn duckdb_timestamp_types_carry_these_parquet_annotations() {
        use parquet::basic::{ConvertedType, LogicalType, TimeUnit};
        const LEGACY_MICROS: ConvertedType = ConvertedType::TIMESTAMP_MICROS;
        const LEGACY_MILLIS: ConvertedType = ConvertedType::TIMESTAMP_MILLIS;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("types.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        write_parquet(
            &conn,
            &path,
            "SELECT TIMESTAMP '2026-01-02 03:04:05' AS ts, \
                    TIMESTAMPTZ '2026-01-02 03:04:05+00' AS tstz, \
                    TIMESTAMP_MS '2026-01-02 03:04:05' AS tms, \
                    TIMESTAMP_NS '2026-01-02 03:04:05' AS tns, \
                    TIMESTAMP_S '2026-01-02 03:04:05' AS tsec",
        );

        let file = File::open(&path).unwrap();
        let reader = SerializedFileReader::new(file).unwrap();
        let schema = reader.metadata().file_metadata().schema_descr();
        let seen: Vec<_> = schema
            .columns()
            .iter()
            .map(|c| {
                let Some(LogicalType::Timestamp(ts)) = c.logical_type_ref() else {
                    panic!("{} carries no timestamp logical type", c.name());
                };
                (
                    c.name().to_owned(),
                    ts.unit,
                    ts.is_adjusted_to_u_t_c,
                    c.converted_type(),
                )
            })
            .collect();
        let want = |name: &str, unit, utc, converted| (name.to_owned(), unit, utc, converted);
        assert_eq!(
            seen,
            vec![
                want("ts", TimeUnit::MICROS, false, LEGACY_MICROS),
                want("tstz", TimeUnit::MICROS, true, LEGACY_MICROS),
                want("tms", TimeUnit::MILLIS, false, LEGACY_MILLIS),
                want("tns", TimeUnit::NANOS, false, ConvertedType::NONE),
                want("tsec", TimeUnit::MICROS, false, LEGACY_MICROS),
            ]
        );
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
        let decoded = StatVal::decode(SampleKind::Bytes, &stats, &raw).unwrap();

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

    /// Fold `paths` in order and return `column`'s rendered `(min, max)`.
    fn samples_of(paths: &[&Path], column: &str) -> (Option<String>, Option<String>) {
        let mut acc = StatsAccumulator::default();
        for path in paths {
            acc.add_file(read_file_stats(path).unwrap());
        }
        let col = acc
            .finish()
            .into_iter()
            .find(|c| c.column_name == column)
            .unwrap_or_else(|| panic!("no column {column}"));
        (col.min_value, col.max_value)
    }

    fn pair(min: &str, max: &str) -> (Option<String>, Option<String>) {
        (Some(min.to_owned()), Some(max.to_owned()))
    }

    #[test]
    fn timestamp_stats_render_by_logical_type_and_unit() {
        // Every naive DuckDB type is written local (not UTC-adjusted; see
        // `duckdb_timestamp_types_carry_these_parquet_annotations`), so no
        // sample here carries a `Z`.
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let path = dir.path().join("units.parquet");
        write_parquet(
            &conn,
            &path,
            "SELECT * FROM (VALUES \
                (TIMESTAMP_MS '1969-12-31 23:59:59.5', \
                 TIMESTAMP '1969-12-31 23:59:59.5', \
                 TIMESTAMP_NS '1969-12-31 23:59:59.999999999', \
                 TIMESTAMP_S '1969-12-31 23:59:59'), \
                (TIMESTAMP_MS '2026-01-02 03:04:05.123', \
                 TIMESTAMP '2026-01-02 03:04:05.123456', \
                 TIMESTAMP_NS '2026-01-02 03:04:05.123456789', \
                 TIMESTAMP_S '2026-01-02 03:04:05') \
             ) t(ms, us, ns, s)",
        );

        assert_eq!(
            samples_of(&[&path], "ms"),
            pair("1969-12-31T23:59:59.500000", "2026-01-02T03:04:05.123000")
        );
        assert_eq!(
            samples_of(&[&path], "us"),
            pair("1969-12-31T23:59:59.500000", "2026-01-02T03:04:05.123456")
        );
        // Nanoseconds floor to microseconds, before the epoch as after it.
        assert_eq!(
            samples_of(&[&path], "ns"),
            pair("1969-12-31T23:59:59.999999", "2026-01-02T03:04:05.123456")
        );
        assert_eq!(
            samples_of(&[&path], "s"),
            pair("1969-12-31T23:59:59.000000", "2026-01-02T03:04:05.000000")
        );

        // Across files of different units the bounds order as instants, not
        // as stored integers: 1 s is 1000 ms but 999999999 ns is 0.999 s, so
        // an integer compare would pick the wrong file for both bounds.
        let one_second = dir.path().join("one_second_ms.parquet");
        let almost = dir.path().join("almost_ns.parquet");
        write_parquet(
            &conn,
            &one_second,
            "SELECT TIMESTAMP_MS '1970-01-01 00:00:01' AS t",
        );
        write_parquet(
            &conn,
            &almost,
            "SELECT TIMESTAMP_NS '1970-01-01 00:00:00.999999999' AS t",
        );
        let want = pair("1970-01-01T00:00:00.999999", "1970-01-01T00:00:01.000000");
        assert_eq!(samples_of(&[&one_second, &almost], "t"), want);
        assert_eq!(samples_of(&[&almost, &one_second], "t"), want);
    }

    #[test]
    fn timestamp_stats_mark_utc_only_when_adjusted() {
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let path = dir.path().join("zones.parquet");
        // The session zone decides how DuckDB reads the TIMESTAMPTZ literal,
        // not how it writes it; pin it so the literal is unambiguous.
        conn.execute_batch("SET TimeZone = 'UTC'").unwrap();
        write_parquet(
            &conn,
            &path,
            "SELECT TIMESTAMP '2026-01-02 03:04:05.123456' AS local, \
                    TIMESTAMPTZ '2026-01-02 03:04:05.123456+00' AS adjusted",
        );
        assert_eq!(
            samples_of(&[&path], "local"),
            pair("2026-01-02T03:04:05.123456", "2026-01-02T03:04:05.123456")
        );
        assert_eq!(
            samples_of(&[&path], "adjusted"),
            pair("2026-01-02T03:04:05.123456Z", "2026-01-02T03:04:05.123456Z")
        );
    }

    #[test]
    fn legacy_converted_timestamp_is_utc_adjusted() {
        // DuckDB always writes a logical type beside the converted one, so
        // it cannot produce this file: a column annotated only with the
        // legacy TIMESTAMP_MILLIS converted type, which the parquet spec
        // defines as UTC-adjusted. The parquet crate's own writer is the
        // only way to build it here.
        use parquet::basic::{Repetition, Type as PhysicalType};
        use parquet::data_type::Int64Type;
        use parquet::file::properties::WriterProperties;
        use parquet::file::writer::SerializedFileWriter;
        use parquet::schema::types::Type;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.parquet");
        let column = Type::primitive_type_builder("t", PhysicalType::INT64)
            .with_repetition(Repetition::REQUIRED)
            .with_converted_type(ConvertedType::TIMESTAMP_MILLIS)
            .build()
            .unwrap();
        let schema = Type::group_type_builder("schema")
            .with_fields(vec![Arc::new(column)])
            .build()
            .unwrap();
        let mut writer = SerializedFileWriter::new(
            File::create(&path).unwrap(),
            Arc::new(schema),
            Arc::new(WriterProperties::builder().build()),
        )
        .unwrap();
        let mut row_group = writer.next_row_group().unwrap();
        let mut col = row_group.next_column().unwrap().unwrap();
        // 1969-12-31 23:59:59.500 and 2026-01-02 03:04:05.123
        col.typed::<Int64Type>()
            .write_batch(&[-500, 1_767_323_045_123], None, None)
            .unwrap();
        col.close().unwrap();
        row_group.close().unwrap();
        writer.close().unwrap();

        let descr = read_schema_leaf(&path);
        assert!(
            descr.logical_type_ref().is_none(),
            "no logical type written"
        );
        assert_eq!(
            samples_of(&[&path], "t"),
            pair("1969-12-31T23:59:59.500000Z", "2026-01-02T03:04:05.123000Z")
        );
    }

    fn read_schema_leaf(path: &Path) -> parquet::schema::types::ColumnDescPtr {
        let reader = SerializedFileReader::new(File::open(path).unwrap()).unwrap();
        reader.metadata().file_metadata().schema_descr().column(0)
    }

    #[test]
    fn utc_timestamp_samples_are_canonical_pattern_text() {
        // An in-range UTC sample is already the text the TIMESTAMP pin's
        // pattern form canonicalizes to, so the two never disagree.
        for raw in [
            -500_000,
            0,
            1_767_323_045_123_456,
            253_402_300_799_999_999, // 9999-12-31T23:59:59.999999
            -62_167_219_200_000_000, // 0000-01-01T00:00:00
        ] {
            let text = render_timestamp(raw, TimestampUnit::Micros, true);
            assert_eq!(
                trawl_core::compare::canonical_timestamp_text(&text).as_deref(),
                Some(text.as_str()),
                "{raw}"
            );
        }
    }

    #[test]
    fn out_of_range_timestamp_stat_renders_verbatim() {
        // DuckDB writes its infinities as ±i64::MAX and accepts years past
        // 9999; none of them is a four-digit calendar date.
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let infinite = dir.path().join("infinite.parquet");
        write_parquet(
            &conn,
            &infinite,
            "SELECT * FROM (VALUES (TIMESTAMP 'infinity'), (TIMESTAMP '-infinity')) t(t)",
        );
        assert_eq!(
            samples_of(&[&infinite], "t"),
            pair(&(-i64::MAX).to_string(), &i64::MAX.to_string())
        );
        let far = dir.path().join("far.parquet");
        write_parquet(
            &conn,
            &far,
            "SELECT * FROM (VALUES (TIMESTAMP '9999-12-31 23:59:59.999999'), \
                                   (TIMESTAMP '10000-01-01 00:00:00')) t(t)",
        );
        assert_eq!(
            samples_of(&[&far], "t"),
            pair("9999-12-31T23:59:59.999999", "253402300800000000")
        );

        // i64::MIN is not writable through DuckDB (it reserves the value), so
        // the integer extremes of every unit go through the renderer
        // directly. Only nanoseconds (1677..2262) stay in range there.
        for (unit, raw, want) in [
            (TimestampUnit::Millis, i64::MIN, None),
            (TimestampUnit::Millis, i64::MAX, None),
            (TimestampUnit::Micros, i64::MIN, None),
            (TimestampUnit::Micros, i64::MAX, None),
            (
                TimestampUnit::Nanos,
                i64::MIN,
                Some("1677-09-21T00:12:43.145224"),
            ),
            (
                TimestampUnit::Nanos,
                i64::MAX,
                Some("2262-04-11T23:47:16.854775"),
            ),
        ] {
            for utc in [false, true] {
                let want = want.map_or_else(
                    || raw.to_string(),
                    |at| format!("{at}{}", if utc { "Z" } else { "" }),
                );
                assert_eq!(render_timestamp(raw, unit, utc), want, "{unit:?} {raw}");
            }
        }
        // Year 0000 renders; the year before does not.
        assert_eq!(
            render_timestamp(-62_167_219_200_001, TimestampUnit::Millis, true),
            "-62167219200001"
        );
        assert_eq!(
            render_timestamp(-62_167_219_200_000, TimestampUnit::Millis, true),
            "0000-01-01T00:00:00.000000Z"
        );
    }

    #[test]
    fn mixed_kind_merge_yields_no_sample_in_either_order() {
        let dir = tempfile::tempdir().unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let file = |name: &str, select: &str| {
            let path = dir.path().join(name);
            write_parquet(&conn, &path, select);
            path
        };
        // Each file also carries `n`, a column whose kind never changes.
        let int = file("int.parquet", "SELECT 5::BIGINT AS c, 1 AS n");
        let ts = file(
            "ts.parquet",
            "SELECT TIMESTAMP '2026-01-02 03:04:05' AS c, 2 AS n",
        );
        let ts2 = file(
            "ts2.parquet",
            "SELECT TIMESTAMP '2026-02-02 03:04:05' AS c, 3 AS n",
        );
        // A TIMESTAMP chunk with no min/max: all NULL.
        let empty_ts = file("empty_ts.parquet", "SELECT NULL::TIMESTAMP AS c, 4 AS n");
        let utc = file(
            "utc.parquet",
            "SELECT TIMESTAMPTZ '2026-01-02 03:04:05+00' AS c, 5 AS n",
        );
        let text = file("text.parquet", "SELECT 'x' AS c, 6 AS n");
        let none = (None, None);

        for (why, order) in [
            ("int then ts", vec![&int, &ts]),
            ("ts then int", vec![&ts, &int]),
            ("ts, int, ts", vec![&ts, &int, &ts2]),
            ("int, ts, ts", vec![&int, &ts, &ts2]),
            ("int then stat-less ts", vec![&int, &empty_ts]),
            ("stat-less ts then int", vec![&empty_ts, &int]),
            ("local then utc", vec![&ts, &utc]),
            ("utc then local", vec![&utc, &ts]),
            ("int then text", vec![&int, &text]),
            ("text then int", vec![&text, &int]),
        ] {
            let paths: Vec<&Path> = order.iter().map(|p| p.as_path()).collect();
            assert_eq!(samples_of(&paths, "c"), none, "{why}");
            // The disagreement is the column's, not the file's.
            assert!(samples_of(&paths, "n").0.is_some(), "{why}: n");
        }

        // Same kind stays sampled, and a stat-less file of that kind is
        // neutral.
        assert_eq!(
            samples_of(&[&empty_ts, &ts2, &ts], "c"),
            pair("2026-01-02T03:04:05.000000", "2026-02-02T03:04:05.000000")
        );
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
