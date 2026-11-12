// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Source glob computation for parquet file selection.
//!
//! Narrows the `read_parquet()` source argument based on time filters
//! and service names extracted from the DSL query, avoiding full-directory
//! scans when possible.
//!
//! [`select`] computes the selection and [`compute_source`] renders it for
//! `/query`. A capture (ADR-0057) reads the same selection for the same
//! text, so it reads what `/query` of its recorded text reads, and
//! [`selection_bytes`] measures the part of it that can hold the
//! capture's window.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Timelike as _, Utc};
use trawl_core::ast::{
    FieldFilter, FilterOp, FilterValue, Query, Scalar, SearchStage, SearchToken, Spanned,
};
use trawl_core::compare::{CompareForm, compare_form};
use trawl_core::schema::{ENVELOPE_TYPES, SERVICE};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::parquet_stats::{
    ParquetStatsError, StatVal, TimestampUnit, read_row_group_bounds,
};

use crate::env_dirs::{date_name, hour_name, parse_date, parse_hour};
use crate::ingest::envelope::MAX_TIME_LEAD;

/// How far before `now - N` a `last=N` walk over hour directories starts,
/// in seconds: room for the `_time` lead ingest allows.
///
/// Compaction files a row in the hour directory of its own clock, at or
/// after the row's arrival, and ingest replaces a `_time` more than
/// [`MAX_TIME_LEAD`] after arrival (ADR-0008 and ADR-0013, amendments of
/// 2026-11-08). So a row in hour directory H has a `_time` at most
/// `MAX_TIME_LEAD` past the end of H, and a row the query matches sits in a
/// directory no older than the hour of `now - N - MAX_TIME_LEAD`. A delayed
/// compaction only moves a row into a later directory, which the walk to
/// `now` covers. One hour is more than the bound needs: it keeps rows
/// stored before the bound, with a lead under an hour, visible.
const TIME_FILTER_PADDING_SECS: u64 = 3600;
const _: () = {
    assert!(MAX_TIME_LEAD.subsec_nanos() == 0);
    assert!(MAX_TIME_LEAD.num_seconds() <= TIME_FILTER_PADDING_SECS.cast_signed());
};

/// The file pattern that selects every service's file in a directory.
const ANY_SERVICE: &str = "*.parquet";

/// Which service files a selection reads in each of its directories.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Services {
    /// Every service's file: [`ANY_SERVICE`].
    Any,
    /// Only these services' files, `{service}.parquet` each, in name
    /// order. Empty when the search stage is a contradiction
    /// (`service=a service=b`), so no stored file can match. Only
    /// [`service_pins`] builds it.
    Only(BTreeSet<String>),
}

impl Services {
    /// The file pattern of each service file, in the order a source lists
    /// them within a directory: none for the empty set.
    fn file_patterns(&self) -> Vec<String> {
        match self {
            Self::Any => vec![ANY_SERVICE.to_owned()],
            Self::Only(services) => services.iter().map(|s| format!("{s}.parquet")).collect(),
        }
    }
}

/// The most distinct services one selection names. The engine probes each
/// list entry, a `stat` or a `glob()`, in every selected directory, so the
/// names multiply the probes; a larger set reads every service instead.
const MAX_PINNED_SERVICES: usize = 32;

/// How deep in parenthesised groups a `service=` filter can sit and still
/// pin. The walk recurses once per level. The parser refuses a search
/// deeper than [`trawl_core::parser::MAX_TREE_DEPTH`], and this bound is
/// tighter still, so a deeper group pins nothing instead of spending the
/// worker's stack.
const MAX_PIN_NESTING: usize = 32;

/// The services a search stage pins: the service files every event it
/// matches is stored in.
///
/// The SQL filter still decides every row, so a pin only has to name each
/// file a matching row can be in. `service=a` pins `{a}` and `service=a,b`
/// pins `{a, b}`. An AND pins what its pinning tokens pin in common, and a
/// token that pins nothing does not narrow it. An OR pins the union of its
/// branches when every branch pins. Everything else pins nothing: an AND
/// with no pinning token (an empty AND, which is true, included), an OR of
/// no branch, `NOT`, `!=` and every other operator, any other field, a
/// pattern, and a value that does not compare as text ([`pinned_name`]).
///
/// Each name is interpolated verbatim into a single-quoted glob entry, so
/// the final set, after every intersection and union, must pass the same
/// `is_valid_service_name` predicate ingest enforces: anything outside the
/// ingest-side charset (quotes, separators, slashes, dots at the front)
/// reads every service rather than being allowed to close one path and
/// open another. A name an intersection removed reaches no path. A final
/// set over [`MAX_PINNED_SERVICES`] reads every service too.
fn service_pins(search: &SearchStage) -> Services {
    match disjunction_pins(&search.groups, 0) {
        Some(services)
            if services.len() <= MAX_PINNED_SERVICES
                && services
                    .iter()
                    .all(|s| trawl_config::is_valid_service_name(s)) =>
        {
            Services::Only(services.into_iter().map(str::to_owned).collect())
        }
        _ => Services::Any,
    }
}

/// What an OR of `branches` pins ([`service_pins`]): nothing unless every
/// branch pins, and nothing for no branch at all.
///
/// Each union merges the smaller set into the larger, so a group of one
/// branch hands its set up unchanged and nesting adds no insertions.
///
/// `nesting` counts the groups around `branches`.
fn disjunction_pins(
    branches: &[Vec<Spanned<SearchToken>>],
    nesting: usize,
) -> Option<BTreeSet<&str>> {
    if branches.is_empty() {
        return None;
    }
    let mut union = BTreeSet::new();
    for branch in branches {
        let mut pinned = conjunction_pins(branch, nesting)?;
        if pinned.len() > union.len() {
            std::mem::swap(&mut union, &mut pinned);
        }
        union.extend(pinned);
    }
    Some(union)
}

/// What an AND of `tokens` pins ([`service_pins`]): nothing when no token
/// pins, an empty AND included.
///
/// Each intersection keeps the smaller set and drops from it what the
/// larger lacks, so it never copies the larger.
fn conjunction_pins(tokens: &[Spanned<SearchToken>], nesting: usize) -> Option<BTreeSet<&str>> {
    let mut common: Option<BTreeSet<&str>> = None;
    for token in tokens {
        let Some(mut pinned) = token_pins(&token.node, nesting) else {
            continue;
        };
        if let Some(mut so_far) = common.take() {
            if pinned.len() > so_far.len() {
                std::mem::swap(&mut pinned, &mut so_far);
            }
            pinned.retain(|name| so_far.contains(name));
        }
        common = Some(pinned);
    }
    common
}

/// What one search token pins ([`service_pins`]). A group nested past
/// [`MAX_PIN_NESTING`] pins nothing.
fn token_pins(token: &SearchToken, nesting: usize) -> Option<BTreeSet<&str>> {
    match token {
        SearchToken::FieldFilter(FieldFilter {
            field,
            op: FilterOp::Eq,
            value,
        }) if field == SERVICE => match value {
            FilterValue::Single(scalar) => pinned_name(scalar).map(|name| BTreeSet::from([name])),
            // One element that pins nothing makes the list pin nothing.
            FilterValue::List(scalars) => scalars.iter().map(pinned_name).collect(),
        },
        SearchToken::Group(branches) if nesting < MAX_PIN_NESTING => {
            disjunction_pins(branches, nesting + 1)
        }
        // `NOT`, text and time tokens, and every other filter.
        _ => None,
    }
}

/// The one service whose file holds every row `service=` `scalar` matches:
/// an exact value the `service` pin compares as text, which only a stored
/// service of the same bytes matches, and which no other name is stored as
/// ([`compaction_can_store_another_name_as`]). A numeric value also matches
/// the same number spelled another way (`service=1` matches a stored `01`,
/// in `01.parquet`), and a pattern matches many names, so neither pins.
fn pinned_name(scalar: &Scalar) -> Option<&str> {
    let Scalar::Exact(value) = scalar else {
        return None;
    };
    // The envelope's own pin (ADR-0009), which the catalog is seeded with.
    let pin = ENVELOPE_TYPES
        .iter()
        .find_map(|(field, ty)| (*field == SERVICE).then_some(*ty));
    match compare_form(pin, FilterOp::Eq, value) {
        Ok(CompareForm::Text(text))
            if text == *value && !compaction_can_store_another_name_as(value) =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// Whether compaction can store a service under the text `value` in the
/// file of a different name, so that `{value}.parquet` does not hold every
/// row whose stored service is `value`.
///
/// Compaction names the file after the service as sent, but reads the WAL
/// with `read_json`'s type detection, and the VARCHAR conform writes back
/// `DuckDB`'s rendering of whatever type it detected. Only two detections
/// change a valid service name: a DATE (`2026-1-1`, `26-01-01`, `epoch` and
/// `infinity` are stored as `YYYY-MM-DD`) and a UUID (an upper-case one is
/// stored lower-cased). So the two canonical renderings are the values that
/// can sit in another name's file. The shape alone decides: `2026-13-01`,
/// which no date renders as, reads every file too, which costs reads and
/// never rows. `a_value_compaction_rewrites_never_pins` runs the real
/// compaction over a corpus to keep this list complete.
fn compaction_can_store_another_name_as(value: &str) -> bool {
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    let hex = |part: &str, len: usize| {
        part.len() == len && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    let parts: Vec<&str> = value.split('-').collect();
    // A DATE renders its year padded to four digits.
    let date = matches!(parts.as_slice(), [year, month, day]
        if year.len() >= 4
            && year.bytes().all(|b| b.is_ascii_digit())
            && digits(month, 2)
            && digits(day, 2));
    // A UUID renders as lower-case hex in groups of 8, 4, 4, 4 and 12.
    let uuid = parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(part, len)| hex(part, len));
    date || uuid
}

/// Extract an exact value for `field` from the search stage, if present.
///
/// Only returns `Some` for single-group queries with a simple equality
/// filter (`env=prod`). Multi-group (OR) queries can't be narrowed to one
/// value safely, and glob/regex values are also ignored. Services are
/// narrowed to a set instead, by [`service_pins`].
fn extract_eq_filter<'a>(
    search: &'a trawl_core::ast::SearchStage,
    field_name: &str,
) -> Option<&'a str> {
    // Can't narrow when OR is involved — different groups may target different values.
    if search.groups.len() != 1 {
        return None;
    }
    search.groups[0].iter().find_map(|t| {
        if let SearchToken::FieldFilter(FieldFilter {
            field,
            op: FilterOp::Eq,
            value: FilterValue::Single(Scalar::Exact(s)),
        }) = &t.node
            && field == field_name
        {
            return Some(s.as_str());
        }
        None
    })
}

/// Compute the `read_parquet()` source argument over the two path
/// dimensions (ADR-0009): `data/{env}/{date}/{HH}/{service}.parquet`,
/// day-level `data/{env}/{date}/{service}.parquet` after rollup.
///
/// `env=X` pins the outer directory; otherwise every env directory on
/// disk is searched. A `service=` filter narrows every directory to the
/// files of the services the search stage pins ([`service_pins`]): one for
/// `service=a`, several for `service=a,b` or `service=a OR service=b`, and
/// none for a contradiction such as `service=a service=b`, which reads no
/// stored file. Each name is the file's verbatim (path encoding is
/// injective by validation, so `api.v2` and `api_v2` are distinct files
/// and pruning is exact). A time filter scopes to the relevant date/hour
/// directories; without one, date-formatted dirs are enumerated per env.
///
/// Returns a `DuckDB` list literal like
/// `['data/prod/2026-08-02/14/*.parquet', ...]` when scoping is
/// possible. Pruning is an optimization only: the SQL WHERE clause always
/// re-filters, so a broader source is never incorrect. No exit may widen
/// to `{base}/**/*.parquet`, though, which reaches past the env dimension
/// into `scheduled/` (materialized saved-query output, whose schema is a
/// query's rather than an event's, so a union over it is a hard error
/// under ADR-0008). Every "nothing to read" exit is [`no_match_source`],
/// including unparseable DSL: the executor parses the same text with the
/// same parser before it reads, so a source built from DSL that does not
/// parse never reaches `read_parquet` at all.
///
/// # Errors
/// As [`select`].
pub(crate) fn compute_source(
    base_dir: &str,
    dsl: &str,
    cancel: &CancelLatch,
) -> Result<String, EngineError> {
    Ok(select(base_dir, dsl, cancel)?.source())
}

/// The selection [`compute_source`] renders, before it is rendered: the
/// rules are [`compute_source`]'s.
///
/// # Errors
/// [`EngineError::Cancelled`] once `cancel` is latched during the parse.
pub(crate) fn select(
    base_dir: &str,
    dsl: &str,
    cancel: &CancelLatch,
) -> Result<SourceSelection, EngineError> {
    let base = base_dir.trim_end_matches('/').to_owned();

    let Ok(ast) = shape(dsl, cancel)? else {
        return Ok(SourceSelection {
            base,
            services: Services::Any,
            elements: Vec::new(),
        });
    };

    let Narrowing { services, envs } = narrowing(&base, &ast);
    let time_filter = ast.search.time_filter.as_ref().map(|tf| tf.node.duration);

    // One clock read for every env, so all of them walk the same hours.
    let now = Utc::now();
    let mut elements: Vec<Element> = Vec::new();
    for env in &envs {
        let env_base = format!("{base}/{env}");
        match time_filter {
            Some(duration) => {
                let scoped = time_scoped_dirs(&env_base, duration, &services, now);
                if scoped.is_empty() {
                    elements.extend(date_scoped_dirs(&env_base));
                } else {
                    elements.extend(scoped);
                }
            }
            None => elements.extend(date_scoped_dirs(&env_base)),
        }
    }

    // No elements is every env existing with no matching date directory,
    // or no env at all: the same "nothing to read" state as a cold start,
    // and the same reason not to fall back to `{base}/**/` over
    // `scheduled/`. [`SourceSelection::source`] renders it as
    // [`no_match_source`].
    Ok(SourceSelection {
        base,
        services,
        elements,
    })
}

/// The env directories a query's `env=` filter selects, by
/// [`compute_source`]'s rules, and how many: the one its `env=` pins, none
/// for a value no env can carry, else every env on disk. Unparseable DSL
/// selects none, as it reads nothing. A service contradiction, which reads
/// no env's files, still selects its envs here: this is the env pin alone.
///
/// Only the first `cap` names in name order are kept. With no pin the envs
/// come from [`env_dirs::first_env_names`](crate::env_dirs::first_env_names),
/// which holds no more than that however many envs there are and reads
/// `cancel` between the root's entries.
///
/// # Errors
/// [`FirstEnvNamesError`](crate::env_dirs::FirstEnvNamesError) from the
/// walk or the parse: cancelled once `cancel` is latched during either, or
/// a data root it could not read. A pinned env walks nothing and cannot fail.
pub(crate) fn first_selected_envs(
    base_dir: &str,
    dsl: &str,
    cap: usize,
    cancel: &CancelLatch,
) -> Result<crate::env_dirs::FirstEnvNames, crate::env_dirs::FirstEnvNamesError> {
    let base = base_dir.trim_end_matches('/');
    let Ok(ast) = shape(dsl, cancel).map_err(|_| crate::env_dirs::FirstEnvNamesError::Cancelled)?
    else {
        return Ok(crate::env_dirs::FirstEnvNames::default());
    };
    Ok(match env_pin(&ast) {
        EnvPin::One(env) => crate::env_dirs::FirstEnvNames {
            names: (cap > 0).then(|| env.to_owned()).into_iter().collect(),
            total: 1,
        },
        EnvPin::Nothing => crate::env_dirs::FirstEnvNames::default(),
        EnvPin::Every => {
            crate::env_dirs::first_env_names(Path::new(base), cap, &|| cancel.latched())?
        }
    })
}

/// Parse `dsl` for its shape alone, under the work's latch (ADR-0063). The
/// selection reads equality pins and the time filter and runs no pattern,
/// so the parse builds none; the executor's own parse refuses a pattern
/// that does not build before anything is read.
///
/// # Errors
/// [`EngineError::Cancelled`] once `cancel` is latched, never a parse
/// error: the caller's fallback is for text that does not parse.
fn shape(
    dsl: &str,
    cancel: &CancelLatch,
) -> Result<Result<Query, Vec<trawl_core::parser::ParseError>>, EngineError> {
    Ok(trawl_core::parser::parse_unbuilt_cancellable(
        dsl,
        &cancel.stop(),
    )?)
}

/// The source for "no on-disk file can match": a path segment that is not a
/// legal env name, so no data directory can ever carry it. The executor
/// reads "no files" as an empty cold side and the SQL filter keeps hot rows
/// correct.
fn no_match_source(base: &str) -> String {
    format!("{base}/.no-such-env/{ANY_SERVICE}")
}

/// What a query's equality filters pin: the service files and the env
/// directories to search. No envs means nothing on disk can match.
struct Narrowing {
    services: Services,
    envs: Vec<String>,
}

/// Read the `service=` and `env=` pins off a parsed query (see
/// [`compute_source`] for the rules).
fn narrowing(base: &str, ast: &Query) -> Narrowing {
    let services = service_pins(&ast.search);

    let envs: Vec<String> = match env_pin(ast) {
        // A service contradiction reads no file in any env, and selecting
        // no env keeps it out of the time walk's fallback to every date
        // directory.
        _ if services == Services::Only(BTreeSet::new()) => Vec::new(),
        EnvPin::One(env) => vec![env.to_owned()],
        // The executor treats "no files" as an empty result and the SQL
        // filter keeps hot rows correct.
        EnvPin::Nothing => Vec::new(),
        // Cold start (no env directories yet) lists none: nothing to
        // read, and no widening to `{base}/**/`, which would reach
        // `scheduled/`.
        EnvPin::Every => crate::env_dirs::list_env_names(Path::new(base)),
    };

    Narrowing { services, envs }
}

/// What a query's `env=` filter selects (see [`compute_source`]).
enum EnvPin<'a> {
    /// The one env it pins.
    One(&'a str),
    /// No env: the value is one no on-disk env can carry, since they were
    /// validated at ingest.
    Nothing,
    /// No pin: every env on disk.
    Every,
}

fn env_pin(ast: &Query) -> EnvPin<'_> {
    match extract_eq_filter(&ast.search, "env") {
        Some(env)
            if trawl_config::is_valid_env_name(env)
                && !trawl_config::RESERVED_ENV_NAMES.contains(&env) =>
        {
            EnvPin::One(env)
        }
        Some(_) => EnvPin::Nothing,
        None => EnvPin::Every,
    }
}

/// What one `read_parquet()` source reads: the partition directories and
/// the service files every directory is read for.
///
/// It is structured rather than a source string because a capture needs
/// two things from it: the `read_parquet()` argument ([`Self::source`]) and
/// the bytes behind it ([`selection_bytes`]), and the second must expand
/// exactly the directories and service files the first names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceSelection {
    /// The data root, without a trailing slash.
    base: String,
    /// The service files read in each directory.
    services: Services,
    /// The directories, in the order the source lists them.
    elements: Vec<Element>,
}

/// One directory of a [`SourceSelection`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    /// The directory's path.
    dir: String,
    /// Read through `**`: the pattern matches files at any depth under
    /// `dir`, `dir` itself included. Otherwise only the files directly in
    /// `dir`.
    recursive: bool,
}

impl Element {
    fn hour_or_day(dir: String) -> Self {
        Self {
            dir,
            recursive: false,
        }
    }

    fn date(dir: String) -> Self {
        Self {
            dir,
            recursive: true,
        }
    }

    /// The element's `read_parquet()` list entry.
    fn glob(&self, file_pattern: &str) -> String {
        if self.recursive {
            format!("'{}/**/{file_pattern}'", self.dir)
        } else {
            format!("'{}/{file_pattern}'", self.dir)
        }
    }
}

impl SourceSelection {
    /// The `read_parquet()` source argument: a `DuckDB` list literal with
    /// one entry per directory and service file, by directory and then by
    /// service name, or [`no_match_source`] when that is no entry: no
    /// directory was selected, or no service can match.
    pub(crate) fn source(&self) -> String {
        let patterns = self.services.file_patterns();
        let globs: Vec<String> = self
            .elements
            .iter()
            .flat_map(|element| patterns.iter().map(|pattern| element.glob(pattern)))
            .collect();
        if globs.is_empty() {
            return no_match_source(&self.base);
        }
        format!("[{}]", globs.join(", "))
    }

    /// Whether a file name matches one of the selection's file patterns
    /// the way `DuckDB`'s `glob()` does: `*` matches any run of bytes,
    /// including an empty one and a leading dot, a pinned name matches
    /// only itself, and the match is case-sensitive.
    fn matches(&self, name: &std::ffi::OsStr) -> bool {
        let name = name.as_encoded_bytes();
        match &self.services {
            Services::Any => name.ends_with(b".parquet"),
            Services::Only(services) => name
                .strip_suffix(b".parquet")
                .and_then(|stem| std::str::from_utf8(stem).ok())
                .is_some_and(|stem| services.contains(stem)),
        }
    }
}

/// The bytes of parquet `selection` reaches that can hold an event of the
/// window `[start, end)`: the summed size of every regular file one of its
/// file patterns matches in its directories, less each file whose footer
/// statistics put every row group's `_time` outside the window. Each
/// directory is walked once, whatever the number of services.
///
/// Partitions are named by when compaction wrote them and ingest keeps the
/// `_time` a sender says (`ingest::envelope`), so no directory name bounds
/// the `_time` of what it holds: the selection is the whole of what
/// `/query` reads for the same text, and only the files themselves can say
/// they hold nothing in the window. The engine's `_time` bounds skip the
/// same row groups by the same statistics, so a file left out here is one
/// whose rows the read would not keep. The proof is per row group and must
/// cover every one ([`read_row_group_bounds`]): a file counts whole when its
/// footer gives none, which is no `_time` column, a row group whose `_time`
/// chunk has no min or max, `_time` not a timestamp, one row group whose
/// bounds reach into the window, or a footer the reader refuses. Ticks are
/// read as UTC whether or not the file marks the column UTC-adjusted, as
/// the engine's UTC session casts them.
///
/// The walk lists the directories on disk; it never builds a path the disk
/// does not hold. It checks `cancel` before each entry and stops with
/// [`EngineError::Cancelled`] once the work is cancelled, so an abandoned
/// capture stops walking.
///
/// This is a budget estimate, not a promise. The caller takes it under the
/// publication read guard, before the engine runs, so no canonical Parquet
/// publication in this process lands between the measurement and the read.
/// Retention takes no such guard: a directory that is gone, or a file that
/// vanishes between the listing and its footer, counts as zero.
/// [`crate::env_dirs::is_absent`] is not asked here, because `DuckDB` reads
/// nothing at a path that is gone or dangles, so zero is the read's own
/// answer for a budget estimate. Any other IO error is returned, so the
/// caller cannot mistake an unreadable directory for an empty one.
///
/// Symlinks are not followed and count as zero. `DuckDB` would read
/// through a live link to a file, but trawld never writes a symlink into
/// the data root, which is owner-only (ADR-0052), so a link there is not
/// data this budget measures. A dangling link matches nothing in `DuckDB`
/// either.
pub(crate) fn selection_bytes(
    selection: &SourceSelection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    cancel: &CancelLatch,
) -> Result<u64, EngineError> {
    let window = Window {
        start: i128::from(start.timestamp_micros()),
        end: i128::from(end.timestamp_micros()),
    };
    let mut total: u64 = 0;
    for element in &selection.elements {
        let bytes = dir_bytes(
            selection,
            Path::new(&element.dir),
            element.recursive,
            window,
            cancel,
        )?;
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// A half-open window `[start, end)` in microseconds since the epoch.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: i128,
    end: i128,
}

/// The bytes under one directory of `selection`, for [`selection_bytes`].
fn dir_bytes(
    selection: &SourceSelection,
    dir: &Path,
    recursive: bool,
    window: Window,
    cancel: &CancelLatch,
) -> Result<u64, EngineError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(EngineError::Io(e)),
    };
    let mut total: u64 = 0;
    for entry in entries {
        if cancel.latched() {
            return Err(EngineError::Cancelled);
        }
        let entry = entry.map_err(EngineError::Io)?;
        // `DirEntry::file_type` does not traverse a symlink.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(EngineError::Io(e)),
        };
        let bytes = if file_type.is_dir() {
            // `**` descends into every directory, a dot-named one or one
            // whose own name matches a pattern included, and matches only
            // files.
            if !recursive {
                continue;
            }
            dir_bytes(selection, &entry.path(), true, window, cancel)?
        } else if file_type.is_file() && selection.matches(&entry.file_name()) {
            file_bytes(&entry, window)?
        } else {
            continue;
        };
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// One matching regular file's bytes, or zero when its footer proves it
/// holds no `_time` in `window` or it vanished (see [`selection_bytes`]).
fn file_bytes(entry: &std::fs::DirEntry, window: Window) -> Result<u64, EngineError> {
    let len = match entry.metadata() {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(EngineError::Io(e)),
    };
    match read_row_group_bounds(&entry.path(), trawl_core::schema::TIME) {
        Ok(groups) => {
            let outside = groups.is_some_and(|groups| {
                groups
                    .iter()
                    .all(|(min, max)| outside_window(Some(min), Some(max), window))
            });
            Ok(if outside { 0 } else { len })
        }
        Err(ParquetStatsError::Open { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(0)
        }
        Err(ParquetStatsError::Open { source, .. }) => Err(EngineError::Io(source)),
        // A footer the reader refuses reaches the engine whole, which
        // reads or refuses it on its own terms.
        Err(_) => Ok(len),
    }
}

/// Whether a column whose values lie in `[min, max]` holds nothing in
/// `window`: both bounds are timestamps, and the latest is before the
/// window or the earliest at or after its end. A bound read at a finer unit
/// than the microsecond is widened outward to whole microseconds, so a cast
/// that truncates or rounds it cannot move a value across the window.
fn outside_window(min: Option<&StatVal>, max: Option<&StatVal>, window: Window) -> bool {
    let (Some(min), Some(max)) = (
        min.and_then(|min| micros(min, false)),
        max.and_then(|max| micros(max, true)),
    ) else {
        return false;
    };
    max < window.start || min >= window.end
}

/// A timestamp sample in microseconds since the epoch, rounded down, or up
/// when `ceil`. `None` for any other sample.
fn micros(sample: &StatVal, ceil: bool) -> Option<i128> {
    let StatVal::Timestamp { raw, unit, .. } = sample else {
        return None;
    };
    let raw = i128::from(*raw);
    Some(match unit {
        TimestampUnit::Millis => raw * 1_000,
        TimestampUnit::Micros => raw,
        TimestampUnit::Nanos if ceil => {
            raw.div_euclid(1_000) + i128::from(raw.rem_euclid(1_000) != 0)
        }
        TimestampUnit::Nanos => raw.div_euclid(1_000),
    })
}

/// Time-scoped directories for one env root at `now`: hour-level for
/// today, day-level and/or hour-level for historical dates,
/// existence-filtered. Returns an empty vec when nothing on disk matches
/// the window.
///
/// A historical start day is selected whole, every hour of it: the walk
/// starts at that day's midnight rather than at the padded start's hour.
fn time_scoped_dirs(
    base: &str,
    duration: trawl_core::ast::TrawlDuration,
    services: &Services,
    now: DateTime<Utc>,
) -> Vec<Element> {
    let total_secs = duration
        .to_seconds()
        .saturating_add(TIME_FILTER_PADDING_SECS);

    let start = now - chrono::Duration::seconds(i64::try_from(total_secs).unwrap_or(i64::MAX));
    let from = if start.date_naive() == now.date_naive() {
        start.naive_utc()
    } else {
        start.date_naive().and_time(chrono::NaiveTime::MIN)
    };

    // Only directories that exist on disk are kept, which avoids sending
    // DuckDB a list of entirely nonexistent paths: that would trigger a
    // "No files found" error before the executor safety net catches it.
    partition_dirs(base, from, now.naive_utc(), services)
        .into_iter()
        .map(Element::hour_or_day)
        .collect()
}

/// The existing partition directories of one env root from `from`'s hour
/// through `now`'s, in walk order: per date, the date directory when it
/// holds day-level files, then its hour directories.
///
/// Today's hours are listed up to `now`'s. A historical date is checked
/// for a consolidated day-level file and for remaining hour directories,
/// since both coexist during a partial rollup (one service consolidated,
/// another not). `from`'s own date is listed from `from`'s hour, every
/// later date from midnight.
fn partition_dirs(
    base: &str,
    from: NaiveDateTime,
    now: NaiveDateTime,
    services: &Services,
) -> Vec<String> {
    let today = now.date();

    let mut dirs = Vec::new();
    let mut cursor = from
        .date()
        .and_hms_opt(from.hour(), 0, 0)
        .expect("valid hour from Timelike::hour()");

    while cursor <= now {
        let day = date_name(cursor.date());

        if cursor.date() == today {
            // Today may have day-level consolidated files from a
            // previous server session or an earlier daily rollup.
            // Check once, then list the hours of the time range.
            if has_day_level_files(base, &day, services) {
                dirs.push(format!("{base}/{day}"));
            }
            while cursor <= now {
                let hour = hour_name(cursor_hour(cursor));
                dirs.push(format!("{base}/{day}/{hour}"));
                cursor += chrono::Duration::hours(1);
            }
        } else {
            let has_day = has_day_level_files(base, &day, services);
            let has_hours = has_hour_dirs(base, &day);

            if has_day {
                dirs.push(format!("{base}/{day}"));
            }
            if has_hours || !has_day {
                for h in cursor_hour(cursor)..24 {
                    dirs.push(format!("{base}/{day}/{}", hour_name(h)));
                }
            }

            // Skip to next day (advance cursor past remaining hours of this day).
            cursor = (cursor.date() + chrono::Duration::days(1))
                .and_hms_opt(0, 0, 0)
                .expect("valid midnight from date + 1 day");
        }
    }

    dirs.retain(|dir| Path::new(dir).exists());
    dirs
}

/// The hour of `cursor`, as an hour directory counts it.
fn cursor_hour(cursor: NaiveDateTime) -> u8 {
    u8::try_from(cursor.hour()).expect("Timelike::hour() is below 24")
}

/// Date-scoped directories for one env root, for queries without a
/// `last=` filter.
///
/// Enumerates the date directories ([`parse_date`]) in the env base, each read
/// through `**`. Returns an empty vec when the env base holds no date dirs.
fn date_scoped_dirs(base: &str) -> Vec<Element> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if parse_date(name_str).is_some() && entry.path().is_dir() {
            dirs.push(format!("{base}/{name_str}"));
        }
    }

    // `read_dir` yields entries in filesystem order; sort so the emitted
    // source list is stable across calls.
    dirs.sort();
    dirs.into_iter().map(Element::date).collect()
}

/// Check if a date directory has day-level parquet files (consolidated).
///
/// For pinned services, does one `stat()` per service until one has its
/// day file, and none for the empty set. For every service, checks if the
/// date directory contains any direct `.parquet` files.
fn has_day_level_files(base: &str, day: &str, services: &Services) -> bool {
    let day_dir = std::path::Path::new(base).join(day);

    match services {
        Services::Any => {
            // Wildcard: check if any .parquet files exist directly in date dir.
            let Ok(entries) = std::fs::read_dir(&day_dir) else {
                return false;
            };
            entries.flatten().any(|e| {
                let p = e.path();
                !p.is_dir() && p.extension().is_some_and(|ext| ext == "parquet")
            })
        }
        // One member's day file is enough: the day directory's entry for
        // a member without one matches nothing.
        Services::Only(services) => services
            .iter()
            .any(|service| day_dir.join(format!("{service}.parquet")).exists()),
    }
}

/// Check if a date directory still has hourly subdirectories (00-23).
///
/// Used to detect mixed-state dirs where some services are consolidated
/// at day level while others remain in hourly dirs.
fn has_hour_dirs(base: &str, day: &str) -> bool {
    let day_dir = std::path::Path::new(base).join(day);
    let Ok(entries) = std::fs::read_dir(&day_dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let p = e.path();
        if !p.is_dir() {
            return false;
        }
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| parse_hour(n).is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve a bare (non-list) source pattern with the same matcher
    /// `read_parquet` uses, so a test can assert what a source really
    /// reaches on disk rather than what its text looks like.
    fn glob_matches(pattern: &str) -> Vec<String> {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!("SELECT file FROM glob('{pattern}')"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// A date-shaped directory that names no calendar date is not a date
    /// partition: a query without `last=` does not read it.
    #[test]
    fn date_scoped_dirs_skips_noncalendar_dates() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("prod");
        for day in ["2026-01-15", "2026-13-45"] {
            let hour = base.join(day).join("07");
            std::fs::create_dir_all(&hour).unwrap();
            std::fs::write(hour.join("svc.parquet"), b"").unwrap();
        }
        let base = base.to_str().unwrap();

        assert_eq!(
            date_scoped_dirs(base),
            [Element::date(format!("{base}/2026-01-15"))]
        );
    }

    #[test]
    fn no_time_filter_over_empty_root_matches_nothing() {
        // No env directory on disk → nothing to read.
        let source = compute_source("/data", "_severity=error", &running()).unwrap();
        assert_eq!(source, "/data/.no-such-env/*.parquet");
    }

    #[test]
    fn service_filter_narrows_glob() {
        // Exact service filter → narrow to service-specific file.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-01-15");
        let source = compute_source(base, "service=nginx", &running()).unwrap();
        assert_eq!(
            source,
            format!("['{base}/prod/2026-01-15/**/nginx.parquet']")
        );
    }

    #[test]
    fn service_glob_keeps_wildcard() {
        // Glob operator on service → can't narrow, keep *.parquet.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-01-15");
        let source = compute_source(base, "service=ng*", &running()).unwrap();
        assert_eq!(source, format!("['{base}/prod/2026-01-15/**/*.parquet']"));
    }

    #[test]
    fn bad_dsl_matches_nothing() {
        // Unparseable DSL takes the no-match shape like every other
        // "nothing to read" exit, never the `**` glob that reaches
        // `scheduled/`. The executor rejects the same text before it reads.
        let source = compute_source("/data", "| | invalid", &running()).unwrap();
        assert_eq!(source, "/data/.no-such-env/*.parquet");
        assert!(!source.contains("**"));
    }

    #[test]
    fn with_time_filter_returns_list() {
        // Create temp dir with today's hour directories so the filter doesn't prune them.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        // Create hour dirs for current and recent hours, deriving both date
        // and hour from the offset time (handles UTC midnight correctly).
        for h_offset in 0..=3 {
            let dt = now - chrono::Duration::hours(h_offset);
            let date = dt.format("%Y-%m-%d").to_string();
            let hour = dt.format("%H").to_string();
            std::fs::create_dir_all(tmp.path().join("prod").join(&date).join(&hour)).unwrap();
        }
        let source = compute_source(base, "last=1h", &running()).unwrap();
        // Should be a list of hour-directory globs, not the fallback.
        assert!(
            source.starts_with('['),
            "expected list format, got: {source}"
        );
        assert!(source.ends_with(']'), "expected list format, got: {source}");
        assert!(
            source.contains("*.parquet"),
            "expected parquet globs, got: {source}"
        );
        // With 1h + 1h padding, should have at least 2 hour entries.
        let count = source.matches("*.parquet").count();
        assert!(
            count >= 2,
            "expected >=2 hour globs for last=1h, got {count}: {source}"
        );
    }

    #[test]
    fn service_and_time_filter_compose() {
        // Create hour dirs, deriving both date and hour from the offset
        // time (handles UTC midnight correctly).
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        for h_offset in 0..=3 {
            let dt = now - chrono::Duration::hours(h_offset);
            let date = dt.format("%Y-%m-%d").to_string();
            let hour = dt.format("%H").to_string();
            std::fs::create_dir_all(tmp.path().join("prod").join(&date).join(&hour)).unwrap();
        }
        let source = compute_source(base, "service=nginx last=1h", &running()).unwrap();
        assert!(
            source.starts_with('['),
            "expected list format, got: {source}"
        );
        // Should narrow to nginx.parquet, not *.parquet.
        assert!(
            source.contains("nginx.parquet"),
            "expected service-scoped globs, got: {source}"
        );
        assert!(
            !source.contains("*.parquet"),
            "should not contain wildcard when service is known, got: {source}"
        );
    }

    #[test]
    fn strips_trailing_slash() {
        let source = compute_source("/data/", "last=1h", &running()).unwrap();
        assert!(!source.contains("//"), "double slashes in source: {source}");
    }

    #[test]
    fn prefers_day_level_for_historical_service() {
        // Create a temp data dir with a consolidated day-level file.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=nginx last=48h", &running()).unwrap();

        // Should include day-level path for yesterday (no /HH/ component).
        let expected_day_glob = format!("'{base}/prod/{yesterday}/nginx.parquet'");
        assert!(
            source.contains(&expected_day_glob),
            "expected day-level glob for {yesterday}, got: {source}"
        );
    }

    #[test]
    fn falls_back_to_hourly_when_no_day_file() {
        // Create a temp data dir with only hourly files (no day-level).
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        // Create hour dirs but no day-level file.
        let hour_dir = tmp.path().join("prod").join(&yesterday).join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("nginx.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=nginx last=48h", &running()).unwrap();

        // Only hour 14 exists on disk, so only that glob survives filtering.
        let hourly_pattern = format!("{base}/prod/{yesterday}/14/nginx.parquet");
        assert!(
            source.contains(&hourly_pattern),
            "expected hourly glob for existing dir, got: {source}"
        );
    }

    #[test]
    fn wildcard_detects_day_level() {
        // Wildcard service with consolidated day-level files.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        std::fs::write(day_dir.join("postgres.parquet"), b"data").unwrap();

        let source = compute_source(base, "last=48h", &running()).unwrap();

        // Should use day-level glob (*.parquet at date level).
        let expected_day_glob = format!("'{base}/prod/{yesterday}/*.parquet'");
        assert!(
            source.contains(&expected_day_glob),
            "expected day-level wildcard glob for {yesterday}, got: {source}"
        );
    }

    #[test]
    fn day_level_files_service_specific() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        let base = tmp.path().to_str().unwrap();
        assert!(has_day_level_files(base, "2026-01-15", &only(&["nginx"])));
        assert!(!has_day_level_files(
            base,
            "2026-01-15",
            &only(&["postgres"])
        ));
        // Any member's day file counts, wherever it sorts.
        assert!(has_day_level_files(
            base,
            "2026-01-15",
            &only(&["apache", "nginx", "postgres"])
        ));
        assert!(!has_day_level_files(base, "2026-01-15", &only::<&str>(&[])));
    }

    #[test]
    fn day_level_files_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        std::fs::create_dir_all(&day_dir).unwrap();

        // No files yet.
        assert!(!has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            &Services::Any
        ));

        // Add a parquet file.
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        assert!(has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            &Services::Any
        ));
    }

    #[test]
    fn day_level_files_ignores_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");
        let hour_dir = day_dir.join("00");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("nginx.parquet"), b"data").unwrap();

        // Hour subdir files should not count as day-level.
        assert!(!has_day_level_files(
            tmp.path().to_str().unwrap(),
            "2026-01-15",
            &Services::Any
        ));
    }

    #[test]
    fn hour_dirs_detects_hour_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("2026-01-15");

        // No dir at all.
        assert!(!has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));

        // Dir exists but empty.
        std::fs::create_dir_all(&day_dir).unwrap();
        assert!(!has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));

        // Hour subdir present.
        std::fs::create_dir_all(day_dir.join("01")).unwrap();
        assert!(has_hour_dirs(tmp.path().to_str().unwrap(), "2026-01-15"));
    }

    #[test]
    fn wildcard_mixed_state() {
        // Mixed state: day-level file for one service + hourly dir for another.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        let day_dir = tmp.path().join("prod").join(&yesterday);
        std::fs::create_dir_all(&day_dir).unwrap();

        // nginx consolidated at day level.
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();

        // postgres still in hourly dirs.
        let hour_dir = day_dir.join("14");
        std::fs::create_dir_all(&hour_dir).unwrap();
        std::fs::write(hour_dir.join("postgres.parquet"), b"data").unwrap();

        let source = compute_source(base, "last=48h", &running()).unwrap();

        // Should include both the day-level glob and the existing hourly dir.
        let day_glob = format!("'{base}/prod/{yesterday}/*.parquet'");
        let hourly_glob = format!("'{base}/prod/{yesterday}/14/*.parquet'");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob in mixed state, got: {source}"
        );
        assert!(
            source.contains(&hourly_glob),
            "expected hourly glob for existing dir in mixed state, got: {source}"
        );
    }

    #[test]
    fn sanitizes_dotted_service() {
        // Filenames carry the service verbatim (ADR-0009), so a dotted
        // service prunes to its own file: `api.v2` and `api_v2` are
        // distinct.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-01-15");
        let source = compute_source(base, "service=api.v2", &running()).unwrap();
        assert_eq!(
            source,
            format!("['{base}/prod/2026-01-15/**/api.v2.parquet']"),
            "the file pattern carries the service name verbatim"
        );
    }

    #[test]
    fn env_and_service_pruning_matrix() {
        // Two envs on disk, one date dir each. env=X pins the env
        // directory; service=Y pins the file pattern; both compose;
        // neither searches every env.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        for env in ["prod", "lab"] {
            std::fs::create_dir_all(tmp.path().join(env).join("2026-01-15")).unwrap();
        }

        // env only.
        let source = compute_source(base, "env=prod", &running()).unwrap();
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(
            !source.contains("/lab/"),
            "env=prod must prune lab: {source}"
        );

        // service only — both envs searched, file pattern pinned.
        let source = compute_source(base, "service=nginx", &running()).unwrap();
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");
        assert!(!source.contains("*.parquet"), "got: {source}");

        // both.
        let source = compute_source(base, "env=lab service=nginx", &running()).unwrap();
        assert!(source.contains("/lab/"), "got: {source}");
        assert!(!source.contains("/prod/"), "got: {source}");
        assert!(source.contains("nginx.parquet"), "got: {source}");

        // neither — every env, wildcard pattern.
        let source = compute_source(base, "_severity=error", &running()).unwrap();
        assert!(source.contains("/prod/"), "got: {source}");
        assert!(source.contains("/lab/"), "got: {source}");
    }

    #[test]
    fn dotted_and_underscored_services_prune_distinctly() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let date_dir = tmp.path().join("prod").join("2026-01-15");
        std::fs::create_dir_all(&date_dir).unwrap();
        std::fs::write(date_dir.join("api.v2.parquet"), b"a").unwrap();
        std::fs::write(date_dir.join("api_v2.parquet"), b"b").unwrap();

        let dotted = compute_source(base, "service=api.v2", &running()).unwrap();
        let underscored = compute_source(base, "service=api_v2", &running()).unwrap();
        assert!(dotted.contains("api.v2.parquet"), "got: {dotted}");
        assert!(underscored.contains("api_v2.parquet"), "got: {underscored}");
        assert_ne!(dotted, underscored, "the two services must prune apart");
    }

    #[test]
    fn invalid_env_value_matches_nothing() {
        // `env=../evil` can never match on-disk data (env names were
        // validated at ingest) — the source must not glob anything real.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod").join("2026-01-15")).unwrap();

        let source = compute_source(base, "env=../evil", &running()).unwrap();
        assert!(
            source.contains(".no-such-env"),
            "invalid env must yield a no-match source, got: {source}"
        );
        assert!(!source.contains("/prod/"), "got: {source}");
    }

    /// A post-cutover install before its first compaction: `data/` holds the
    /// EPOCH marker and `scheduled/` report runs, and no env directory yet.
    /// The source must not reach into `scheduled/` — those parquet files are
    /// materialized saved-query output, not events, and unioning them into
    /// the log side turns into a hard query error under ADR-0008.
    #[test]
    fn cold_start_source_never_reaches_scheduled_report_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join("EPOCH"), b"2").unwrap();
        let runs = tmp.path().join("scheduled").join("errors-by-host");
        std::fs::create_dir_all(&runs).unwrap();
        let run = runs.join("run_1.parquet");
        std::fs::write(&run, b"report run").unwrap();

        let fallback = format!("{base}/**/*.parquet");
        let source = compute_source(base, "_severity=error", &running()).unwrap();

        assert_eq!(source, format!("{base}/.no-such-env/*.parquet"));
        assert!(
            glob_matches(&source).is_empty(),
            "cold-start source must match nothing, got: {source}"
        );
        // Sanity check: a `**` glob over the data root really does sweep
        // the report run in, which is why no exit may emit one.
        assert!(
            glob_matches(&fallback).contains(&run.to_string_lossy().into_owned()),
            "sanity: the `**` fallback is what reached the scheduled run"
        );
    }

    /// Same, one dimension in: env directories exist, but none holds a date
    /// directory the query could read. Still nothing to read, still no
    /// excuse to glob the whole data root.
    #[test]
    fn no_matching_date_dir_never_reaches_scheduled_report_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::write(tmp.path().join("EPOCH"), b"2").unwrap();
        let runs = tmp.path().join("scheduled").join("errors-by-host");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("run_1.parquet"), b"report run").unwrap();
        // Env dirs exist (so the cold-start branch is not the one under
        // test) but neither carries a `YYYY-MM-DD` directory.
        for env in ["prod", "lab"] {
            std::fs::create_dir_all(tmp.path().join(env)).unwrap();
        }

        let source = compute_source(base, "_severity=error", &running()).unwrap();

        assert_eq!(source, format!("{base}/.no-such-env/*.parquet"));
        assert!(
            glob_matches(&source).is_empty(),
            "date-less source must match nothing, got: {source}"
        );
    }

    #[test]
    fn today_day_level_files_included() {
        // When today's data is consolidated at day level (no hourly dirs),
        // the time-scoped path should include the day-level glob.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();

        // Create day-level file only (no hourly subdirs).
        let day_dir = tmp.path().join("prod").join(&today);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("trawld.parquet"), b"data").unwrap();

        let source = compute_source(base, "service=trawld last=1h", &running()).unwrap();

        // Day-level glob must be present so the consolidated file is found.
        let day_glob = format!("'{base}/prod/{today}/trawld.parquet'");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob for today, got: {source}"
        );
    }

    #[test]
    fn today_mixed_day_and_hourly() {
        // Today has both day-level files and hourly dirs — both should appear.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let now = chrono::Utc::now();
        let today = now.format("%Y-%m-%d").to_string();
        let hour = now.format("%H").to_string();

        let day_dir = tmp.path().join("prod").join(&today);
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("nginx.parquet"), b"data").unwrap();
        std::fs::create_dir_all(day_dir.join(&hour)).unwrap();

        let source = compute_source(base, "last=1h", &running()).unwrap();

        let day_glob = format!("'{base}/prod/{today}/*.parquet'");
        let hourly_glob = format!("{base}/prod/{today}/{hour}/");
        assert!(
            source.contains(&day_glob),
            "expected day-level glob, got: {source}"
        );
        assert!(
            source.contains(&hourly_glob),
            "expected hourly glob, got: {source}"
        );
    }

    #[test]
    fn invalid_service_value_cannot_inject_paths() {
        // A quoted DSL literal can carry `', '` — the list-item separator.
        // It must never reach the glob list: the pattern falls back to the
        // wildcard, so no attacker-chosen path is ever emitted.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        std::fs::create_dir_all(tmp.path().join("prod").join("2026-01-15")).unwrap();

        for dsl in [
            r#"service="nginx', '/**/*""#,
            r#"service="nginx', '/etc/shadow""#,
            "service=../../escaped",
            "service=.hidden",
        ] {
            let source = compute_source(base, dsl, &running()).unwrap();
            assert!(
                source.contains("*.parquet"),
                "invalid service must fall back to the wildcard, got: {source}"
            );
            assert!(
                !source.contains("nginx") && !source.contains("hidden"),
                "invalid service must not reach the glob list, got: {source}"
            );
            assert!(
                !source.contains("/etc/") && !source.contains(".."),
                "invalid service must not escape the data root, got: {source}"
            );
        }
    }

    #[test]
    fn verbatim_service_with_time_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-01-15");
        let source = compute_source(base, "service=host.name last=1h", &running()).unwrap();

        // Filenames carry the service verbatim (ADR-0009).
        assert!(
            source.contains("host.name.parquet"),
            "expected the verbatim service in the glob, got: {source}"
        );
        assert!(
            !source.contains("host_name.parquet"),
            "must not sanitize the service name, got: {source}"
        );
    }

    // -----------------------------------------------------------------
    // Service pins: `service_pins`
    // -----------------------------------------------------------------

    /// The services `dsl`'s search stage pins.
    fn pins(dsl: &str) -> Services {
        let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
        service_pins(&query.search)
    }

    /// A pin of exactly `names`.
    fn only<S: AsRef<str>>(names: &[S]) -> Services {
        Services::Only(names.iter().map(|n| n.as_ref().to_owned()).collect())
    }

    /// An OR pins the union of its branches only when every branch pins;
    /// an AND intersects what its pinning tokens pin. Anything those rules
    /// cannot prove reads every service.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn service_pins_boolean_matrix() {
        let any = Services::Any;
        for (dsl, expected) in [
            // One equality, a list, and an OR of them: the union.
            ("service=a", only(&["a"])),
            ("service=a,b", only(&["a", "b"])),
            ("(service=a OR service=b)", only(&["a", "b"])),
            ("service=a OR service=b", only(&["a", "b"])),
            ("service=a OR service=b,c", only(&["a", "b", "c"])),
            ("service=a last=1h OR service=b", only(&["a", "b"])),
            // An AND intersects the tokens that pin; one that pins
            // nothing does not narrow it.
            ("service=a,b service=b,c", only(&["b"])),
            ("service=a service=b", only::<&str>(&[])),
            (
                "service=a _severity=error -debug \"a phrase\"",
                only(&["a"]),
            ),
            ("service=a k8s_pod=b* host!=c NOT env=lab", only(&["a"])),
            ("service=a last=1h", only(&["a"])),
            // Nested groups follow the same two rules.
            ("service=a,b (service=b OR service=c)", only(&["b"])),
            (
                "((service=a OR service=b) OR (service=c env=prod))",
                only(&["a", "b", "c"]),
            ),
            ("(service=a OR (service=b OR k8s_pod=x))", any.clone()),
            (
                "(service=a OR service=b) (service=c OR k8s_pod=x)",
                only(&["a", "b"]),
            ),
            // One branch that pins nothing reads every service.
            ("(service=a OR k8s_pod=b*)", any.clone()),
            ("service=a OR _severity=error", any.clone()),
            ("service=a OR *", any.clone()),
            // Negation, other operators and patterns never pin.
            ("NOT service=a", any.clone()),
            ("NOT (NOT service=a)", any.clone()),
            ("NOT (service=a OR service=b)", any.clone()),
            ("service!=a", any.clone()),
            ("service!=a,b", any.clone()),
            ("service>=a", any.clone()),
            ("service=a*", any.clone()),
            ("service=/a/", any.clone()),
            ("service=a,b*", any.clone()),
            ("service=a,/b/", any.clone()),
            // Only the field `service`, spelled exactly.
            ("Service=a", any.clone()),
            ("SERVICE=a", any.clone()),
            ("_service=a", any.clone()),
            ("env=prod", any.clone()),
            // No token pins, or there is no group at all (`last=1h`).
            ("*", any.clone()),
            ("last=1h", any.clone()),
            ("_severity=error", any.clone()),
            // A numeric literal also matches a stored service of the same
            // value (`1` matches `01`, in `01.parquet`), so it pins nothing.
            ("service=1", any.clone()),
            ("service=\"1\"", any.clone()),
            ("service=1,api", any.clone()),
            ("(service=1 OR service=api)", any.clone()),
            ("service=1 service=01", any.clone()),
            ("service=nan", any.clone()),
            ("service=inf", any.clone()),
            ("service=1e5", any.clone()),
            ("service=1 service=api", only(&["api"])),
            // Compaction can store another name as a canonical date or UUID
            // (`2026-1-1` is stored as `2026-01-01`, in `2026-1-1.parquet`),
            // so neither shape pins.
            ("service=2026-01-01", any.clone()),
            ("service=2026-01-01,nginx", any.clone()),
            ("(service=2026-01-01 OR service=nginx)", any.clone()),
            ("service=1900-01-01", any.clone()),
            ("service=550e8400-e29b-41d4-a716-446655440000", any.clone()),
            (
                "service=nginx,550e8400-e29b-41d4-a716-446655440000",
                any.clone(),
            ),
            ("service=2026-01-01 service=nginx", only(&["nginx"])),
            // No name is stored as another spelling of a date or a UUID,
            // nor as a boolean or a null word, so those still pin.
            ("service=2026-1-1", only(&["2026-1-1"])),
            ("service=2026-01-01T10", only(&["2026-01-01T10"])),
            (
                "service=550E8400-E29B-41D4-A716-446655440000",
                only(&["550E8400-E29B-41D4-A716-446655440000"]),
            ),
            (
                "service=550e8400e29b41d4a716446655440000",
                only(&["550e8400e29b41d4a716446655440000"]),
            ),
            ("service=true,null", only(&["null", "true"])),
            ("service=nginx,api.v2", only(&["api.v2", "nginx"])),
            // Names are checked on the final set: one the intersection
            // removes never reaches a path, one left in reads everything.
            ("service=a,\".x\"", any.clone()),
            ("service=\"x', '/etc\"", any.clone()),
            ("service=a,\".x\" service=a", only(&["a"])),
            ("service=\"x'\" service=a", only::<&str>(&[])),
            // A repeated name is one name.
            ("service=a,a", only(&["a"])),
            ("service=a OR service=a", only(&["a"])),
        ] {
            assert_eq!(pins(dsl), expected, "{dsl}");
        }

        // At most 32 distinct names, counted on the final set.
        let names: Vec<String> = (0..34).map(|i| format!("svc{i:02}")).collect();
        let list = |names: &[String]| names.join(",");
        assert_eq!(
            pins(&format!("service={}", list(&names[..32]))),
            only(&names[..32])
        );
        assert_eq!(pins(&format!("service={}", list(&names[..33]))), any);
        assert_eq!(
            pins(&format!("service={},svc00", list(&names[..32]))),
            only(&names[..32]),
            "a repeat is not a 33rd name"
        );
        assert_eq!(
            pins(&format!("service={} service=svc05", list(&names[..33]))),
            only(&["svc05"]),
            "33 names narrowed to one pin that one"
        );
        assert_eq!(
            pins(&format!(
                "service={} OR service={}",
                list(&names[..17]),
                list(&names[17..34])
            )),
            any,
            "two branches of 17 are 34 names"
        );
        assert_eq!(
            pins(&format!(
                "service={} OR service={}",
                list(&names[..20]),
                list(&names[12..32])
            )),
            only(&names[..32]),
            "overlapping branches are their distinct names"
        );

        // Shapes the parser never builds. An empty AND branch is true, so
        // `[] OR service=a` matches every event; no branch at all pins
        // nothing rather than the empty set.
        let parsed = |dsl: &str| trawl_core::parser::parse(dsl).unwrap().search;
        let mut search = parsed("service=a");
        search.groups.insert(0, Vec::new());
        assert_eq!(service_pins(&search), any, "an empty first group");
        let mut search = parsed("service=a");
        search.groups.clear();
        assert_eq!(service_pins(&search), any, "no group");
        let no_branch = Spanned::new(SearchToken::Group(Vec::new()), 0..0);
        let mut search = parsed("service=a");
        search.groups = vec![vec![no_branch.clone()]];
        assert_eq!(service_pins(&search), any, "a group of no branch alone");
        let mut search = parsed("service=a");
        search.groups[0].insert(0, no_branch.clone());
        assert_eq!(
            service_pins(&search),
            only(&["a"]),
            "a group of no branch does not narrow an AND"
        );
        let mut search = parsed("service=a");
        search.groups.push(vec![no_branch]);
        assert_eq!(service_pins(&search), any, "nor does it pin an OR branch");
    }

    /// A `service=` filter pins through [`MAX_PIN_NESTING`] groups and no
    /// deeper. Past the bound the walk stops, so a search nested thousands
    /// deep reads every service on the 2 MiB stack of a blocking worker
    /// instead of overflowing it. The parser refuses a search that deep,
    /// so that tree is built in code.
    #[test]
    fn a_group_nested_past_the_bound_pins_nothing() {
        let nested = |depth: usize, parsed: bool| {
            std::thread::Builder::new()
                .stack_size(2 << 20)
                .spawn(move || {
                    let in_brackets =
                        |depth| format!("{}service=a{}", "(".repeat(depth), ")".repeat(depth));
                    if parsed {
                        let dsl = in_brackets(depth);
                        return service_pins(&trawl_core::parser::parse(&dsl).unwrap().search);
                    }
                    let mut search = trawl_core::parser::parse("service=a").unwrap().search;
                    let mut token = search.groups[0].pop().unwrap();
                    for _ in 0..depth {
                        token = Spanned::new(SearchToken::Group(vec![vec![token]]), 0..0);
                    }
                    search.groups[0].push(token);
                    service_pins(&search)
                })
                .unwrap()
                .join()
                .unwrap()
        };
        let a = Services::Only(BTreeSet::from(["a".to_owned()]));
        assert_eq!(nested(1, true), a);
        assert_eq!(nested(MAX_PIN_NESTING, true), a);
        assert_eq!(nested(MAX_PIN_NESTING + 1, true), Services::Any);
        let deepest = trawl_core::parser::MAX_TREE_DEPTH;
        assert_eq!(nested(deepest, true), Services::Any);
        assert_eq!(nested(4000, false), Services::Any);
    }

    /// A pin reads only `{name}.parquet`, so it is sound only while every
    /// row stored with service `name` sits in that file. Compaction names
    /// the file after the service as sent, but its `read_json` detection
    /// can read the value as a DATE or a UUID, and the VARCHAR conform then
    /// stores the canonical rendering instead. Run the real WAL writer and
    /// compaction over valid names, and require that no stored value that
    /// differs from its file's name pins.
    #[tokio::test]
    async fn a_value_compaction_rewrites_never_pins() {
        use crate::ingest::wal::WalWriter;

        const CORPUS: &[&str] = &[
            "2026-1-1",
            "2026-01-01",
            "26-01-01",
            "12-30-00",
            "01-02-2026",
            "31-12-2026",
            "1-1-1",
            "20260101",
            "2026-01-01T10",
            "epoch",
            "infinity",
            "-infinity",
            "INFINITY",
            "550E8400-E29B-41D4-A716-446655440000",
            "550e8400e29b41d4a716446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "true",
            "TRUE",
            "false",
            "t",
            "yes",
            "null",
            "NULL",
            "nan",
            "1",
            "01",
            "1.0",
            "1e5",
            "0x10",
            "1-2",
            "a",
            "cafe",
            "dead-beef",
            "api.v2",
            "nginx",
        ];

        let dir = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (dir.path().join("wal"), dir.path().join("data"));
        let wal = WalWriter::new(wal_dir.clone());
        wal.ensure_dir().unwrap();
        for name in CORPUS {
            assert!(trawl_config::is_valid_service_name(name), "{name}");
            let event = serde_json::json!({
                "_time": "2026-01-03T00:05:00Z",
                "_ingested": "2026-01-03T00:05:01Z",
                "service": name,
                "message": "probe",
            });
            let mut line = serde_json::to_vec(&event).unwrap();
            line.push(b'\n');
            wal.write("prod", name, &line).unwrap();
        }
        let failures = crate::ingest::compaction::compact_once(
            &wal_dir,
            &data_dir,
            std::time::Duration::ZERO,
            false,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .unwrap();
        assert_eq!(failures, 0);

        let files = glob_matches(&format!("{}/**/*.parquet", data_dir.display()));
        assert_eq!(files.len(), CORPUS.len(), "{files:?}");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut rewrites = Vec::new();
        for file in &files {
            let name = Path::new(file).file_stem().unwrap().to_str().unwrap();
            let stored: Vec<String> = conn
                .prepare(&format!(
                    "SELECT DISTINCT service FROM read_parquet('{file}')"
                ))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            for value in stored.into_iter().filter(|value| value != name) {
                rewrites.push(format!("{name} -> {value}"));
                assert_eq!(
                    pinned_name(&Scalar::Exact(value.clone())),
                    None,
                    "{value} is stored in {name}.parquet"
                );
            }
        }
        assert!(
            rewrites.iter().any(|r| r == "2026-1-1 -> 2026-01-01"),
            "the corpus still makes compaction rewrite a value: {rewrites:?}"
        );
    }

    /// Every file `source` reaches: each entry of its list, or the bare
    /// source, expanded by `DuckDB`'s own `glob()`.
    fn source_files(source: &str) -> Vec<String> {
        let entries: Vec<&str> = match source.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            Some(list) => list.split(", ").map(|e| e.trim_matches('\'')).collect(),
            None => vec![source],
        };
        entries.into_iter().flat_map(glob_matches).collect()
    }

    /// A selection that pins several services names only their files,
    /// each once, and charges exactly those files, in both layouts a
    /// selection reads: hour and day directories for `last=`, and each
    /// date read through `**` otherwise. One day is rolled up for one
    /// service only, and a pinned service may have no file at all.
    #[test]
    fn multi_service_selection_matches_accounting() {
        use std::collections::BTreeMap;
        use trawl_core::ast::{TimeUnit, TrawlDuration};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let base = root.to_str().unwrap();
        // 2026-11-07 rolled up nginx alone; postgres and redis are still
        // hourly there. 2026-11-08 is hourly for all three.
        let mut files = vec!["prod/2026-11-07/nginx.parquet".to_owned()];
        for hh in ["10", "14"] {
            for service in ["postgres", "redis"] {
                files.push(format!("prod/2026-11-07/{hh}/{service}.parquet"));
            }
        }
        for hh in ["12", "13"] {
            for service in ["nginx", "postgres", "redis"] {
                files.push(format!("prod/2026-11-08/{hh}/{service}.parquet"));
            }
        }
        // A distinct power of two per file, so a total names its files.
        let sizes: BTreeMap<String, u64> = files
            .iter()
            .enumerate()
            .map(|(i, rel)| {
                let path = root.join(rel).to_string_lossy().into_owned();
                (path, write_len(root, rel, 1 << i))
            })
            .collect();

        // `last=1d` at 13:37 walks all of 2026-11-07 and today to 13:00.
        let q = at("2026-11-08T13:37:00Z");
        let one_day = TrawlDuration {
            quantity: 1,
            unit: TimeUnit::Days,
        };
        let (earliest, latest) = ("2026-11-07T00:00:00Z", "2026-11-08T13:37:00Z");
        for (dsl, services) in [
            ("service=nginx,postgres", &["nginx", "postgres"][..]),
            (
                "(service=nginx OR service=postgres)",
                &["nginx", "postgres"],
            ),
            ("service=postgres,redis", &["postgres", "redis"]),
            ("service=nginx,absent", &["nginx"]),
            ("service=redis OR service=absent", &["redis"]),
            ("service=absent,gone", &[]),
            ("service=nginx service=postgres", &[]),
            ("*", &["nginx", "postgres", "redis"]),
        ] {
            let expected: BTreeSet<&str> = sizes
                .keys()
                .filter(|path| {
                    services
                        .iter()
                        .any(|s| path.ends_with(&format!("/{s}.parquet")))
                })
                .map(String::as_str)
                .collect();
            let charge: u64 = expected.iter().map(|path| sizes[*path]).sum();

            let pinned = service_pins(&trawl_core::parser::parse(dsl).unwrap().search);
            let time_scoped = SourceSelection {
                base: base.to_owned(),
                elements: time_scoped_dirs(&format!("{base}/prod"), one_day, &pinned, q),
                services: pinned,
            };
            assert!(time_scoped.elements.iter().all(|e| !e.recursive));
            let date_scoped = select(base, &executed(dsl, earliest, latest), &running()).unwrap();
            assert!(date_scoped.elements.iter().all(|e| e.recursive));

            for (layout, selection) in [("time", &time_scoped), ("date", &date_scoped)] {
                let source = selection.source();
                let matched = source_files(&source);
                let distinct: BTreeSet<&str> = matched.iter().map(String::as_str).collect();
                assert_eq!(distinct, expected, "{layout} {dsl}: {source}");
                assert_eq!(
                    distinct.len(),
                    matched.len(),
                    "{layout} {dsl}: a file twice"
                );
                assert_eq!(
                    selection_bytes(selection, at(earliest), at(latest), &running()).unwrap(),
                    charge,
                    "{layout} {dsl}: {source}"
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // Capture windows (ADR-0057): `select`, `selection_bytes`
    // -----------------------------------------------------------------

    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("test instant")
            .with_timezone(&Utc)
    }

    /// Create `{root}/{rel}` as a directory.
    fn mkdir(root: &Path, rel: &str) {
        std::fs::create_dir_all(root.join(rel)).unwrap();
    }

    /// Write `len` bytes to `{root}/{rel}`, creating its directory: a file
    /// whose footer no reader accepts.
    fn write_len(root: &Path, rel: &str, len: usize) -> u64 {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; len]).unwrap();
        u64::try_from(len).unwrap()
    }

    /// Write `{root}/{rel}` as parquet with one row per `_time`, each a
    /// `DuckDB` timestamp of `ty`, and return its size.
    fn write_times(root: &Path, rel: &str, ty: &str, times: &[&str]) -> u64 {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rows: Vec<String> = times
            .iter()
            .map(|t| format!("(CAST('{t}' AS {ty}), 'm')"))
            .collect();
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT * FROM (VALUES {}) AS t(_time, message)) \
                 TO '{}' (FORMAT PARQUET)",
                rows.join(", "),
                path.display()
            ))
            .unwrap();
        std::fs::metadata(path).unwrap().len()
    }

    /// Write `{root}/{rel}` as parquet of two row groups of 2,048 rows,
    /// every `_time` of the first `first` and of the second `second`, and
    /// return its path.
    fn write_two_groups(root: &Path, rel: &str, first: &str, second: &str) -> std::path::PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT CASE WHEN i < 2048 THEN TIMESTAMP '{first}' \
                 ELSE TIMESTAMP '{second}' END AS _time, 'm' AS message \
                 FROM range(4096) t(i)) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 2048)",
                path.display()
            ))
            .unwrap();
        path
    }

    /// The bytes [`selection_bytes`] charges for `service`'s files over
    /// `[09:00, 09:30)` on 2026-10-23.
    fn charged(base: &str, service: &str) -> u64 {
        let text = executed(
            &format!("service={service}"),
            "2026-10-23T09:00:00Z",
            "2026-10-23T09:30:00Z",
        );
        selection_bytes(
            &select(base, &text, &running()).unwrap(),
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T09:30:00Z"),
            &running(),
        )
        .unwrap()
    }

    /// The selected directories relative to `root`, for readable asserts.
    fn rel_dirs(selection: &SourceSelection, root: &Path) -> Vec<String> {
        let prefix = format!("{}/", root.to_str().unwrap());
        selection
            .elements
            .iter()
            .map(|e| e.dir.strip_prefix(&prefix).unwrap_or(&e.dir).to_owned())
            .collect()
    }

    /// A latch nothing sets.
    fn running() -> CancelLatch {
        CancelLatch::never()
    }

    /// A capture's executed text for `[earliest, latest)` over `head`.
    fn executed(head: &str, earliest: &str, latest: &str) -> String {
        let text = format!("{head} earliest={earliest} latest={latest}");
        trawl_core::parser::parse(&text).expect("the executed text parses");
        text
    }

    #[test]
    fn a_window_selects_every_date_directory_as_query_does() {
        // The window's clauses never prune by directory name: a partition
        // is named by when compaction wrote it, not by the `_time` it holds.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-10-21/13");
        mkdir(tmp.path(), "prod/2026-10-23/09");
        mkdir(tmp.path(), "prod/notes");
        mkdir(tmp.path(), "lab/2026-10-22");
        let text = executed(
            "env=prod service=nginx",
            "2026-10-23T09:00:00Z",
            "2026-10-23T09:30:00Z",
        );

        let selection = select(base, &text, &running()).unwrap();

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            ["prod/2026-10-21", "prod/2026-10-23"]
        );
        assert!(selection.elements.iter().all(|e| e.recursive));
        assert_eq!(
            selection.source(),
            compute_source(base, &text, &running()).unwrap()
        );
        assert_eq!(
            selection.source(),
            format!(
                "['{base}/prod/2026-10-21/**/nginx.parquet', \
                 '{base}/prod/2026-10-23/**/nginx.parquet']"
            )
        );
    }

    #[test]
    fn an_ancient_window_selects_only_the_directories_on_disk() {
        // Year 1 to now is some 17 million hours; none is built.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let held = write_times(
            tmp.path(),
            "prod/2026-10-23/09/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:10:00"],
        );
        write_times(
            tmp.path(),
            "prod/2026-10-22/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-22 12:00:00"],
        );
        let text = executed(
            "service=nginx",
            "0001-01-01T00:00:00Z",
            "0001-01-01T01:00:00Z",
        );

        let begun = std::time::Instant::now();
        let selection = select(base, &text, &running()).unwrap();
        let ancient = selection_bytes(
            &selection,
            at("0001-01-01T00:00:00Z"),
            at("0001-01-01T01:00:00Z"),
            &running(),
        )
        .unwrap();
        let elapsed = begun.elapsed();

        assert_eq!(
            rel_dirs(&selection, tmp.path()),
            ["prod/2026-10-22", "prod/2026-10-23"]
        );
        assert_eq!(ancient, 0, "no stored event is that old");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "took {elapsed:?}"
        );
        let today = selection_bytes(
            &selection,
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T10:00:00Z"),
            &running(),
        )
        .unwrap();
        assert_eq!(today, held);
    }

    #[test]
    fn selection_bytes_counts_the_files_that_can_hold_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        // A day-level file two days old whose sender ran ahead: its
        // `_time` is inside the window.
        let skewed = write_times(
            tmp.path(),
            "prod/2026-10-21/nginx.parquet",
            "TIMESTAMP",
            &["2026-10-21 13:00:00", "2026-10-23 09:10:00"],
        );
        // This hour, every `_time` before the window.
        write_times(
            tmp.path(),
            "prod/2026-10-23/09/postgres.parquet",
            "TIMESTAMP",
            &["2026-10-23 08:00:00", "2026-10-23 08:59:59.999999"],
        );
        // The last microsecond inside the window.
        let last = write_times(
            tmp.path(),
            "prod/2026-10-23/09/redis.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:29:59.999999"],
        );
        // The window's end, which it excludes.
        write_times(
            tmp.path(),
            "prod/2026-10-23/10/api.parquet",
            "TIMESTAMP",
            &["2026-10-23 09:30:00"],
        );
        // UTC-adjusted ticks are read as instants too.
        let zoned = write_times(
            tmp.path(),
            "prod/2026-10-23/10/edge.parquet",
            "TIMESTAMPTZ",
            &["2026-10-23 09:00:00+00"],
        );
        // No footer to prove anything: counted whole.
        let unreadable = write_len(tmp.path(), "prod/2026-10-23/10/torn.parquet", 13);
        let (start, end) = (at("2026-10-23T09:00:00Z"), at("2026-10-23T09:30:00Z"));

        let every = select(
            base,
            &executed("*", "2026-10-23T09:00:00Z", "2026-10-23T09:30:00Z"),
            &running(),
        )
        .unwrap();
        assert_eq!(
            selection_bytes(&every, start, end, &running()).unwrap(),
            skewed + last + zoned + unreadable
        );
        let nginx = select(
            base,
            &executed(
                "service=nginx",
                "2026-10-23T09:00:00Z",
                "2026-10-23T09:30:00Z",
            ),
            &running(),
        )
        .unwrap();
        assert_eq!(
            selection_bytes(&nginx, start, end, &running()).unwrap(),
            skewed
        );
    }

    #[test]
    fn a_footer_without_timestamp_bounds_counts_whole() {
        let window = Window {
            start: i128::from(at("2026-10-23T09:00:00Z").timestamp_micros()),
            end: i128::from(at("2026-10-23T09:30:00Z").timestamp_micros()),
        };
        let early = StatVal::Timestamp {
            raw: at("2026-10-23T08:00:00Z").timestamp_micros(),
            unit: TimestampUnit::Micros,
            utc: false,
        };
        assert!(outside_window(Some(&early), Some(&early), window));
        assert!(!outside_window(None, Some(&early), window), "no min");
        assert!(!outside_window(Some(&early), None, window), "no max");
        let text = StatVal::Bytes(b"2026-10-23 08:00:00".to_vec());
        assert!(
            !outside_window(Some(&text), Some(&text), window),
            "a text `_time` orders as text, not as instants"
        );
        assert!(!outside_window(
            Some(&StatVal::Int(0)),
            Some(&StatVal::Int(1)),
            window
        ));
    }

    /// A row group whose `_time` chunk carries no statistics proves nothing
    /// about its rows, however far outside the window the other groups'
    /// statistics put theirs: `DuckDB` reads it, so the file counts whole.
    #[test]
    fn a_row_group_without_time_statistics_counts_its_file_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let path = write_two_groups(
            tmp.path(),
            "prod/2026-10-23/09/app.parquet",
            "2026-10-22 09:00:00",
            "2026-10-23 09:10:00",
        );
        trawl_engine::parquet_stats::fixture::drop_statistics(&path, 1, trawl_core::schema::TIME);
        let len = std::fs::metadata(&path).unwrap().len();

        let read: i64 = duckdb::Connection::open_in_memory()
            .unwrap()
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}') \
                     WHERE _time >= TIMESTAMP '2026-10-23 09:00:00' \
                     AND _time < TIMESTAMP '2026-10-23 09:30:00'",
                    path.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(read, 2048, "DuckDB reads the group without statistics");
        assert_eq!(charged(base, "app"), len);
    }

    /// A file is left out only when each of its row groups' statistics put
    /// that group outside the window, and counts whole otherwise.
    #[test]
    fn a_file_is_left_out_only_when_every_row_group_is_outside_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let hour = "prod/2026-10-23/09";
        // Both groups before the window.
        write_two_groups(
            tmp.path(),
            &format!("{hour}/before.parquet"),
            "2026-10-22 09:00:00",
            "2026-10-23 08:59:59.999999",
        );
        // One group before it and one after it: the file's earliest and
        // latest `_time` span the window, but no group holds any of it.
        write_two_groups(
            tmp.path(),
            &format!("{hour}/around.parquet"),
            "2026-10-23 08:00:00",
            "2026-10-23 09:30:00",
        );
        // One group inside it.
        let inside = write_two_groups(
            tmp.path(),
            &format!("{hour}/inside.parquet"),
            "2026-10-22 09:00:00",
            "2026-10-23 09:29:59.999999",
        );
        // No `_time` at all.
        let untimed = tmp.path().join(format!("{hour}/untimed.parquet"));
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT 'm' AS message FROM range(4096)) TO '{}' (FORMAT PARQUET)",
                untimed.display()
            ))
            .unwrap();

        assert_eq!(charged(base, "before"), 0);
        assert_eq!(charged(base, "around"), 0);
        assert_eq!(
            charged(base, "inside"),
            std::fs::metadata(inside).unwrap().len()
        );
        assert_eq!(
            charged(base, "untimed"),
            std::fs::metadata(untimed).unwrap().len()
        );
    }

    #[test]
    fn finer_timestamps_widen_outward_to_whole_microseconds() {
        let start = at("2026-10-23T09:00:00Z");
        let window = Window {
            start: i128::from(start.timestamp_micros()),
            end: i128::from(at("2026-10-23T09:30:00Z").timestamp_micros()),
        };
        let nanos = |offset: i64| StatVal::Timestamp {
            raw: start.timestamp_nanos_opt().unwrap() + offset,
            unit: TimestampUnit::Nanos,
            utc: true,
        };
        // 500 ns before the start could round onto it: not provably out.
        let close = nanos(-500);
        assert!(!outside_window(Some(&close), Some(&close), window));
        // A whole microsecond before it is out.
        let before = nanos(-1_000);
        assert!(outside_window(Some(&before), Some(&before), window));
        let millis = StatVal::Timestamp {
            raw: start.timestamp_millis() - 1,
            unit: TimestampUnit::Millis,
            utc: false,
        };
        assert!(outside_window(Some(&millis), Some(&millis), window));
        assert_eq!(micros(&nanos(-1), false), Some(window.start - 1));
        assert_eq!(micros(&nanos(-1), true), Some(window.start));
        assert_eq!(micros(&nanos(1), false), Some(window.start));
        assert_eq!(micros(&nanos(1), true), Some(window.start + 1));
    }

    #[test]
    fn an_empty_selection_reads_nothing_and_measures_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let (start, end) = (at("2026-10-23T09:00:00Z"), at("2026-10-23T09:15:00Z"));

        let cold_start = select(
            base,
            &executed("*", "2026-10-23T09:00:00Z", "2026-10-23T09:15:00Z"),
            &running(),
        )
        .unwrap();
        assert!(cold_start.elements.is_empty(), "{cold_start:?}");
        assert_eq!(
            cold_start.source(),
            format!("{base}/.no-such-env/*.parquet")
        );
        assert!(glob_matches(&cold_start.source()).is_empty());
        assert_eq!(
            selection_bytes(&cold_start, start, end, &running()).unwrap(),
            0
        );

        mkdir(tmp.path(), "prod/2026-10-23/09");
        let bad_env = select(base, "env=../evil", &running()).unwrap();
        assert!(bad_env.elements.is_empty());
        let bad_service = select(base, r#"service="x', '/etc""#, &running()).unwrap();
        assert_eq!(bad_service.services, Services::Any);
        assert!(!bad_service.source().contains("/etc"));
        // A service contradiction selects no directory, with or without
        // `last=`: the time walk finds no hour of 2026-10-23 today, and
        // must not fall back to every date directory.
        for contradiction in [
            "service=a service=b",
            "service=a service=b last=1h",
            "service=a,b service=c env=prod",
        ] {
            let nothing = select(base, contradiction, &running()).unwrap();
            assert!(nothing.elements.is_empty(), "{contradiction}: {nothing:?}");
            assert_eq!(
                nothing.source(),
                format!("{base}/.no-such-env/*.parquet"),
                "{contradiction}"
            );
            assert!(glob_matches(&nothing.source()).is_empty());
            assert_eq!(
                selection_bytes(&nothing, start, end, &running()).unwrap(),
                0
            );
        }
        let unparseable = select(base, "| | broken", &running()).unwrap();
        assert_eq!(
            unparseable.source(),
            format!("{base}/.no-such-env/*.parquet")
        );
    }

    #[test]
    #[cfg(unix)]
    fn selection_bytes_follows_no_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 5);
        let hour = tmp.path().join("prod/2026-10-23/09");
        std::os::unix::fs::symlink(
            tmp.path().join("nowhere.parquet"),
            hour.join("dangling.parquet"),
        )
        .unwrap();
        // A live link to a large file, and one to a directory of them,
        // outside the data layout.
        write_len(tmp.path(), "elsewhere/big.parquet", 4096);
        std::os::unix::fs::symlink(
            tmp.path().join("elsewhere/big.parquet"),
            hour.join("linked.parquet"),
        )
        .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), hour.join("linkdir")).unwrap();

        let selection = select(base, "*", &running()).unwrap();

        assert_eq!(rel_dirs(&selection, tmp.path()), ["prod/2026-10-23"]);
        assert_eq!(
            selection_bytes(
                &selection,
                at("2026-10-23T09:00:00Z"),
                at("2026-10-23T09:10:00Z"),
                &running()
            )
            .unwrap(),
            5,
            "only the regular file counts; no link is followed"
        );
    }

    #[test]
    fn selection_bytes_counts_a_vanished_directory_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-22/23/nginx.parquet", 9);
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 2);
        let selection = select(base, "*", &running()).unwrap();
        let (start, end) = (at("2026-10-23T00:10:00Z"), at("2026-10-23T09:10:00Z"));
        assert_eq!(
            selection_bytes(&selection, start, end, &running()).unwrap(),
            11
        );

        // Retention removed the oldest day after the walk listed it.
        std::fs::remove_dir_all(tmp.path().join("prod/2026-10-22")).unwrap();
        assert_eq!(
            selection_bytes(&selection, start, end, &running()).unwrap(),
            2
        );
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        write_len(tmp.path(), "prod/2026-10-23/09/nginx.parquet", 2);
        let selection = select(base, "*", &running()).unwrap();
        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));

        let walked = selection_bytes(
            &selection,
            at("2026-10-23T09:00:00Z"),
            at("2026-10-23T09:10:00Z"),
            &cancelled,
        );

        assert!(matches!(walked, Err(EngineError::Cancelled)), "{walked:?}");
    }

    /// The selection parses run on the worker under the work's latch: a
    /// job latched before it starts answers cancelled at the parse, builds
    /// no pattern, and never takes the unparseable fallback.
    #[test]
    fn a_latched_selection_parse_is_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-10-23/09");
        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));
        let builds = trawl_core::pattern::thread_builds();

        for dsl in ["message=/a+b/ last=1h", "env=prod service=nginx"] {
            let selected = select(base, dsl, &cancelled);
            assert!(
                matches!(selected, Err(EngineError::Cancelled)),
                "{dsl}: {selected:?}"
            );
            let source = compute_source(base, dsl, &cancelled);
            assert!(
                matches!(source, Err(EngineError::Cancelled)),
                "{dsl}: {source:?}"
            );
            let envs = first_selected_envs(base, dsl, 2, &cancelled);
            assert!(
                matches!(envs, Err(crate::env_dirs::FirstEnvNamesError::Cancelled)),
                "{dsl}: {envs:?}"
            );
        }
        assert_eq!(trawl_core::pattern::thread_builds(), builds);
    }

    /// A real parse error keeps each selection's fallback: nothing to read.
    #[test]
    fn an_unparseable_selection_still_reads_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        mkdir(tmp.path(), "prod/2026-10-23/09");

        let selection = select(base, "| | broken", &running()).unwrap();
        assert!(selection.elements.is_empty(), "{selection:?}");
        assert_eq!(selection.source(), format!("{base}/.no-such-env/*.parquet"));
        let envs = first_selected_envs(base, "| | broken", 2, &running()).unwrap();
        assert_eq!((envs.names, envs.total), (Vec::new(), 0));
    }

    /// The bounded env selection applies the `env=` pin rules the
    /// source's narrowing does, and walks the root only without a pin.
    #[test]
    fn first_selected_envs_follow_the_env_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        for env in ["lab", "prod", "stage"] {
            std::fs::create_dir(tmp.path().join(env)).unwrap();
        }
        let first =
            |dsl: &str, cap: usize| first_selected_envs(base, dsl, cap, &running()).unwrap();
        let names = |dsl: &str| (first(dsl, 2).names, first(dsl, 2).total);
        assert_eq!(names("*"), (vec!["lab".to_owned(), "prod".to_owned()], 3));
        assert_eq!(names("env=absent"), (vec!["absent".to_owned()], 1));
        assert_eq!(first("env=prod", 0).names, Vec::<String>::new());
        assert_eq!(first("env=prod", 0).total, 1);
        for nothing in ["env=wal", "env=Prod", "service=("] {
            assert_eq!(names(nothing), (Vec::new(), 0), "{nothing}");
        }
        for (dsl, envs) in [("*", 3), ("env=prod", 1), ("env=wal", 0)] {
            assert_eq!(
                first(dsl, 1024).names,
                narrowing(base, &trawl_core::parser::parse(dsl).unwrap()).envs
            );
            assert_eq!(first(dsl, 1024).total, envs);
        }

        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));
        let walked = first_selected_envs(base, "*", 2, &cancelled);
        assert!(
            matches!(walked, Err(crate::env_dirs::FirstEnvNamesError::Cancelled)),
            "{walked:?}"
        );
    }

    /// Drift guard: the files `selection_bytes` sizes must be the files
    /// `read_parquet` reaches through the same elements, on every
    /// non-symlink shape a date directory can hold.
    #[test]
    fn selection_bytes_matches_what_duckdb_glob_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_str().unwrap();
        let day = "prod/2026-10-23";
        for name in [
            "nginx.parquet",
            "09/nginx.parquet",
            "09/redis.parquet",
            "09/.parquet",
            "09/.hidden.parquet",
            "09/nginx.parquet.tmp",
            "09/upper.PARQUET",
            "09/nginx.parquetx",
            "09/deeper/nginx.parquet",
            ".stage/nginx.parquet",
            "dir.parquet/nginx.parquet",
            "notes.txt",
        ] {
            write_len(tmp.path(), &format!("{day}/{name}"), 1);
        }

        for dsl in [
            "*",
            "service=nginx",
            "service=absent",
            "service=nginx,redis",
            "service=redis OR service=nginx",
            "service=nginx,absent",
        ] {
            let selection = select(base, dsl, &running()).unwrap();
            assert_eq!(rel_dirs(&selection, tmp.path()), [day]);
            let duckdb = source_files(&selection.source()).len();
            assert_eq!(
                selection_bytes(
                    &selection,
                    at("2026-10-23T09:00:00Z"),
                    at("2026-10-23T09:10:00Z"),
                    &running()
                )
                .unwrap(),
                u64::try_from(duckdb).unwrap(),
                "one unreadable byte per file, so the counts must agree for {dsl:?}"
            );
        }
    }

    /// A row compacted at C, with a `_time` lead L ≤ [`MAX_TIME_LEAD`] over
    /// it, sits in C's hour directory, or in C's day directory once that day
    /// rolled up. A `last=N` query at Q ≥ C matches it when C + L ≥ Q − N,
    /// so the walk must select that directory for every C ≥ Q − N −
    /// `MAX_TIME_LEAD`, the worst lead. Each fixture keeps Q's own hour
    /// directory, so the empty-selection fallback cannot answer for the
    /// walk, and no file may be selected twice.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn last_window_reaches_every_row_within_the_lead() {
        use std::collections::HashSet;
        use trawl_core::ast::{TimeUnit, TrawlDuration};

        #[derive(Debug, Clone, Copy)]
        enum Layout {
            /// Every hour holds an hourly file.
            Hourly,
            /// Historical days hold only their rolled-up day file; today is
            /// hourly.
            Rolled,
            /// Every day, today included, holds a day file for one service
            /// and hourly files for another (a partial rollup).
            Mixed,
        }

        let hour = chrono::Duration::hours(1);
        let floor_hour = |t: DateTime<Utc>| {
            t.date_naive()
                .and_hms_opt(t.hour(), 0, 0)
                .unwrap()
                .and_utc()
        };
        // Q at 00:10 makes N = 1h cross midnight.
        for q in [at("2026-11-08T00:10:00Z"), at("2026-11-08T13:37:00Z")] {
            let today = q.date_naive();
            for unit in [TimeUnit::Minutes, TimeUnit::Hours, TimeUnit::Days] {
                let n = TrawlDuration { quantity: 1, unit };
                let window = chrono::Duration::seconds(n.to_seconds().cast_signed());
                let earliest = q - window - MAX_TIME_LEAD;

                // Every minute from the earliest matching C to Q, both ends
                // included, and each hour boundary in between ±1µs.
                let mut compactions = Vec::new();
                let mut c = earliest;
                while c <= q {
                    compactions.push(c);
                    c += chrono::Duration::minutes(1);
                }
                compactions.push(q);
                let mut boundary = floor_hour(earliest) + hour;
                while boundary <= q {
                    for micros in [-1, 0, 1] {
                        let c = boundary + chrono::Duration::microseconds(micros);
                        if (earliest..=q).contains(&c) {
                            compactions.push(c);
                        }
                    }
                    boundary += hour;
                }

                for layout in [Layout::Hourly, Layout::Rolled, Layout::Mixed] {
                    let tmp = tempfile::tempdir().unwrap();
                    let root = tmp.path();
                    // Directories from three hours before the walk's start,
                    // so the walk has older ones to leave out.
                    let mut h = floor_hour(q - window - chrono::Duration::hours(3));
                    while h <= q {
                        let day = h.format("%Y-%m-%d");
                        let hh = h.format("%H");
                        let historical = h.date_naive() != today;
                        match layout {
                            Layout::Rolled if historical => {
                                write_len(root, &format!("prod/{day}/svc.parquet"), 1);
                            }
                            Layout::Hourly | Layout::Rolled => {
                                write_len(root, &format!("prod/{day}/{hh}/svc.parquet"), 1);
                            }
                            Layout::Mixed => {
                                write_len(root, &format!("prod/{day}/svc.parquet"), 1);
                                write_len(root, &format!("prod/{day}/{hh}/other.parquet"), 1);
                            }
                        }
                        h += hour;
                    }
                    let base = format!("{}/prod", root.display());

                    for services in [Services::Any, only(&["svc"]), only(&["other", "svc"])] {
                        let label = format!("Q={q} N={n:?} {layout:?} {services:?}");
                        let selected = time_scoped_dirs(&base, n, &services, q);
                        assert!(selected.iter().all(|e| !e.recursive), "{label}");
                        let dirs: HashSet<&str> = selected.iter().map(|e| e.dir.as_str()).collect();
                        assert_eq!(dirs.len(), selected.len(), "{label}: a directory twice");
                        let recent = format!("{base}/{}", q.format("%Y-%m-%d/%H"));
                        assert!(dirs.contains(recent.as_str()), "{label}: {dirs:?}");

                        for c in &compactions {
                            let day_dir = format!("{base}/{}", c.format("%Y-%m-%d"));
                            let hour_dir = format!("{base}/{}", c.format("%Y-%m-%d/%H"));
                            let historical = c.date_naive() != today;
                            let holders = match layout {
                                Layout::Rolled if historical => vec![day_dir],
                                Layout::Hourly | Layout::Rolled => vec![hour_dir],
                                Layout::Mixed => vec![day_dir, hour_dir],
                            };
                            for dir in holders {
                                assert!(
                                    dirs.contains(dir.as_str()),
                                    "{label}: a row compacted at {c} is in {dir}, \
                                     which the walk left out: {dirs:?}"
                                );
                            }
                        }

                        let patterns = services.file_patterns();
                        let matched: Vec<String> = selected
                            .iter()
                            .flat_map(|e| patterns.iter().map(move |p| format!("{}/{p}", e.dir)))
                            .flat_map(|pattern| glob_matches(&pattern))
                            .collect();
                        let distinct: HashSet<&String> = matched.iter().collect();
                        assert!(!matched.is_empty(), "{label}");
                        assert_eq!(distinct.len(), matched.len(), "{label}: a file twice");
                    }
                }
            }
        }

        // The walk still prunes: at 13:37, `last=1m` starts at 12:36, so the
        // 11:00 directory is left out.
        let tmp = tempfile::tempdir().unwrap();
        for hh in ["11", "12", "13"] {
            write_len(tmp.path(), &format!("prod/2026-11-08/{hh}/svc.parquet"), 1);
        }
        let base = format!("{}/prod", tmp.path().display());
        let minute = TrawlDuration {
            quantity: 1,
            unit: TimeUnit::Minutes,
        };
        let dirs: Vec<String> =
            time_scoped_dirs(&base, minute, &Services::Any, at("2026-11-08T13:37:00Z"))
                .into_iter()
                .map(|e| e.dir)
                .collect();
        assert_eq!(
            dirs,
            [
                format!("{base}/2026-11-08/12"),
                format!("{base}/2026-11-08/13")
            ]
        );
    }
}
