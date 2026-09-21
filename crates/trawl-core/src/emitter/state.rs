// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt::Write as _;

use crate::ast::TrawlDuration;

use super::SqlValue;

/// A single CTE in the emitted SQL.
struct Cte {
    name: String,
    sql: String,
}

/// Which quoted region an emitted SQL scan is inside.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Quoted {
    No,
    /// `'…'` — a string literal.
    String,
    /// `"…"` — a quoted identifier.
    Ident,
}

impl Quoted {
    /// Advance the quote state across one character of emitted SQL.
    ///
    /// `peek` is the character that follows `ch`. Returns `true` when the
    /// pair is a doubled delimiter (`''`, `""`) — one escaped character
    /// inside its own literal, never a close — which the caller must emit
    /// whole and step past; `DuckDB` has no backslash escape in either
    /// form, so there is nothing else to track.
    ///
    /// Param inlining and CTE indentation read this one transition rule,
    /// so the two scans cannot disagree about where a literal ends.
    fn advance(&mut self, ch: char, peek: Option<char>) -> bool {
        let kind = match ch {
            '\'' => Quoted::String,
            '"' => Quoted::Ident,
            _ => return false,
        };
        if *self == kind && peek == Some(ch) {
            return true;
        }
        if *self == Quoted::No {
            *self = kind;
        } else if *self == kind {
            *self = Quoted::No;
        }
        false
    }
}

/// `DuckDB` PIVOT specification, set by a `pivot` stage. A non-terminal
/// one is flushed to a CTE before the following stage; a terminal one is
/// rendered by `finalize`.
pub(crate) struct PivotSpec {
    pub agg_sql: String,
    pub on_field: String,
    pub by_fields: Vec<String>,
}

/// When to flush accumulated state to a CTE before processing a new stage.
#[derive(Clone, Copy)]
pub(crate) enum FlushCondition {
    /// Flush if any prior aggregation or projection would be clobbered.
    IfModified,
    /// Flush if modified OR if there's an existing ORDER BY.
    IfModifiedOrOrdered,
    /// Flush if modified OR if there's an existing LIMIT.
    IfModifiedOrLimited,
    /// Flush if modified OR if there is a pending ORDER BY or LIMIT.
    ///
    /// What an aggregating stage needs: SQL applies both clauses after
    /// the aggregation, so leaving either on the same SELECT would make
    /// `head 2 | stats count()` count the whole input and then limit a
    /// one-row result. Flushing puts them in the CTE the aggregate reads,
    /// which is where the pipeline reads them.
    IfModifiedOrderedOrLimited,
    /// Always flush (stage requires clean state for param ordering).
    Always,
}

/// Accumulates SQL clauses as the emitter walks the AST.
///
/// The emitter pushes clauses into the state, and flushes to CTEs when
/// pipe stages are incompatible (e.g. `where` after `stats`).
pub(crate) struct EmitterState {
    step: usize,
    source: String,
    pub(crate) select: Vec<String>,
    pub(crate) where_clauses: Vec<String>,
    pub(crate) group_by: Vec<String>,
    pub(crate) order_by: Vec<String>,
    pub(crate) limit: Option<u64>,
    pub(crate) has_aggregation: bool,
    pub(crate) has_projection: bool,
    /// Persistent flag: set when any stage defines a complete output column set
    /// (table/fields, stats, top, rare, timechart, pivot). Unlike `has_aggregation`
    /// and `has_projection`, this is not reset on CTE flush — it tracks whether
    /// the pipeline as a whole produced an explicit schema.
    pub(crate) had_explicit_columns: bool,
    /// The time filter from the search stage, used by `timechart` auto-bucketing.
    /// Not reset on CTE flush — this is query-wide context.
    pub(crate) time_filter: Option<TrawlDuration>,
    /// One probe per explicit `timechart on <field>`, in pipeline order,
    /// excluding `_time` — see [`super::TimechartInputCheck`]. Captured
    /// as each timechart is processed, because the relation a stage
    /// reads exists only while that stage is being emitted: by the end
    /// of the pipeline it has been folded into a CTE, projected away, or
    /// overwritten by a later stage's own `_time`.
    ///
    /// Query-wide context like `time_filter`, so a CTE flush does not
    /// clear it.
    pub(crate) timechart_input_checks: Vec<super::TimechartInputCheck>,
    /// `USING SAMPLE` clause set by `sample` stage.
    pub(crate) sample: Option<String>,
    /// Set by `pivot` stage — overrides normal `build_select()` in `finalize()`.
    pivot: Option<PivotSpec>,
    ctes: Vec<Cte>,
    params: Vec<SqlValue>,
    /// How text search binds `_raw` in this pass (see [`RawBinding`]).
    raw_binding: RawBinding,
    /// The pin scope typing comparisons at the current point of emission
    /// (ADR-0011): seeded with the full catalog snapshot by
    /// [`Self::with_compare_pins`] — during search emission the scope is
    /// the root — and advanced per pipe stage by
    /// [`Self::advance_pin_scope`], so a `where` after a `rename` or a
    /// computed `let` resolves against the schema actually in force.
    /// Distinct from the hot-branch conformance pins passed to
    /// [`Self::with_hot_source`]. The scope stays empty for pin-blind
    /// emission (embedded mode, [`super::emit`]).
    pin_scope: crate::pin_scope::PinScope,
    /// The statement's `now()` instant (ADR-0017 §3), handed in by the
    /// caller and never sampled here. Every source shape funnels through
    /// [`Self::with_source`], so there is exactly one way for an emission
    /// to acquire an anchor and no way for it to acquire two.
    anchor: crate::context::EvalContext,
    /// How many of [`Self::params`] are already inside a CTE — i.e. how
    /// many render before anything the current level emits.
    ///
    /// The remainder were pushed at the current level, where the only
    /// parameter-bearing clause is the WHERE, which renders after the
    /// SELECT list. That difference is the whole input to
    /// [`Self::emit_ordered_select`].
    flushed_params: usize,
}

/// How the `_raw` column is bound by text search in one emission pass.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawBinding {
    /// `_raw` may be bound; no text search has done so yet.
    Available,
    /// A text-search predicate bound `_raw`, so this query needs a raw-free
    /// variant for sources that lack the column.
    Bound,
    /// The raw-free pass: text search substitutes a typed NULL for `_raw`.
    Suppressed,
}

/// Validate a source path for use in `DuckDB` table-valued functions.
///
/// `DuckDB`'s `read_parquet()`/`read_json_auto()` don't support parameterized
/// paths, so the path must be sanitized before interpolation into SQL.
///
/// The refusals say what is wrong and nothing about where. The path is
/// server-minted and embeds `data_dir`, so a daemon whose data directory
/// holds a space or a non-ASCII byte would otherwise hand its own absolute
/// filesystem layout back to any `Query` holder in a 400. The path goes to
/// the operator's log at `debug` instead, which is where the answer to
/// "which path" belongs.
pub fn validate_source_path(source: &str) -> Result<(), super::EmitError> {
    if source.is_empty() {
        return Err(super::EmitError::UnsupportedOperation {
            message: "source path cannot be empty".to_string(),
        });
    }
    if !source
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_.*?{}[]-".contains(&b))
    {
        tracing::debug!(source, "source path contains invalid characters");
        return Err(super::EmitError::UnsupportedOperation {
            message: "source path contains invalid characters".to_string(),
        });
    }
    if source.split('/').any(|component| component == "..") {
        tracing::debug!(source, "source path contains path traversal");
        return Err(super::EmitError::UnsupportedOperation {
            message: "source path contains path traversal".to_string(),
        });
    }
    Ok(())
}

/// A refused source path never travels in the refusal.
///
/// Both arms are reachable from a plain `Query` holder on a daemon whose
/// `data_dir` carries a space or a non-ASCII byte, and the path they are
/// handed is the server-minted glob with that directory inside it. No `/`
/// in either message is the cheap whole-class check: the leak was an
/// absolute path, and an absolute path cannot hide from it.
#[cfg(test)]
#[test]
fn source_path_errors_carry_no_path() {
    for source in [
        "/var/lib/trawl data/logs/*.parquet",
        "/var/lib/trawl/../etc",
    ] {
        let message = match validate_source_path(source) {
            Err(super::EmitError::UnsupportedOperation { message }) => message,
            other => panic!("expected a refusal for {source}, got {other:?}"),
        };
        assert!(
            !message.contains('/'),
            "the refusal must not carry the path: {message}"
        );
    }
}

/// Validate a list-format source for `read_parquet()`.
///
/// Expected format: `['path1', 'path2', ...]`. Each individual path
/// inside the list must pass [`validate_source_path`].
fn validate_source_list(source: &str) -> Result<(), super::EmitError> {
    let inner = source
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| super::EmitError::UnsupportedOperation {
            message: "invalid source list format".to_string(),
        })?;

    for segment in inner.split(',') {
        let trimmed = segment.trim();
        let path = trimmed
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .ok_or_else(|| super::EmitError::UnsupportedOperation {
                message: format!("invalid source list element: {trimmed}"),
            })?;
        validate_source_path(path)?;
    }

    Ok(())
}

/// Build a `DuckDB` reader expression from a source path.
///
/// Handles four source formats:
/// - Subquery: `(SELECT …)` → passed through verbatim
/// - List: `['path1', 'path2']` → `read_parquet([...], union_by_name=true)`
/// - JSON/ndjson file: `*.json` or `*.ndjson` → `read_json(...)`
/// - Parquet glob: everything else → `read_parquet('...', union_by_name=true)`
fn build_reader(source: &str) -> Result<String, super::EmitError> {
    if source.starts_with('(') {
        // Raw SQL subquery — used by `from saved` resolution for run=all
        // UNION ALL sources with injected synthetic columns.
        Ok(source.to_string())
    } else if source.starts_with('[') {
        validate_source_list(source)?;
        Ok(format!("read_parquet({source}, union_by_name=true)"))
    } else {
        validate_source_path(source)?;
        let ext = std::path::Path::new(source)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if ext.eq_ignore_ascii_case("ndjson") || ext.eq_ignore_ascii_case("json") {
            // Explicit format params prevent DuckDB from inferring the ndjson
            // as a single JSON column. field_appearance_threshold=0 is critical:
            // the default (0.1) causes DuckDB to fall back to MAP(VARCHAR, JSON)
            // when events have heterogeneous schemas where most fields appear in
            // less than 10% of records (e.g. internal telemetry with 64 keys
            // mixed with external events that only have 5 keys).
            // Schema detection stays on DuckDB's bounded default sample — see
            // `hot_reader` below for why whole-file detection is not the way to
            // keep sparse columns alive.
            Ok(format!(
                "read_json('{source}', format='newline_delimited', records=true, \
                 auto_detect=true, field_appearance_threshold=0)"
            ))
        } else {
            Ok(format!("read_parquet('{source}', union_by_name=true)"))
        }
    }
}

/// Build the `read_json` reader expression for a hot-buffer ndjson snapshot.
///
/// `field_appearance_threshold=0` prevents `DuckDB` from collapsing
/// heterogeneous-schema events into a single MAP column.
///
/// Schema detection deliberately keeps `DuckDB`'s bounded default sample.
/// A sparse column — `_repairs`, which only repaired events carry
/// (ADR-0008) — first appearing past that prefix throws an `unknown key`
/// error for the whole query, but `sample_size=-1` is the wrong cure: it
/// re-parses the entire snapshot on *every* query and SSE poll (~2.7x the
/// read cost, and it grows with the buffer). The snapshot writer instead
/// hoists one event per novel key to the front of the file, so the whole key
/// set is inside the prefix (see `HotBuffer::build_snapshot`).
fn hot_reader(hot: &str) -> Result<String, super::EmitError> {
    validate_source_path(hot)?;
    Ok(format!(
        "read_json('{hot}', format='newline_delimited', records=true, \
         auto_detect=true, field_appearance_threshold=0)"
    ))
}

/// Conform expression for one pinned field on the hot branch, applied
/// untyped — the emitter has no `DESCRIBE`, so the expression must be valid
/// whatever type `read_json` inferred for the column.
///
/// Text first, then the guarded cast, exactly as compaction conforms the
/// same event on its way to parquet ([`crate::conform`]): the hot value a
/// query reads is the one the corpus will durably hold, so a result cannot
/// flip when the compactor runs. Both halves matter — the text form pins
/// the cast domain to VARCHAR whatever `read_json` inferred from the rest
/// of the snapshot, and the guard refuses a cast that would alter the value
/// (`'1.5'` is not 2, `'TRUE'` is not `true`) instead of silently
/// rewriting it.
///
/// A hot value that does not conform degrades to NULL, which is also what
/// keeps it from throwing the hot+cold union (ADR-0008).
fn conform_untyped(quoted: &str, pin: crate::schema::CanonicalType) -> String {
    crate::conform::guarded_cast(&crate::conform::untyped_text(quoted), pin)
}

/// The pins that get their own `REPLACE` entry: everything except the
/// envelope TIMESTAMP columns, which already carry their unconditional
/// `TRY_CAST`s — a second entry for the same identifier would be
/// `Parser Error: Duplicate entry`.
///
/// The comparison is exact (on the quoted name): pins reach the emitter
/// from the server's folded catalog — field names are ASCII-lowercased at
/// ingest, at boot seeding, and in compaction's proposals — so one
/// `DuckDB` identifier has exactly one pin spelling and there is nothing
/// left to fold at emit time. The timestamp skip is therefore the only
/// identifier-level dedupe this list needs.
///
/// Returned in pin order (`FieldTypes` iterates sorted), so the emitted
/// SQL stays deterministic.
fn conformable_pins(
    pins: &crate::schema::FieldTypes,
) -> Vec<(String, crate::schema::CanonicalType)> {
    let timestamps: Vec<String> = crate::schema::TIMESTAMP_COLUMNS
        .iter()
        .map(|col| super::fields::quote_field(col))
        .collect();
    pins.iter()
        .map(|(field, ty)| (super::fields::quote_field(field), ty))
        .filter(|(quoted, _)| !timestamps.contains(quoted))
        .collect()
}

/// The `REPLACE` list conforming a hot-buffer read to the catalog, shared
/// by every lane that reads the snapshot ([`EmitterState::with_hot_source`]
/// and [`EmitterState::with_hot_only_source`]) so hot rows carry the same
/// types whether or not cold data happens to exist.
fn hot_replace_list(hot_pins: &crate::schema::FieldTypes) -> String {
    let mut parts = Vec::with_capacity(crate::schema::TIMESTAMP_COLUMNS.len() + hot_pins.len());
    for col in crate::schema::TIMESTAMP_COLUMNS {
        parts.push(format!("TRY_CAST(\"{col}\" AS TIMESTAMP) AS \"{col}\""));
    }
    for (quoted, ty) in conformable_pins(hot_pins) {
        parts.push(format!("{} AS {quoted}", conform_untyped(&quoted, ty)));
    }
    parts.join(", ")
}

/// Public accessor for the parquet/list source reader expression, so the
/// engine can `DESCRIBE` the same cold source the emitter reads from.
pub fn source_reader(source: &str) -> Result<String, super::EmitError> {
    build_reader(source)
}

/// Public accessor for the hot-buffer reader expression (see [`hot_reader`]).
pub fn hot_source_reader(hot: &str) -> Result<String, super::EmitError> {
    hot_reader(hot)
}

impl EmitterState {
    pub(crate) fn new(
        source: &str,
        anchor: crate::context::EvalContext,
    ) -> Result<Self, super::EmitError> {
        let reader = build_reader(source)?;
        Ok(Self::with_source(reader, anchor))
    }

    /// Construct with a composite source that unions parquet with hot buffer ndjson.
    ///
    /// The hot source is read via [`hot_reader`] under a `REPLACE` list
    /// that conforms the hot branch to the catalog:
    ///
    /// - both envelope TIMESTAMP columns get their unconditional `TRY_CAST`s
    ///   (ADR-0008 — survives empty `hot_pins`, so catalog-less callers keep
    ///   the timestamp guarantee; ingest already canonicalized them to UTC,
    ///   so they need none of the zone-aware rung's work), and
    /// - every pinned field (excluding the timestamp columns, already
    ///   handled) gets its [`conform_untyped`] expression, so a hot value
    ///   that disagrees with the write-time pin degrades to NULL instead of
    ///   throwing the union.
    ///
    /// One `REPLACE` entry per identifier is guaranteed by construction:
    /// pin names are ASCII-folded before they ever reach the catalog, and
    /// [`conformable_pins`] skips the timestamp columns' identifiers.
    ///
    /// The cold branch is deliberately plain: parquet is write-time
    /// conformant (ADR-0009), and a defensive cold cast would mask a real
    /// invariant breach.
    pub(crate) fn with_hot_source(
        primary: &str,
        hot: &str,
        hot_pins: &crate::schema::FieldTypes,
        anchor: crate::context::EvalContext,
    ) -> Result<Self, super::EmitError> {
        let primary_reader = build_reader(primary)?;
        let hot_reader = hot_reader(hot)?;
        let hot_replace = hot_replace_list(hot_pins);

        let composite = format!(
            "(SELECT * FROM {primary_reader} UNION ALL BY NAME \
             SELECT * REPLACE ({hot_replace}) FROM {hot_reader})"
        );
        Ok(Self::with_source(composite, anchor))
    }

    /// Construct with the hot-buffer ndjson as the sole source, carrying the
    /// same `REPLACE` conformance the union's hot branch gets.
    ///
    /// The executor reads hot-only whenever there is provably no cold data to
    /// hide (a genuine cold start, ADR-0008). That is a change of *sources*,
    /// never a change of *types*: reading the raw ndjson would hand the query
    /// whatever `read_json` inferred — a JSON numeric `200` under a VARCHAR
    /// pin binds as BIGINT and matches `status=200.0` by implicit cast, while
    /// the conformed hot+cold union and the SSE filter compare the text
    /// `'200'` and reject it. Sharing one `REPLACE` list with
    /// [`Self::with_hot_source`] keeps a result from flipping the moment the
    /// first parquet lands (ADR-0011).
    pub(crate) fn with_hot_only_source(
        hot: &str,
        hot_pins: &crate::schema::FieldTypes,
        anchor: crate::context::EvalContext,
    ) -> Result<Self, super::EmitError> {
        let hot_reader = hot_reader(hot)?;
        let hot_replace = hot_replace_list(hot_pins);
        Ok(Self::with_source(
            format!("(SELECT * REPLACE ({hot_replace}) FROM {hot_reader})"),
            anchor,
        ))
    }

    /// The one constructor: every source shape ends here, so the anchor
    /// is a required argument of building any emission at all.
    fn with_source(source: String, anchor: crate::context::EvalContext) -> Self {
        Self {
            step: 0,
            source,
            select: Vec::new(),
            where_clauses: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            has_aggregation: false,
            has_projection: false,
            had_explicit_columns: false,
            time_filter: None,
            timechart_input_checks: Vec::new(),
            sample: None,
            pivot: None,
            ctes: Vec::new(),
            params: Vec::new(),
            raw_binding: RawBinding::Available,
            pin_scope: crate::pin_scope::PinScope::unpinned(),
            anchor,
            flushed_params: 0,
        }
    }

    /// The statement's `now()` anchor — read by the `now` translation
    /// arm to bind its parameter, and stamped onto
    /// [`super::EmittedQuery::anchor`] for the `rust_stages` tail.
    pub(crate) fn anchor(&self) -> crate::context::EvalContext {
        self.anchor
    }

    /// Attach the comparison pin set (ADR-0011). Builder-style so
    /// the `emit*` entry points can funnel through one constructor per
    /// source shape.
    ///
    /// The clone is a refcount bump, not a map copy ([`crate::schema::FieldTypes`]
    /// shares its entries behind an `Arc`): every emission takes one, the
    /// raw-free pass takes a second, and the executor's hot-only fallback
    /// re-emits both again for one logical query.
    pub(crate) fn with_compare_pins(mut self, pins: &crate::schema::FieldTypes) -> Self {
        self.pin_scope = crate::pin_scope::PinScope::root(pins);
        self
    }

    /// The pin typing a comparison against `dsl_name` at the current point
    /// of emission, looked up through [`crate::schema::catalog_key`] (an
    /// ASCII fold — the DSL has no aliases).
    pub(crate) fn compare_pin(&self, dsl_name: &str) -> Option<crate::schema::CanonicalType> {
        self.pin_scope.pin_for(dsl_name)
    }

    /// Advance the pin scope over one processed pipe stage (ADR-0011) —
    /// called after the stage's own expressions were emitted, so they
    /// resolved against the incoming schema.
    pub(crate) fn advance_pin_scope(&mut self, stage: &crate::ast::PipeStage) {
        self.pin_scope.advance(stage);
    }

    /// The scope currently in force — stamped onto
    /// [`super::EmittedQuery::rust_stage_pins`] at the kv split.
    pub(crate) fn pin_scope(&self) -> &crate::pin_scope::PinScope {
        &self.pin_scope
    }

    /// Emit the raw-free variant of this query: text search binds a typed
    /// NULL instead of the `_raw` column.
    pub(crate) fn without_raw_column(mut self) -> Self {
        self.raw_binding = RawBinding::Suppressed;
        self
    }

    /// Whether text search bound the `_raw` column during this pass.
    pub(crate) fn bound_raw_column(&self) -> bool {
        self.raw_binding == RawBinding::Bound
    }

    /// The expression text search uses for the `_raw` side of its predicate.
    ///
    /// `_raw` is a server guarantee, not a guarantee of every source a query
    /// can be pointed at: user-owned parquet read in embedded mode has no
    /// such column, and binding it there fails the whole query. The raw-free
    /// pass substitutes a typed NULL, which under the predicate's
    /// three-valued logic degrades bare-word search to `message` alone —
    /// exactly what the in-memory filter does for an event without `_raw`
    /// (ADR-0009: bare search covers `_raw` *where present*).
    pub(crate) fn raw_column(&mut self) -> &'static str {
        match self.raw_binding {
            RawBinding::Suppressed => "NULL::VARCHAR",
            RawBinding::Available | RawBinding::Bound => {
                self.raw_binding = RawBinding::Bound;
                "\"_raw\""
            }
        }
    }

    /// Push a parameter value and return the `?` placeholder string.
    pub(crate) fn push_param(&mut self, val: SqlValue) -> String {
        self.params.push(val);
        "?".to_string()
    }

    /// Append a WHERE clause fragment (will be AND-joined with others).
    pub(crate) fn push_where(&mut self, clause: String) {
        self.where_clauses.push(clause);
    }

    /// Run a closure that may push WHERE clauses, capturing them separately.
    ///
    /// Swaps the WHERE buffer, runs the closure, then restores the original —
    /// the restore happens even when the closure fails, so a failed emission
    /// never leaks half-built clauses into the caller's buffer. Parameters
    /// pushed during the closure are kept (ordering is preserved). Used by
    /// multi-group (OR) search emission to collect per-group clauses.
    pub(crate) fn collect_where_clauses(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), super::EmitError>,
    ) -> Result<Vec<String>, super::EmitError> {
        let original = std::mem::take(&mut self.where_clauses);
        let outcome = f(self);
        let collected = std::mem::replace(&mut self.where_clauses, original);
        outcome.map(|()| collected)
    }

    pub(crate) fn flush_if(&mut self, condition: FlushCondition) {
        let should_flush = match condition {
            FlushCondition::IfModified => self.has_aggregation || self.has_projection,
            FlushCondition::IfModifiedOrOrdered => {
                self.has_aggregation || self.has_projection || !self.order_by.is_empty()
            }
            FlushCondition::IfModifiedOrLimited => {
                self.has_aggregation || self.has_projection || self.limit.is_some()
            }
            FlushCondition::IfModifiedOrderedOrLimited => {
                self.has_aggregation
                    || self.has_projection
                    || !self.order_by.is_empty()
                    || self.limit.is_some()
            }
            FlushCondition::Always => true,
        };
        if should_flush {
            self.flush_to_cte();
        }
    }

    /// Flush the current state into a CTE and reset for the next stage.
    pub(crate) fn flush_to_cte(&mut self) {
        let cte_name = format!("_s{}", self.step);
        let sql = self.build_select();

        self.ctes.push(Cte {
            name: cte_name.clone(),
            sql,
        });

        // reset for next stage — the new source is the CTE we just created
        self.step += 1;
        self.source = format!("\"{cte_name}\"");
        self.select.clear();
        self.where_clauses.clear();
        self.group_by.clear();
        self.order_by.clear();
        self.limit = None;
        self.has_aggregation = false;
        self.has_projection = false;
        self.sample = None;
        // Everything pushed so far now renders inside that CTE.
        self.flushed_params = self.params.len();
    }

    /// Emit SELECT-list expressions that may push parameters, keeping the
    /// parameter list aligned with the placeholders in the rendered SQL.
    ///
    /// `DuckDB` binds `?` positionally, so the parameter list has to run
    /// in the order the placeholders appear in the text. A built SELECT
    /// renders `SELECT … FROM … WHERE …`, but the emitter walks the
    /// search stage first: a parameter the search predicate pushed sits
    /// first in the list while its placeholder sits last in the
    /// statement. So the moment an aggregating stage pushes a parameter
    /// of its own into the SELECT list, the two lists disagree and the
    /// predicate's value is fed to the SELECT's placeholder — a
    /// conversion error when the types clash, and a silent value swap
    /// when they don't (an `earliest=` bound against `now()`'s anchor).
    ///
    /// `let`/`extract`/`eventstats` avoid that by flushing to a CTE
    /// unconditionally, since a CTE renders before the outer SELECT. An
    /// aggregating stage cannot pay that: most aggregates push nothing,
    /// and wrapping every `stats count() by host` in a CTE would nest the
    /// commonest query in the language for nothing. So the decision comes
    /// from what the emission did — the parameter list grew — never from
    /// what its expressions are called.
    ///
    /// `build` therefore runs at most twice, and the first run is
    /// discarded whole. That is sound because the only state a SELECT
    /// expression emission can touch is the parameter list (it quotes
    /// fields, translates calls and pushes literals; it binds no `_raw`
    /// and appends no WHERE clause), so truncating the parameters undoes
    /// it exactly.
    pub(crate) fn emit_ordered_select<T>(
        &mut self,
        build: impl Fn(&mut Self) -> Result<T, super::EmitError>,
    ) -> Result<T, super::EmitError> {
        let pending = self.params.len();
        if pending == self.flushed_params {
            // Nothing at this level has pushed yet, so nothing this
            // build pushes can land out of order.
            return build(self);
        }

        let where_count = self.where_clauses.len();
        let attempt = build(self)?;
        debug_assert_eq!(
            where_count,
            self.where_clauses.len(),
            "a SELECT expression emission must not append WHERE clauses"
        );
        if self.params.len() == pending {
            return Ok(attempt);
        }

        self.params.truncate(pending);
        self.flush_to_cte();
        build(self)
    }

    /// A statement that reads one column of the relation a stage is
    /// about to read, and returns no rows.
    ///
    /// The whole point is WHEN it is taken. `SELECT <col> FROM (<the
    /// relation as built so far>) LIMIT 0` renders the current level and
    /// every CTE behind it, so the `from`, `let`, `rename`, `stats` and
    /// search filters that precede the stage are all in force — the
    /// probe sees exactly the column the stage will read, typed exactly
    /// as the stage will see it, which is a question nothing about the
    /// finished query can answer any more.
    ///
    /// `LIMIT 0` because only the column's declared type is wanted.
    /// `DuckDB` still binds the relation, so the probe costs one bind
    /// and no scan.
    ///
    /// The caller pairs this with [`Self::params`] as they stand now:
    /// the placeholders in the returned text are exactly those pushed so
    /// far, in push order, by the same invariant the finished statement
    /// relies on ([`Self::emit_ordered_select`]).
    pub(crate) fn stage_input_probe(&self, column: &str) -> String {
        let body = format!(
            "SELECT {} FROM (\n{}\n) LIMIT 0",
            super::fields::quote_field(column),
            self.build_select()
        );
        if self.ctes.is_empty() {
            return body;
        }
        self.with_ctes(&body)
    }

    /// Prefix `body` with the accumulated CTEs. Callers with no CTEs to
    /// render skip it; the loop is the one rendering of a `WITH` list.
    fn with_ctes(&self, body: &str) -> String {
        let mut sql = String::from("WITH ");
        for (i, cte) in self.ctes.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&cte.name);
            sql.push_str(" AS (\n");
            Self::push_indented_cte(&mut sql, &cte.sql);
            sql.push(')');
        }
        sql.push('\n');
        sql.push_str(body);
        sql
    }

    /// Build a SELECT statement from the current accumulated state.
    fn build_select(&self) -> String {
        let mut sql = String::new();

        if self.select.is_empty() {
            sql.push_str("SELECT *");
        } else {
            sql.push_str("SELECT ");
            sql.push_str(&self.select.join(", "));
        }

        sql.push_str("\nFROM ");
        sql.push_str(&self.source);

        // USING SAMPLE (between FROM and WHERE)
        if let Some(ref sample) = self.sample {
            sql.push('\n');
            sql.push_str(sample);
        }

        if !self.where_clauses.is_empty() {
            sql.push_str("\nWHERE ");
            sql.push_str(&self.where_clauses.join(" AND "));
        }

        if !self.group_by.is_empty() {
            sql.push_str("\nGROUP BY ");
            sql.push_str(&self.group_by.join(", "));
        }

        if !self.order_by.is_empty() {
            sql.push_str("\nORDER BY ");
            sql.push_str(&self.order_by.join(", "));
        }

        if let Some(limit) = self.limit {
            let _ = write!(sql, "\nLIMIT {limit}");
        }

        sql
    }

    /// Set pivot mode — overrides normal SELECT in finalize.
    pub(crate) fn set_pivot(&mut self, agg_sql: String, on_field: String, by_fields: Vec<String>) {
        self.pivot = Some(PivotSpec {
            agg_sql,
            on_field,
            by_fields,
        });
    }

    pub(crate) fn has_pivot(&self) -> bool {
        self.pivot.is_some()
    }

    /// Flush a pending PIVOT to a CTE so that downstream stages can operate
    /// on the pivot-generated columns.
    ///
    /// `DuckDB` doesn't support parameterized PIVOT, so we inline all
    /// accumulated `?` placeholders across all CTEs and the PIVOT body,
    /// then clear the param list. Subsequent stages can add fresh `?`
    /// params as normal.
    ///
    /// Clearing the list also resets [`Self::flushed_params`]: that
    /// counter answers "how many parameters render before anything this
    /// level emits", and after inlining the answer is none — there are no
    /// parameters at all. A stale count lets
    /// [`Self::emit_ordered_select`] read a post-pivot parameter count
    /// that has merely climbed back to the old value as "this level has
    /// pushed nothing", and skip the flush it exists to perform.
    pub(crate) fn flush_pivot_to_cte(&mut self) -> Result<(), super::EmitError> {
        let Some(pivot) = self.pivot.take() else {
            return Ok(());
        };

        let pivot_sql = self.build_pivot(&pivot);

        // Inline params sequentially across all CTEs + the pivot body.
        // DuckDB binds ? left-to-right across the entire statement, so
        // we must process them in the same order.
        let mut param_idx = 0;
        for cte in &mut self.ctes {
            let (inlined, consumed) =
                Self::inline_params_counted(&cte.sql, &self.params, param_idx);
            cte.sql = inlined;
            param_idx = consumed;
        }
        let (pivot_inlined, consumed) =
            Self::inline_params_counted(&pivot_sql, &self.params, param_idx);

        // Every accumulated parameter must have found a placeholder: the
        // list is about to be dropped, so one left behind is a `?` that
        // survives into SQL nothing will ever bind — or that silently
        // takes the next stage's value. Two integers to check, and
        // undiagnosable downstream, so it is a real error rather than a
        // debug assertion.
        if consumed != self.params.len() {
            return Err(super::EmitError::UnsupportedOperation {
                message: format!(
                    "pivot inlining consumed {consumed} of {} parameters",
                    self.params.len()
                ),
            });
        }
        self.params.clear();
        self.flushed_params = 0;

        // Push the pivot as a new CTE.
        let cte_name = format!("_s{}", self.step);
        self.ctes.push(Cte {
            name: cte_name.clone(),
            sql: pivot_inlined,
        });

        // Reset for downstream stages.
        self.step += 1;
        self.source = format!("\"{cte_name}\"");
        self.select.clear();
        self.where_clauses.clear();
        self.group_by.clear();
        self.order_by.clear();
        self.limit = None;
        self.has_aggregation = false;
        self.has_projection = false;

        Ok(())
    }

    /// Produce the final SQL string including any accumulated CTEs.
    ///
    /// For PIVOT queries, inlines all `?` params directly into the SQL
    /// and clears the param list — `DuckDB` doesn't support parameterized
    /// PIVOT statements.
    pub(crate) fn finalize(&mut self) -> Result<String, super::EmitError> {
        let body = if let Some(pivot) = &self.pivot {
            self.build_pivot(pivot)
        } else {
            self.build_select()
        };

        if self.ctes.is_empty() {
            // Never a pending pivot: `process_pivot` opens with a
            // `flush_to_cte`, so a pivot always leaves at least one CTE
            // behind and reaches the inlining below.
            return Ok(body);
        }

        let sql = self.with_ctes(&body);

        if self.pivot.is_some() {
            let (inlined, consumed) = Self::inline_params_counted(&sql, &self.params, 0);
            // The same refusal [`Self::flush_pivot_to_cte`] makes, for
            // the same reason: the parameter list is dropped on the next
            // line, so one left behind is a `?` nothing will ever bind.
            // A terminal pivot inlines the whole statement at once, so
            // the count is over every placeholder in it.
            if consumed != self.params.len() {
                return Err(super::EmitError::UnsupportedOperation {
                    message: format!(
                        "pivot inlining consumed {consumed} of {} parameters",
                        self.params.len()
                    ),
                });
            }
            self.params.clear();
            return Ok(inlined);
        }

        Ok(sql)
    }

    /// Append a CTE body with its SQL lines indented, without inserting bytes
    /// after newlines that belong to string literals or quoted identifiers.
    fn push_indented_cte(output: &mut String, body: &str) {
        if body.is_empty() {
            return;
        }

        output.push_str("  ");
        let mut quoted = Quoted::No;
        let mut chars = body.chars().peekable();
        while let Some(ch) = chars.next() {
            output.push(ch);

            if quoted.advance(ch, chars.peek().copied()) {
                // A doubled delimiter is one escaped character.
                output.push(ch);
                chars.next();
            } else if ch == '\n' && quoted == Quoted::No && chars.peek().is_some() {
                output.push_str("  ");
            }
        }

        if !body.ends_with('\n') {
            output.push('\n');
        }
    }

    /// Build a `DuckDB` PIVOT statement from the current source.
    fn build_pivot(&self, pivot: &PivotSpec) -> String {
        let on_field = super::fields::quote_field(&pivot.on_field);
        let mut sql = format!(
            "PIVOT {}\nON {on_field}\nUSING {}",
            self.source, pivot.agg_sql
        );

        if !pivot.by_fields.is_empty() {
            let quoted: Vec<String> = pivot
                .by_fields
                .iter()
                .map(|f| super::fields::quote_field(f))
                .collect();
            let _ = write!(sql, "\nGROUP BY {}", quoted.join(", "));
        }

        sql
    }

    /// Replace `?` placeholders starting from `start_idx` in the params slice.
    /// Returns the inlined SQL and the next param index (for chaining across
    /// multiple SQL fragments).
    ///
    /// The scan is quote-aware, and has to be: a `?` inside a SQL string
    /// literal or a quoted identifier is data, not a placeholder. The
    /// emitter authors both — `sev()`'s digits guard carries the regex
    /// `'[+-]?[0-9]+'`, and a backticked field name is a client-chosen
    /// key that may contain any character, `?` included (ADR-0013 ruling
    /// 7) — so a quote-blind scan splices the next user literal into the
    /// middle of the regex (`'[+-]'m1'[0-9]+'`, a parser error) and
    /// shifts every later parameter by one. Doubled quotes (`''`, `""`)
    /// are escapes inside their literal, never a close; `DuckDB` has no
    /// backslash escape in either form, so there is nothing else to
    /// track.
    fn inline_params_counted(sql: &str, params: &[SqlValue], start_idx: usize) -> (String, usize) {
        let mut result = String::with_capacity(sql.len());
        let mut param_idx = start_idx;
        let mut quoted = Quoted::No;
        let mut chars = sql.chars().peekable();
        while let Some(ch) = chars.next() {
            if quoted.advance(ch, chars.peek().copied()) {
                // A doubled delimiter is one escaped character.
                result.push(ch);
                result.push(ch);
                chars.next();
                continue;
            }
            if quoted == Quoted::No && ch == '?' && param_idx < params.len() {
                match &params[param_idx] {
                    SqlValue::String(s) => {
                        result.push('\'');
                        // escape single quotes by doubling them
                        result.push_str(&s.replace('\'', "''"));
                        result.push('\'');
                    }
                    SqlValue::Int(i) => {
                        let _ = write!(result, "{i}");
                    }
                    SqlValue::Float(f) => {
                        let _ = write!(result, "{f}");
                    }
                    SqlValue::Bool(b) => {
                        result.push_str(if *b { "TRUE" } else { "FALSE" });
                    }
                    // The typed literal form of the bound parameter, one
                    // rendering shared with `SqlValue`'s `Display`,
                    // because PIVOT (which cannot take parameters) must
                    // read the very instant the parameterized lanes bind.
                    SqlValue::Timestamp(at) => {
                        result.push_str(&super::timestamp_literal(*at));
                    }
                }
                param_idx += 1;
            } else {
                result.push(ch);
            }
        }
        (result, param_idx)
    }

    /// Consume the state and return the accumulated parameters.
    pub(crate) fn into_params(self) -> Vec<SqlValue> {
        self.params
    }

    /// The parameters pushed up to this point, in push order — what a
    /// probe taken now has to bind ([`Self::stage_input_probe`]).
    pub(crate) fn params_so_far(&self) -> Vec<SqlValue> {
        self.params.clone()
    }

    /// Whether the result columns should be reordered to put well-known fields first.
    ///
    /// Returns `true` when no stage in the pipeline defined a complete output
    /// column set (table/fields, stats, top, rare, timechart, pivot).
    pub(crate) fn needs_column_reorder(&self) -> bool {
        !self.had_explicit_columns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inlining scan is quote-aware: a `?` inside a string literal or
    /// a quoted identifier is data. `sev()`'s digits guard puts one in
    /// emitter-authored SQL, so a quote-blind scan would splice the next
    /// user literal into the middle of it — a parser error, and every
    /// later parameter shifted by one.
    #[test]
    fn inlining_skips_placeholders_inside_quoted_regions() {
        let sql =
            "SELECT regexp_full_match(x, '[+-]?[0-9]+'), \"a?b\" FROM t WHERE m = ? AND n = ?";
        let (inlined, used) = EmitterState::inline_params_counted(
            sql,
            &[SqlValue::String("m1".into()), SqlValue::Int(7)],
            0,
        );
        assert_eq!(
            inlined,
            "SELECT regexp_full_match(x, '[+-]?[0-9]+'), \"a?b\" \
             FROM t WHERE m = 'm1' AND n = 7"
        );
        assert_eq!(used, 2, "only the two real placeholders were consumed");
    }

    /// A doubled quote is an escape inside its own literal, never a
    /// close: `'it''s ?'` stays one literal, and the `?` after it is the
    /// placeholder.
    #[test]
    fn inlining_treats_doubled_quotes_as_escapes() {
        let (inlined, used) = EmitterState::inline_params_counted(
            "SELECT 'it''s ?', \"q\"\"?\", ? FROM t",
            &[SqlValue::String("v".into())],
            0,
        );
        assert_eq!(inlined, "SELECT 'it''s ?', \"q\"\"?\", 'v' FROM t");
        assert_eq!(used, 1);
    }

    /// Chaining across fragments keeps the index, and a fragment whose
    /// only `?` is quoted consumes nothing.
    #[test]
    fn inlining_chains_the_parameter_index_across_fragments() {
        let params = [SqlValue::String("a".into()), SqlValue::Bool(true)];
        let (first, used) = EmitterState::inline_params_counted("WHERE x = ?", &params, 0);
        assert_eq!(first, "WHERE x = 'a'");
        let (second, used) = EmitterState::inline_params_counted("AND r LIKE '%?%'", &params, used);
        assert_eq!(second, "AND r LIKE '%?%'");
        let (third, used) = EmitterState::inline_params_counted("AND b = ?", &params, used);
        assert_eq!(third, "AND b = TRUE");
        assert_eq!(used, 2);
    }

    /// CTE indentation preserves every byte inside a string literal, including
    /// CRLF and a doubled quote, while still indenting the next SQL line.
    #[test]
    fn cte_indentation_never_enters_a_multiline_string_literal() {
        let mut output = String::new();
        EmitterState::push_indented_cte(
            &mut output,
            "SELECT 'it''s\r\nstill data' AS value\nFROM logs",
        );
        assert_eq!(
            output,
            "  SELECT 'it''s\r\nstill data' AS value\n  FROM logs\n"
        );
    }
}
