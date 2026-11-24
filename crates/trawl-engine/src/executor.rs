// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query execution against `DuckDB`.

use std::borrow::Cow;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;
use std::sync::Arc;

use chrono::{Offset as _, Utc};
use duckdb::Connection;
use duckdb::core::LogicalTypeId;
use duckdb::types::{TimeUnit, ValueRef};
use trawl_core::ast::{PipeStage, Query, Spanned};
use trawl_core::context::EvalContext;
use trawl_core::emitter::{
    self, ColdColumns, EmittedQuery, HotColumns, InputColumns, SqlValue, TimechartInputCheck,
};
use trawl_core::parser;
use trawl_core::pattern::Preparer;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes, catalog_key};

use crate::cancel::CancelLatch;
use crate::column_type::{StoredType, column_type};
use crate::error::{DuckDbDiagnostic, EngineError, missing_column};
use crate::scratch::ScratchLock;
use crate::timing::{Fallback, PhaseClock, QueryPhase};
use crate::value::{
    Column, ColumnType, QueryResult, SchemaColumn, SchemaResult, Value, land_i128, land_u64,
};

/// The result columns that hold stored timestamps, named from the
/// statement's logical types before its first row is read, so a zero-row
/// result names the same columns.
#[derive(Debug, Default)]
struct TimestampColumns {
    /// Every column `DuckDB` returned as a timestamp type, or as a list of
    /// them at any depth ([`holds_timestamps`]): the tail's output renders
    /// these as the wire's text.
    rendered: Vec<String>,
    /// The scalar columns whose logical type is exactly TIMESTAMP: the
    /// tail compares these as instants where no pin names them. The other
    /// timestamp types are left out, because the all-SQL lane compares
    /// them differently from a Timestamp pin.
    instants: Vec<String>,
}

/// How many statements the query and export lanes handed `DuckDB` to bind.
///
/// Process-global, so a reader that shares its test binary with binding
/// tests must serialize against them (see
/// `tests/complexity_admission.rs`).
///
/// It also keeps the text of the last statement each thread bound. Per
/// thread, because the lanes run synchronously on their caller's thread,
/// so parallel tests cannot read each other's statements.
#[cfg(any(test, feature = "test-support"))]
pub mod prepare_probe {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static PREPARES: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static LAST_STATEMENT: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) fn record(sql: &str) {
        PREPARES.fetch_add(1, Ordering::SeqCst);
        LAST_STATEMENT.with(|last| *last.borrow_mut() = Some(sql.to_owned()));
    }

    #[must_use]
    pub fn count() -> u64 {
        PREPARES.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn last_statement() -> Option<String> {
        LAST_STATEMENT.with(|last| last.borrow().clone())
    }
}

/// What the schema probe asked `DuckDB` (ADR-0062), for the tests that
/// bound it: one `DESCRIBE` per request, never a bind attempt.
///
/// The probe never goes through [`prepare_probe`]: a lane's last prepared
/// statement stays its own. Each test-only fault is keyed by a marker the
/// `DESCRIBE` text must contain, so parallel tests over different
/// directories never meet each other's faults.
#[cfg(any(test, feature = "test-support"))]
pub mod schema_probe {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    static DESCRIBES: AtomicU64 = AtomicU64::new(0);
    static NEXT_FAULT: AtomicU64 = AtomicU64::new(0);
    static FAULTS: Mutex<Vec<Fault>> = Mutex::new(Vec::new());

    thread_local! {
        static ON_THIS_THREAD: Cell<u64> = const { Cell::new(0) };
        static LAST_STATEMENT: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    type Hook = Arc<dyn Fn() + Send + Sync>;

    struct Fault {
        id: u64,
        marker: String,
        kind: FaultKind,
    }

    enum FaultKind {
        Park(Hook),
        Claim { column: String, ty: String },
    }

    fn faults() -> MutexGuard<'static, Vec<Fault>> {
        FAULTS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn install(marker: &str, kind: FaultKind) -> FaultGuard {
        let id = NEXT_FAULT.fetch_add(1, Ordering::SeqCst);
        faults().push(Fault {
            id,
            marker: marker.to_owned(),
            kind,
        });
        FaultGuard(id)
    }

    #[derive(Debug)]
    #[must_use = "the fault is removed when the guard drops"]
    pub struct FaultGuard(u64);

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            faults().retain(|fault| fault.id != self.0);
        }
    }

    pub(super) fn record(sql: &str) {
        DESCRIBES.fetch_add(1, Ordering::SeqCst);
        ON_THIS_THREAD.with(|n| n.set(n.get() + 1));
        LAST_STATEMENT.with(|last| *last.borrow_mut() = Some(sql.to_owned()));
    }

    /// Run every park hook whose marker `sql` contains, outside the lock,
    /// so a parked probe never holds up another test's faults.
    pub(super) fn park(sql: &str) {
        let hooks: Vec<Hook> = faults()
            .iter()
            .filter(|fault| sql.contains(&fault.marker))
            .filter_map(|fault| match &fault.kind {
                FaultKind::Park(hook) => Some(Arc::clone(hook)),
                FaultKind::Claim { .. } => None,
            })
            .collect();
        for hook in hooks {
            hook();
        }
    }

    pub(super) fn claim(sql: &str, columns: &mut Vec<(String, String)>) {
        for fault in faults().iter().filter(|fault| sql.contains(&fault.marker)) {
            if let FaultKind::Claim { column, ty } = &fault.kind
                && !columns.iter().any(|(name, _)| name == column)
            {
                columns.push((column.clone(), ty.clone()));
            }
        }
    }

    #[must_use]
    pub fn count() -> u64 {
        DESCRIBES.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn count_on_this_thread() -> u64 {
        ON_THIS_THREAD.with(Cell::get)
    }

    #[must_use]
    pub fn last_statement() -> Option<String> {
        LAST_STATEMENT.with(|last| last.borrow().clone())
    }

    /// Park every probe whose `DESCRIBE` contains `marker` in `hook`,
    /// after its prepare and before its execute, until the guard drops.
    pub fn park_when(marker: &str, hook: impl Fn() + Send + Sync + 'static) -> FaultGuard {
        install(marker, FaultKind::Park(Arc::new(hook)))
    }

    /// Make every probe whose `DESCRIBE` contains `marker` report `column`
    /// of type `ty` as well as what the reader carries, until the guard
    /// drops.
    pub fn claim_present(marker: &str, column: &str, ty: &str) -> FaultGuard {
        install(
            marker,
            FaultKind::Claim {
                column: column.to_owned(),
                ty: ty.to_owned(),
            },
        )
    }
}

fn prepare_statement<'c>(conn: &'c Connection, sql: &str) -> duckdb::Result<duckdb::Statement<'c>> {
    #[cfg(any(test, feature = "test-support"))]
    prepare_probe::record(sql);
    conn.prepare(sql)
}

/// What a hot ndjson file a test wrote carries, for the `hot_columns` a hot
/// lane takes: every key any line holds, and among them the keys whose
/// every value is JSON `null`.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn hot_columns_of(path: impl AsRef<Path>) -> HotColumns {
    let mut keys = std::collections::BTreeSet::new();
    let mut non_null = std::collections::BTreeSet::new();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for line in text.lines() {
        let Ok(serde_json::Value::Object(event)) = serde_json::from_str(line) else {
            continue;
        };
        for (key, value) in event {
            if !value.is_null() {
                non_null.insert(key.clone());
            }
            keys.insert(key);
        }
    }
    let null_only: Vec<&String> = keys.difference(&non_null).collect();
    HotColumns::new(&keys, null_only)
}

/// How a query lane bounds the rows it hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCap {
    /// Refuse a result longer than this with
    /// [`EngineError::ResultTooLarge`]. The interactive lanes use it, where
    /// a cut answer on screen would pass for the whole one.
    Refuse(usize),
    /// Keep the first `rows` rows of the answer and drop the rest, as the
    /// Parquet export's `LIMIT` does. The CSV and JSON exports use it.
    ///
    /// A pipeline that runs wholly in SQL stops reading at `rows`. A
    /// pipeline with a Rust tail cannot: the tail may aggregate, and a
    /// count over the first `rows` events is a wrong answer, not a short
    /// one. Its SQL half reads under [`RowCap::Refuse`]`(tail_input)`, and
    /// the cut applies to what the tail returns.
    Truncate { rows: usize, tail_input: usize },
}

impl RowCap {
    fn sql_read(self, rust_tail: bool) -> Self {
        match self {
            Self::Truncate { tail_input, .. } if rust_tail => Self::Refuse(tail_input),
            cap => cap,
        }
    }

    fn trim(self, result: &mut QueryResult) {
        if let Self::Truncate { rows, .. } = self {
            result.rows.truncate(rows);
        }
    }

    /// `DuckDB` builds a whole result before the first row is handed back,
    /// so a cut made only while copying rows out still pays for every row
    /// the query matched. `Truncate` therefore wraps the SQL in a `LIMIT`
    /// unless the cap is unbounded.
    fn statement(self, sql: &str) -> Cow<'_, str> {
        match self {
            Self::Truncate { rows, .. } => match export_row_limit(rows) {
                Some(limit) => format!("SELECT * FROM ({sql}) LIMIT {limit}").into(),
                None => sql.into(),
            },
            Self::Refuse(_) => sql.into(),
        }
    }
}

/// The four query and export lanes, bound to a caller's cancellation
/// latch.
///
/// Over the query database, each lane here holds the batch statement's RE2
/// allowance (ADR-0063) before the statement or any probe of it binds, and
/// keeps it until the lane returns.
#[derive(Debug)]
pub struct Cancellable<'a> {
    executor: &'a Executor,
    cancel: &'a CancelLatch,
    clock: PhaseClock,
}

impl Cancellable<'_> {
    /// Record the lanes' phases on `clock` (ADR-0046).
    #[must_use]
    pub fn timed(mut self, clock: &PhaseClock) -> Self {
        self.clock = clock.clone();
        self
    }

    /// [`Executor::run_query`], stopping at the bind-to-execute boundary
    /// once the latch is set, under the caller's [`RowCap`].
    pub fn run_query(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        cap: RowCap,
    ) -> Result<QueryResult, EngineError> {
        self.executor.run_query_latched(
            dsl,
            source,
            pins,
            SourceOrigin::Corpus,
            cap,
            self.cancel,
            &self.clock,
            self.executor.re2_threads,
        )
    }

    /// [`Self::run_query`] over a recorded run: a `from saved` source, or
    /// a capture of one. Its stored columns carry their own types, so a
    /// live pin in `pins` types only a name the input probe found in the
    /// run, and a field the run does not carry reads as an unpinned NULL
    /// (ADR-0062).
    pub fn run_recorded_query(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        cap: RowCap,
    ) -> Result<QueryResult, EngineError> {
        self.executor.run_query_latched(
            dsl,
            source,
            pins,
            SourceOrigin::Recorded,
            cap,
            self.cancel,
            &self.clock,
            self.executor.re2_threads,
        )
    }

    /// [`Executor::run_query_with_hot`], stopping at the bind-to-execute
    /// boundary once the latch is set, under the caller's [`RowCap`].
    #[allow(clippy::too_many_arguments)]
    pub fn run_query_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        cap: RowCap,
    ) -> Result<QueryResult, EngineError> {
        self.executor.run_query_with_hot_latched(
            dsl,
            source,
            hot_source,
            hot_columns,
            hot_pins,
            pins,
            cap,
            self.cancel,
            &self.clock,
            self.executor.re2_threads,
        )
    }

    /// [`Executor::export_parquet`], stopping at the bind-to-execute
    /// boundary once the latch is set.
    pub fn export_parquet(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<usize, EngineError> {
        self.executor.export_parquet_latched(
            dsl,
            source,
            pins,
            output_path,
            max_rows,
            self.cancel,
            &self.clock,
            self.executor.re2_threads,
        )
    }

    /// [`Executor::export_parquet_with_hot`], stopping at the
    /// bind-to-execute boundary once the latch is set.
    #[allow(clippy::too_many_arguments)]
    pub fn export_parquet_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<usize, EngineError> {
        self.executor.export_parquet_with_hot_latched(
            dsl,
            source,
            hot_source,
            hot_columns,
            hot_pins,
            pins,
            output_path,
            max_rows,
            self.cancel,
            &self.clock,
            self.executor.re2_threads,
        )
    }
}

#[must_use]
fn spill_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

fn private_spill_dir() -> std::io::Result<tempfile::TempDir> {
    crate::scratch::private_dir_in(&spill_dir(), crate::scratch::TMPDIR_SETTING, |_| Ok(()))
}

/// The path goes into SQL as text, so one that is not UTF-8 is refused
/// rather than converted: a lossy conversion would point `DuckDB` at a
/// different path than the private directory that was checked (ADR-0052).
fn temp_directory_statement(dir: &Path, setting: &str) -> std::io::Result<String> {
    let text = dir.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "the query spill directory {} is not valid UTF-8, so it cannot be \
                 handed to the query engine; point {setting} at a path that is",
                dir.display()
            ),
        )
    })?;
    Ok(format!("SET temp_directory='{}'", text.replace('\'', "''")))
}

/// Turn on `DuckDB`'s structured error diagnostics as the database's
/// default. The setting is local by default, but a global value is what a
/// connection without a local one reads, and a clone starts with none.
const ERRORS_AS_JSON_SQL: &str = "SET GLOBAL errors_as_json = true";

/// The budgets of the query database (ADR-0056): `DuckDB`'s
/// `max_temp_directory_size`, `memory_limit` and `threads`.
///
/// All three are database-global, so every connection cloned from the
/// database shares them: one query's spill counts against the cap of every
/// other query on the same database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryBudgets {
    /// Bytes the database may hold in its spill directory.
    pub spill_max_bytes: NonZeroU64,
    /// Bytes of memory `DuckDB` may manage for the database.
    pub memory_limit: NonZeroU64,
    pub threads: NonZeroU32,
}

impl QueryBudgets {
    /// Byte values go in as `'<n> bytes'`: `DuckDB` reads `M` and `MB` as
    /// 1000-based, while trawl's byte sizes are 1024-based, so handing it the
    /// configured spelling would set a different number.
    fn statements(&self) -> String {
        format!(
            "SET GLOBAL max_temp_directory_size = '{} bytes'; \
             SET GLOBAL memory_limit = '{} bytes'; \
             SET GLOBAL threads = {};",
            self.spill_max_bytes, self.memory_limit, self.threads
        )
    }

    /// Refuse a connection that does not read the budgets back.
    ///
    /// `DuckDB` reads a byte setting back through its human-readable format
    /// (`512.0 MiB`), so the comparison is with `format_bytes` of the
    /// configured value, the same function over the same 1024 base, rather
    /// than with the number. The format rounds, so this proves `DuckDB`
    /// took the setting at the configured magnitude, not to the byte.
    fn check_applied(&self, conn: &Connection) -> Result<(), EngineError> {
        // `format_bytes` takes a signed count, so a budget past `i64::MAX`
        // cannot be checked and is refused as unchecked.
        let checkable = |setting: &'static str, bytes: NonZeroU64| {
            i64::try_from(bytes.get()).map_err(|_| EngineError::BudgetNotApplied {
                setting,
                configured: format!("{bytes} bytes"),
                applied: String::from("a value past what the readback can check"),
            })
        };
        let spill = checkable("max_temp_directory_size", self.spill_max_bytes)?;
        let memory = checkable("memory_limit", self.memory_limit)?;
        let (spill_applied, spill_expected, memory_applied, memory_expected, threads, json) = conn
            .query_row(
                "SELECT current_setting('max_temp_directory_size'), format_bytes(?), \
                        current_setting('memory_limit'), format_bytes(?), \
                        current_setting('threads'), current_setting('errors_as_json')",
                duckdb::params![spill, memory],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, bool>(5)?,
                    ))
                },
            )?;
        let mismatch = |setting, configured: String, applied: String| {
            Err(EngineError::BudgetNotApplied {
                setting,
                configured,
                applied,
            })
        };
        if spill_applied != spill_expected {
            return mismatch("max_temp_directory_size", spill_expected, spill_applied);
        }
        if memory_applied != memory_expected {
            return mismatch("memory_limit", memory_expected, memory_applied);
        }
        if i64::from(self.threads.get()) != threads {
            return mismatch("threads", self.threads.to_string(), threads.to_string());
        }
        // Not a budget, but without it an exhausted budget cannot classify
        // as one: every failure would read as a generic database error.
        if !json {
            return mismatch("errors_as_json", "true".to_owned(), json.to_string());
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Executor {
    pub fn current_setting(&self, name: &str) -> Result<String, EngineError> {
        Ok(self
            .conn
            .query_row("SELECT current_setting(?)::VARCHAR", [name], |row| {
                row.get(0)
            })?)
    }

    pub fn count_rows_for_test(&self, sql: &str) -> Result<u64, EngineError> {
        Ok(self.raw_count_rows_for_test(sql)?)
    }

    pub fn query_i64_for_test(&self, sql: &str) -> Result<i64, EngineError> {
        Ok(self.conn.query_row(sql, [], |row| row.get(0))?)
    }

    pub fn raw_count_rows_for_test(&self, sql: &str) -> Result<u64, duckdb::Error> {
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while rows.next()?.is_some() {
            count += 1;
        }
        Ok(count)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl DiagnosticConnection {
    pub fn current_setting(&self, name: &str) -> Result<String, EngineError> {
        Ok(self
            .conn
            .query_row("SELECT current_setting(?)::VARCHAR", [name], |row| {
                row.get(0)
            })?)
    }

    /// Run `sql` in place of the sample's `SELECT 1`, discarding its rows,
    /// so a test can make a probe overrun or fail in `DuckDB` itself.
    pub fn select_for_test(&self, sql: &str) -> Result<(), EngineError> {
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    }
}

/// A connection to a query database that runs only the engine sample's
/// `SELECT 1` (ADR-0056).
///
/// Made by [`Executor::diagnostic_connection`]. It is its own type so the
/// health sampler can hold it without holding a query executor, and so no
/// pool can take it as one.
#[derive(Debug)]
pub struct DiagnosticConnection {
    // Declared first, so it drops first, as in `Executor`.
    conn: Connection,
    _spill: Arc<Spill>,
}

impl DiagnosticConnection {
    pub fn select_one(&self) -> Result<(), EngineError> {
        self.conn
            .query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(EngineError::from)
    }

    pub fn interrupt_handle(&self) -> Arc<duckdb::InterruptHandle> {
        self.conn.interrupt_handle()
    }
}

/// What every connection to one database holds behind its connection: the
/// database's private spill directory and, for the query database, the
/// lock on the scratch directory that spill directory sits in (ADR-0056).
///
/// [`Executor`] and [`DiagnosticConnection`] declare their connection
/// before it, so the connection closes first. The last connection of the
/// database to close removes the spill directory and only then releases
/// the lock.
#[derive(Debug)]
struct Spill {
    // Declared first, so the directory is removed while the lock holds.
    dir: tempfile::TempDir,
    scratch: Option<Arc<ScratchLock>>,
}

impl Spill {
    /// A captured result's row staging file: in the scratch directory when
    /// the database has one, named so the boot clearing reclaims one a
    /// killed trawld left, and otherwise in the database's own private
    /// spill directory, which nothing else reads.
    fn stage_file(&self) -> std::io::Result<tempfile::NamedTempFile> {
        match &self.scratch {
            Some(lock) => lock.scratch().stage_file(),
            None => crate::scratch::stage_file_in(self.dir.path()),
        }
    }
}

/// Query executor backed by an in-memory `DuckDB` connection.
#[derive(Debug)]
pub struct Executor {
    // Declared first, so it drops first: `DuckDB` closes and removes its
    // spill files before the directory holding them goes.
    conn: Connection,
    spill: Arc<Spill>,
    /// The threads each regex a statement binds compiles on, which its
    /// RE2 allowance counts ([`Cancellable`]): the query database's
    /// budget, or `None` for a database with no budgets, which holds no
    /// allowance.
    re2_threads: Option<NonZeroU32>,
}

impl Executor {
    /// Points `temp_directory` at a private directory made for this
    /// database in [`spill_dir`], so `DuckDB` can spill to disk even when
    /// the process working directory is read-only (ADR-0052).
    ///
    /// It sets no budgets, so `DuckDB` sizes its memory and spill caps from
    /// the host.
    pub fn new() -> Result<Self, EngineError> {
        Self::open(
            Spill {
                dir: private_spill_dir()?,
                scratch: None,
            },
            crate::scratch::TMPDIR_SETTING,
            None,
        )
    }

    /// Open the query database (ADR-0056): spill into a private directory
    /// inside the scratch directory `scratch` locks, under `budgets`.
    ///
    /// The budgets are database-global and go on before anything clones
    /// the database, so every later clone shares them. A clone then reads
    /// each one back, and a setting `DuckDB` rejects or reads back
    /// differently refuses the open.
    pub fn open_query_database(
        scratch: Arc<ScratchLock>,
        budgets: &QueryBudgets,
    ) -> Result<Self, EngineError> {
        let dir = scratch.scratch().private_spill_dir()?;
        let root = Self::open(
            Spill {
                dir,
                scratch: Some(scratch),
            },
            crate::scratch::SCRATCH_SETTING,
            Some(budgets),
        )?;
        budgets.check_applied(&root.try_clone()?.conn)?;
        Ok(root)
    }

    /// `errors_as_json` goes on first, as the database's default, so every
    /// error from this database and its clones carries the structured
    /// diagnostic the error classification reads ([`DuckDbDiagnostic`]),
    /// including a refused budget below.
    fn open(
        spill: Spill,
        setting: &str,
        budgets: Option<&QueryBudgets>,
    ) -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(ERRORS_AS_JSON_SQL)?;
        conn.execute_batch(&temp_directory_statement(spill.dir.path(), setting)?)?;
        if let Some(budgets) = budgets {
            conn.execute_batch(&budgets.statements())?;
        }
        Self::configure(&conn)?;
        Ok(Self {
            conn,
            spill: Arc::new(spill),
            re2_threads: budgets.map(|budgets| budgets.threads),
        })
    }

    /// Clone the connection the engine sample runs on (ADR-0056).
    pub fn diagnostic_connection(&self) -> Result<DiagnosticConnection, EngineError> {
        let conn = self.conn.try_clone()?;
        Self::configure(&conn)?;
        Ok(DiagnosticConnection {
            conn,
            _spill: Arc::clone(&self.spill),
        })
    }

    /// Create a new executor sharing the same underlying database.
    ///
    /// A clone shares the database, not the session: settings are not
    /// inherited, so the clone is configured in its own right.
    pub fn try_clone(&self) -> Result<Self, EngineError> {
        let conn = self.conn.try_clone()?;
        Self::configure(&conn)?;
        Ok(Self {
            conn,
            spill: Arc::clone(&self.spill),
            re2_threads: self.re2_threads,
        })
    }

    /// Pin the settings a query's answer depends on: currently the session
    /// time zone, which must be UTC on every connection.
    ///
    /// The shared `DuckDB` runtime includes ICU and defaults `TimeZone` to the
    /// host zone, and two things read it: the hot branch's TIMESTAMP conform
    /// and `now()::TIMESTAMP`, the anchor of every `last=Xh` window.
    fn configure(conn: &Connection) -> Result<(), EngineError> {
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)?;
        Ok(())
    }

    pub fn interrupt_handle(&self) -> Arc<duckdb::InterruptHandle> {
        self.conn.interrupt_handle()
    }

    /// Run the query and export lanes under a caller's cancellation latch.
    ///
    /// The interrupt handle above is the first half of stopping work and
    /// cannot be the whole of it: an interrupt raised while `DuckDB` is
    /// binding may be swallowed, and binding is where an over-budget
    /// query spends its time (ADR-0024). The latch is read at the
    /// bind-to-execute boundary, so a cancellation that the interrupt
    /// missed still prevents execution.
    #[must_use]
    pub fn cancellable<'a>(&'a self, cancel: &'a CancelLatch) -> Cancellable<'a> {
        Cancellable {
            executor: self,
            cancel,
            clock: PhaseClock::off(),
        }
    }

    /// `pins` types the search-stage comparisons (ADR-0011): the server's
    /// catalog snapshot, or an empty `FieldTypes` in embedded mode.
    ///
    /// A result past `max_rows` is refused ([`RowCap::Refuse`]); a lane
    /// that cuts instead goes through [`Cancellable::run_query`].
    ///
    /// Timestamps render in the wire's RFC 3339, in UTC.
    pub fn run_query(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        self.run_query_latched(
            dsl,
            source,
            pins,
            SourceOrigin::Corpus,
            RowCap::Refuse(max_rows),
            &CancelLatch::never(),
            &PhaseClock::off(),
            None,
        )
    }

    /// The query lane under `cancel` and `clock`, holding the statement's
    /// RE2 allowance on `re2` threads, or none for `None` ([`Cancellable`]).
    #[allow(clippy::too_many_arguments)]
    fn run_query_latched(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        origin: SourceOrigin,
        cap: RowCap,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
    ) -> Result<QueryResult, EngineError> {
        // The query's one ledger: its statement's RE2 allowance and its
        // Rust tail's patterns, held until the lane returns.
        let mut ledger = Preparer::new().with_stop(cancel.stop());
        let ast = clock
            .time(QueryPhase::Emit, || {
                parser::parse_cancellable(dsl, &cancel.stop())
            })?
            .map_err(EngineError::Parse)?;
        cancel.check()?;
        let resolved = clock.time(QueryPhase::Source, || self.resolve_source(source));
        cancel.check()?;
        // With no hot buffer to fall back to, the outcome policy returns
        // every failure the probe can have (see [`cold_action`]), so a
        // failed probe is the lane's answer as it stands.
        let columns = self.input_columns(&ast, &resolved, None, cancel, clock)?;
        let pins = origin.pins(pins, &columns.cold);
        let emitted = clock.time(QueryPhase::Emit, || {
            resolved.emit_cold(&ast, &pins, &columns, EvalContext::capture())
        })?;
        cancel.check()?;
        let outcome = self.read_emitted(emitted, cap, cancel, clock, re2, &mut ledger);
        // Same gate as the hot lanes, one column over: with no hot buffer to
        // fall back to, `HotOnly` is unreachable (see [`cold_action`]) — but a
        // "no files" answer over a source that still reaches files is the
        // silent cold-data drop either way (ADR-0008).
        let read = match self.cold_action_for(
            classify_query_outcome(outcome.as_ref().map(|read| &read.result)),
            &resolved,
            HotLane::Absent,
            clock,
        ) {
            ColdAction::ColdDataUnread => return Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome | ColdAction::HotOnly => outcome?,
        };
        clock.time(QueryPhase::Post, || {
            read.finish(&ast, cap, &mut ledger, cancel)
        })
    }

    /// Handles cold-start gracefully: when the cold source matches no file at
    /// all, falls back to querying just the hot buffer so events ingested
    /// before the first compaction are visible. A columnless union result is
    /// not taken as proof of that — it is checked against the evidence
    /// resolution gathered, because a source that still reaches files
    /// answering "no files" is a race, not an empty window (ADR-0008).
    /// `hot_pins` conforms the hot branch (pins ∩ snapshot keys); `pins`
    /// is the full catalog snapshot typing the comparisons — one
    /// interpretation per query (ADR-0011). `hot_columns` is what the
    /// snapshot carries (ADR-0062). `max_rows` refuses as in
    /// [`Self::run_query`].
    #[allow(clippy::too_many_arguments)]
    pub fn run_query_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        self.run_query_with_hot_latched(
            dsl,
            source,
            hot_source,
            hot_columns,
            hot_pins,
            pins,
            RowCap::Refuse(max_rows),
            &CancelLatch::never(),
            &PhaseClock::off(),
            None,
        )
    }

    /// The hot query lane under `cancel` and `clock`, holding each
    /// statement's RE2 allowance on `re2` threads, or none for `None`
    /// ([`Cancellable`]).
    #[allow(clippy::too_many_arguments)]
    fn run_query_with_hot_latched(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        cap: RowCap,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
    ) -> Result<QueryResult, EngineError> {
        // The query's one ledger, as on the cold lane. The hot-only
        // fallback's statement holds its allowance on it in place of the
        // union's, once the union's statement has dropped.
        let mut ledger = Preparer::new().with_stop(cancel.stop());
        let ast = clock
            .time(QueryPhase::Emit, || {
                parser::parse_cancellable(dsl, &cancel.stop())
            })?
            .map_err(EngineError::Parse)?;
        cancel.check()?;
        let resolved = clock.time(QueryPhase::Source, || self.resolve_source(source));
        cancel.check()?;
        let probed = self.input_columns(
            &ast,
            &resolved,
            Some((hot_columns, hot_pins)),
            cancel,
            clock,
        );
        // One anchor for the request. The hot-only fallback is the same
        // logical query reading a narrower source, so a second clock read
        // would let it answer a `now()` comparison differently from the
        // union attempt it replaces (ADR-0017 §3).
        let anchor = EvalContext::capture();
        // A failed probe is the union's failure: it takes the outcome
        // policy below the way the statement's own failure would.
        let (columns, outcome) = match probed {
            Ok(columns) => {
                let emitted = clock.time(QueryPhase::Emit, || {
                    resolved.emit_union(&ast, hot_source, hot_pins, pins, &columns, anchor)
                })?;
                cancel.check()?;
                let outcome = self.read_emitted(emitted, cap, cancel, clock, re2, &mut ledger);
                (Some(columns), outcome)
            }
            Err(e) => (None, Err(e)),
        };

        let read = match self.cold_action_for(
            classify_query_outcome(outcome.as_ref().map(|read| &read.result)),
            &resolved,
            HotLane::Present,
            clock,
        ) {
            ColdAction::HotOnly => {
                clock.fallback(Fallback::HotOnly);
                // Hot-only keeps both halves of the interpretation: the same
                // comparison pins, and the same hot-column conformance the
                // union's hot branch applies. Reading the raw ndjson would
                // let `read_json`'s inference type the columns, so
                // `status=200.0` over a VARCHAR-pinned field would match a
                // hot numeric `200` here and stop matching the moment a
                // parquet file appeared.
                let columns = hot_only_columns(columns.as_ref(), hot_columns);
                let hot_emitted = clock.time(QueryPhase::Emit, || {
                    emitter::emit_hot_only(&ast, hot_source, hot_pins, pins, &columns, anchor)
                })?;
                cancel.check()?;
                self.read_emitted(hot_emitted, cap, cancel, clock, re2, &mut ledger)?
            }
            ColdAction::ColdDataUnread => return Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome => outcome?,
        };
        clock.time(QueryPhase::Post, || {
            read.finish(&ast, cap, &mut ledger, cancel)
        })
    }

    /// Read an emitted query's SQL under the lane's cap, its timestamps in
    /// the text its Rust tail needs ([`sql_timestamp_text`]), with its RE2
    /// allowance held on `ledger` first ([`hold_re2`]).
    #[allow(clippy::too_many_arguments)]
    fn read_emitted(
        &self,
        emitted: EmittedQuery,
        cap: RowCap,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
        ledger: &mut Preparer,
    ) -> Result<SqlRead, EngineError> {
        hold_re2(&emitted, re2, ledger)?;
        let sql_text = sql_timestamp_text(&emitted);
        let sql_cap = cap.sql_read(!emitted.rust_stages.is_empty());
        let (result, timestamp_columns) =
            self.execute_emitted_tracked(&emitted, sql_cap, sql_text, cancel, clock)?;
        Ok(SqlRead {
            emitted,
            result,
            timestamp_columns,
        })
    }

    /// Execute a pre-emitted query (SQL + params) against `DuckDB`.
    ///
    /// Timestamps render in the wire's RFC 3339, in UTC.
    pub fn execute_emitted(
        &self,
        query: &EmittedQuery,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        self.execute_emitted_tracked(
            query,
            RowCap::Refuse(max_rows),
            TimestampText::Wire,
            &CancelLatch::never(),
            &PhaseClock::off(),
        )
        .map(|(result, _)| result)
    }

    /// [`Self::execute_emitted`], additionally reporting which result
    /// columns hold stored timestamps ([`TimestampColumns`]).
    ///
    /// Only the Rust-tail lane needs that: it compares the exact TIMESTAMP
    /// columns as instants, reads `DuckDB`'s cast text, and renders the
    /// wire's text afterwards ([`render_tracked_timestamps`]).
    fn execute_emitted_tracked(
        &self,
        query: &EmittedQuery,
        cap: RowCap,
        timestamps: TimestampText,
        cancel: &CancelLatch,
        clock: &PhaseClock,
    ) -> Result<(QueryResult, TimestampColumns), EngineError> {
        // Before the bind, so a cancellation that landed during the union's
        // bind stops the hot-only fallback from starting a second one.
        cancel.check()?;
        run_timechart_probes(&self.conn, &query.timechart_input_checks, cancel, clock)?;
        // Re-read after the probes: each one is a bind of its own, and
        // this is the last moment before the statement's.
        cancel.check()?;
        // Every lane's SQL read reaches `DuckDB` here — cold, hot union
        // and hot-only — so this is the one place a `Truncate` becomes a
        // `LIMIT`.
        let statement = cap.statement(&query.sql);
        clock.bind_attempt();
        let prepared = clock.time(QueryPhase::Bind, || {
            prepare_statement(&self.conn, &statement)
        });
        let mut stmt = match prepared {
            Ok(s) => s,
            Err(e) if is_no_files_error(&e) => {
                return Ok((QueryResult::empty(), TimestampColumns::default()));
            }
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let params = bind_params(&query.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        // The bind-to-execute boundary (ADR-0024). A long bind is where an
        // interrupt is most likely to be swallowed, so the latch is read
        // here rather than trusting that one to have survived every
        // `DuckDB` API phase.
        cancel.check()?;

        let _execute = clock.guard(QueryPhase::Execute);

        // start query execution — column metadata is only available
        // after DuckDB resolves table-valued functions like read_parquet()
        let mut result_rows = match stmt.query(param_refs.as_slice()) {
            Ok(r) => r,
            Err(e) if is_no_files_error(&e) => {
                return Ok((QueryResult::empty(), TimestampColumns::default()));
            }
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let stmt_ref =
            result_rows
                .as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        // Typed before the first row is read, so a zero-row or all-NULL
        // column is typed from execution too.
        let columns = typed_columns(stmt_ref);
        let instants = exact_timestamp_columns(stmt_ref);
        let col_count = columns.len();

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            match cap {
                RowCap::Refuse(max_rows) if rows.len() >= max_rows => {
                    return Err(EngineError::ResultTooLarge(max_rows));
                }
                // The `LIMIT` above already stops `DuckDB` here; this
                // holds the cap for the unbounded shape it leaves off.
                RowCap::Truncate { rows: keep, .. } if rows.len() >= keep => break,
                _ => {}
            }
            let mut cells = Vec::with_capacity(col_count);
            for (i, column) in columns.iter().enumerate() {
                cells.push(extract_value(row, i, &column.name, timestamps)?);
            }
            rows.push(cells);
        }

        let timestamp_columns = TimestampColumns {
            rendered: columns
                .iter()
                .filter(|col| holds_timestamps(&col.ty))
                .map(|col| col.name.clone())
                .collect(),
            instants,
        };

        Ok((QueryResult { columns, rows }, timestamp_columns))
    }

    /// Resolve a source argument against the files actually on disk — once,
    /// before anything is read (ADR-0008).
    ///
    /// The matcher is split by element shape, for cost: a `glob()` round trip
    /// through `DuckDB` costs roughly a millisecond, and a service-pinned
    /// window is all literals (`.../{HH}/nginx.parquet`, one element per
    /// hour), so a 30-day query would pay ~700ms just to resolve. A literal
    /// element — no `*`, `?` or `[` — is answered by a single
    /// [`std::fs::metadata`] call instead. Patterns go through `glob()`.
    ///
    /// The two halves must agree on what "matches" means, because both feed
    /// the same no-silent-cold-drop verdict; `fs_matcher_agrees_with_duckdb_glob_on_literals`
    /// is the drift guard.
    fn resolve_source(&self, source: &str) -> ResolvedSource {
        resolve_list_source(source, |pattern| {
            if has_glob_meta(pattern) {
                self.glob_has_match(pattern)
            } else {
                literal_path_is_file(pattern)
            }
        })
    }

    /// What the request's input carries, described at most once before
    /// emission (ADR-0062).
    ///
    /// The description is used for the whole request, the hot-only cold start
    /// included ([`InputColumns::hot_only`]), and never kept past it.
    fn input_columns(
        &self,
        ast: &Query,
        resolved: &ResolvedSource,
        hot: Option<(&HotColumns, &FieldTypes)>,
        cancel: &CancelLatch,
        clock: &PhaseClock,
    ) -> Result<InputColumns, EngineError> {
        let hot_columns = hot.map(|(columns, _)| columns.clone());
        if !needs_input_columns(ast, hot) {
            return Ok(InputColumns {
                cold: ColdColumns::Unprobed,
                hot: hot_columns,
            });
        }
        // A pipeline the emitter refuses never reaches `DuckDB`, the probe
        // included: the expansion budget is checked before any statement
        // binds (ADR-0024).
        emitter::validate_pipeline(&ast.pipeline)?;
        let cold = if resolved.evidence == ListEvidence::NoneMatching {
            ColdColumns::NoFiles
        } else {
            let reader = emitter::source_reader(&resolved.sql)?;
            let described = clock.time(QueryPhase::Probe, || self.describe_reader(&reader, cancel));
            cancel.check()?;
            described?
        };
        Ok(InputColumns {
            cold,
            hot: hot_columns,
        })
    }

    fn describe_reader(
        &self,
        reader: &str,
        cancel: &CancelLatch,
    ) -> Result<ColdColumns, EngineError> {
        let sql = format!("DESCRIBE SELECT * FROM {reader}");
        cancel.check()?;
        #[cfg(any(test, feature = "test-support"))]
        schema_probe::record(&sql);
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => return describe_failure(e),
        };
        #[cfg(any(test, feature = "test-support"))]
        schema_probe::park(&sql);
        cancel.check()?;
        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(e) => return describe_failure(e),
        };
        let mut columns: Vec<(String, String)> = Vec::new();
        while let Some(row) = rows.next()? {
            columns.push((row.get(0)?, row.get(1)?));
        }
        #[cfg(any(test, feature = "test-support"))]
        schema_probe::claim(&sql, &mut columns);
        Ok(ColdColumns::described(columns))
    }

    /// Whether the resolved source has any concrete files behind it.
    ///
    /// A list source already carries its evidence from resolution, so this
    /// never re-globs: a file that matched at resolution and vanished before
    /// the read must surface as [`EngineError::ColdDataUnread`], not be
    /// silently re-resolved away.
    fn cold_presence(&self, resolved: &ResolvedSource) -> ColdPresence {
        match resolved.evidence {
            ListEvidence::NotAList => {
                if self.glob_has_match(&resolved.sql) {
                    ColdPresence::Present
                } else {
                    ColdPresence::Absent
                }
            }
            ListEvidence::Matching => ColdPresence::Present,
            ListEvidence::NoneMatching => ColdPresence::Absent,
        }
    }

    /// Route a classified outcome through [`cold_action`], paying the
    /// cold-file presence check only where the table's verdict depends on it.
    fn cold_action_for(
        &self,
        outcome: HotColdOutcome,
        resolved: &ResolvedSource,
        hot: HotLane,
        clock: &PhaseClock,
    ) -> ColdAction {
        let cold = if outcome.consults_cold_presence(hot) {
            // A plain glob is globbed here, lazily: file discovery, so
            // `source` (ADR-0046).
            clock.time(QueryPhase::Source, || self.cold_presence(resolved))
        } else {
            // The table answers the same for both values here, so the
            // argument is inert — and the happy path never globs.
            ColdPresence::Absent
        };
        cold_action(outcome, cold, hot)
    }

    /// Whether `pattern` matches at least one file on disk. Errs on the side
    /// of "matches" when the `glob` call itself fails.
    fn glob_has_match(&self, pattern: &str) -> bool {
        self.conn
            .query_row("SELECT count(*)::BIGINT FROM glob(?)", [pattern], |row| {
                row.get::<_, i64>(0)
            })
            .map_or(true, |n| n > 0)
    }

    /// Describe the schema without reading row data — embedded mode's schema
    /// introspection (`trawl schema fields --data`).
    ///
    /// Errors propagate rather than degrading an irreconcilable cross-file
    /// mix to `VARCHAR`: every trawl-written file conforms to the field
    /// catalog at write time, so a union conflict can only mean
    /// foreign/nonconformant parquet (ADR-0009).
    pub fn describe_schema(&self, source: &str) -> Result<SchemaResult, EngineError> {
        // Validate source path before interpolation — DuckDB doesn't truly
        // parameterize table-valued function arguments.
        emitter::validate_source_path(source)?;

        let mut stmt = self
            .conn
            .prepare("DESCRIBE SELECT * FROM read_parquet(?, union_by_name=true)")?;
        let mut rows = stmt.query([source])?;

        let mut columns = Vec::new();
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let data_type: String = row.get(1)?;
            columns.push(SchemaColumn { name, data_type });
        }

        let file_count: i64 =
            self.conn
                .query_row("SELECT count(*)::BIGINT FROM glob(?)", [source], |row| {
                    row.get(0)
                })?;

        Ok(SchemaResult {
            columns,
            file_count: u64::try_from(file_count).unwrap_or(0),
        })
    }

    /// Sample distinct values for a field (for autocomplete).
    pub fn sample_field_values(
        &self,
        source: &str,
        field: &str,
        limit: usize,
    ) -> Result<Vec<String>, EngineError> {
        if !field
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return Err(EngineError::Emit(
                trawl_core::emitter::EmitError::UnsupportedOperation {
                    message: format!("invalid field name: {field}"),
                },
            ));
        }

        trawl_core::emitter::validate_source_path(source)?;

        let sql = format!(
            r#"SELECT DISTINCT "{field}" FROM read_parquet(?, union_by_name=true)
               WHERE "{field}" IS NOT NULL
               ORDER BY 1
               LIMIT ?"#
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = stmt.query(duckdb::params![source, limit_i64])?;

        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            if let Ok(val) = row.get::<_, String>(0) {
                values.push(val);
            }
        }

        Ok(values)
    }

    /// Export query results directly to a Parquet file via `DuckDB` `COPY TO`.
    ///
    /// `max_rows` is a two-mode parameter: a row cap, or `usize::MAX` —
    /// the caller-facing sentinel for an unbounded export, which emits no
    /// LIMIT at all. Any cap above `DuckDB`'s INT64 LIMIT domain reads as
    /// the sentinel too, since a larger literal is a conversion error.
    /// Both halves are the one decision `export_row_limit` takes, on the
    /// `usize` itself — so the contract holds at every pointer width.
    pub fn export_parquet(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<usize, EngineError> {
        self.export_parquet_latched(
            dsl,
            source,
            pins,
            output_path,
            max_rows,
            &CancelLatch::never(),
            &PhaseClock::off(),
            None,
        )
    }

    /// The export lane under `cancel` and `clock`, holding the statement's
    /// RE2 allowance on `re2` threads, or none for `None` ([`Cancellable`]).
    #[allow(clippy::too_many_arguments)]
    fn export_parquet_latched(
        &self,
        dsl: &str,
        source: &str,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
    ) -> Result<usize, EngineError> {
        // The export's one ledger: its statement's RE2 allowance, held
        // until the lane returns.
        let mut ledger = Preparer::new().with_stop(cancel.stop());
        let ast = clock
            .time(QueryPhase::Emit, || {
                parser::parse_cancellable(dsl, &cancel.stop())
            })?
            .map_err(EngineError::Parse)?;
        cancel.check()?;
        let resolved = clock.time(QueryPhase::Source, || self.resolve_source(source));
        cancel.check()?;
        // As on the query lane: with no hot buffer, a failed probe is the
        // lane's answer as it stands.
        let columns = self.input_columns(&ast, &resolved, None, cancel, clock)?;
        let emitted = clock.time(QueryPhase::Emit, || {
            resolved.emit_cold(&ast, pins, &columns, EvalContext::capture())
        })?;
        cancel.check()?;
        let outcome = self.export_parquet_from_emitted(
            &emitted,
            output_path,
            max_rows,
            cancel,
            clock,
            re2,
            &mut ledger,
        );
        // The same gate the query lanes run, on the same classification. An
        // all-missing source keeps surfacing DuckDB's own "no files" error
        // here — deliberately loud, since there is no empty answer an export
        // could write — but a source that still reaches files must not fail
        // that way (ADR-0008).
        match self.cold_action_for(
            classify_export_outcome(&outcome),
            &resolved,
            HotLane::Absent,
            clock,
        ) {
            // Discarding `outcome`'s error text is safe by construction:
            // `classify_export_outcome` maps only `is_no_files_error` errors
            // to `NoColumns` (every other `Database` error classifies
            // `Recoverable` and propagates verbatim), so the error dropped
            // here is always the known "no files match" message — which says
            // strictly less than `ColdDataUnread`.
            ColdAction::ColdDataUnread => Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome | ColdAction::HotOnly => outcome,
        }
    }

    /// Export with hot buffer union, falling back to hot-only on cold start,
    /// and return the number of rows written.
    #[allow(clippy::too_many_arguments)]
    pub fn export_parquet_with_hot(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
    ) -> Result<usize, EngineError> {
        self.export_parquet_with_hot_latched(
            dsl,
            source,
            hot_source,
            hot_columns,
            hot_pins,
            pins,
            output_path,
            max_rows,
            &CancelLatch::never(),
            &PhaseClock::off(),
            None,
        )
    }

    /// The hot export lane under `cancel` and `clock`, holding each
    /// statement's RE2 allowance on `re2` threads, or none for `None`
    /// ([`Cancellable`]).
    #[allow(clippy::too_many_arguments)]
    fn export_parquet_with_hot_latched(
        &self,
        dsl: &str,
        source: &str,
        hot_source: &str,
        hot_columns: &HotColumns,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        output_path: &Path,
        max_rows: usize,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
    ) -> Result<usize, EngineError> {
        // The export's one ledger, as on the cold lane.
        let mut ledger = Preparer::new().with_stop(cancel.stop());
        let ast = clock
            .time(QueryPhase::Emit, || {
                parser::parse_cancellable(dsl, &cancel.stop())
            })?
            .map_err(EngineError::Parse)?;
        cancel.check()?;
        let resolved = clock.time(QueryPhase::Source, || self.resolve_source(source));
        cancel.check()?;
        let probed = self.input_columns(
            &ast,
            &resolved,
            Some((hot_columns, hot_pins)),
            cancel,
            clock,
        );
        // One anchor, as on the query lane (ADR-0017 §3).
        let anchor = EvalContext::capture();
        // A failed probe is the union's failure, as on the query lane.
        let (columns, outcome) = match probed {
            Ok(columns) => {
                let emitted = clock.time(QueryPhase::Emit, || {
                    resolved.emit_union(&ast, hot_source, hot_pins, pins, &columns, anchor)
                })?;
                cancel.check()?;
                let outcome = self.export_parquet_from_emitted(
                    &emitted,
                    output_path,
                    max_rows,
                    cancel,
                    clock,
                    re2,
                    &mut ledger,
                );
                (Some(columns), outcome)
            }
            Err(e) => (None, Err(e)),
        };

        match self.cold_action_for(
            classify_export_outcome(&outcome),
            &resolved,
            HotLane::Present,
            clock,
        ) {
            ColdAction::HotOnly => {
                clock.fallback(Fallback::HotOnly);
                // Hot-only, conformed like the union's hot branch — an
                // export must not write JSON-inferred types where the
                // hot+cold lane would have written the catalog's.
                let columns = hot_only_columns(columns.as_ref(), hot_columns);
                let hot_emitted = clock.time(QueryPhase::Emit, || {
                    emitter::emit_hot_only(&ast, hot_source, hot_pins, pins, &columns, anchor)
                })?;
                cancel.check()?;
                self.export_parquet_from_emitted(
                    &hot_emitted,
                    output_path,
                    max_rows,
                    cancel,
                    clock,
                    re2,
                    &mut ledger,
                )
            }
            // Same invariant as `export_parquet`: only an `is_no_files_error`
            // reaches `NoColumns`, so the discarded error text is always that
            // known message and never a distinct diagnosis.
            ColdAction::ColdDataUnread => Err(EngineError::ColdDataUnread),
            ColdAction::ReturnOutcome => outcome,
        }
    }

    /// Internal: stage an emitted query into a temp table and COPY to
    /// parquet, with its RE2 allowance held on `ledger` first
    /// ([`hold_re2`]).
    #[allow(clippy::too_many_arguments)]
    fn export_parquet_from_emitted(
        &self,
        emitted: &EmittedQuery,
        output_path: &Path,
        max_rows: usize,
        cancel: &CancelLatch,
        clock: &PhaseClock,
        re2: Option<NonZeroU32>,
        ledger: &mut Preparer,
    ) -> Result<usize, EngineError> {
        // Same rule as the query lane: no second bind starts once the
        // caller has asked for this work to stop.
        cancel.check()?;
        hold_re2(emitted, re2, ledger)?;
        // And the same bucket-source refusal, for the same reason: an
        // export writes its answer to a file, where a wrong bucket is
        // harder to notice than on screen, not easier.
        run_timechart_probes(&self.conn, &emitted.timechart_input_checks, cancel, clock)?;
        if !emitted.rust_stages.is_empty() {
            return Err(EngineError::Emit(
                trawl_core::emitter::EmitError::UnsupportedOperation {
                    message: "parquet export is not supported for queries with post-processing stages (e.g. extract kv)".into(),
                },
            ));
        }

        let path_str = output_path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "output path is not valid UTF-8".into(),
            })
        })?;

        // Which shape the staging SELECT takes is one decision,
        // `export_row_limit`, read off the caller's `usize` before any
        // narrowing conversion.
        let create_sql = match export_row_limit(max_rows) {
            Some(max_rows) => format!(
                "CREATE TEMP TABLE __trawl_export AS (SELECT * FROM ({}) LIMIT {max_rows})",
                emitted.sql
            ),
            None => format!(
                "CREATE TEMP TABLE __trawl_export AS (SELECT * FROM ({}))",
                emitted.sql
            ),
        };

        let params = bind_params(&emitted.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

        let cleanup = |conn: &Connection| {
            let _ = conn.execute_batch("DROP TABLE IF EXISTS __trawl_export");
        };

        // Same re-read as the query lane, for the same reason: the
        // probes above each bound a statement, and the staging SELECT
        // is about to bind another.
        cancel.check()?;
        clock.bind_attempt();
        let prepared = clock.time(QueryPhase::Bind, || {
            prepare_statement(&self.conn, &create_sql)
        });
        let rows = match prepared {
            Ok(mut stmt) => {
                // The export's bind-to-execute boundary: the staging
                // SELECT is the whole query, so this is the same bind the
                // query lane pays and the same recovery applies.
                if cancel.latched() {
                    cleanup(&self.conn);
                    return Err(EngineError::Cancelled);
                }
                // Staging is the export's `execute` (ADR-0046). The
                // staged table holds exactly the rows the COPY writes, so
                // its count is the export's row count; a prepared CTAS
                // reports no changed rows to count instead.
                let staged = clock.time(QueryPhase::Execute, || {
                    stmt.execute(param_refs.as_slice())?;
                    self.conn
                        .query_row("SELECT count(*) FROM __trawl_export", [], |row| {
                            row.get::<_, usize>(0)
                        })
                });
                match staged {
                    Ok(rows) => rows,
                    Err(e) => {
                        cleanup(&self.conn);
                        return Err(staging_failure(e));
                    }
                }
            }
            Err(e) => {
                cleanup(&self.conn);
                return Err(staging_failure(e));
            }
        };

        // Staging and COPY are separate statements. A cancellation that
        // arrived during staging must stop here, even if staging succeeded.
        if cancel.latched() {
            cleanup(&self.conn);
            return Err(EngineError::Cancelled);
        }

        // COPY to parquet. DuckDB's COPY TO doesn't support parameterized
        // paths, so we escape single quotes by doubling them (DuckDB convention).
        let safe_path = path_str.replace('\'', "''");
        let copy_sql =
            format!("COPY __trawl_export TO '{safe_path}' (FORMAT PARQUET, COMPRESSION SNAPPY)");
        // The COPY binds its own statement, opaquely, so the whole of it
        // and its cleanup is `copy` and never `bind` (ADR-0046).
        clock.time(QueryPhase::Copy, || {
            if let Err(e) = self.conn.execute_batch(&copy_sql) {
                cleanup(&self.conn);
                let _ = std::fs::remove_file(output_path);
                return Err(e.into());
            }
            cleanup(&self.conn);
            Ok(rows)
        })
    }

    /// Write an in-memory [`QueryResult`] to a Parquet file, every column
    /// in an exact type, and return the number of rows written.
    ///
    /// Refused with [`EngineError::NotParquetStorable`] when the result has
    /// no columns, a `mixed` column, or an `integer` column holding both a
    /// negative and a value above `i64::MAX` (see [`StoredType`]).
    pub fn write_query_result_to_parquet(
        &self,
        result: &QueryResult,
        path: &Path,
    ) -> Result<usize, EngineError> {
        if result.columns.is_empty() {
            return Err(EngineError::NotParquetStorable {
                reason: "it has no columns".to_owned(),
            });
        }
        let stored = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                StoredType::of(&column.ty, result.rows.iter().map(|row| &row[index])).map_err(
                    |why| EngineError::NotParquetStorable {
                        reason: format!("column {:?} {why}", column.name),
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        let path_str = path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "output path is not valid UTF-8".into(),
            })
        })?;

        let tmp_json = format!("{path_str}.ndjson.tmp");
        let written = write_wire_ndjson(result, Path::new(&tmp_json));
        let copy_result = written.and_then(|()| {
            // The keys are positional (`c0`, `c1`, ...), so neither a
            // column name nor a duplicate one reaches the `columns` struct.
            let declared = stored
                .iter()
                .enumerate()
                .map(|(index, ty)| format!("c{index}: '{}'", ty.text_sql()))
                .collect::<Vec<_>>()
                .join(", ");
            let projected = stored
                .iter()
                .zip(&result.columns)
                .enumerate()
                .map(|(index, (ty, column))| {
                    format!(
                        "{} AS \"{}\"",
                        ty.cast_text(&format!("c{index}")),
                        column.name.replace('"', "\"\"")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let safe_json = tmp_json.replace('\'', "''");
            let safe_path = path_str.replace('\'', "''");
            let copy_sql = format!(
                "COPY (SELECT {projected} FROM read_json('{safe_json}', \
                 format='newline_delimited', records=true, auto_detect=false, \
                 columns={{{declared}}})) \
                 TO '{safe_path}' (FORMAT PARQUET, COMPRESSION SNAPPY)"
            );
            Ok(self.conn.execute_batch(&copy_sql)?)
        });

        let _ = std::fs::remove_file(&tmp_json);

        copy_result?;
        Ok(result.rows.len())
    }

    /// Read a Parquet file into a [`QueryResult`].
    pub fn read_parquet_to_result(
        &self,
        path: &Path,
        max_rows: usize,
    ) -> Result<QueryResult, EngineError> {
        let path_str = path.to_str().ok_or_else(|| {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "parquet path is not valid UTF-8".into(),
            })
        })?;

        let safe_path = path_str.replace('\'', "''");
        // The same INT64 LIMIT domain the export lane clamps against: a
        // larger `usize` is a Conversion Error, not a bigger cap. There is
        // no unbounded shape here — the caller's `max_result_rows` is
        // always a cap — so the ceiling is `i64::MAX`.
        let max_rows = i64::try_from(max_rows).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT * FROM read_parquet('{safe_path}', union_by_name=true) LIMIT {max_rows}"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let mut result_rows = stmt.query([])?;

        let stmt_ref =
            result_rows
                .as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let columns = typed_columns(stmt_ref);

        let mut rows = Vec::new();
        while let Some(row) = result_rows.next()? {
            let mut cells = Vec::with_capacity(columns.len());
            for (i, column) in columns.iter().enumerate() {
                cells.push(extract_value(row, i, &column.name, TimestampText::Wire)?);
            }
            rows.push(cells);
        }

        Ok(QueryResult { columns, rows })
    }

    /// Write a captured result (ADR-0057) to the parquet file at `dest`,
    /// every row in the result's order, and say how each column is stored.
    ///
    /// The file's columns are named by position, `c0`, `c1`, ..., so a
    /// display name, a duplicate one included, never reaches it: names and
    /// types live in the capture's metadata. A column `StoredType` refuses
    /// is stored as `VARCHAR` holding each cell's wire JSON instead, and is
    /// [`StoredAs::WireText`].
    ///
    /// `COPY` orders the staged rows by their index `r`, so the file's order
    /// is the result's whatever order `read_json` reads in.
    ///
    /// `dest` is a file the caller made, 0600 in the results root. `COPY`
    /// writes into it in place (`USE_TMP_FILE false`), truncating it first,
    /// so its inode and mode survive: `DuckDB`'s default would write a
    /// `tmp_` sibling and rename it over `dest`, a new inode with the
    /// umask's mode. A `dest` that does not exist is created under the
    /// umask. The file is `fsync`ed before this returns; the directory is
    /// the caller's to sync once it renames the file. On an error `dest`
    /// may hold part of a file, and is the caller's to remove.
    ///
    /// Refused with [`EngineError::NotParquetStorable`] when the result has
    /// no columns: a parquet file holds at least one.
    pub fn write_capture_parquet(
        &self,
        result: &QueryResult,
        dest: &Path,
    ) -> Result<CaptureFile, EngineError> {
        if result.columns.is_empty() {
            return Err(EngineError::NotParquetStorable {
                reason: "it has no columns".to_owned(),
            });
        }
        // A ragged row is the encoder's to refuse, below, rather than a
        // panic here.
        let stored: Vec<Option<StoredType>> = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                StoredType::of(
                    &column.ty,
                    result.rows.iter().filter_map(|row| row.get(index)),
                )
                .ok()
            })
            .collect();
        let stored_as: Vec<StoredAs> = stored
            .iter()
            .map(|ty| {
                if ty.is_some() {
                    StoredAs::Native
                } else {
                    StoredAs::WireText
                }
            })
            .collect();
        let dest_str = utf8_path(dest, "capture path")?;

        let stage = self.spill.stage_file()?;
        let stage_str = read_path(stage.path(), "capture staging path")?;
        write_capture_ndjson(result, &stored_as, stage.as_file())?;
        let declared = std::iter::once("r: 'UBIGINT'".to_owned())
            .chain(stored.iter().enumerate().map(|(index, ty)| {
                let text = ty
                    .as_ref()
                    .map_or_else(|| "VARCHAR".to_owned(), StoredType::text_sql);
                format!("c{index}: '{text}'")
            }))
            .collect::<Vec<_>>()
            .join(", ");
        let projected = stored
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let key = format!("c{index}");
                let value = ty
                    .as_ref()
                    .map_or_else(|| key.clone(), |ty| ty.cast_text(&key));
                format!("{value} AS {key}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let copy_sql = format!(
            "COPY (SELECT {projected} FROM read_json('{}', format='newline_delimited', \
             records=true, auto_detect=false, columns={{{declared}}}) ORDER BY r) \
             TO '{}' (FORMAT PARQUET, COMPRESSION SNAPPY, \
             ROW_GROUP_SIZE {CAPTURE_ROW_GROUP_ROWS}, USE_TMP_FILE false)",
            stage_str.replace('\'', "''"),
            dest_str.replace('\'', "''"),
        );
        let copied = self.conn.execute(&copy_sql, []);
        drop(stage);
        let copied = copied?;
        if copied != result.rows.len() {
            return Err(EngineError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the capture copy wrote {copied} rows of {}",
                    result.rows.len()
                ),
            )));
        }

        let file = std::fs::File::open(dest)?;
        file.sync_all()?;
        let bytes = file.metadata()?.len();
        Ok(CaptureFile {
            stored_as,
            rows: u64::try_from(copied).unwrap_or(u64::MAX),
            bytes,
        })
    }

    /// Read rows `offset` up to `offset + limit` of the captured result in
    /// the file at `path`, the columns at `positions` in that order, a
    /// repeated position repeated.
    ///
    /// A row is its 0-based position in the file, which is the result's
    /// order: the rows are selected by `file_row_number` and ordered by it,
    /// never by scan order.
    ///
    /// `path` must not carry a glob metacharacter, because `read_parquet`
    /// expands one even in a bound path.
    pub fn read_capture_page(
        &self,
        path: &Path,
        columns: &[ColumnType],
        stored_as: &[StoredAs],
        positions: &[usize],
        offset: u64,
        limit: u64,
    ) -> Result<Vec<Vec<Value>>, EngineError> {
        if columns.len() != stored_as.len() {
            return Err(invalid_input(format!(
                "a capture of {} columns has {} storage entries",
                columns.len(),
                stored_as.len()
            )));
        }
        let selected = positions
            .iter()
            .map(|&position| {
                columns
                    .get(position)
                    .map(|ty| Selected {
                        position,
                        ty,
                        stored_as: stored_as[position],
                    })
                    .ok_or_else(|| {
                        invalid_input(format!(
                            "column position {position} is past the capture's {} columns",
                            columns.len()
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.read_capture_rows(path, &selected, offset, limit)
    }

    /// Read one cell of the captured result in the file at `path`: row
    /// `row`, the column at `position`, recorded as `column` and stored as
    /// `stored_as`. `None` past the last row.
    ///
    /// `position` must be a column of the capture, checked against
    /// its metadata by the caller.
    pub fn read_capture_cell(
        &self,
        path: &Path,
        column: &ColumnType,
        stored_as: StoredAs,
        row: u64,
        position: usize,
    ) -> Result<Option<Value>, EngineError> {
        let selected = [Selected {
            position,
            ty: column,
            stored_as,
        }];
        let mut rows = self.read_capture_rows(path, &selected, row, 1)?;
        Ok(rows.pop().and_then(|mut cells| cells.pop()))
    }

    fn read_capture_rows(
        &self,
        path: &Path,
        selected: &[Selected<'_>],
        from: u64,
        limit: u64,
    ) -> Result<Vec<Vec<Value>>, EngineError> {
        let path_str = read_path(path, "capture path")?;
        let names: Vec<String> = selected
            .iter()
            .map(|column| format!("c{}", column.position))
            .collect();
        // With no column chosen the page still has its rows, each empty.
        let projection = if names.is_empty() {
            "file_row_number".to_owned()
        } else {
            names.join(", ")
        };
        let sql = format!(
            "SELECT {projection} FROM read_parquet($1, file_row_number = true) \
             WHERE file_row_number >= $2 ORDER BY file_row_number LIMIT $3"
        );
        // `file_row_number` is a BIGINT, so past `i64::MAX` there is no row.
        let from = i64::try_from(from).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(duckdb::params![path_str, from, limit])?;
        let stmt_ref =
            rows.as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let found = typed_columns(stmt_ref);
        for (column, found) in selected.iter().zip(&found) {
            let recorded = match column.stored_as {
                StoredAs::Native => column.ty.clone(),
                StoredAs::WireText => ColumnType::String,
            };
            if found.ty != recorded {
                return Err(invalid_data(format!(
                    "the captured column c{} is stored as {}, not as the recorded {recorded}",
                    column.position, found.ty
                )));
            }
        }

        let mut page = Vec::new();
        let mut texts: Vec<(usize, Vec<Option<String>>)> = selected
            .iter()
            .enumerate()
            .filter(|(_, column)| column.stored_as == StoredAs::WireText)
            .map(|(index, _)| (index, Vec::new()))
            .collect();
        while let Some(row) = rows.next()? {
            let mut cells = Vec::with_capacity(selected.len());
            for (index, (column, name)) in selected.iter().zip(&names).enumerate() {
                cells.push(match column.stored_as {
                    StoredAs::Native => extract_value(row, index, name, TimestampText::Wire)?,
                    StoredAs::WireText => Value::Null,
                });
            }
            for (index, column_texts) in &mut texts {
                column_texts.push(row.get::<_, Option<String>>(*index)?);
            }
            page.push(cells);
        }
        for (index, column_texts) in texts {
            let column = &selected[index];
            let landed = land_wire_texts(column.position, column.ty, column_texts)?;
            for (cells, cell) in page.iter_mut().zip(landed) {
                cells[index] = cell;
            }
        }
        Ok(page)
    }
}

/// The rows of one row group in a captured result's file: one `DuckDB`
/// vector. A capture holds at most a few thousand rows (ADR-0057), so a
/// page of them decodes one row group or two rather than the whole file:
/// `file_row_number`'s statistics skip the groups before the page.
pub const CAPTURE_ROW_GROUP_ROWS: usize = 2048;

/// How a captured result's column is stored in its parquet file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredAs {
    /// As its [`StoredType`], the column's own type exactly.
    Native,
    /// As `VARCHAR` holding each cell's result-wire JSON, for a column no
    /// [`StoredType`] holds exactly: a `mixed` one, or integers that need
    /// both a negative and the range above `i64::MAX`.
    WireText,
}

/// What [`Executor::write_capture_parquet`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFile {
    pub stored_as: Vec<StoredAs>,
    pub rows: u64,
    /// The file's length in bytes once synced.
    pub bytes: u64,
}

/// A captured column a read selects.
struct Selected<'a> {
    /// Its position, which names it in the file (`c3`).
    position: usize,
    ty: &'a ColumnType,
    stored_as: StoredAs,
}

/// `path` as UTF-8, which every path spliced into `DuckDB` text must be.
fn utf8_path<'a>(path: &'a Path, what: &str) -> Result<&'a str, EngineError> {
    path.to_str().ok_or_else(|| {
        EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
            message: format!("the {what} is not valid UTF-8"),
        })
    })
}

/// The characters `DuckDB` reads as a glob pattern in a path it reads from,
/// the set `trawl-config` refuses in every directory setting.
const GLOB_METACHARACTERS: &[char] = &['*', '?', '[', ']', '{', '}'];

/// `path` as UTF-8, for `read_json` or `read_parquet` to read exactly that
/// file: both expand a glob metacharacter, bound or spliced, so a path
/// with one could read another file or several.
fn read_path<'a>(path: &'a Path, what: &str) -> Result<&'a str, EngineError> {
    let text = utf8_path(path, what)?;
    if let Some(found) = text.chars().find(|c| GLOB_METACHARACTERS.contains(c)) {
        return Err(invalid_input(format!(
            "the {what} carries the glob character '{found}', which DuckDB would expand"
        )));
    }
    Ok(text)
}

fn invalid_input(message: String) -> EngineError {
    EngineError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn invalid_data(message: String) -> EngineError {
    EngineError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

fn land_wire_texts(
    position: usize,
    ty: &ColumnType,
    texts: Vec<Option<String>>,
) -> Result<Vec<Value>, EngineError> {
    let rows = texts
        .into_iter()
        .map(|text| {
            text.map_or(Ok(serde_json::Value::Null), |text| {
                serde_json::from_str(&text)
            })
            .map(|cell| serde_json::Value::Array(vec![cell]))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            invalid_data(format!(
                "the captured column c{position} holds a text that is not JSON: {e}"
            ))
        })?;
    let column = serde_json::to_value(Column::new(format!("c{position}"), ty.clone()))
        .map_err(|e| invalid_data(format!("the type of captured column c{position}: {e}")))?;
    let wire = serde_json::json!({ "columns": [column], "rows": rows });
    let landed: QueryResult = serde_json::from_value(wire).map_err(|e| {
        invalid_data(format!(
            "the captured column c{position} holds a cell its type does not admit: {e}"
        ))
    })?;
    Ok(landed
        .rows
        .into_iter()
        .map(|mut cells| cells.pop().unwrap_or(Value::Null))
        .collect())
}

fn write_capture_ndjson(
    result: &QueryResult,
    stored_as: &[StoredAs],
    file: &std::fs::File,
) -> Result<(), EngineError> {
    use std::io::Write as _;

    let mut out = std::io::BufWriter::new(file);
    for (index, cells) in wire_rows(result)?.into_iter().enumerate() {
        let mut record = serde_json::Map::new();
        record.insert("r".to_owned(), index.into());
        for (position, (cell, stored_as)) in cells.into_iter().zip(stored_as).enumerate() {
            let cell = match (stored_as, cell) {
                (_, serde_json::Value::Null) => serde_json::Value::Null,
                (StoredAs::Native, cell) => cell,
                (StoredAs::WireText, cell) => serde_json::Value::String(cell.to_string()),
            };
            record.insert(format!("c{position}"), cell);
        }
        // A failed write inside the encoder is the staging file's fault,
        // not the result's: it answers as the I/O error it is, whose public
        // message is fixed, rather than as an encoding refusal quoting it.
        serde_json::to_writer(&mut out, &record).map_err(|e| {
            if e.is_io() {
                EngineError::Io(e.into())
            } else {
                unencodable(&e)
            }
        })?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn unencodable(e: &serde_json::Error) -> EngineError {
    EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
        message: format!("failed to encode the result: {e}"),
    })
}

fn wire_rows(result: &QueryResult) -> Result<Vec<Vec<serde_json::Value>>, EngineError> {
    let wire = serde_json::to_value(result).map_err(|e| unencodable(&e))?;
    let rows = match wire {
        serde_json::Value::Object(mut body) => body.remove("rows"),
        _ => None,
    };
    let Some(serde_json::Value::Array(rows)) = rows else {
        unreachable!("a QueryResult encodes as an object with a rows array");
    };
    Ok(rows
        .into_iter()
        .map(|row| {
            let serde_json::Value::Array(cells) = row else {
                unreachable!("a QueryResult row encodes as an array");
            };
            cells
        })
        .collect())
}

/// The cells are encoded by [`QueryResult`]'s own serde, so an integer is
/// a decimal string, a non-finite float is `NaN`, `Infinity` or
/// `-Infinity`, and a timestamp is RFC 3339 with its offset: text the
/// writer's casts read exactly. A cell its column's type refuses fails the
/// encoding, naming the column.
fn write_wire_ndjson(result: &QueryResult, path: &Path) -> Result<(), EngineError> {
    use std::io::Write as _;

    let rows = wire_rows(result)?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    for cells in rows {
        let record = cells
            .into_iter()
            .enumerate()
            .map(|(index, cell)| (format!("c{index}"), cell))
            .collect::<serde_json::Map<_, _>>();
        serde_json::to_writer(&mut file, &record).map_err(|e| {
            if e.is_io() {
                EngineError::Io(e.into())
            } else {
                unencodable(&e)
            }
        })?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
    Ok(())
}

/// Hold the RE2 allowance of `emitted`'s statement on `ledger` (ADR-0063).
///
/// Call it before the statement or any probe of it binds: `DuckDB`
/// compiles a regex at bind, then once more on every thread that runs the
/// statement.
fn hold_re2(
    emitted: &EmittedQuery,
    re2: Option<NonZeroU32>,
    ledger: &mut Preparer,
) -> Result<(), EngineError> {
    let Some(threads) = re2 else {
        return Ok(());
    };
    ledger
        .hold_re2(emitted.regex_sites.re2_allowance(threads))
        .map_err(EngineError::RegexResources)
}

/// A query lane's SQL read, with the emitted query that ran it: the tail
/// [`Self::finish`] applies is that query's.
struct SqlRead {
    emitted: EmittedQuery,
    result: QueryResult,
    timestamp_columns: TimestampColumns,
}

impl SqlRead {
    /// The query lanes' tail after the SQL read: the Rust stages and the
    /// wire rendering of their timestamp columns, the cap's cut, and the
    /// column reorder — the engine's share of `post` (ADR-0046).
    ///
    /// The Rust stages run under the split-point pins plus a Timestamp pin
    /// for each exact TIMESTAMP column of the SQL step that those pins
    /// leave unpinned (ADR-0011). A caller's pin wins.
    ///
    /// The Rust stages prepare their patterns through `ledger`, the
    /// query's, so they share its cap with the statement's RE2 allowance.
    fn finish(
        self,
        ast: &Query,
        cap: RowCap,
        ledger: &mut Preparer,
        cancel: &CancelLatch,
    ) -> Result<QueryResult, EngineError> {
        let Self {
            emitted,
            mut result,
            timestamp_columns,
        } = self;
        if !emitted.rust_stages.is_empty() {
            let mut pins = emitted.rust_stage_pins.clone();
            pins.fill_unpinned(
                timestamp_columns.instants.iter().map(String::as_str),
                CanonicalType::Timestamp,
            );
            result = crate::post_process::apply_rust_stages(
                result,
                &emitted.rust_stages,
                &pins,
                emitted.anchor,
                ast.search.time_filter.as_ref().map(|tf| tf.node.duration),
                ledger,
                cancel,
            )?;
            let tracked = tail_timestamp_scope(&timestamp_columns.rendered, &emitted.rust_stages);
            render_tracked_timestamps(&mut result, &tracked, cancel)?;
        }
        cap.trim(&mut result);
        if emitted.needs_column_reorder {
            result.reorder_log_columns();
        }
        Ok(result)
    }
}

/// The description a hot-only read emits with: the hot half of the
/// request's ([`InputColumns::hot_only`]). A probe that failed described
/// nothing, but a hot-only read happens only where no cold file is behind
/// the source, so the cold half is still none.
fn hot_only_columns(probed: Option<&InputColumns>, hot: &HotColumns) -> InputColumns {
    probed.map_or_else(
        || InputColumns {
            cold: ColdColumns::NoFiles,
            hot: Some(hot.clone()),
        },
        InputColumns::hot_only,
    )
}

/// Whether emitting `ast` over this input needs the input described
/// first (ADR-0062).
fn needs_input_columns(ast: &Query, hot: Option<(&HotColumns, &FieldTypes)>) -> bool {
    emitter::needs_input_columns(ast)
        || hot
            .is_some_and(|(columns, hot_pins)| emitter::hot_needs_input_columns(columns, hot_pins))
}

fn describe_failure(e: duckdb::Error) -> Result<ColdColumns, EngineError> {
    if is_no_files_error(&e) {
        Ok(ColdColumns::NoFiles)
    } else if is_binder_column_error(&e) {
        Err(remap_binder_error(&e))
    } else {
        Err(e.into())
    }
}

/// `DuckDB`'s "no matching files" message, carried by a
/// [`DUCKDB_IO_ERROR`] diagnostic. Matched as text, so a `DuckDB` upgrade has to re-check it;
/// the outcome-policy tests below provoke the real error against the
/// bundled build.
const DUCKDB_NO_FILES_MSG: &str = "No files found that match the pattern";
const DUCKDB_IO_ERROR: &str = "IO";
const DUCKDB_CONVERSION_ERROR: &str = "Conversion";

/// Reads the structured diagnostic: an `IO` exception whose message is
/// the no-files message. The same words under another type (a table named
/// after them, say) are not this error.
fn is_no_files_error(e: &duckdb::Error) -> bool {
    DuckDbDiagnostic::of(e).is_some_and(|d| {
        d.exception_type == DUCKDB_IO_ERROR && d.exception_message.contains(DUCKDB_NO_FILES_MSG)
    })
}

/// Read off the structured diagnostic alone ([`missing_column`]): the
/// `Binder` type with the `COLUMN_NOT_FOUND` subtype. A type failure over a
/// column the input has (`sum` over text, say) is the `NO_MATCHING_FUNCTION`
/// subtype, so it is a database error whatever the column is called.
fn is_binder_column_error(e: &duckdb::Error) -> bool {
    missing_column(e).is_some()
}

/// The `DuckDB` types a `timechart` bucket source may have: the
/// timestamp family and nothing else.
///
/// `DATE` is deliberately absent. `DuckDB` resolves
/// `time_bucket(INTERVAL, <untyped NULL>)` to the `DATE` overload, so a
/// bucket source that is nothing at all buckets happily and charts every
/// row into one NULL bucket; and a real date column cannot be told from
/// that one by its type. Refusing both costs a reader with a date column
/// one cast, and says so.
fn bucket_input_is_timestamp(id: LogicalTypeId) -> bool {
    matches!(
        id,
        LogicalTypeId::Timestamp
            | LogicalTypeId::TimestampS
            | LogicalTypeId::TimestampMs
            | LogicalTypeId::TimestampNs
            | LogicalTypeId::TimestampTZ
    )
}

/// The `DuckDB` SQL name for a bucket type a refusal reports.
///
/// Trawl-authored, so what the reader sees does not depend on `DuckDB`'s
/// error wording.
fn duckdb_type_name(id: LogicalTypeId) -> String {
    match id {
        LogicalTypeId::SqlNull => "NULL".to_string(),
        LogicalTypeId::Varchar | LogicalTypeId::StringLiteral => "VARCHAR".to_string(),
        LogicalTypeId::IntegerLiteral => "INTEGER".to_string(),
        other => format!("{other:?}").to_uppercase(),
    }
}

/// [`validate_timechart_inputs`], booked as `probe` when there is a probe
/// to run (ADR-0046).
///
/// The probes' binds and executions are theirs alone: they never count
/// as a main-statement bind attempt and never land in `bind` or
/// `execute`. A query with no timechart does not enter the phase here,
/// so its `query_probe_us` is the schema probe's alone, and absent when
/// that probe was skipped too ([`Executor::input_columns`]).
fn run_timechart_probes(
    conn: &Connection,
    checks: &[TimechartInputCheck],
    cancel: &CancelLatch,
    clock: &PhaseClock,
) -> Result<(), EngineError> {
    if checks.is_empty() {
        return validate_timechart_inputs(conn, checks, cancel);
    }
    clock.time(QueryPhase::Probe, || {
        validate_timechart_inputs(conn, checks, cancel)
    })
}

/// Refuse a `timechart on <column>` whose bucket source is not a
/// timestamp, before the statement it belongs to is bound.
///
/// One probe per timechart, each reading its own stage's input
/// relation, so the answer is about the column that stage buckets and
/// not about whatever the finished query happens to call `_time`.
///
/// A probe that fails is an answer too, and only one failure is
/// silence: the no-files case, which the query lane answers with an
/// empty result rather than an error. Every other failure propagates:
/// a `_time` filter over a source with no `_time` column fails HERE
/// while the main statement survives it, because
/// `time_bucket(…, "d") AS "_time"` gives `DuckDB` an output alias to
/// resolve that filter against.
fn validate_timechart_inputs(
    conn: &Connection,
    checks: &[TimechartInputCheck],
    cancel: &CancelLatch,
) -> Result<(), EngineError> {
    for check in checks {
        cancel.check()?;
        let mut stmt = match prepare_statement(conn, &check.sql) {
            Ok(stmt) => stmt,
            Err(e) if is_no_files_error(&e) => continue,
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        cancel.check()?;

        let params = bind_params(&check.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let rows = match stmt.query(param_refs.as_slice()) {
            Ok(rows) => rows,
            Err(e) if is_no_files_error(&e) => continue,
            Err(e) if is_binder_column_error(&e) => return Err(remap_binder_error(&e)),
            Err(e) => return Err(e.into()),
        };

        let bound =
            rows.as_ref()
                .ok_or(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "statement unavailable after query execution".into(),
                )))?;
        let id = bound.column_logical_type(0).id();
        if !bucket_input_is_timestamp(id) {
            // `Refused`: the engine asked `DuckDB` and got its answer, so
            // the sentence is the engine's own, about the reader's column.
            return Err(EngineError::Refused {
                message: format!(
                    "timechart on '{}' is not a timestamp: {}",
                    check.column,
                    duckdb_type_name(id)
                ),
            });
        }
    }
    // A cancellation that landed during the last probe's bind stops the
    // caller before it binds the statement these probes were about.
    cancel.check()?;
    Ok(())
}

/// The exception type of a `DuckDB` error — `"Conversion"` for a failed
/// cast, `"Binder"` for a statement that does not bind — read from its
/// structured diagnostic.
fn error_class(e: &duckdb::Error) -> Option<String> {
    DuckDbDiagnostic::of(e).map(|d| d.exception_type)
}

/// Check if a `DuckDB` error carries the `Conversion` class — the class every
/// failed cast reports, whether its cause is a *schema* the sources cannot
/// reconcile or a *value* that cannot be converted.
///
/// No production path classifies conversion errors: write-time catalog
/// conformance makes a surviving `Conversion` error mean a nonconformant
/// corpus, which errors loudly (ADR-0009). The function is retained because
/// the ADR-evidence tests below pin the `DuckDB` behaviour that design rests
/// on.
pub fn is_conversion_error(e: &duckdb::Error) -> bool {
    error_class(e).as_deref() == Some(DUCKDB_CONVERSION_ERROR)
}

/// Classification of a read's outcome, shared by all four entry points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotColdOutcome {
    /// The read produced columns (rows may be empty) — an authoritative
    /// result; return it as-is. An export's `Ok(())` lands here too.
    Columns,
    /// The read matched no files: on the query lanes `execute_emitted`
    /// mapped "no files match the pattern" to an empty result, on the export
    /// lanes the same error arrives raw. Usually the empty window, but the
    /// resolved source can also have raced a file move, so this is an
    /// observation, not proof: it still needs a cold-file presence check.
    NoColumns,
    /// Any other database failure — hot-only is only provably safe when a
    /// cold-file presence check says no cold files exist.
    Recoverable,
    /// A non-recoverable error — propagate it. That includes every
    /// [`EngineError::Emit`]: an emitter refusal, and an unknown field
    /// `DuckDB` could not bind. A field the input does not carry reads as
    /// NULL (ADR-0062), so a missing column that survives to the binder is a
    /// resolver defect, and reading less data would only hide it.
    Fatal,
}

/// Whether the lane executing a read has a hot buffer to fall back to.
///
/// A property of the ENTRY POINT, not of the data: `run_query` and
/// `export_parquet` have none, so [`cold_action`] never hands them
/// [`ColdAction::HotOnly`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotLane {
    Present,
    Absent,
}

/// What a lane should do with a classified outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdAction {
    ReturnOutcome,
    /// Read hot-only — no cold data can be hidden by doing so.
    HotOnly,
    /// The read answered "no files" while cold files exist behind the
    /// source. On the query lanes the outcome held is a substituted empty
    /// *success*, so returning it would be exactly the silent cold-data drop
    /// ADR-0008 forbids; on the export lanes it is a raw IO error that says
    /// nothing useful. Both surface [`EngineError::ColdDataUnread`] instead:
    /// explicit, retryable, and never a 200 with rows missing.
    ColdDataUnread,
}

/// Decide the next step for a classified outcome — the one outcome policy,
/// consulted by all four entry points (ADR-0008, ADR-0062).
///
/// | outcome          | hot=P, cold=P      | hot=P, cold=A | hot=A, cold=P      | hot=A, cold=A |
/// |------------------|--------------------|---------------|--------------------|---------------|
/// | `Columns`        | `Return`           | `Return`      | `Return`           | `Return`      |
/// | `Fatal`          | `Return`           | `Return`      | `Return`           | `Return`      |
/// | `NoColumns`      | `ColdDataUnread`   | `HotOnly`     | `ColdDataUnread`   | `Return`      |
/// | `Recoverable`    | `Return`           | `HotOnly`     | `Return`           | `Return`      |
///
/// Two invariants read off it: a hot-only read happens exactly where it
/// cannot hide cold data, and an empty answer is only ever returned when the
/// source provably reaches no files. The hot-only read is the cold start
/// (ADR-0008) and nothing else: no failure of the query's own makes the lane
/// read less data. `cold` is [`ColdPresence`], `hot` is
/// [`HotLane`]; the match is exhaustive with no wildcard arms, so a new
/// variant of either must state its rule or fail to compile.
fn cold_action(outcome: HotColdOutcome, cold: ColdPresence, hot: HotLane) -> ColdAction {
    match outcome {
        // An authoritative result and a non-recoverable error are the
        // source's own answer on every lane.
        HotColdOutcome::Columns | HotColdOutcome::Fatal => ColdAction::ReturnOutcome,
        HotColdOutcome::NoColumns => match cold {
            ColdPresence::Present => ColdAction::ColdDataUnread,
            ColdPresence::Absent => match hot {
                HotLane::Present => ColdAction::HotOnly,
                HotLane::Absent => ColdAction::ReturnOutcome,
            },
        },
        // An unexpected failure may degrade to hot-only only where there is
        // provably no cold data it would be hiding.
        HotColdOutcome::Recoverable => match (cold, hot) {
            (ColdPresence::Absent, HotLane::Present) => ColdAction::HotOnly,
            (ColdPresence::Absent, HotLane::Absent)
            | (ColdPresence::Present, HotLane::Present | HotLane::Absent) => {
                ColdAction::ReturnOutcome
            }
        },
    }
}

impl HotColdOutcome {
    /// Whether [`cold_action`]'s verdict for this outcome can actually differ
    /// between the two [`ColdPresence`] values — i.e. whether the presence
    /// check is worth paying for.
    ///
    /// Derived FROM the table rather than restated beside it, so it can never
    /// drift out of agreement with it. The happy path (`Columns`) and every
    /// propagated failure (`Fatal`) answer `false`, which is what keeps a
    /// successful query from ever globbing.
    fn consults_cold_presence(self, hot: HotLane) -> bool {
        cold_action(self, ColdPresence::Present, hot)
            != cold_action(self, ColdPresence::Absent, hot)
    }
}

fn classify_query_outcome(outcome: Result<&QueryResult, &EngineError>) -> HotColdOutcome {
    match outcome {
        Ok(r) if !r.columns.is_empty() => HotColdOutcome::Columns,
        // No columns: `execute_emitted` substituted an empty result for a
        // "no files match the pattern" read error.
        Ok(_) => HotColdOutcome::NoColumns,
        Err(EngineError::Database(_)) => HotColdOutcome::Recoverable,
        // Emit (an emitter refusal, or an unknown field the query path
        // remapped), Refused, ResultTooLarge, AggregationCapped,
        // ResourceExhausted, RegexResources, parse, IO — propagate. No narrower
        // source makes any of them mean something else. A hot-only retry
        // would spend an exhausted budget, shared by every query on the
        // database, again, or prepare the same patterns and be refused again.
        Err(_) => HotColdOutcome::Fatal,
    }
}

/// The export surfaces "no files" raw where the query path substitutes an
/// empty result, so it arrives as `Database` here. An unknown field is
/// remapped at staging as on the query path ([`staging_failure`]), so it
/// arrives as `Emit`, beside the emitter's own refusals (a `rust_stages`
/// pipeline, a non-UTF-8 path).
fn classify_export_outcome(outcome: &Result<usize, EngineError>) -> HotColdOutcome {
    match outcome {
        Ok(_) => HotColdOutcome::Columns,
        Err(EngineError::Database(e)) if is_no_files_error(e) => HotColdOutcome::NoColumns,
        Err(EngineError::Database(_)) => HotColdOutcome::Recoverable,
        // As on the query lane: an emitter refusal, a `Refused`, an
        // exhausted budget and a regex memory refusal all propagate, and
        // an export that swallowed one would write the file.
        Err(_) => HotColdOutcome::Fatal,
    }
}

/// `None`, no LIMIT clause at all, answers both the unbounded sentinel
/// `usize::MAX` and any `usize` above `i64::MAX`: `DuckDB`'s LIMIT domain
/// is INT64, so a bigger LIMIT would be a Conversion Error, not a cap.
///
/// The sentinel is tested on the `usize` before the conversion: on a
/// 32-bit target `usize::MAX` converts cleanly, which would silently turn
/// the documented unbounded export into a real `LIMIT 4294967295`.
fn export_row_limit(max_rows: usize) -> Option<i64> {
    if max_rows == usize::MAX {
        return None;
    }
    i64::try_from(max_rows).ok()
}

/// What a source's shape — and, for a list, the filesystem underneath it —
/// says about the cold files behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListEvidence {
    /// Not a list: a plain glob, or the `(subquery)` form `from saved`
    /// builds. Nothing was globbed, so presence is unknown until asked.
    NotAList,
    /// A list at least one of whose elements reaches a file — and the
    /// fail-closed home for a list shape [`glob_list_items`] cannot split,
    /// which must never be read as "no cold data".
    Matching,
    /// A list no element of which reaches a file: the genuine empty window.
    NoneMatching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdPresence {
    Present,
    Absent,
}

/// A source argument resolved against the filesystem — once, before the
/// read, on every lane (ADR-0008).
///
/// `read_parquet` rejects a whole list source when a single element matches
/// nothing, and the server emits one glob per hour in range, so a
/// sparse-traffic service routinely gets elements pointing at hour
/// directories holding no file of its own. Resolution narrows the list to
/// the elements that reach a file before the read, rather than retrying
/// after one failed — and the evidence it gathers on the way is what the
/// outcome policy later reads instead of globbing again.
pub(crate) struct ResolvedSource {
    /// The source text to emit — the original for a plain glob, an
    /// all-matching list and an all-missing list; the narrowed list when
    /// some but not all elements match.
    sql: String,
    evidence: ListEvidence,
}

impl ResolvedSource {
    /// The crate's only call to [`emitter::emit_with_pins`]: every lane that
    /// reads parquet reads a source that has been resolved.
    fn emit_cold(
        &self,
        query: &Query,
        pins: &FieldTypes,
        columns: &InputColumns,
        anchor: EvalContext,
    ) -> Result<EmittedQuery, emitter::EmitError> {
        emitter::emit_with_pins(query, &self.sql, pins, columns, anchor)
    }

    /// The crate's only call to [`emitter::emit_with_hot_source`], for the
    /// same reason as [`Self::emit_cold`]. (`emit_hot_only` stays a free
    /// call — it reads no cold source at all.)
    fn emit_union(
        &self,
        query: &Query,
        hot_source: &str,
        hot_pins: &FieldTypes,
        pins: &FieldTypes,
        columns: &InputColumns,
        anchor: EvalContext,
    ) -> Result<EmittedQuery, emitter::EmitError> {
        emitter::emit_with_hot_source(
            query, &self.sql, hot_source, hot_pins, pins, columns, anchor,
        )
    }
}

/// What a cold-only read's source holds, which decides what the live
/// catalog's pins may type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceOrigin {
    /// The corpus the catalog describes: its pins type every read, a
    /// field the files do not carry included.
    Corpus,
    /// A recorded run, typed by the columns it stores. A live pin types
    /// a name the input probe found in it and nothing else, so a field
    /// the run does not carry reads as an unpinned NULL (ADR-0062).
    Recorded,
}

impl SourceOrigin {
    /// The pins the emitter types this read with. A recorded run keeps
    /// the pins of the names its description carries: every pin while
    /// the probe was skipped, since every name then binds as a column,
    /// and none when the run reaches no file.
    fn pins<'a>(self, pins: &'a FieldTypes, cold: &ColdColumns) -> Cow<'a, FieldTypes> {
        match (self, cold) {
            (Self::Corpus, _) | (Self::Recorded, ColdColumns::Unprobed) => Cow::Borrowed(pins),
            (Self::Recorded, ColdColumns::NoFiles) => Cow::Owned(FieldTypes::new()),
            (Self::Recorded, ColdColumns::Described(columns)) => {
                let mut carried = FieldTypes::new();
                for (name, _) in columns {
                    if let Some(ty) = pins.pin_for(name) {
                        carried.insert(&catalog_key(name), ty);
                    }
                }
                Cow::Owned(carried)
            }
        }
    }
}

#[cfg(test)]
#[test]
fn a_recorded_source_keeps_only_the_pins_of_the_names_it_carries() {
    let mut pins = FieldTypes::new();
    pins.insert("severity", CanonicalType::Severity);
    pins.insert("f", CanonicalType::Double);
    let names = |pins: &FieldTypes| pins.iter().map(|(n, _)| n.to_owned()).collect::<Vec<_>>();
    let described = ColdColumns::described([("Severity", "BIGINT"), ("host", "VARCHAR")]);

    let recorded = SourceOrigin::Recorded;
    assert_eq!(names(&recorded.pins(&pins, &described)), ["severity"]);
    assert!(recorded.pins(&pins, &ColdColumns::NoFiles).is_empty());
    assert_eq!(
        names(&recorded.pins(&pins, &ColdColumns::Unprobed)),
        ["f", "severity"]
    );
    assert_eq!(
        names(&SourceOrigin::Corpus.pins(&pins, &described)),
        ["f", "severity"]
    );
}

/// Whether a source element carries a `DuckDB` glob metacharacter — the test
/// that decides whether resolution asks the filesystem or asks `glob()`.
fn has_glob_meta(element: &str) -> bool {
    element.contains(['*', '?', '['])
}

/// Whether a literal path (no glob metacharacters) is a file `read_parquet`
/// could open — the filesystem twin of [`Executor::glob_has_match`].
///
/// `metadata` follows symlinks, which is what `glob()` does too: a symlink to
/// a real file matches, a broken one does not. A directory named like a
/// parquet file is not a file, and `glob()` on a literal path does not match
/// one either (probed, not assumed).
///
/// A metadata error that is not `NotFound` (a permission or IO fault) retains
/// the element: the element must reach `read_parquet` and fail loudly there,
/// never be dropped into a silently narrower answer.
fn literal_path_is_file(path: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// Resolve `source` against a file matcher — the one owner of list-source
/// resolution semantics.
///
/// List elements are retained verbatim in their original order, duplicates
/// included, and never expanded to concrete filenames: the narrowed source
/// must reach exactly the data the original reached, and a glob expanded at
/// resolution time would freeze a file list that compaction is still
/// appending to.
fn resolve_list_source(source: &str, matches: impl Fn(&str) -> bool) -> ResolvedSource {
    let Some(items) = glob_list_items(source) else {
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::NotAList,
        };
    };
    if items.is_empty() {
        // List-shaped but unparseable: nothing to narrow, and the evidence
        // must not read as "no cold data" (fail closed).
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::Matching,
        };
    }

    let matching: Vec<&str> = items.iter().copied().filter(|item| matches(item)).collect();
    if matching.is_empty() {
        // The genuine empty window: the original stands, and the read's
        // "no files" answer is the truth.
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::NoneMatching,
        };
    }
    if matching.len() == items.len() {
        return ResolvedSource {
            sql: source.to_owned(),
            evidence: ListEvidence::Matching,
        };
    }

    let quoted: Vec<String> = matching.iter().map(|item| format!("'{item}'")).collect();
    ResolvedSource {
        sql: format!("[{}]", quoted.join(", ")),
        evidence: ListEvidence::Matching,
    }
}

/// Split a `DuckDB` list-of-globs source (`['a', 'b']`, as built by the
/// server's source resolver) into its quoted elements.
///
/// Returns `None` when `source` is not list-shaped (a plain glob), and
/// `Some(vec![])` when it is list-shaped but cannot be parsed — the caller
/// treats that as "assume cold files exist" so a parse gap never turns into
/// a silent cold-data drop.
fn glob_list_items(source: &str) -> Option<Vec<&str>> {
    let trimmed = source.trim();
    let inner = trimmed.strip_prefix('[')?;
    let Some(inner) = inner.strip_suffix(']') else {
        return Some(Vec::new());
    };

    let mut items = Vec::new();
    let mut rest = inner;
    while let Some(open) = rest.find('\'') {
        let after = &rest[open + 1..];
        // An unterminated quote means we don't understand the source.
        let Some(close) = after.find('\'') else {
            return Some(Vec::new());
        };
        items.push(&after[..close]);
        rest = &after[close + 1..];
    }
    Some(items)
}

/// Remap a `DuckDB` binder error about missing columns to `EngineError::Emit`
/// so it surfaces as HTTP 400 instead of 500.
///
/// Called only on an error [`is_binder_column_error`] accepted, so its
/// diagnostic names the column, and the refusal names it too. The column is
/// the query's own field name, so it crosses the trust boundary intact.
fn remap_binder_error(e: &duckdb::Error) -> EngineError {
    let message = missing_column(e).map_or_else(
        || "unknown field in query".to_owned(),
        |field| format!("unknown field: {field}"),
    );
    EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation { message })
}

/// An export staging statement's failure, as the lane answers it: an
/// unknown field is the same sanitized 400 the query lane gives
/// ([`remap_binder_error`]), and never a reason to export less data
/// (ADR-0062). Every other failure is the database's.
fn staging_failure(e: duckdb::Error) -> EngineError {
    if is_binder_column_error(&e) {
        remap_binder_error(&e)
    } else {
        e.into()
    }
}

fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|val| -> Box<dyn duckdb::ToSql> {
            match val {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                // The statement's `now()` anchor (ADR-0017 §3). Bound as
                // microseconds because that is `DuckDB`'s TIMESTAMP
                // domain and the anchor is truncated to it at capture, so
                // the bound value is exact, never rounded at the wire.
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect()
}

/// The names of a statement's scalar result columns whose `DuckDB` logical
/// type is exactly TIMESTAMP. Read, like [`typed_columns`], before the
/// first row.
fn exact_timestamp_columns(stmt: &duckdb::Statement<'_>) -> Vec<String> {
    stmt.column_names()
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| stmt.column_logical_type(*idx).id() == LogicalTypeId::Timestamp)
        .map(|(_, name)| name)
        .collect()
}

/// A statement's result columns, each typed from its `DuckDB` logical
/// type ([`column_type`]). Read once the statement has run and before its
/// first row, so a zero-row result is typed too.
fn typed_columns(stmt: &duckdb::Statement<'_>) -> Vec<Column> {
    stmt.column_names()
        .into_iter()
        .enumerate()
        .map(|(idx, name)| Column::new(name, column_type(&stmt.column_logical_type(idx))))
        .collect()
}

/// Temporal values render through this module's own formatters
/// ([`format_timestamp`], [`format_date`], [`format_time`]) while the
/// value is still an integer count, in the spelling `timestamps` picks.
///
/// The cell has to agree with the type [`crate::column_type::column_type`]
/// gives its column, or the wire encoder refuses it; a `HUGEINT` or
/// `UHUGEINT` outside `i64` ∪ `u64` has no `integer` cell at all and is
/// refused here, by `column`'s name ([`EngineError::IntegerOutOfRange`]).
fn extract_value(
    row: &duckdb::Row<'_>,
    idx: usize,
    column: &str,
    timestamps: TimestampText,
) -> Result<Value, EngineError> {
    Ok(match row.get_ref_unwrap(idx) {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Boolean(b),
        ValueRef::TinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::SmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::Int(i) => Value::Integer(i64::from(i)),
        ValueRef::BigInt(i) => Value::Integer(i),
        // Past `i64::MAX` the magnitude lands in `Value::UInt` —
        // `trawl_api::value` owns that rule for every decoder, this one
        // included — and past `u64::MAX` it has no variant.
        ValueRef::HugeInt(i) => land_huge(i, column)?,
        ValueRef::UHugeInt(u) => land_uhuge(u, column)?,
        ValueRef::UTinyInt(i) => Value::Integer(i64::from(i)),
        ValueRef::USmallInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UInt(i) => Value::Integer(i64::from(i)),
        ValueRef::UBigInt(i) => land_u64(i),
        ValueRef::Float(f) => Value::Float(f64::from(f)),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(format!("<blob {} bytes>", bytes.len())),
        ValueRef::Timestamp(unit, val) => Value::String(format_timestamp(unit, val, timestamps)),
        ValueRef::Date32(days) => Value::String(format_date(days)),
        ValueRef::Time64(unit, val) => Value::String(format_time(unit, val)),
        ValueRef::List(..) | ValueRef::Array(..) => match row.get::<_, duckdb::types::Value>(idx) {
            Ok(value) => convert_duckdb_value(value, column, timestamps)?,
            Err(_) => Value::Null,
        },
        _ => row.get::<_, String>(idx).map_or(Value::Null, Value::String),
    })
}

/// This is the nested twin of [`extract_value`] — a `list()` aggregate
/// arrives here element by element — so it lands the same integer widths
/// and renders the same temporal text the same way, refusing a huge
/// integer by the same column name.
fn convert_duckdb_value(
    v: duckdb::types::Value,
    column: &str,
    timestamps: TimestampText,
) -> Result<Value, EngineError> {
    Ok(match v {
        duckdb::types::Value::Null => Value::Null,
        duckdb::types::Value::Boolean(b) => Value::Boolean(b),
        duckdb::types::Value::TinyInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::SmallInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::Int(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::BigInt(i) => Value::Integer(i),
        duckdb::types::Value::HugeInt(i) => land_huge(i, column)?,
        duckdb::types::Value::UHugeInt(u) => land_uhuge(u, column)?,
        duckdb::types::Value::UTinyInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::USmallInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::UInt(i) => Value::Integer(i64::from(i)),
        duckdb::types::Value::UBigInt(i) => land_u64(i),
        duckdb::types::Value::Float(f) => Value::Float(f64::from(f)),
        duckdb::types::Value::Double(f) => Value::Float(f),
        duckdb::types::Value::Text(s) | duckdb::types::Value::Enum(s) => Value::String(s),
        duckdb::types::Value::Blob(bytes) => Value::String(format!("<blob {} bytes>", bytes.len())),
        // A `list(_time)` element is the same instant a `_time` cell is,
        // so it renders the same way: the column is `list<timestamp>`,
        // and a debug spelling would not be a timestamp at all.
        duckdb::types::Value::Timestamp(unit, val) => {
            Value::String(format_timestamp(unit, val, timestamps))
        }
        duckdb::types::Value::Date32(days) => Value::String(format_date(days)),
        duckdb::types::Value::Time64(unit, val) => Value::String(format_time(unit, val)),
        duckdb::types::Value::List(elements) | duckdb::types::Value::Array(elements) => {
            Value::Array(
                elements
                    .into_iter()
                    .map(|element| convert_duckdb_value(element, column, timestamps))
                    .collect::<Result<_, _>>()?,
            )
        }
        other => Value::String(format!("{other:?}")),
    })
}

fn land_huge(v: i128, column: &str) -> Result<Value, EngineError> {
    land_i128(v).ok_or_else(|| EngineError::IntegerOutOfRange {
        column: column.to_owned(),
    })
}

fn land_uhuge(v: u128, column: &str) -> Result<Value, EngineError> {
    u64::try_from(v)
        .map(land_u64)
        .map_err(|_| EngineError::IntegerOutOfRange {
            column: column.to_owned(),
        })
}
#[cfg(test)]
#[test]
fn list_ubigint_renders_digits() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut stmt = conn
        .prepare(
            "SELECT list(v ORDER BY v) FROM (VALUES \
             (1::UBIGINT), (9223372036854775808::UBIGINT), \
             (18446744073709551615::UBIGINT)) t(v)",
        )
        .expect("prepare");
    let mut rows = stmt.query([]).expect("query");
    let row = rows.next().expect("step").expect("one row");
    let cell = extract_value(row, 0, "ids", TimestampText::Wire)
        .expect("every element fits an integer cell");

    assert_eq!(
        cell,
        Value::Array(vec![
            Value::Integer(1),
            Value::UInt(9_223_372_036_854_775_808),
            Value::UInt(u64::MAX),
        ]),
        "every element must be a number, never debug text"
    );
    let rendered = cell.to_string();
    assert!(
        !rendered.contains("UBigInt"),
        "debug text leaked into the cell: {rendered}"
    );
}

/// `test_all_types()`'s columns as [`column_type`] maps them, in order.
#[cfg(test)]
const ALL_TYPES_MAPPED: [&str; 55] = [
    "bool boolean",
    "tinyint integer",
    "smallint integer",
    "int integer",
    "bigint integer",
    "hugeint integer",
    "uhugeint integer",
    "utinyint integer",
    "usmallint integer",
    "uint integer",
    "ubigint integer",
    "bignum string",
    "date date",
    "time time",
    "timestamp timestamp",
    "timestamp_s timestamp",
    "timestamp_ms timestamp",
    "timestamp_ns timestamp",
    "time_tz time",
    "timestamp_tz timestamp",
    "float float",
    "double float",
    "dec_4_1 string",
    "dec_9_4 string",
    "dec_18_6 string",
    "dec38_10 string",
    "uuid string",
    "interval string",
    "varchar string",
    "blob string",
    "bit string",
    "small_enum string",
    "medium_enum string",
    "large_enum string",
    "int_array list<integer>",
    "double_array list<float>",
    "date_array list<date>",
    "timestamp_array list<timestamp>",
    "timestamptz_array list<timestamp>",
    "varchar_array list<string>",
    "nested_int_array list<list<integer>>",
    "struct string",
    "struct_of_arrays string",
    "array_of_structs list<string>",
    "map string",
    "union string",
    "fixed_int_array list<integer>",
    "fixed_varchar_array list<string>",
    "fixed_nested_int_array list<list<integer>>",
    "fixed_nested_varchar_array list<list<string>>",
    "fixed_struct_array list<string>",
    "struct_of_fixed_array string",
    "fixed_array_of_int_list list<list<integer>>",
    "list_of_fixed_int_array list<list<integer>>",
    "geometry string",
];

/// `time_ns` is left out because the bundled duckdb-rs cannot read a
/// `TIME_NS` cell at all (`get_ref_unwrap` panics), whatever its type.
#[cfg(test)]
#[test]
fn every_duckdb_type_agrees_with_its_cells() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut stmt = conn
        .prepare("SELECT * EXCLUDE (time_ns) FROM test_all_types()")
        .expect("prepare");
    let mut rows = stmt.query([]).expect("query");
    let columns = typed_columns(rows.as_ref().expect("statement after query"));

    let mapped: Vec<String> = columns
        .iter()
        .map(|column| format!("{} {}", column.name, column.ty))
        .collect();
    assert_eq!(mapped, ALL_TYPES_MAPPED);

    let mut refused = Vec::new();
    let mut landed = 0;
    while let Some(row) = rows.next().expect("step") {
        for (i, column) in columns.iter().enumerate() {
            let cell = match extract_value(row, i, &column.name, TimestampText::Wire) {
                Ok(cell) => cell,
                Err(EngineError::IntegerOutOfRange { column }) => {
                    refused.push(column);
                    continue;
                }
                Err(other) => panic!("{}: {other}", column.name),
            };
            let one = QueryResult {
                columns: vec![column.clone()],
                rows: vec![vec![cell.clone()]],
            };
            let wire = serde_json::to_string(&one).unwrap_or_else(|e| {
                panic!(
                    "{} ({}): {cell:?} does not encode: {e}",
                    column.name, column.ty
                )
            });
            let back: QueryResult = serde_json::from_str(&wire)
                .unwrap_or_else(|e| panic!("{}: {wire} does not decode: {e}", column.name));
            // Through `Debug`, so a NaN element compares equal to itself.
            assert_eq!(
                format!("{:?}", back.rows[0][0]),
                format!("{cell:?}"),
                "{}",
                column.name
            );
            landed += 1;
        }
    }
    assert_eq!(refused, ["hugeint", "hugeint", "uhugeint"]);
    assert_eq!(landed, columns.len() * 3 - refused.len());
}

#[cfg(test)]
#[test]
fn zero_row_statements_are_typed() {
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut stmt = conn
        .prepare("SELECT NULL AS nothing, 1::UBIGINT AS n, [1.5::DOUBLE] AS l WHERE false")
        .expect("prepare");
    let mut rows = stmt.query([]).expect("query");
    let columns = typed_columns(rows.as_ref().expect("statement after query"));
    assert!(rows.next().expect("step").is_none());
    assert_eq!(
        columns,
        [
            Column::new("nothing", ColumnType::Integer),
            Column::new("n", ColumnType::Integer),
            Column::new("l", ColumnType::list(ColumnType::Float)),
        ]
    );
}

const fn to_micros(unit: TimeUnit, val: i64) -> i64 {
    match unit {
        TimeUnit::Second => val * 1_000_000,
        TimeUnit::Millisecond => val * 1_000,
        TimeUnit::Microsecond => val,
        TimeUnit::Nanosecond => val / 1_000,
    }
}

/// `DuckDB`'s infinite timestamps: the two extremes of its count, in every
/// unit. They are not instants, so they spell `DuckDB`'s own words, the ones
/// `trawl_core::compare::Instant::cast_text` uses too, rather than a civil
/// time no calendar holds.
const TIMESTAMP_INFINITY: i64 = i64::MAX;
const TIMESTAMP_NEG_INFINITY: i64 = -i64::MAX;

#[derive(Debug, Clone, Copy)]
enum TimestampText {
    /// The result wire's RFC 3339 in UTC
    /// ([`trawl_api::zone::wire_timestamp`]).
    Wire,
    /// `DuckDB`'s own `CAST(ts AS VARCHAR)` text, in UTC: what a Rust tail
    /// reads ([`sql_timestamp_text`]).
    Cast,
}

fn format_timestamp(unit: TimeUnit, val: i64, text: TimestampText) -> String {
    // The sentinels come first: `to_micros` would overflow on them.
    match val {
        TIMESTAMP_INFINITY => return "infinity".to_owned(),
        TIMESTAMP_NEG_INFINITY => return "-infinity".to_owned(),
        _ => {}
    }
    let micros = to_micros(unit, val);
    match text {
        TimestampText::Wire => trawl_api::zone::wire_timestamp(micros, Utc.fix()),
        TimestampText::Cast => cast_timestamp(micros),
    }
}

/// An instant as `DuckDB`'s `CAST(ts AS VARCHAR)` writes it, in UTC:
/// `YYYY-MM-DD HH:MM:SS`, and the fraction without its trailing zeros
/// when it has one. Any year `DuckDB` holds, by the civil calendar below.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn cast_timestamp(micros: i64) -> String {
    let (total_secs, sub_secs) = if micros >= 0 {
        (micros / 1_000_000, (micros % 1_000_000) as u32)
    } else {
        let s = (micros - 999_999) / 1_000_000;
        let us = (micros - s * 1_000_000) as u32;
        (s, us)
    };

    let (days, day_secs) = if total_secs >= 0 {
        ((total_secs / 86400) as i32, (total_secs % 86400) as u32)
    } else {
        let d = (total_secs - 86399) / 86400;
        let s = (total_secs - d * 86400) as u32;
        (d as i32, s)
    };

    let (y, m, d) = days_to_ymd(days);
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    if sub_secs == 0 {
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}")
    } else {
        let frac = format!("{sub_secs:06}");
        let trimmed = frac.trim_end_matches('0');
        format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02}.{trimmed}")
    }
}

/// [`cast_timestamp`]'s text back as the microsecond count it renders,
/// [`TIMESTAMP_INFINITY`] and [`TIMESTAMP_NEG_INFINITY`] included.
///
/// `None` when the text is not that rendering (a cell the tail replaced
/// with something else) or its year is outside chrono's calendar.
fn parse_cast_timestamp(text: &str) -> Option<i64> {
    match text {
        "infinity" => Some(TIMESTAMP_INFINITY),
        "-infinity" => Some(TIMESTAMP_NEG_INFINITY),
        _ => chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
            .ok()
            .map(|at| at.and_utc().timestamp_micros()),
    }
}

/// The wire's RFC 3339 in UTC, unless a Rust tail follows,
/// because that tail is a second evaluator, not a printer. So the tail
/// gets `DuckDB`'s own cast text in UTC, the text the SQL lane compares
/// and prints, and [`render_tracked_timestamps`] renders the wire's text
/// afterwards.
fn sql_timestamp_text(emitted: &EmittedQuery) -> TimestampText {
    if emitted.rust_stages.is_empty() {
        TimestampText::Wire
    } else {
        TimestampText::Cast
    }
}

/// Follow the SQL result's TIMESTAMP columns, and its lists of them,
/// through the Rust tail's own rename/alias lineage.
///
/// Display lineage and comparison pins follow the same stage rules
/// ([`PinScope::advance`]), so they can never disagree about which column
/// is "still `_time`".
fn tail_timestamp_scope(timestamp_columns: &[String], stages: &[Spanned<PipeStage>]) -> PinScope {
    let mut seed = FieldTypes::new();
    for name in timestamp_columns {
        seed.insert(name, CanonicalType::Timestamp);
    }
    let mut scope = PinScope::root(&seed);
    for stage in stages {
        scope.advance(&stage.node);
    }
    scope
}

fn holds_timestamps(ty: &ColumnType) -> bool {
    match ty {
        ColumnType::Timestamp => true,
        ColumnType::List { items } => holds_timestamps(items),
        _ => false,
    }
}

/// The type a tracked column becomes when its cast text is rendered: a
/// `string` column a `timestamp` one, and a list of strings, at any depth,
/// the same list of timestamps. `None` for any other type, which the
/// tail's cells gave a column that is no longer stored timestamps (a
/// group key's text of a list, a value a kv pair overwrote).
fn rendered_timestamp_type(ty: &ColumnType) -> Option<ColumnType> {
    match ty {
        ColumnType::String => Some(ColumnType::Timestamp),
        ColumnType::List { items } => rendered_timestamp_type(items).map(ColumnType::list),
        _ => None,
    }
}

fn render_tracked_cell(cell: &Value) -> Option<Value> {
    match cell {
        Value::Null => Some(Value::Null),
        Value::String(text) => parse_cast_timestamp(text).map(|micros| {
            Value::String(format_timestamp(
                TimeUnit::Microsecond,
                micros,
                TimestampText::Wire,
            ))
        }),
        Value::Array(elements) => elements
            .iter()
            .map(render_tracked_cell)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        _ => None,
    }
}

/// Render the columns the Rust tail's lineage ([`tail_timestamp_scope`])
/// still calls a stored TIMESTAMP, or a stored list of them, in the wire's
/// text, and type them `timestamp` (or the list of it).
///
/// A column is rendered whole or not at all: if any text in it is not
/// cast text (the tail replaced it, or its year is outside chrono's
/// calendar), the column stays the type the tail left.
fn render_tracked_timestamps(
    result: &mut QueryResult,
    tracked: &PinScope,
    cancel: &CancelLatch,
) -> Result<(), EngineError> {
    if tracked.is_empty() {
        return Ok(());
    }
    for idx in 0..result.columns.len() {
        let column = &result.columns[idx];
        if tracked.pin_for(&column.name).is_none() {
            continue;
        }
        let Some(rendered_type) = rendered_timestamp_type(&column.ty) else {
            continue;
        };
        // A tail's answer can be a million rows: the render reads the
        // latch every stride, as the tail's own row passes do.
        let mut rendered = Vec::with_capacity(result.rows.len());
        for (at, row) in result.rows.iter().enumerate() {
            if at.is_multiple_of(crate::post_process::TAIL_CANCEL_STRIDE) {
                cancel.check()?;
            }
            let Some(cell) = render_tracked_cell(row.get(idx).unwrap_or(&Value::Null)) else {
                break;
            };
            rendered.push(cell);
        }
        if rendered.len() < result.rows.len() {
            continue;
        }
        for (row, cell) in result.rows.iter_mut().zip(rendered) {
            row[idx] = cell;
        }
        result.columns[idx].ty = rendered_type;
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn a_tracked_render_observes_cancellation() {
    let rows = crate::post_process::TAIL_CANCEL_STRIDE * 2;
    let mut result = QueryResult {
        columns: vec![Column::new("seen", ColumnType::String)],
        rows: (0..rows)
            .map(|_| vec![Value::String("2026-01-15 09:00:00".to_owned())])
            .collect(),
    };
    let untouched = result.clone();
    let tracked = tail_timestamp_scope(&["seen".into()], &[]);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let outcome = render_tracked_timestamps(&mut result, &tracked, &CancelLatch::new(flag));
    assert!(
        matches!(outcome, Err(EngineError::Cancelled)),
        "{outcome:?}"
    );
    assert_eq!(result, untouched);

    render_tracked_timestamps(&mut result, &tracked, &CancelLatch::never())
        .expect("a latch that never stops");
    assert_eq!(
        result.columns[0].ty,
        ColumnType::Timestamp,
        "it renders uncancelled"
    );
}

#[cfg(test)]
#[test]
fn tracked_lists_render_element_by_element() {
    let text = |s: &str| Value::String(s.to_owned());
    let list = Value::Array;
    let strings = ColumnType::list(ColumnType::String);
    let mut result = QueryResult {
        columns: vec![
            Column::new("seen", strings.clone()),
            Column::new("deep", ColumnType::list(strings.clone())),
            Column::new("bad", strings),
            Column::new("key", ColumnType::String),
        ],
        rows: vec![
            vec![
                list(vec![text("2026-01-15 09:00:00.123456"), Value::Null]),
                list(vec![list(vec![text("infinity")]), list(vec![])]),
                list(vec![text("2026-01-15 09:00:00")]),
                text("[2026-01-15 09:00:00]"),
            ],
            vec![
                Value::Null,
                list(vec![list(vec![text("1969-12-31 23:59:59.5")])]),
                list(vec![text("not a timestamp")]),
                text("[2026-01-15 09:00:00]"),
            ],
        ],
    };
    let untouched = result.clone();
    let tracked = tail_timestamp_scope(
        &["seen".into(), "deep".into(), "bad".into(), "key".into()],
        &[],
    );
    render_tracked_timestamps(&mut result, &tracked, &CancelLatch::never())
        .expect("a latch that never stops");

    let timestamps = ColumnType::list(ColumnType::Timestamp);
    assert_eq!(
        result.columns,
        vec![
            Column::new("seen", timestamps.clone()),
            Column::new("deep", ColumnType::list(timestamps)),
            untouched.columns[2].clone(),
            untouched.columns[3].clone(),
        ]
    );
    assert_eq!(
        result.rows[0][0],
        list(vec![text("2026-01-15T09:00:00.123456Z"), Value::Null])
    );
    assert_eq!(result.rows[1][0], Value::Null);
    assert_eq!(
        result.rows[0][1],
        list(vec![list(vec![text("infinity")]), list(vec![])])
    );
    assert_eq!(
        result.rows[1][1],
        list(vec![list(vec![text("1969-12-31T23:59:59.500000Z")])])
    );
    for row in 0..2 {
        assert_eq!(result.rows[row][2..], untouched.rows[row][2..]);
    }
}

fn format_date(days: i32) -> String {
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A time of day as `DuckDB` casts it to text: `HH:MM:SS`, and the
/// fraction without its trailing zeros when it has one.
fn format_time(unit: TimeUnit, val: i64) -> String {
    let micros = to_micros(unit, val);
    let total_secs = micros / 1_000_000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    let fraction = micros % 1_000_000;
    if fraction == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let digits = format!("{fraction:06}");
        format!("{h:02}:{m:02}:{s:02}.{}", digits.trim_end_matches('0'))
    }
}

/// Uses the civil calendar algorithm from Howard Hinnant's `date` library.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn days_to_ymd(days: i32) -> (i32, u32, u32) {
    // shift epoch from 0000-03-01 to 1970-01-01
    let z = i64::from(days) + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d)
}

#[cfg(test)]
#[test]
fn timestamps_render_as_rfc3339_with_micros() {
    // 2024-01-15T10:00:00Z and 1969-12-31T23:59:59.5Z.
    let at = 1_705_312_800_000_000;
    let pre_epoch = -500_000;
    for (unit, val, text, expected) in [
        (
            TimeUnit::Microsecond,
            at,
            TimestampText::Wire,
            "2024-01-15T10:00:00.000000Z",
        ),
        (
            TimeUnit::Microsecond,
            at + 120_450,
            TimestampText::Wire,
            "2024-01-15T10:00:00.120450Z",
        ),
        (
            TimeUnit::Microsecond,
            pre_epoch,
            TimestampText::Wire,
            "1969-12-31T23:59:59.500000Z",
        ),
        (
            TimeUnit::Second,
            0,
            TimestampText::Wire,
            "1970-01-01T00:00:00.000000Z",
        ),
        (
            TimeUnit::Millisecond,
            1_705_312_800_123,
            TimestampText::Wire,
            "2024-01-15T10:00:00.123000Z",
        ),
        (
            TimeUnit::Nanosecond,
            1_705_312_800_123_456_789,
            TimestampText::Wire,
            "2024-01-15T10:00:00.123456Z",
        ),
        (
            TimeUnit::Microsecond,
            at,
            TimestampText::Cast,
            "2024-01-15 10:00:00",
        ),
        (
            TimeUnit::Microsecond,
            pre_epoch,
            TimestampText::Cast,
            "1969-12-31 23:59:59.5",
        ),
    ] {
        assert_eq!(
            format_timestamp(unit, val, text),
            expected,
            "{unit:?} {val}"
        );
    }
    for unit in [
        TimeUnit::Second,
        TimeUnit::Millisecond,
        TimeUnit::Microsecond,
        TimeUnit::Nanosecond,
    ] {
        for text in [TimestampText::Wire, TimestampText::Cast] {
            assert_eq!(format_timestamp(unit, i64::MAX, text), "infinity");
            assert_eq!(format_timestamp(unit, -i64::MAX, text), "-infinity");
        }
    }
}

/// Past chrono's calendar the wire still writes `DuckDB`'s own finite
/// extremes, `test_all_types()`'s, on the same civil date as the cast text.
#[cfg(test)]
#[test]
fn timestamps_past_chronos_calendar_keep_their_civil_date() {
    let min = -9_223_372_022_400_000_000;
    let max = 9_223_372_036_854_775_806;
    let wire = |micros| format_timestamp(TimeUnit::Microsecond, micros, TimestampText::Wire);
    assert_eq!(wire(min), "-290308-12-22T00:00:00.000000Z");
    assert_eq!(cast_timestamp(min), "-290308-12-22 00:00:00");
    assert_eq!(wire(max), "+294247-01-10T04:00:54.775806Z");
    assert_eq!(cast_timestamp(max), "294247-01-10 04:00:54.775806");
}

#[cfg(test)]
#[test]
fn times_keep_their_fraction() {
    assert_eq!(format_time(TimeUnit::Microsecond, 0), "00:00:00");
    assert_eq!(
        format_time(TimeUnit::Microsecond, 45_296_500_000),
        "12:34:56.5"
    );
    assert_eq!(
        format_time(TimeUnit::Microsecond, 45_296_000_001),
        "12:34:56.000001"
    );
    assert_eq!(
        format_time(TimeUnit::Microsecond, 86_400_000_000),
        "24:00:00"
    );
}

/// One parquet fixture for the `timechart on` lane, two rows, one
/// column per bucket-source type worth refusing — plus `label`, whose
/// values include the literal string `_time`, for the pivot case.
#[cfg(test)]
fn timechart_fixture(dir: &tempfile::TempDir) -> String {
    let path = dir
        .path()
        .join("buckets.parquet")
        .to_str()
        .expect("temp path is valid UTF-8")
        .to_string();
    Connection::open_in_memory()
        .expect("in-memory duckdb")
        .execute_batch(&format!(
            "COPY (SELECT * FROM (VALUES \
             ('web-1', 'c1', 1::BIGINT, 1.5::DOUBLE, TRUE, DATE '2026-01-01', \
              TIMESTAMP '2026-01-01 00:05:00', TIMESTAMPTZ '2026-01-01 00:05:00+00', '_time'), \
             ('web-2', 'c2', 2::BIGINT, 2.5::DOUBLE, FALSE, DATE '2026-01-02', \
              TIMESTAMP '2026-01-02 00:05:00', TIMESTAMPTZ '2026-01-02 00:05:00+00', 'other')) \
             AS t(hostname, column_name, n, f, bo, d, good, tz, label)) \
             TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("fixture writes");
    path
}

#[cfg(test)]
fn temp_directory(exec: &Executor) -> std::path::PathBuf {
    exec.conn
        .query_row("SELECT current_setting('temp_directory')", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("temp_directory")
        .into()
}

#[cfg(unix)]
#[test]
fn an_executor_spills_into_a_private_directory_of_its_own() {
    use std::os::unix::fs::MetadataExt as _;
    let me = tempfile::tempfile()
        .expect("probe file")
        .metadata()
        .unwrap()
        .uid();
    let exec = Executor::new().expect("executor");
    let dir = temp_directory(&exec);
    let meta = std::fs::symlink_metadata(&dir).expect("the spill directory exists");
    assert!(meta.is_dir(), "{}", dir.display());
    assert_eq!(meta.mode() & 0o7777, 0o700, "{}", dir.display());
    assert_eq!(meta.uid(), me, "{}", dir.display());
    assert_ne!(dir, spill_dir(), "not the shared temp directory itself");
    assert_eq!(dir.parent(), Some(spill_dir().as_path()));

    let clone = exec.try_clone().expect("clone");
    assert_eq!(temp_directory(&clone), dir, "a clone shares the directory");
    let other = Executor::new().expect("a second executor");
    assert_ne!(temp_directory(&other), dir, "another database has its own");
    drop(exec);
    assert!(dir.is_dir(), "the clone still holds it");
    drop(clone);
    assert!(!dir.exists(), "the last clone removed it");
}

#[cfg(unix)]
#[test]
fn a_spill_path_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/trawl-spill-\xff"));
    let error = temp_directory_statement(dir, crate::scratch::TMPDIR_SETTING)
        .expect_err("a non-UTF-8 spill path");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("TMPDIR"), "{error}");
    assert_eq!(
        temp_directory_statement(Path::new("/tmp/it's"), crate::scratch::TMPDIR_SETTING)
            .expect("a UTF-8 path"),
        "SET temp_directory='/tmp/it''s'"
    );
}

#[cfg(unix)]
#[test]
fn a_spill_directory_left_open_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let parent = tempfile::tempdir().expect("parent");
    let mut made = None;
    let error =
        crate::scratch::private_dir_in(parent.path(), crate::scratch::TMPDIR_SETTING, |dir| {
            made = Some(dir.to_owned());
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        })
        .expect_err("a 0755 spill directory");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let text = error.to_string();
    assert!(text.contains("mode 0755"), "{text}");
    assert!(text.contains("TMPDIR"), "{text}");
    assert!(
        !made.expect("created").exists(),
        "the refused directory is removed"
    );
}

#[cfg(unix)]
#[test]
fn a_spill_directory_another_user_owns_is_refused() {
    let parent = tempfile::tempdir().expect("parent");
    let setting = crate::scratch::TMPDIR_SETTING;
    let dir = crate::scratch::private_dir_in(parent.path(), setting, |_| Ok(())).expect("private");
    let euid = rustix::process::geteuid().as_raw();
    crate::scratch::check_private(dir.path(), euid, setting).expect("its own");
    let error = crate::scratch::check_private(dir.path(), euid.wrapping_add(1), setting)
        .expect_err("another uid");
    assert!(error.to_string().contains("mode 0700"), "{error}");
}

#[cfg(test)]
fn timechart_refusal(source: &str, dsl: &str) -> String {
    match timechart_outcome(source, dsl) {
        Err(EngineError::Refused { message }) => message,
        Err(other) => panic!("expected the bucket-type refusal, got {other:?}"),
        Ok(result) => panic!("expected a refusal, got {} rows", result.rows.len()),
    }
}

#[cfg(test)]
fn timechart_outcome(source: &str, dsl: &str) -> Result<QueryResult, EngineError> {
    Executor::new()
        .expect("executor")
        .run_query(dsl, source, &FieldTypes::new(), usize::MAX)
}

#[cfg(test)]
#[test]
fn timechart_input_refuses_each_non_timestamp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    for (column, type_name) in [
        ("hostname", "VARCHAR"),
        ("n", "BIGINT"),
        ("f", "DOUBLE"),
        ("bo", "BOOLEAN"),
        ("d", "DATE"),
    ] {
        assert_eq!(
            timechart_refusal(&source, &format!("* | timechart on {column} count()")),
            format!("timechart on '{column}' is not a timestamp: {type_name}"),
        );
    }

    // A column the pipeline itself minted out of nothing. The type is
    // INTEGER because that is what `DuckDB` declares a bare NULL column
    // to be in a relation — while the same NULL inside the bucket call
    // resolves onto the DATE overload, which is exactly why the bound
    // output could not tell this from a date column and the stage's
    // input can.
    assert_eq!(
        timechart_refusal(&source, "* | let t = null | timechart on t count()"),
        "timechart on 't' is not a timestamp: INTEGER",
        "an untyped NULL binds, so only the input check can catch it"
    );

    for column in ["good", "tz"] {
        let result = Executor::new()
            .expect("executor")
            .run_query(
                &format!("* | timechart on {column} span=1d count()"),
                &source,
                &FieldTypes::new(),
                usize::MAX,
            )
            .unwrap_or_else(|e| panic!("{column} is a bucket source: {e:?}"));
        assert_eq!(result.rows.len(), 2, "one bucket per day, on {column}");
    }
}

#[cfg(test)]
#[test]
fn export_refuses_bad_timechart_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);
    let out = dir.path().join("export.parquet");
    let hot = dir.path().join("hot.ndjson");
    // The envelope columns the hot branch conforms are part of the
    // buffer's contract, `_ingested` included.
    std::fs::write(
        &hot,
        "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
         \"hostname\":\"web-3\"}\n",
    )
    .expect("hot buffer writes");

    let dsl = "* | let t = null | timechart on t count()";
    let expected = "timechart on 't' is not a timestamp: INTEGER";
    let exec = Executor::new().expect("executor");

    let cold = exec
        .export_parquet(dsl, &source, &FieldTypes::new(), &out, usize::MAX)
        .expect_err("the cold export lane refuses it");
    let with_hot = exec
        .export_parquet_with_hot(
            dsl,
            &source,
            hot.to_str().expect("temp path is valid UTF-8"),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            usize::MAX,
        )
        .expect_err("and so does the hot+cold one");

    for outcome in [cold, with_hot] {
        match outcome {
            EngineError::Refused { message } => assert_eq!(message, expected),
            other => panic!("expected the bucket-type refusal, got {other:?}"),
        }
    }
    assert!(
        !out.exists(),
        "a refused export must not leave a file behind"
    );
}

#[cfg(test)]
#[test]
fn timechart_input_checked_before_stats_hides_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    assert_eq!(
        timechart_refusal(
            &source,
            "* | let t = null | timechart on t count() | stats count()"
        ),
        "timechart on 't' is not a timestamp: INTEGER"
    );
}

#[cfg(test)]
#[test]
fn timechart_input_checked_before_default_timechart_retypes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    assert_eq!(
        timechart_refusal(
            &source,
            "* | let t = null | timechart on t count() | timechart count()"
        ),
        "timechart on 't' is not a timestamp: INTEGER"
    );
}

/// A `pivot` can name a column after a data value, so a query whose
/// output carries a HUGEINT called `_time` is perfectly valid. The
/// bucket it actually charted was a TIMESTAMP, and the query succeeds.
#[cfg(test)]
#[test]
fn pivot_made_time_column_is_not_the_bucket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    let result = Executor::new()
        .expect("executor")
        .run_query(
            "* | timechart on good span=1d count() by label | pivot count() on label",
            &source,
            &FieldTypes::new(),
            usize::MAX,
        )
        .expect("a pivot-made _time column is not this query's bucket");
    assert!(
        result.columns.iter().any(|c| c.name == "_time"),
        "the pivot names a column after the `_time` label value: {:?}",
        result.columns
    );
}

#[cfg(test)]
#[test]
fn two_timecharts_attribute_the_failing_stage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    assert_eq!(
        timechart_refusal(
            &source,
            "* | timechart on good count() by hostname | timechart on hostname count()"
        ),
        "timechart on 'hostname' is not a timestamp: VARCHAR",
        "the column that failed, never the one that bucketed fine"
    );
}

#[cfg(test)]
#[test]
fn on_column_named_column_name_is_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);

    assert_eq!(
        timechart_refusal(&source, "* | timechart on column_name count()"),
        "timechart on 'column_name' is not a timestamp: VARCHAR"
    );
}

#[cfg(test)]
#[test]
fn failed_probe_is_not_silence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);
    let dsl = "last=10000d * | timechart on d span=1d count()";

    let message = match timechart_outcome(&source, dsl) {
        // Either shape is a refusal: the probe's own filter fails to
        // bind (a missing column, remapped) before it can report the
        // DATE it would have refused anyway.
        Err(e @ (EngineError::Emit(_) | EngineError::Refused { .. })) => e.to_string(),
        Err(other) => panic!("expected a refusal, got {other:?}"),
        Ok(result) => panic!("expected a refusal, got {} rows", result.rows.len()),
    };
    assert!(
        message.contains("_time") || message.contains("timechart on 'd'"),
        "either the probe's unbindable filter or the DATE bucket, never a \
         successful chart: {message}"
    );
}

#[cfg(test)]
#[test]
fn empty_glob_probe_stays_silent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = format!("{}/nothing/*.parquet", dir.path().display());

    let result = Executor::new()
        .expect("executor")
        .run_query(
            "* | timechart on good count()",
            &empty,
            &FieldTypes::new(),
            usize::MAX,
        )
        .expect("an empty window is an empty result, probe or no probe");
    assert!(result.rows.is_empty(), "{:?}", result.rows);
}

#[cfg(test)]
#[test]
fn type_refusal_survives_hot_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
         \"hostname\":\"web-3\"}\n",
    )
    .expect("hot buffer writes");

    let outcome = Executor::new().expect("executor").run_query_with_hot(
        "* | let t = null | timechart on t count()",
        &source,
        hot.to_str().expect("temp path is valid UTF-8"),
        &hot_columns_of(&hot),
        &FieldTypes::new(),
        &FieldTypes::new(),
        usize::MAX,
    );
    match outcome {
        Err(EngineError::Refused { message }) => assert_eq!(
            message, "timechart on 't' is not a timestamp: INTEGER",
            "the refusal reaches the caller intact"
        ),
        Err(other) => panic!("expected the bucket-type refusal, got {other:?}"),
        Ok(result) => panic!(
            "expected a refusal, got {} rows over {} columns",
            result.rows.len(),
            result.columns.len()
        ),
    }

    // And it is hard at the policy: any verdict but `Fatal` here would
    // send the lane off to re-emit and re-run the whole query hot-only
    // before landing back on the same refusal.
    let refused = || EngineError::Refused {
        message: "timechart on 'x' is not a timestamp: VARCHAR".to_string(),
    };
    assert_eq!(
        classify_query_outcome(Err(&refused())),
        HotColdOutcome::Fatal,
        "the query lane must not treat a refusal as a missing column"
    );
    assert_eq!(
        classify_export_outcome(&Err(refused())),
        HotColdOutcome::Fatal,
        "nor the export lane"
    );
}

/// A column neither source has is no error with a hot buffer either: it
/// reads as NULL over the union (ADR-0062), so `where` keeps no row and the
/// answer still carries the union's columns.
#[cfg(test)]
#[test]
fn missing_column_still_benign_with_hot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = timechart_fixture(&dir);
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
         \"hostname\":\"web-3\"}\n",
    )
    .expect("hot buffer writes");

    let clock = PhaseClock::new();
    let result = Executor::new()
        .expect("executor")
        .cancellable(&CancelLatch::never())
        .timed(&clock)
        .run_query_with_hot(
            "* | where nosuchcolumn == 1",
            &source,
            hot.to_str().expect("temp path is valid UTF-8"),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            RowCap::Refuse(usize::MAX),
        )
        .expect("an absent column reads NULL with a hot buffer too");
    assert!(result.rows.is_empty(), "{:?}", result.rows);
    assert!(
        result.columns.iter().any(|c| c.name == "hostname"),
        "the union's columns: {:?}",
        result.columns
    );
    let totals = clock.close_at(std::time::Instant::now()).expect("on");
    assert_eq!(totals.attempts, 1, "one bind, no retry");
    assert_eq!(totals.fallback, Fallback::None);
}

/// A source with no `_raw` column at all: user-owned parquet read in
/// embedded mode. A text search there reads `_raw` as NULL (ADR-0062), so
/// it searches `message` alone, and the stage's probe has a `hostname` to
/// ask `DuckDB` about.
#[cfg(test)]
fn rawless_fixture(dir: &tempfile::TempDir) -> String {
    let path = dir
        .path()
        .join("rawless.parquet")
        .to_str()
        .expect("temp path is valid UTF-8")
        .to_string();
    Connection::open_in_memory()
        .expect("in-memory duckdb")
        .execute_batch(&format!(
            "COPY (SELECT * FROM (VALUES ('needle', 'web-1'), ('haystack', 'web-2')) \
             AS t(message, hostname)) TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("fixture writes");
    path
}

#[cfg(test)]
#[test]
fn refusal_survives_a_source_without_raw() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = rawless_fixture(&dir);

    assert_eq!(
        timechart_refusal(&source, "needle | timechart on hostname count()"),
        "timechart on 'hostname' is not a timestamp: VARCHAR",
        "the bucket's own refusal, not the absent `_raw`"
    );
}

#[cfg(test)]
#[test]
fn refusal_survives_hot_only_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
         \"hostname\":\"web-3\"}\n",
    )
    .expect("hot buffer writes");
    // No parquet behind it: the cold read matches no files, so the
    // outcome policy answers `HotOnly`.
    let cold = format!("{}/nothing/*.parquet", dir.path().display());

    let outcome = Executor::new().expect("executor").run_query_with_hot(
        "* | let t = null | timechart on t count()",
        &cold,
        hot.to_str().expect("temp path is valid UTF-8"),
        &hot_columns_of(&hot),
        &FieldTypes::new(),
        &FieldTypes::new(),
        usize::MAX,
    );
    match outcome {
        Err(EngineError::Refused { message }) => assert_eq!(
            message, "timechart on 't' is not a timestamp: INTEGER",
            "the hot-only retry refuses what the union attempt never got to see"
        ),
        Err(other) => panic!("expected the bucket-type refusal, got {other:?}"),
        Ok(result) => panic!(
            "expected a refusal, got {} rows over {} columns",
            result.rows.len(),
            result.columns.len()
        ),
    }
}

#[cfg(test)]
#[test]
fn export_refusal_survives_hot_only_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hot = dir.path().join("hot.ndjson");
    std::fs::write(
        &hot,
        "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
         \"hostname\":\"web-3\"}\n",
    )
    .expect("hot buffer writes");
    let cold = format!("{}/nothing/*.parquet", dir.path().display());
    let out = dir.path().join("export.parquet");

    let outcome = Executor::new()
        .expect("executor")
        .export_parquet_with_hot(
            "* | let t = null | timechart on t count()",
            &cold,
            hot.to_str().expect("temp path is valid UTF-8"),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            usize::MAX,
        )
        .expect_err("the hot-only export refuses the bucket source too");
    match outcome {
        EngineError::Refused { message } => {
            assert_eq!(message, "timechart on 't' is not a timestamp: INTEGER");
        }
        other => panic!("expected the bucket-type refusal, got {other:?}"),
    }
    assert!(
        !out.exists(),
        "a refused export must not leave a file behind"
    );
}

#[cfg(test)]
#[test]
fn bucket_types_accepted_and_named() {
    for id in [
        LogicalTypeId::Timestamp,
        LogicalTypeId::TimestampS,
        LogicalTypeId::TimestampMs,
        LogicalTypeId::TimestampNs,
        LogicalTypeId::TimestampTZ,
    ] {
        assert!(bucket_input_is_timestamp(id), "{id:?} must chart");
    }
    for (id, name) in [
        (LogicalTypeId::Date, "DATE"),
        (LogicalTypeId::SqlNull, "NULL"),
        (LogicalTypeId::Varchar, "VARCHAR"),
        (LogicalTypeId::StringLiteral, "VARCHAR"),
        (LogicalTypeId::IntegerLiteral, "INTEGER"),
        (LogicalTypeId::Bigint, "BIGINT"),
        (LogicalTypeId::Double, "DOUBLE"),
        (LogicalTypeId::Time, "TIME"),
        (LogicalTypeId::Interval, "INTERVAL"),
        (LogicalTypeId::Boolean, "BOOLEAN"),
    ] {
        assert!(!bucket_input_is_timestamp(id), "{id:?} must not chart");
        assert_eq!(duckdb_type_name(id), name, "{id:?}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use duckdb::Connection;

    use super::{
        ColdAction, ColdPresence, EngineError, EvalContext, Executor, FieldTypes, HotColdOutcome,
        HotLane, ListEvidence, RowCap, cold_action, emitter, error_class, export_row_limit,
        glob_list_items, has_glob_meta, hot_columns_of, is_conversion_error, is_no_files_error,
        literal_path_is_file, resolve_list_source,
    };
    use crate::cancel::CancelLatch;
    use crate::timing::{Fallback, PhaseClock, PhaseTotals, QueryPhase, Transition};

    /// The cell is larger than the writer's buffer, so the staging write
    /// fails inside `serde_json::to_writer` itself.
    #[test]
    fn a_capture_staging_write_failure_is_an_io_error() {
        use crate::value::{Column, ColumnType, QueryResult, Value};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stage.ndjson");
        std::fs::write(&path, b"").unwrap();
        // Opened read-only, so every write fails with EBADF.
        let file = std::fs::File::open(&path).unwrap();
        let result = QueryResult {
            columns: vec![Column::new("big", ColumnType::String)],
            rows: vec![vec![Value::String("x".repeat(64 * 1024))]],
        };

        let err = super::write_capture_ndjson(&result, &[super::StoredAs::Native], &file)
            .expect_err("a write to a read-only handle must fail");
        assert!(
            matches!(err, EngineError::Io(_)),
            "a staging write failure must be an I/O error, got {err:?}"
        );
    }

    /// `/dev/full` opens fine and fails every write with ENOSPC, and
    /// the cell is larger than the writer's buffer, so the failure surfaces
    /// from `serde_json::to_writer` rather than from `File::create`.
    #[test]
    fn a_scheduled_result_write_failure_is_an_io_error() {
        use crate::value::{Column, ColumnType, QueryResult, Value};

        let result = QueryResult {
            columns: vec![Column::new("big", ColumnType::String)],
            rows: vec![vec![Value::String("x".repeat(64 * 1024))]],
        };

        let err = super::write_wire_ndjson(&result, std::path::Path::new("/dev/full"))
            .expect_err("a write to /dev/full must fail");
        assert!(
            matches!(err, EngineError::Io(_)),
            "a scheduled result write failure must be an I/O error, got {err:?}"
        );
    }

    fn json_errors_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::ERRORS_AS_JSON_SQL).unwrap();
        conn
    }

    fn write_meta_parquet(conn: &Connection, path: &Path, meta_expr: &str) {
        conn.execute_batch(&format!(
            "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"_time\", \
                          'svc' AS service, {meta_expr} AS meta) \
             TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }

    fn read_parquet_remap_error(dir: &Path) -> duckdb::Error {
        let conn = json_errors_conn();
        let struct_file = dir.join("struct.parquet");
        let varchar_file = dir.join("varchar.parquet");
        write_meta_parquet(&conn, &struct_file, "{'a': 1}");
        write_meta_parquet(&conn, &varchar_file, "'plain'");
        let glob = format!("{}/*.parquet", dir.display());
        conn.prepare("SELECT * FROM read_parquet(?, union_by_name=true)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([glob.as_str()])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("STRUCT-vs-VARCHAR read_parquet union must error")
    }

    fn union_conversion_error() -> duckdb::Error {
        let conn = json_errors_conn();
        conn.prepare("SELECT {'a': 1} AS meta UNION ALL BY NAME SELECT 'plain' AS meta")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("STRUCT-vs-VARCHAR UNION ALL BY NAME must error")
    }

    fn corruption_error(dir: &Path) -> duckdb::Error {
        let conn = json_errors_conn();
        let bad = dir.join("truncated.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();
        let path = format!("{}", bad.display());
        conn.prepare("SELECT * FROM read_parquet(?)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([path.as_str()])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("truncated parquet must error")
    }

    fn missing_column_error() -> duckdb::Error {
        let conn = json_errors_conn();
        conn.prepare("SELECT nonexistent_col FROM (SELECT 1 AS x)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("missing-column query must error")
    }

    fn data_conversion_error() -> duckdb::Error {
        let conn = json_errors_conn();
        conn.prepare("SELECT CAST(x AS INTEGER) FROM (SELECT unnest(['1', 'abc']) AS x)")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err("casting 'abc' to INTEGER must error")
    }

    #[test]
    fn is_conversion_error_matches_real_strings() {
        let dir = tempfile::tempdir().unwrap();

        let remap = read_parquet_remap_error(dir.path());
        assert!(
            is_conversion_error(&remap),
            "read_parquet remap error should classify as a conversion error: {remap}"
        );

        let conversion = union_conversion_error();
        assert!(
            is_conversion_error(&conversion),
            "UNION ALL BY NAME conversion error should classify as a conversion error: {conversion}"
        );

        let corrupt = corruption_error(dir.path());
        assert!(
            !is_conversion_error(&corrupt),
            "corruption error must NOT classify as a conversion error (handled by quarantine): {corrupt}"
        );

        let missing = missing_column_error();
        assert!(
            !is_conversion_error(&missing),
            "a missing-column error must NOT classify as a conversion error: {missing}"
        );
    }

    #[test]
    fn genuine_data_conversion_error_carries_the_conversion_class() {
        let data = data_conversion_error();
        assert_eq!(
            error_class(&data).as_deref(),
            Some("Conversion"),
            "a genuine data-conversion error is Conversion-class too: {data}"
        );
        assert!(is_conversion_error(&data));
        assert!(is_conversion_error(&union_conversion_error()));
    }

    #[test]
    fn conflicting_kind_mixes_all_carry_the_conversion_class() {
        // Compaction's rollup and merge fallbacks trigger on the conversion
        // class alone (there is no second source to describe until the
        // fallback runs), so the class must cover every cross-file mix. Pin
        // the full kind matrix on both the read_parquet and UNION ALL BY
        // NAME paths.
        let mixes = [
            ("{'a': 1}", "'plain'"),
            ("{'a': 1}", "[1, 2]"),
            ("{'a': 1}", "42"),
            ("[1, 2]", "'plain'"),
            ("'plain'", "{'a': 1}"),
        ];
        for (a, b) in mixes {
            let dir = tempfile::tempdir().unwrap();
            let conn = json_errors_conn();
            write_meta_parquet(&conn, &dir.path().join("a.parquet"), a);
            write_meta_parquet(&conn, &dir.path().join("b.parquet"), b);
            let glob = format!("{}/*.parquet", dir.path().display());

            let pq = conn
                .prepare("SELECT * FROM read_parquet(?, union_by_name=true)")
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([glob.as_str()])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("an irreconcilable kind mix must error on read_parquet");
            assert!(
                is_conversion_error(&pq),
                "read_parquet mix {a} vs {b} must trigger the cast fallback: {pq}"
            );

            let un = conn
                .prepare(&format!(
                    "SELECT {a} AS meta UNION ALL BY NAME SELECT {b} AS meta"
                ))
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("an irreconcilable kind mix must error on UNION ALL BY NAME");
            assert!(
                is_conversion_error(&un),
                "union mix {a} vs {b} must trigger the cast fallback: {un}"
            );
        }
    }

    #[test]
    fn irreconcilable_drift_describes_raw_and_errors_loudly_at_read() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("a.parquet"), "{'a': 1}");
        write_meta_parquet(&setup, &dir.path().join("b.parquet"), "'plain'");

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());

        let schema = exec
            .describe_schema(&glob)
            .expect("DESCRIBE reads no rows, so it resolves without erroring");
        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert!(
            meta.data_type.to_ascii_uppercase().starts_with("STRUCT"),
            "describe reports the footer union raw (the complex side), \
             never a coerced VARCHAR; got `{}`",
            meta.data_type
        );

        let result = exec.run_query("* | fields meta", &glob, &FieldTypes::new(), usize::MAX);
        assert!(
            matches!(result, Err(EngineError::Database(ref e)) if is_conversion_error(e)),
            "a query over the irreconcilable mix must error loudly (the \
             coerced VARCHAR retry is deleted); got {result:?}"
        );
    }

    #[test]
    fn describe_schema_keeps_union_able_struct_drift() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("a.parquet"), "{'x': 1}");
        write_meta_parquet(&setup, &dir.path().join("b.parquet"), "{'y': 2}");

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());
        let schema = exec
            .describe_schema(&glob)
            .expect("describe_schema must tolerate union-able struct drift");

        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the `meta` column");
        assert!(
            meta.data_type.to_ascii_uppercase().starts_with("STRUCT"),
            "union-able cross-file STRUCT drift must report the merged STRUCT \
             type (a real query reads it as a STRUCT), got `{}`",
            meta.data_type
        );

        let result = exec
            .run_query("* | fields meta", &glob, &FieldTypes::new(), usize::MAX)
            .expect("SELECT meta over the merged-STRUCT glob must succeed");
        assert_eq!(
            result.row_count(),
            2,
            "both rows must survive the merged-STRUCT union read"
        );
    }

    #[test]
    fn run_query_with_hot_returns_hot_on_cold_start() {
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/nonexistent/*.parquet", dir.path().display());
        let result = exec
            .run_query_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &hot_columns_of(&hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
            )
            .expect("cold-start query must return hot rows, not error");
        assert_eq!(
            result.row_count(),
            1,
            "the single hot row must survive the cold-start hot-only fallback"
        );
    }

    #[test]
    fn duckdb_error_classes_parsed_from_live_errors() {
        let dir = tempfile::tempdir().unwrap();

        // On the bundled 1.5.5 the per-file remap variant is also a
        // Conversion-class error ("failed to cast column ..."), not a Binder
        // "Struct remap" one, which is why the classifier needs no Binder
        // arm at all.
        let remap = read_parquet_remap_error(dir.path());
        assert_eq!(
            error_class(&remap).as_deref(),
            Some("Conversion"),
            "read_parquet remap error carries the Conversion class: {remap}"
        );

        let conversion = union_conversion_error();
        assert_eq!(
            error_class(&conversion).as_deref(),
            Some("Conversion"),
            "UNION ALL BY NAME conflict carries the Conversion class: {conversion}"
        );

        let corrupt = corruption_error(dir.path());
        assert_ne!(
            error_class(&corrupt).as_deref(),
            Some("Conversion"),
            "corruption must not carry the Conversion class: {corrupt}"
        );

        let missing = missing_column_error();
        assert_eq!(
            error_class(&missing).as_deref(),
            Some("Binder"),
            "missing-column error carries the Binder class: {missing}"
        );
    }

    #[test]
    fn trigger_variants_produce_conversion_class_errors() {
        let dir = tempfile::tempdir().unwrap();
        let conn = json_errors_conn();
        let variants = [
            r#""not-a-date""#,
            r#""2026-13-45T99:99:99Z""#,
            r#"{"nested":1}"#,
            "12345",
        ];
        for (i, variant) in variants.iter().enumerate() {
            let f = dir.path().join(format!("trig{i}.ndjson"));
            std::fs::write(&f, format!(r#"{{"timestamp":{variant},"service":"svc"}}"#)).unwrap();
            let err = conn
                .prepare(&format!(
                    "SELECT * REPLACE (CAST(\"timestamp\" AS TIMESTAMP) AS \"timestamp\") \
                     FROM read_json(['{}'], format='newline_delimited', records=true, \
                     auto_detect=true, union_by_name=true, field_appearance_threshold=0, \
                     maximum_depth=2)",
                    f.display()
                ))
                .and_then(|mut stmt| {
                    let mut rows = stmt.query([])?;
                    while rows.next()?.is_some() {}
                    Ok(())
                })
                .expect_err("a hard CAST over a malformed timestamp must error");
            assert_eq!(
                error_class(&err).as_deref(),
                Some("Conversion"),
                "variant {variant} must carry the Conversion class: {err}"
            );
        }
    }

    #[test]
    fn cold_action_matrix_is_exhaustive() {
        use ColdAction::{ColdDataUnread as Unread, HotOnly, ReturnOutcome as Return};
        use ColdPresence::{Absent as ColdAbsent, Present as ColdPresent};
        use HotColdOutcome::{Columns, Fatal, NoColumns, Recoverable};
        use HotLane::{Absent as NoHot, Present as Hot};

        let matrix = [
            // (outcome, cold, hot, action, why)
            (Columns, ColdPresent, Hot, Return, "authoritative result"),
            (Columns, ColdAbsent, Hot, Return, "authoritative result"),
            (Columns, ColdPresent, NoHot, Return, "authoritative result"),
            (Columns, ColdAbsent, NoHot, Return, "authoritative result"),
            (Fatal, ColdPresent, Hot, Return, "never masked"),
            (Fatal, ColdAbsent, Hot, Return, "never masked"),
            (Fatal, ColdPresent, NoHot, Return, "never masked"),
            (Fatal, ColdAbsent, NoHot, Return, "never masked"),
            (
                NoColumns,
                ColdPresent,
                Hot,
                Unread,
                "cold data would vanish",
            ),
            (NoColumns, ColdAbsent, Hot, HotOnly, "genuine cold start"),
            (
                NoColumns,
                ColdPresent,
                NoHot,
                Unread,
                "cold data would vanish",
            ),
            (
                NoColumns,
                ColdAbsent,
                NoHot,
                Return,
                "genuinely empty window",
            ),
            (
                Recoverable,
                ColdPresent,
                Hot,
                Return,
                "failure over cold data",
            ),
            (Recoverable, ColdAbsent, Hot, HotOnly, "nothing to hide"),
            (
                Recoverable,
                ColdPresent,
                NoHot,
                Return,
                "failure over cold data",
            ),
            (
                Recoverable,
                ColdAbsent,
                NoHot,
                Return,
                "failure, no hot lane",
            ),
        ];
        for (outcome, cold, hot, expected, why) in matrix {
            assert_eq!(
                cold_action(outcome, cold, hot),
                expected,
                "{outcome:?} × cold={cold:?} × hot={hot:?} must be {expected:?} ({why})"
            );
        }

        // What reaches the table: an unknown field is never a reason to
        // read less data (ADR-0062). Every lane remaps it to `Emit`, the
        // export at staging, and the emitter's own refusals are `Emit` too.
        // Every one of them is the caller's answer.
        let emit = || {
            EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
                message: "unknown field: nosuch".to_owned(),
            })
        };
        assert_eq!(super::classify_query_outcome(Err(&emit())), Fatal);
        assert_eq!(super::classify_export_outcome(&Err(emit())), Fatal);
        let staged = super::staging_failure(missing_column_error());
        match &staged {
            EngineError::Emit(e) => assert_eq!(
                e.to_string(),
                "unsupported operation: unknown field: nonexistent_col"
            ),
            other => panic!("a missing column at staging is the unknown field: {other:?}"),
        }
        assert_eq!(
            super::classify_export_outcome(&Err(staged)),
            Fatal,
            "a missing column at staging is never retried against less data"
        );
    }

    #[test]
    fn the_happy_path_never_pays_the_presence_check() {
        for hot in [HotLane::Present, HotLane::Absent] {
            assert!(
                !HotColdOutcome::Columns.consults_cold_presence(hot),
                "a columnful result must not provoke a presence glob"
            );
            assert!(
                !HotColdOutcome::Fatal.consults_cold_presence(hot),
                "a propagated error must not provoke a presence glob"
            );
            assert!(
                HotColdOutcome::NoColumns.consults_cold_presence(hot),
                "a no-files answer is exactly what the presence check settles"
            );
        }
        assert!(
            HotColdOutcome::Recoverable.consults_cold_presence(HotLane::Present),
            "a hot lane may degrade an unexpected failure — only presence says whether"
        );
        assert!(
            !HotColdOutcome::Recoverable.consults_cold_presence(HotLane::Absent),
            "without a hot lane the failure propagates either way"
        );
    }

    #[test]
    fn unexpected_cold_failure_returns_error_not_hot_only() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("truncated.parquet");
        std::fs::write(&bad, b"PAR1\x00\x00").unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let result = exec.run_query_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
        );
        assert!(
            result.is_err(),
            "an unreadable cold file must surface as an error, not hot-only success"
        );
    }

    /// A field neither the cold files nor the hot snapshot carry reads as
    /// NULL over both of them (ADR-0062): one bind of the union, an answer
    /// with the union's columns, and no hot-only read that would leave the
    /// cold rows out.
    #[test]
    fn missing_column_with_cold_files_returns_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("cold.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("{}/*.parquet", dir.path().display());
        let run = |dsl: &str| {
            let clock = PhaseClock::new();
            let result = exec
                .cancellable(&CancelLatch::never())
                .timed(&clock)
                .run_query_with_hot(
                    dsl,
                    &source,
                    hot.to_str().unwrap(),
                    &hot_columns_of(&hot),
                    &FieldTypes::new(),
                    &FieldTypes::new(),
                    RowCap::Refuse(usize::MAX),
                )
                .unwrap_or_else(|e| panic!("{dsl}: an absent field reads NULL, not {e:?}"));
            let totals = close(&clock);
            assert_eq!(totals.attempts, 1, "{dsl}: one bind of the union");
            assert_eq!(totals.fallback, Fallback::None, "{dsl}: no hot-only read");
            assert!(
                result.columns.iter().any(|c| c.name == "service"),
                "{dsl}: the union's own columns, not an empty stand-in: {:?}",
                result.columns
            );
            result
        };

        assert_eq!(
            run("nonexistent_field=value").row_count(),
            0,
            "NULL = 'value' matches nothing"
        );
        assert_eq!(
            run("nonexistent_field!=value").row_count(),
            2,
            "NULL != 'value' matches the cold row and the hot row"
        );
    }

    #[test]
    fn glob_list_items_splits_list_sources() {
        assert_eq!(glob_list_items("/data/**/*.parquet"), None);
        assert_eq!(
            glob_list_items("['/data/2024-01-15/10/*.parquet']"),
            Some(vec!["/data/2024-01-15/10/*.parquet"])
        );
        assert_eq!(
            glob_list_items(" ['/a/*.parquet', '/b/*.parquet'] "),
            Some(vec!["/a/*.parquet", "/b/*.parquet"])
        );
        // Commas inside a quoted element stay part of that element.
        assert_eq!(
            glob_list_items("['/a,b/*.parquet']"),
            Some(vec!["/a,b/*.parquet"])
        );
        // Unparseable list shapes yield an empty vec → "assume present".
        assert_eq!(glob_list_items("['/a/*.parquet"), Some(Vec::new()));
        assert_eq!(glob_list_items("['/a/*.parquet]"), Some(Vec::new()));
    }

    #[test]
    fn has_glob_meta_splits_the_matcher() {
        assert!(!has_glob_meta("/data/prod/2024-01-15/10/nginx.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/10/*.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/1?/nginx.parquet"));
        assert!(has_glob_meta("/data/prod/2024-01-15/[01]0/nginx.parquet"));
    }

    #[test]
    #[cfg(unix)]
    fn fs_matcher_agrees_with_duckdb_glob_on_literals() {
        let dir = tempfile::tempdir().unwrap();
        let exec = Executor::new().unwrap();
        let setup = Connection::open_in_memory().unwrap();

        let present = dir.path().join("present.parquet");
        write_meta_parquet(&setup, &present, "'plain'");

        let missing = dir.path().join("missing.parquet");

        let dir_named_parquet = dir.path().join("dir.parquet");
        std::fs::create_dir(&dir_named_parquet).unwrap();

        let good_link = dir.path().join("good-link.parquet");
        std::os::unix::fs::symlink(&present, &good_link).unwrap();
        let broken_link = dir.path().join("broken-link.parquet");
        std::os::unix::fs::symlink(&missing, &broken_link).unwrap();

        let shapes = [
            (present.clone(), true, "an existing parquet file"),
            (missing.clone(), false, "a missing path"),
            (dir_named_parquet, false, "a directory named like a parquet"),
            (good_link, true, "a symlink to a real file"),
            (broken_link, false, "a broken symlink"),
        ];

        for (path, expected, what) in shapes {
            let path = path.to_str().unwrap();
            assert_eq!(
                literal_path_is_file(path),
                expected,
                "fs matcher disagrees with the pinned answer for {what}"
            );
            assert_eq!(
                exec.glob_has_match(path),
                expected,
                "DuckDB glob() disagrees with the fs matcher for {what}"
            );
        }
    }

    #[test]
    fn literal_path_is_file_retains_on_a_non_notfound_error() {
        // A path component that is a file makes the lookup ENOTDIR, not
        // ENOENT.
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        let file = dir.path().join("present.parquet");
        write_meta_parquet(&setup, &file, "'plain'");

        let through_a_file = file.join("nested.parquet");
        let err = std::fs::metadata(&through_a_file).expect_err("must not resolve");
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "fixture must provoke a non-NotFound error"
        );
        assert!(
            literal_path_is_file(through_a_file.to_str().unwrap()),
            "a non-NotFound metadata error must retain the element"
        );
    }

    #[test]
    fn cold_presence_reads_resolved_list_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let empty_hour = dir.path().join("10");
        std::fs::create_dir_all(&empty_hour).unwrap();
        let exec = Executor::new().unwrap();

        let absent = format!("['{}/*.parquet']", empty_hour.display());
        let resolved = exec.resolve_source(&absent);
        assert_eq!(
            resolved.evidence,
            ListEvidence::NoneMatching,
            "a list whose globs match no file resolves to no evidence of files"
        );
        assert_eq!(
            exec.cold_presence(&resolved),
            ColdPresence::Absent,
            "a list whose globs match no file must report no cold files"
        );

        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &empty_hour.join("cold.parquet"), "'plain'");
        let resolved = exec.resolve_source(&absent);
        assert_eq!(
            resolved.evidence,
            ListEvidence::Matching,
            "a list whose globs match a file resolves to matching evidence"
        );
        assert_eq!(
            exec.cold_presence(&resolved),
            ColdPresence::Present,
            "a list whose globs match a file must report cold files present"
        );
    }

    #[test]
    fn plain_glob_resolves_lazily_and_is_globbed_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());

        let resolved = exec.resolve_source(&glob);
        assert_eq!(resolved.evidence, ListEvidence::NotAList);
        assert_eq!(
            resolved.sql, glob,
            "a plain glob is passed through verbatim"
        );
        assert_eq!(exec.cold_presence(&resolved), ColdPresence::Absent);

        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("cold.parquet"), "'plain'");
        assert_eq!(
            exec.cold_presence(&exec.resolve_source(&glob)),
            ColdPresence::Present
        );
    }

    #[test]
    fn subquery_source_is_never_a_list() {
        let source = "(SELECT * FROM read_parquet('/data/**/*.parquet'))";
        let resolved = resolve_list_source(source, |_| panic!("must not glob a subquery source"));
        assert_eq!(resolved.evidence, ListEvidence::NotAList);
        assert_eq!(resolved.sql, source);
    }

    #[test]
    fn resolve_list_source_narrows_to_matching_elements() {
        let source = "['/a/*.parquet', '/b/*.parquet', '/c/*.parquet']";
        let resolved = resolve_list_source(source, |item| item != "/b/*.parquet");
        assert_eq!(resolved.evidence, ListEvidence::Matching);
        assert_eq!(resolved.sql, "['/a/*.parquet', '/c/*.parquet']");
    }

    #[test]
    fn resolve_list_source_preserves_order_and_duplicates() {
        let source = "['/z/*.parquet', '/a/*.parquet', '/z/*.parquet', '/m/*.parquet']";
        let resolved = resolve_list_source(source, |item| item != "/m/*.parquet");
        assert_eq!(
            resolved.sql,
            "['/z/*.parquet', '/a/*.parquet', '/z/*.parquet']"
        );
    }

    #[test]
    fn resolve_list_source_keeps_the_original_when_nothing_is_narrowed() {
        let source = "['/a/*.parquet', '/b/*.parquet']";

        let all = resolve_list_source(source, |_| true);
        assert_eq!(all.evidence, ListEvidence::Matching);
        assert_eq!(all.sql, source);

        let none = resolve_list_source(source, |_| false);
        assert_eq!(none.evidence, ListEvidence::NoneMatching);
        assert_eq!(none.sql, source);
    }

    #[test]
    fn resolve_list_source_fails_closed_on_an_unparseable_list() {
        for source in ["['/a/*.parquet", "['/a/*.parquet]", "[]"] {
            let resolved = resolve_list_source(source, |_| {
                panic!("an unparseable list must not be globbed")
            });
            assert_eq!(
                resolved.evidence,
                ListEvidence::Matching,
                "unparseable list `{source}` must fail closed"
            );
            assert_eq!(resolved.sql, source, "and must be emitted verbatim");
        }
    }

    #[test]
    fn partial_list_source_miss_keeps_cold_rows() {
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        let result = exec
            .run_query_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &hot_columns_of(&hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
            )
            .expect("a partial list-source miss must not fail the query");
        assert_eq!(
            result.row_count(),
            2,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, alongside the hot row"
        );
    }

    #[test]
    fn partial_list_source_miss_keeps_cold_rows_without_a_hot_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        let result = exec
            .run_query("*", &source, &FieldTypes::new(), usize::MAX)
            .expect("a partial list-source miss must not fail the query");
        assert_eq!(
            result.row_count(),
            1,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, with no hot buffer to rescue it"
        );
    }

    #[test]
    fn export_without_hot_keeps_cold_rows_on_partial_list_miss() {
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        exec.export_parquet("*", &source, &FieldTypes::new(), &out, 1000)
            .expect("a partial list-source miss must not fail the export");

        let rows: i64 = exec
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    out.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "the surviving cold row must reach the export");
    }

    #[test]
    fn empty_window_without_a_hot_buffer_is_an_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec
            .run_query("*", &source, &FieldTypes::new(), usize::MAX)
            .expect("an empty window must be an empty success, not an error");
        assert_eq!(result.row_count(), 0, "an empty window has no rows");
    }

    #[test]
    #[cfg(unix)]
    fn a_field_reading_query_over_an_empty_window_is_an_empty_success() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();

        let exec = Executor::new().unwrap();
        for root in [&hour, &dangling] {
            let list = format!("['{}/*.parquet']", root.display());
            let glob = format!("{}/*.parquet", root.display());
            for (source, described) in [(&list, false), (&glob, true)] {
                for dsl in [
                    "service=svc",
                    r#"earliest="2024-01-15T09:00:00Z" latest="2024-01-15T11:00:00Z" service=svc | sort -_time, -_ingested"#,
                    "host=web1 OR f!=x | table host, f",
                    "* | where isnull(f) | stats count() by g",
                ] {
                    let before = super::schema_probe::count_on_this_thread();
                    let result = exec
                        .run_query(dsl, source, &FieldTypes::new(), usize::MAX)
                        .unwrap_or_else(|e| panic!("{dsl} over {source}: {e:?}"));
                    assert_eq!(
                        super::schema_probe::count_on_this_thread() - before,
                        u64::from(described),
                        "{dsl} over {source}: a list resolution proved empty is not described"
                    );
                    assert_eq!(result.row_count(), 0, "{dsl} over {source}: no rows");
                    assert!(
                        result.columns.is_empty(),
                        "{dsl} over {source}: no file, no column: {:?}",
                        result.columns
                    );
                }
            }
        }
    }

    #[test]
    fn export_without_hot_stays_loud_for_an_empty_window() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec.export_parquet("*", &source, &FieldTypes::new(), &out, 1000);
        assert!(
            matches!(result, Err(EngineError::Database(ref e)) if is_no_files_error(e)),
            "an export over an empty window keeps DuckDB's own error; got {result:?}"
        );
    }

    #[test]
    fn no_files_outcome_with_cold_data_errors_instead_of_empty_success() {
        // The missing hot snapshot provokes the no-files error the way
        // production does while the cold element still matches, so narrowing
        // cannot rescue it and the outcome policy is the last line of defence.
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &hour.join("cold.parquet"), "'plain'");
        let missing_hot = dir.path().join("hot.ndjson"); // never written

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        let result = exec.run_query_with_hot(
            "*",
            &source,
            missing_hot.to_str().unwrap(),
            &hot_columns_of(&missing_hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            usize::MAX,
        );
        assert!(
            matches!(result, Err(crate::error::EngineError::ColdDataUnread)),
            "cold data on disk plus a no-files union must be an explicit \
             error, not an empty success; got {result:?}"
        );
    }

    #[test]
    fn export_with_hot_falls_back_to_hot_only_for_empty_list_source() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!("['{}/*.parquet']", hour.display());
        exec.export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
        .expect("hot-only export must succeed when the cold list matches no file");

        let rows: i64 = exec
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    out.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "the exported parquet must carry the hot row");
    }

    #[test]
    fn export_with_hot_keeps_cold_rows_on_partial_list_miss() {
        let dir = tempfile::tempdir().unwrap();
        let full_hour = dir.path().join("10");
        let empty_hour = dir.path().join("11");
        std::fs::create_dir_all(&full_hour).unwrap();
        std::fs::create_dir_all(&empty_hour).unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &full_hour.join("svc.parquet"), "'plain'");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:01Z\",\"_ingested\":\"2024-01-15T10:00:01Z\",\"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        let out = dir.path().join("export.parquet");

        let exec = Executor::new().unwrap();
        let source = format!(
            "['{}/*.parquet', '{}/*.parquet']",
            full_hour.display(),
            empty_hour.display()
        );
        exec.export_parquet_with_hot(
            "*",
            &source,
            hot.to_str().unwrap(),
            &hot_columns_of(&hot),
            &FieldTypes::new(),
            &FieldTypes::new(),
            &out,
            1000,
        )
        .expect("a partial list-source miss must not fail the export");

        let rows: i64 = exec
            .conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    out.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rows, 2,
            "the cold row behind the matching glob element must survive an \
             empty sibling element, alongside the hot row"
        );
    }

    #[test]
    fn describe_schema_reconciles_union_able_scalar_drift() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Connection::open_in_memory().unwrap();
        write_meta_parquet(&setup, &dir.path().join("00.parquet"), "CAST(1 AS BIGINT)");
        write_meta_parquet(
            &setup,
            &dir.path().join("01.parquet"),
            "CAST(1.5 AS DOUBLE)",
        );

        let exec = Executor::new().unwrap();
        let glob = format!("{}/*.parquet", dir.path().display());
        let schema = exec
            .describe_schema(&glob)
            .expect("union-able scalar drift must describe cleanly");

        let meta = schema
            .columns
            .iter()
            .find(|c| c.name == "meta")
            .expect("schema must include the drifted `meta` column");
        assert_eq!(
            meta.data_type.to_ascii_uppercase(),
            "DOUBLE",
            "describe must report DuckDB's reconciled type (DOUBLE), not \
             the first-seen BIGINT, got `{}`",
            meta.data_type
        );
    }

    #[test]
    fn export_row_limit_sentinel_is_target_independent() {
        assert_eq!(
            export_row_limit(usize::MAX),
            None,
            "`usize::MAX` is the unbounded sentinel on every target"
        );

        assert_eq!(export_row_limit(0), Some(0));
        assert_eq!(export_row_limit(2), Some(2));
        assert_eq!(export_row_limit(5_000), Some(5_000));

        // `i64::MAX` is the largest cap DuckDB can name in a LIMIT. Ask
        // only where a `usize` can hold it — on a 32-bit target the value
        // does not exist, and the sentinel correctly answers first.
        if let Ok(top) = usize::try_from(i64::MAX) {
            assert_eq!(export_row_limit(top), Some(i64::MAX));
        }

        // Out of the INT64 domain but not the sentinel (`max_export_rows`
        // is operator-set). Only reachable where `usize` is wider than
        // `i64`'s positive range.
        if let Ok(above) = usize::try_from(1_u128 << 63) {
            assert_eq!(export_row_limit(above), None);
        }
    }

    #[test]
    fn row_cap_statement_pushes_truncate_into_the_sql() {
        let sql = "SELECT * FROM read_parquet(?) WHERE \"service\" = ? ORDER BY \"_time\"";

        assert_eq!(
            RowCap::Truncate {
                rows: 10,
                tail_input: 50,
            }
            .statement(sql),
            format!("SELECT * FROM ({sql}) LIMIT 10"),
        );
        assert_eq!(
            RowCap::Truncate {
                rows: 0,
                tail_input: 50,
            }
            .statement(sql),
            format!("SELECT * FROM ({sql}) LIMIT 0"),
        );

        for max_rows in [5, 100_000, usize::MAX] {
            assert_eq!(RowCap::Refuse(max_rows).statement(sql), sql);
        }

        assert_eq!(
            RowCap::Truncate {
                rows: usize::MAX,
                tail_input: usize::MAX,
            }
            .statement(sql),
            sql
        );

        let wrapped = RowCap::Truncate {
            rows: 10,
            tail_input: 50,
        }
        .statement(sql);
        assert_eq!(wrapped.matches('?').count(), sql.matches('?').count());
    }

    /// Enter-then-exit pairs for `phases`, the transition log of
    /// a lane that booked each of them in turn.
    fn booked(phases: &[QueryPhase]) -> Vec<Transition> {
        phases
            .iter()
            .flat_map(|&p| [Transition::Enter(p), Transition::Exit(p)])
            .collect()
    }

    fn close(clock: &PhaseClock) -> PhaseTotals {
        clock
            .close_at(std::time::Instant::now())
            .expect("the clock records")
    }

    #[test]
    fn a_cancelled_parse_is_a_cancellation_on_every_lane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(&hot, "").expect("hot buffer writes");
        let hot_path = hot.to_str().expect("temp path is valid UTF-8");
        let cold = format!("{}/nonexistent/*.parquet", dir.path().display());
        let out = dir.path().join("out.parquet");
        let exec = Executor::new().expect("executor");
        let none = FieldTypes::new();
        let broken = "* | where (";

        let lanes = |latch: &CancelLatch| {
            let lanes = exec.cancellable(latch);
            [
                lanes
                    .run_query(broken, &cold, &none, RowCap::Refuse(10))
                    .map(|_| ()),
                lanes
                    .run_query_with_hot(
                        broken,
                        &cold,
                        hot_path,
                        &hot_columns_of(&hot),
                        &none,
                        &none,
                        RowCap::Refuse(10),
                    )
                    .map(|_| ()),
                lanes
                    .export_parquet(broken, &cold, &none, &out, 10)
                    .map(drop),
                lanes
                    .export_parquet_with_hot(
                        broken,
                        &cold,
                        hot_path,
                        &hot_columns_of(&hot),
                        &none,
                        &none,
                        &out,
                        10,
                    )
                    .map(drop),
            ]
        };
        let names = ["query", "hot query", "export", "hot export"];

        let set = CancelLatch::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        )));
        for (lane, outcome) in names.iter().zip(lanes(&set)) {
            assert!(
                matches!(outcome, Err(EngineError::Cancelled)),
                "{lane}: {outcome:?}"
            );
        }
        for (lane, outcome) in names.iter().zip(lanes(&CancelLatch::never())) {
            assert!(
                matches!(outcome, Err(EngineError::Parse(_))),
                "{lane}: {outcome:?}"
            );
        }
    }

    #[test]
    fn hot_only_fallback_reports_its_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2024-01-15T10:00:00Z\",\"_ingested\":\"2024-01-15T10:00:00Z\",\
             \"service\":\"svc\",\"message\":\"hi\"}\n",
        )
        .expect("hot buffer writes");
        let cold = format!("{}/nonexistent/*.parquet", dir.path().display());
        let clock = PhaseClock::new();

        let result = Executor::new()
            .expect("executor")
            .cancellable(&CancelLatch::never())
            .timed(&clock)
            .run_query_with_hot(
                "*",
                &cold,
                hot.to_str().expect("temp path is valid UTF-8"),
                &hot_columns_of(&hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                RowCap::Refuse(usize::MAX),
            )
            .expect("cold start answers from the hot buffer");
        assert_eq!(result.rows.len(), 1);

        let totals = close(&clock);
        assert_eq!(totals.fallback, Fallback::HotOnly);
        assert_eq!(totals.fallback.as_str(), "hot_only");
        assert_eq!(
            totals.attempts, 2,
            "the union's bind, then the hot-only one"
        );
        assert_eq!(
            clock.transitions(),
            booked(&[
                QueryPhase::Emit,
                QueryPhase::Source,
                QueryPhase::Emit,
                // The union matches no files at its bind.
                QueryPhase::Bind,
                // The presence check globs the cold side: nothing there.
                QueryPhase::Source,
                QueryPhase::Emit,
                QueryPhase::Bind,
                QueryPhase::Execute,
                QueryPhase::Post,
            ])
        );
    }

    /// Timechart probes bind and run statements of their own, and all of
    /// it is `probe`: the probe phase closes before the main statement's
    /// bind opens, no probe counts as a bind attempt, and the statement
    /// `DuckDB` last prepared when `bind` opens is the probe's — so the
    /// probe's prepare ran before `bind` began, and the main statement's
    /// ran inside it.
    #[test]
    fn timechart_probes_count_only_in_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = super::timechart_fixture(&dir);
        let dsl = "* | timechart on good span=1d count()";
        let clock = PhaseClock::new();
        let at_bind = std::sync::Arc::new(std::sync::Mutex::new(None));
        {
            let at_bind = std::sync::Arc::clone(&at_bind);
            clock.hold_on_enter(QueryPhase::Bind, move || {
                *at_bind.lock().expect("unpoisoned") = super::prepare_probe::last_statement();
            });
        }

        Executor::new()
            .expect("executor")
            .cancellable(&CancelLatch::never())
            .timed(&clock)
            .run_query(dsl, &source, &FieldTypes::new(), RowCap::Refuse(usize::MAX))
            .expect("a TIMESTAMP bucket source charts");

        let totals = close(&clock);
        assert!(totals.get(QueryPhase::Probe).is_some(), "{totals:?}");
        assert_eq!(totals.attempts, 1, "probes are not bind attempts");
        assert_eq!(totals.fallback, Fallback::None);
        assert_eq!(
            clock.transitions(),
            booked(&[
                QueryPhase::Emit,
                QueryPhase::Source,
                // The schema probe: `good` is a field beyond the envelope,
                // so the input is described before the emit.
                QueryPhase::Probe,
                QueryPhase::Emit,
                QueryPhase::Probe,
                QueryPhase::Bind,
                QueryPhase::Execute,
                QueryPhase::Post,
            ])
        );

        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        let emitted = emitter::emit_with_pins(
            &query,
            &source,
            &FieldTypes::new(),
            &emitter::InputColumns::unprobed(),
            EvalContext::capture(),
        )
        .expect("the query emits");
        let [probe] = emitted.timechart_input_checks.as_slice() else {
            panic!("one timechart, one probe");
        };
        assert_eq!(
            at_bind.lock().expect("unpoisoned").as_deref(),
            Some(probe.sql.as_str()),
            "the probe was prepared before bind opened"
        );
        assert_eq!(
            super::prepare_probe::last_statement().as_deref(),
            Some(emitted.sql.as_str()),
            "the main statement was prepared inside bind"
        );
    }

    #[test]
    fn parquet_export_books_copy_apart_from_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = super::timechart_fixture(&dir);
        let out = dir.path().join("out.parquet");
        let clock = PhaseClock::new();
        Executor::new()
            .expect("executor")
            .cancellable(&CancelLatch::never())
            .timed(&clock)
            .export_parquet("*", &source, &FieldTypes::new(), &out, 100)
            .expect("exports");
        let totals = close(&clock);
        assert_eq!(totals.attempts, 1);
        assert_eq!(
            clock.transitions(),
            booked(&[
                QueryPhase::Emit,
                QueryPhase::Source,
                QueryPhase::Emit,
                QueryPhase::Bind,
                QueryPhase::Execute,
                QueryPhase::Copy,
            ])
        );
    }
}

#[cfg(test)]
mod structured_error_classification {
    use std::num::{NonZeroU32, NonZeroU64};

    use super::{
        ColdAction, ColdPresence, EngineError, Executor, FieldTypes, HotColdOutcome, HotLane,
        QueryBudgets, classify_export_outcome, classify_query_outcome, cold_action, hot_columns_of,
        is_binder_column_error, is_no_files_error, remap_binder_error,
    };
    use crate::scratch::ScratchLock;

    const MIB: u64 = 1024 * 1024;

    fn query_database(spill: u64, memory: u64) -> (tempfile::TempDir, Executor) {
        let parent = tempfile::tempdir().expect("scratch parent");
        let scratch = ScratchLock::open(&parent.path().join("query-scratch")).expect("scratch");
        let budgets = QueryBudgets {
            spill_max_bytes: NonZeroU64::new(spill).unwrap(),
            memory_limit: NonZeroU64::new(memory).unwrap(),
            threads: NonZeroU32::new(2).unwrap(),
        };
        let exec = Executor::open_query_database(std::sync::Arc::new(scratch), &budgets)
            .expect("the query database opens");
        (parent, exec)
    }

    fn raw_error(exec: &Executor, sql: &str) -> duckdb::Error {
        exec.conn
            .prepare(sql)
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })
            .expect_err(sql)
    }

    /// A full sort of two million 64-character keys, answered with one
    /// row, so the sort has to spill and the result never needs memory.
    const SPILLING_SORT: &str = "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY s) AS rn \
         FROM (SELECT md5(i::VARCHAR) || md5((i + 1)::VARCHAR) AS s FROM range(2000000) t(i)))";

    /// One 66 MB string: an aggregate state `DuckDB` cannot spill.
    const UNSPILLABLE_AGGREGATE: &str =
        "SELECT length(string_agg(md5(i::VARCHAR), ',')) FROM range(2000000) t(i)";

    #[test]
    fn every_connection_reports_structured_diagnostics() {
        let (_scratch, query_db) = query_database(64 * MIB, 64 * MIB);
        let plain = Executor::new().expect("executor");
        let diagnostic = query_db.diagnostic_connection().expect("diagnostic");
        for (name, exec) in [
            ("query database", &query_db),
            (
                "query database clone",
                &query_db.try_clone().expect("clone"),
            ),
            ("executor", &plain),
            ("executor clone", &plain.try_clone().expect("clone")),
        ] {
            assert_eq!(
                exec.current_setting("errors_as_json").unwrap(),
                "true",
                "{name}"
            );
            let error = raw_error(exec, "SELECT nonexistent_col FROM (SELECT 1 AS x)");
            assert!(
                crate::error::DuckDbDiagnostic::of(&error).is_some(),
                "{name}: {error}"
            );
        }
        assert_eq!(
            diagnostic.current_setting("errors_as_json").unwrap(),
            "true"
        );
    }

    #[test]
    fn a_real_spill_cap_failure_is_resource_exhausted() {
        let (_scratch, exec) = query_database(4 * MIB, 32 * MIB);
        let raw = raw_error(&exec, SPILLING_SORT);
        let shown = raw.to_string();
        assert!(
            shown.contains("failed to offload data block")
                && shown.contains("max_temp_directory_size"),
            "the spill cap, not memory, stopped it: {shown}"
        );
        assert!(
            matches!(EngineError::from(raw), EngineError::ResourceExhausted),
            "{shown}"
        );
        assert!(matches!(
            exec.count_rows_for_test(SPILLING_SORT),
            Err(EngineError::ResourceExhausted)
        ));
        exec.count_rows_for_test("SELECT 1")
            .expect("the database answers a small query after the failure");
    }

    #[test]
    fn a_real_memory_limit_failure_is_resource_exhausted() {
        let (_scratch, exec) = query_database(512 * MIB, 8 * MIB);
        let raw = raw_error(&exec, UNSPILLABLE_AGGREGATE);
        let shown = raw.to_string();
        assert!(
            !shown.contains("offload") && shown.contains("Out of Memory"),
            "memory, not the spill cap, stopped it: {shown}"
        );
        assert!(
            matches!(EngineError::from(raw), EngineError::ResourceExhausted),
            "{shown}"
        );
    }

    #[test]
    fn the_no_files_fallback_reads_the_io_diagnostic() {
        let exec = Executor::new().expect("executor");
        let dir = tempfile::tempdir().expect("dir");
        let glob = format!("{}/*.parquet", dir.path().display());
        let no_files = raw_error(&exec, &format!("SELECT * FROM read_parquet('{glob}')"));
        assert!(is_no_files_error(&no_files), "{no_files}");
        // The same words as a table name are a Catalog error, not this one.
        let named = raw_error(
            &exec,
            r#"SELECT * FROM "No files found that match the pattern""#,
        );
        assert!(
            named
                .to_string()
                .contains("No files found that match the pattern")
        );
        assert!(!is_no_files_error(&named), "{named}");
    }

    #[test]
    fn the_binder_column_fallback_reads_the_binder_diagnostic() {
        let exec = Executor::new().expect("executor");
        let binder = raw_error(&exec, "SELECT nonexistent_col FROM (SELECT 1 AS x)");
        assert!(is_binder_column_error(&binder), "{binder}");
        let remapped = remap_binder_error(&binder);
        assert_eq!(
            remapped.to_string(),
            "emit error: unsupported operation: unknown field: nonexistent_col"
        );
        // A message about a column under another type is not a binder error.
        let catalog = raw_error(&exec, r#"SELECT * FROM "Binder Error: column not found""#);
        assert!(!is_binder_column_error(&catalog), "{catalog}");
    }

    /// The binder failures over a missing name that `DuckDB` 1.5.5 raises
    /// without the `COLUMN_NOT_FOUND` subtype (`EXCLUDE`, `REPLACE`, a struct
    /// key, a table) never take the binder-column fallback.
    #[test]
    fn a_malformed_or_non_binder_diagnostic_is_never_the_fallback() {
        fn failure(message: &str) -> duckdb::Error {
            duckdb::Error::DuckDBFailure(duckdb::ffi::Error::new(1), Some(message.to_owned()))
        }
        let exec = Executor::new().expect("executor");
        let found = failure(
            r#"{"exception_type":"Binder","exception_message":"Referenced column \"nope\" not found in FROM clause!","error_subtype":"COLUMN_NOT_FOUND","name":"nope","position":"7"}"#,
        );
        assert!(is_binder_column_error(&found), "{found}");
        assert_eq!(
            remap_binder_error(&found).to_string(),
            "emit error: unsupported operation: unknown field: nope"
        );
        let renamed = failure(
            r#"{"exception_type":"Binder","exception_message":"Referenced column \"other\" not found","error_subtype":"COLUMN_NOT_FOUND","name":"nope"}"#,
        );
        assert_eq!(
            remap_binder_error(&renamed).to_string(),
            "emit error: unsupported operation: unknown field: nope",
            "the field is the diagnostic's name, not a quoted word of its text"
        );

        let never = [
            raw_error(&exec, "SELECT * EXCLUDE (nope) FROM (SELECT 1 AS x)"),
            raw_error(&exec, "SELECT * REPLACE (1 AS nope) FROM (SELECT 1 AS x)"),
            raw_error(&exec, "SELECT struct_extract({'a': 1}, 'nope')"),
            raw_error(&exec, "SELECT nope.x FROM (SELECT 1 AS x)"),
            raw_error(&exec, "SELECT sum(mycolumn) FROM (SELECT 'a' AS mycolumn)"),
            raw_error(&exec, r#"SELECT * FROM "Referenced column not found""#),
            raw_error(
                &exec,
                r#"SELECT error('{"exception_type":"Binder","error_subtype":"COLUMN_NOT_FOUND"}')"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"Referenced column \"nope\" not found in FROM clause!"}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"column not found","error_subtype":"NO_MATCHING_FUNCTION","name":"nope"}"#,
            ),
            failure(
                r#"{"exception_type":"Catalog","exception_message":"column not found","error_subtype":"COLUMN_NOT_FOUND","name":"nope"}"#,
            ),
            failure(
                r#"{"exception_type":"binder","exception_message":"x","error_subtype":"COLUMN_NOT_FOUND","name":"nope"}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":"column_not_found","name":"nope"}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":"COLUMN_NOT_FOUND"}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":["COLUMN_NOT_FOUND"],"name":"nope"}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":"COLUMN_NOT_FOUND","name":7}"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":"NO_MATCHING_FUNCTION","error_subtype":"COLUMN_NOT_FOUND","name":"nope"}"#,
            ),
            failure(
                r#"[{"exception_type":"Binder","exception_message":"x","error_subtype":"COLUMN_NOT_FOUND","name":"nope"}]"#,
            ),
            failure(
                r#"{"exception_type":"Binder","exception_message":"x","error_subtype":"COLUMN_NOT_FOUND","name":"nope"} trailing"#,
            ),
            failure(r#"Binder Error: Referenced column "nope" not found in FROM clause!"#),
            duckdb::Error::DuckDBFailure(duckdb::ffi::Error::new(1), None),
            duckdb::Error::InvalidColumnName("Binder Error: column not found".into()),
        ];
        for error in never {
            assert!(!is_binder_column_error(&error), "{error:?}");
        }
    }

    /// A field the input does not carry reads as NULL (ADR-0062), so a
    /// missing column reaches the binder only through a resolver defect.
    /// The defect is planted: the probe's description claims `nope` and
    /// `no_column`, which the fixture does not carry.
    #[test]
    fn only_a_missing_column_is_an_unknown_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = super::timechart_fixture(&dir);
        let marker = dir.path().display().to_string();
        let _defects = ["nope", "no_column"]
            .map(|column| super::schema_probe::claim_present(&marker, column, "BIGINT"));
        let (_scratch, exec) = query_database(64 * MIB, 64 * MIB);
        let run = |dsl: &str| exec.run_query(dsl, &source, &FieldTypes::new(), usize::MAX);
        for (dsl, field) in [
            ("* | stats sum(nope)", "nope"),
            ("* | stats sum(nope) by hostname", "nope"),
            ("* | let x = nope * 2", "nope"),
            ("* | fields nope", "nope"),
            ("* | sort nope", "nope"),
            ("nope=1", "nope"),
            ("* | stats count() by nope", "nope"),
            ("* | timechart on nope count()", "nope"),
            ("* | stats sum(no_column)", "no_column"),
        ] {
            match run(dsl) {
                Err(EngineError::Emit(e)) => assert_eq!(
                    e.to_string(),
                    format!("unsupported operation: unknown field: {field}"),
                    "{dsl}"
                ),
                other => panic!("{dsl}: expected the unknown-field refusal, got {other:?}"),
            }
        }
        for dsl in [
            "* | stats sum(column_name)",
            "* | stats avg(column_name)",
            "* | stats sum(column_name) by hostname",
            "* | let x = column_name * 2",
            "* | stats sum(hostname)",
            "* | stats avg(hostname)",
            "* | let x = hostname * 2",
            "* | let x = lower(n)",
            "* | where hostname > 5",
        ] {
            let outcome = run(dsl);
            assert!(
                matches!(outcome, Err(EngineError::Database(_))),
                "{dsl}: {outcome:?}"
            );
        }
    }

    #[test]
    fn a_type_failure_over_cold_data_does_not_reach_the_hot_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = super::timechart_fixture(&dir);
        let hot = dir.path().join("hot.ndjson");
        std::fs::write(
            &hot,
            "{\"_time\":\"2026-01-03T00:05:00Z\",\"_ingested\":\"2026-01-03T00:05:01Z\",\
             \"column_name\":5,\"hot_only\":7}\n",
        )
        .expect("hot buffer writes");
        let hot = hot.to_str().expect("temp path is valid UTF-8");
        // The shipped budgets: `read_json`'s buffers alone outgrow 64 MiB.
        let (_scratch, exec) = query_database(512 * MIB, 256 * MIB);
        let query = |dsl: &str| {
            exec.run_query_with_hot(
                dsl,
                &source,
                hot,
                &hot_columns_of(hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
            )
        };
        let export = |dsl: &str, name: &str| {
            let out = dir.path().join(name);
            exec.export_parquet_with_hot(
                dsl,
                &source,
                hot,
                &hot_columns_of(hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                &out,
                usize::MAX,
            )
            .map(|_| out)
        };

        let typed = "* | stats sum(column_name)";
        let outcome = query(typed);
        assert!(
            matches!(outcome, Err(EngineError::Database(_))),
            "{outcome:?}"
        );
        let outcome = export(typed, "typed.parquet");
        assert!(
            matches!(outcome, Err(EngineError::Database(_))),
            "{outcome:?}"
        );

        let missing = "* | stats sum(hot_only)";
        let result = query(missing).expect("the union answers");
        assert_eq!(result.rows.len(), 1, "{result:?}");
        let out = export(missing, "missing.parquet").expect("the union exports");
        assert!(out.is_file());
    }

    #[test]
    fn a_type_failure_stays_an_execution_error_whatever_the_field_is_called() {
        let (_scratch, exec) = query_database(512 * MIB, 256 * MIB);
        for field in [
            format!("{}column", "界".repeat(30)),
            format!("{}column", "e\u{301}".repeat(40)),
            format!("{}column", "\u{1161}a".repeat(40)),
            "mycolumn".to_owned(),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let source = dir.path().join("cold.parquet");
            let source = source.to_str().expect("temp path is valid UTF-8");
            duckdb::Connection::open_in_memory()
                .expect("in-memory duckdb")
                .execute_batch(&format!(
                    "COPY (SELECT 'x' AS \"{field}\", 'web-1' AS hostname) \
                     TO '{source}' (FORMAT PARQUET)"
                ))
                .expect("cold fixture writes");
            let hot = dir.path().join("hot.ndjson");
            std::fs::write(
                &hot,
                serde_json::json!({
                    "_time": "2026-01-03T00:05:00Z",
                    "_ingested": "2026-01-03T00:05:01Z",
                    field.as_str(): 5,
                })
                .to_string()
                    + "\n",
            )
            .expect("hot buffer writes");
            let hot = hot.to_str().expect("temp path is valid UTF-8");
            let dsl = format!("* | stats sum(`{field}`)");
            let outcome = exec.run_query_with_hot(
                &dsl,
                source,
                hot,
                &hot_columns_of(hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                usize::MAX,
            );
            let Err(EngineError::Database(duckdb::Error::DuckDBFailure(_, Some(message)))) =
                &outcome
            else {
                panic!("{dsl}: {outcome:?}");
            };
            let diagnostic: serde_json::Value = serde_json::from_str(message).expect("diagnostic");
            assert_eq!(
                (&diagnostic["exception_type"], &diagnostic["error_subtype"]),
                (&"Binder".into(), &"NO_MATCHING_FUNCTION".into()),
                "{dsl}: {message}"
            );
        }
    }

    #[test]
    fn resource_exhausted_is_fatal_not_retried() {
        use trawl_core::pattern::RegexRefusal;
        let exhausted = || {
            [
                EngineError::ResourceExhausted,
                EngineError::RegexResources(RegexRefusal::Budget),
                EngineError::RegexResources(RegexRefusal::Reservation),
            ]
        };
        for error in exhausted() {
            let query = classify_query_outcome(Err(&error));
            let export = classify_export_outcome(&Err(error));
            for outcome in [query, export] {
                assert_eq!(outcome, HotColdOutcome::Fatal);
                for cold in [ColdPresence::Present, ColdPresence::Absent] {
                    for hot in [HotLane::Present, HotLane::Absent] {
                        assert_eq!(cold_action(outcome, cold, hot), ColdAction::ReturnOutcome);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod export_row_count {
    use std::path::Path;

    use duckdb::Connection;

    use super::{Executor, FieldTypes, hot_columns_of};

    const COLD_ROWS: usize = 5;
    const HOT_ROWS: usize = 3;

    /// (DSL, `max_rows`) for the three shapes the count must hold under.
    const SHAPES: [(&str, usize); 3] = [("service=nope", usize::MAX), ("*", 2), ("*", usize::MAX)];

    fn write_cold(path: &Path) {
        Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT TIMESTAMP '2024-01-15 10:00:00' + to_seconds(i) AS \"_time\", \
                              'svc' AS service, 'm' || i AS message \
                       FROM range({COLD_ROWS}) t(i)) \
                 TO '{}' (FORMAT PARQUET)",
                path.display()
            ))
            .unwrap();
    }

    fn write_hot(path: &Path) {
        use std::fmt::Write as _;

        let mut lines = String::new();
        for i in 0..HOT_ROWS {
            writeln!(
                lines,
                "{{\"_time\":\"2024-01-15T11:00:0{i}Z\",\"_ingested\":\"2024-01-15T11:00:0{i}Z\",\"service\":\"svc\",\"message\":\"h{i}\"}}"
            )
            .unwrap();
        }
        std::fs::write(path, lines).unwrap();
    }

    fn file_rows(path: &Path) -> usize {
        let rows: i64 = Connection::open_in_memory()
            .unwrap()
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}')",
                    path.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        usize::try_from(rows).unwrap()
    }

    fn expected(dsl: &str, max_rows: usize, available: usize) -> usize {
        if dsl == "*" {
            available.min(max_rows)
        } else {
            0
        }
    }

    #[test]
    fn cold_export_returns_the_written_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        write_cold(&hour.join("svc.parquet"));
        let source = format!("['{}/*.parquet']", hour.display());
        let exec = Executor::new().unwrap();
        for (i, (dsl, max_rows)) in SHAPES.into_iter().enumerate() {
            let out = dir.path().join(format!("cold-{i}.parquet"));
            let rows = exec
                .export_parquet(dsl, &source, &FieldTypes::new(), &out, max_rows)
                .unwrap();
            assert_eq!(rows, file_rows(&out), "{dsl} / {max_rows}");
            assert_eq!(
                rows,
                expected(dsl, max_rows, COLD_ROWS),
                "{dsl} / {max_rows}"
            );
        }
    }

    #[test]
    fn hot_only_export_returns_the_written_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        let hot = dir.path().join("hot.ndjson");
        write_hot(&hot);
        let source = format!("['{}/*.parquet']", hour.display());
        let exec = Executor::new().unwrap();
        for (i, (dsl, max_rows)) in SHAPES.into_iter().enumerate() {
            let out = dir.path().join(format!("hot-{i}.parquet"));
            let rows = exec
                .export_parquet_with_hot(
                    dsl,
                    &source,
                    hot.to_str().unwrap(),
                    &hot_columns_of(&hot),
                    &FieldTypes::new(),
                    &FieldTypes::new(),
                    &out,
                    max_rows,
                )
                .unwrap();
            assert_eq!(rows, file_rows(&out), "{dsl} / {max_rows}");
            assert_eq!(
                rows,
                expected(dsl, max_rows, HOT_ROWS),
                "{dsl} / {max_rows}"
            );
        }
    }

    #[test]
    fn hot_and_cold_export_returns_the_written_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        write_cold(&hour.join("svc.parquet"));
        let hot = dir.path().join("hot.ndjson");
        write_hot(&hot);
        let source = format!("['{}/*.parquet']", hour.display());
        let exec = Executor::new().unwrap();
        for (i, (dsl, max_rows)) in SHAPES.into_iter().enumerate() {
            let out = dir.path().join(format!("union-{i}.parquet"));
            let rows = exec
                .export_parquet_with_hot(
                    dsl,
                    &source,
                    hot.to_str().unwrap(),
                    &hot_columns_of(&hot),
                    &FieldTypes::new(),
                    &FieldTypes::new(),
                    &out,
                    max_rows,
                )
                .unwrap();
            assert_eq!(rows, file_rows(&out), "{dsl} / {max_rows}");
            assert_eq!(
                rows,
                expected(dsl, max_rows, COLD_ROWS + HOT_ROWS),
                "{dsl} / {max_rows}"
            );
        }
    }

    #[test]
    fn cancellable_exports_return_the_written_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let hour = dir.path().join("10");
        std::fs::create_dir_all(&hour).unwrap();
        write_cold(&hour.join("svc.parquet"));
        let hot = dir.path().join("hot.ndjson");
        write_hot(&hot);
        let source = format!("['{}/*.parquet']", hour.display());
        let exec = Executor::new().unwrap();
        let latch = crate::cancel::CancelLatch::never();
        let lanes = exec.cancellable(&latch);
        let cold_out = dir.path().join("cold.parquet");
        let rows = lanes
            .export_parquet("*", &source, &FieldTypes::new(), &cold_out, usize::MAX)
            .unwrap();
        assert_eq!(rows, file_rows(&cold_out));
        assert_eq!(rows, COLD_ROWS);
        let union_out = dir.path().join("union.parquet");
        let rows = lanes
            .export_parquet_with_hot(
                "*",
                &source,
                hot.to_str().unwrap(),
                &hot_columns_of(&hot),
                &FieldTypes::new(),
                &FieldTypes::new(),
                &union_out,
                usize::MAX,
            )
            .unwrap();
        assert_eq!(rows, file_rows(&union_out));
        assert_eq!(rows, COLD_ROWS + HOT_ROWS);
    }
}
