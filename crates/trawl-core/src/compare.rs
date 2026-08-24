// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin-aware comparison rules for search-stage field filters (ADR-0011
//! slice A).
//!
//! One rule table, two consumers: the SQL emitter's field-filter arm
//! ([`crate::emitter`]) renders these forms to SQL, and the in-memory
//! [`crate::filter::CompiledFilter`] evaluates the same forms against JSON
//! events — batch/live parity is part of the contract, so the decision of
//! *how* a comparison binds under a catalog pin lives here and nowhere
//! else.
//!
//! The rules (the ADR-0011 slice A table):
//!
//! | pinned type | operation | form |
//! |---|---|---|
//! | unpinned | all | [`CompareForm::Native`] — literal-driven, unchanged |
//! | VARCHAR | `=` / `!=` / IN element, non-numeric literal | [`CompareForm::Text`] — compare as text (`'accepted'`) |
//! | VARCHAR | `=` / `!=` / IN element, numeric literal | [`CompareForm::TextOrNumeric`] — the text OR both sides' [`crate::conform::DECIMAL_COMPARISON_SPACE`] reading |
//! | VARCHAR | ordered + numeric literal | [`CompareForm::NumericOnText`] — the same DECIMAL reading, both sides; a text without one NULLs out |
//! | VARCHAR | ordered + non-numeric literal | [`CompareForm::Native`] — lexical, unchanged |
//! | VARCHAR | glob / regex | [`PatternForm::Native`] — unchanged |
//! | `TIMESTAMP` | glob / regex | [`PatternForm::Rfc3339Text`] — the canonical RFC 3339 UTC-microsecond text |
//! | `DOUBLE` | glob / regex | [`PatternForm::DoubleText`] — `DuckDB`'s DOUBLE rendering (`200.0`, `1e-07`) |
//! | `BIGINT` | glob / regex | [`PatternForm::BigIntText`] — the conformed integer's decimal text (`"0404"` globs as `404`) |
//! | `BOOLEAN` | glob / regex | [`PatternForm::BooleanText`] — `true`/`false` (`"TRUE"` globs as `true`) |
//! | typed pins | everything else | [`CompareForm::Conformed`] — the literal binds natively (SQL unchanged); the LIVE mirror conforms the value first |
//!
//! "Numeric literal" is decided by content (the same i64-then-f64 ladder as
//! [`coerce_filter_value`]): the AST discards quote provenance, so
//! `status>"400"` is indistinguishable from `status>400`. Documented, not
//! fixed here (fixing it is a parser change, out of slice-A scope).
//!
//! ## Why the equality rung carries a numeric reading
//!
//! A VARCHAR pin does NOT mean the stored text is the text the wire
//! carried. The column is conformed from whatever `read_json` inferred for
//! it — on the hot branch `json_extract_string(to_json(col), '$')`, in
//! compaction `TRY_CAST(col AS VARCHAR)` — so a wire `200` sitting in a
//! batch that also carries `200.5` infers DOUBLE and stores `"200.0"`, in
//! parquet, durably (probed in `trawl-core/tests/filter_parity.rs`). The
//! live matcher only ever sees the wire JSON, so an EXACT-text equality is
//! unmirrorable: `status=200` would be a batch miss and a live hit — the
//! silent divergence [`crate::filter`]'s invariant forbids.
//!
//! The numeric reading is the inference-independent half: every rendering
//! `DuckDB` can produce for a number (`200`, `200.0`, `2e2`) reads back to
//! that same value, and the wire value's own reading equals it. So an
//! equality against a numeric literal matches on EITHER — the exact text
//! (`"200"`, the enum-shaped case ADR-0011 is about) or the numeric reading
//! (`"200.0"`, the same value spelled by `read_json`'s inference). `!=` is
//! its complement (text differs AND the reading differs, `COALESCE`d TRUE
//! so `status!=200` still returns `"accepted"`), and both sides evaluate
//! the identical rule.
//!
//! Residual, pre-existing and out of slice-A scope: `read_json` also infers
//! TIMESTAMP for date-shaped STRINGS, and conformance then stores
//! `DuckDB`'s space-separated rendering — so an equality against a
//! timestamp-shaped literal can still differ from the wire text. That
//! predates pin-awareness (a non-numeric literal bound the same string
//! before this rule table existed) and is a write-path fidelity question,
//! not a comparison rule.
//!
//! ## One comparison space: `DECIMAL(38,6)`
//!
//! Both numeric rungs — the equality arm and the ordered one — read the
//! COLUMN and the LITERAL through the same
//! [`crate::conform::decimal_reading`], which is the space the conform
//! guard already compares in (ADR-0011 ruling #6). The literal binds as
//! its own text and is cast by the identical expression the column is, so
//! it never round-trips through `f64` and neither side can read one string
//! differently from the other.
//!
//! That is a correctness property, not tidiness. A DOUBLE comparison
//! collapses every integer above 2^53 onto the nearest representable
//! neighbour — in both engines alike, so parity testing could never see
//! it: `id=1737000000123456789` returned THREE distinct stored ids, and
//! `id!=9007199254740993` silently suppressed the genuinely different
//! `9007199254740992`. Snowflake ids and nanosecond epochs sit in
//! VARCHAR-pinned fields in exactly that shape. `DECIMAL(38,6)` is exact
//! for every `i64` and out to 10^32.
//!
//! Never `BIGINT`, for the reason that rules it out of the conform ladder
//! too: `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2 (pinned by
//! `trawl-engine/tests/duckdb_probe.rs`), so an integer space would make
//! `dur>1` and `dur>1.5` disagree about a stored `"1.5"`.
//!
//! Two costs, and both are paid by the two engines TOGETHER — they narrow
//! what matches, never what agrees:
//!
//! 1. **fractions quantize at 10^-6**, rounded half away from zero, so
//!    `"0.0000005"` reads as `0.000001` and two values a nanosecond apart
//!    compare equal. Sub-microsecond ordering is not something a
//!    VARCHAR-pinned field can express;
//! 2. **`nan`, `inf` and magnitudes at or above 10^32 have no reading at
//!    all**, which is a NULL — UNKNOWN, never a false match, and `NOT`
//!    cannot invert it into one. DOUBLE's ordering is total and used to
//!    sort a stored `"nan"` above every number, so `dur>1` returned it;
//!    now it matches nothing.
//!
//! The domain is `DuckDB`'s cast domain, not Rust's number parser:
//! [`decimal_micros`] is its live mirror — ASCII whitespace trimmed, `_`
//! separators between digits, `"0404"`, `"1e3"` and `".5"` read, radix
//! prefixes refused, and the cast's own laxness reproduced down to
//! `'- '` reading zero — executed side by side in
//! `trawl-engine/tests/duckdb_probe.rs`. Nothing COMPARES through
//! [`try_cast_double`] any more: it renders what a DOUBLE-pinned column
//! stores, for the pin's PATTERN text ([`PatternForm::DoubleText`]) and
//! for `tonumber()` in streaming eval, which reads the same cast domain
//! (`crate::eval`).

use crate::ast::{FilterOp, LiteralValue};
use crate::emitter::SqlValue;
use crate::emitter::coerce_filter_value;
use crate::schema::CanonicalType;

/// How one search-stage comparison binds its literal.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareForm {
    /// Today's literal-driven binding — the unpinned default.
    Native(SqlValue),
    /// A TYPED pin: the literal binds exactly as [`Self::Native`] would —
    /// the SQL is byte-identical, because the column on disk already IS
    /// the pinned type — but the live matcher must read the value's
    /// CONFORMED value first and compare THAT.
    ///
    /// Batch compares what conformance stored; the wire value is only the
    /// same thing when it already reads as its pin. Every value the
    /// round-trip guard nulls out answered differently otherwise: a wire
    /// `1.5` under a BIGINT pin is NULL in both batch lanes, so
    /// `duration>1` is UNKNOWN there and was TRUE live, and `"TRUE"`
    /// under a BOOLEAN pin made `NOT flag=true` fire on a stream while
    /// `/query` returned nothing.
    Conformed {
        pin: CanonicalType,
        literal: SqlValue,
    },
    /// Compare as text: the literal binds as a string (`'accepted'`).
    Text(String),
    /// The VARCHAR-pinned equality form for a NUMERIC literal: the value
    /// matches when its stored TEXT is the literal **or** the two
    /// [`crate::conform::DECIMAL_COMPARISON_SPACE`] readings agree.
    ///
    /// The text half is ADR-0011's rule; the numeric half is what makes it
    /// mirrorable, because the stored text of a number is `read_json`'s
    /// inference rendered, not the wire spelling (see the module doc).
    ///
    /// One string, carried once: the numeric arm casts the SAME literal
    /// text the text arm compares, so the two arms cannot disagree about
    /// what the literal is.
    TextOrNumeric(String),
    /// Ordered numeric comparison over a VARCHAR column: both sides read
    /// through [`crate::conform::decimal_reading`], so a stored text
    /// outside that domain is NULL and doesn't match.
    ///
    /// A LITERAL outside it (`nan`, `inf`, `1e40`) needs no special case:
    /// its own reading is NULL too, and the comparison is UNKNOWN for
    /// every row — the same answer on both engines, and one `NOT` cannot
    /// invert into a match.
    NumericOnText(String),
    /// The SEVERITY pin's equality-class form for a BAND token
    /// (ADR-0013): `_severity=error` is the whole ERROR band, 17-20, and
    /// `!=` its complement — the semantics the deleted
    /// `level=` alias carried, now riding an unforgeable name through the
    /// pin rule table instead of a name special case.
    SeverityBand { lo: u8, hi: u8 },
    /// The SEVERITY pin's exact form: an integer literal, an `OTel` exact
    /// short name (`error2` → 18), or a band token under an ORDERED
    /// operator (`_severity>=warn` → `>= 13`).
    SeverityExact(i64),
}

/// Why a literal cannot bind under a field's pin.
///
/// One variant, deliberately: the SEVERITY pin is the only one with a
/// closed vocabulary, and every other rule table entry is total over
/// literals by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompareError {
    /// A `SEVERITY`-pinned comparison against a literal that names no
    /// point on the `OTel` ladder.
    UnknownSeverityToken { token: String },
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSeverityToken { token } => write!(
                f,
                "unknown severity value '{token}' — a severity field takes a \
                 band token ({}), an exact OTel short name (error2, warn3), \
                 or a number on the 1-24 ladder",
                crate::severity::CANONICAL_TOKENS.join(", ")
            ),
        }
    }
}

impl std::error::Error for CompareError {}

/// How one glob/regex pattern binds its column.
///
/// A pattern needs ONE text per pinned type, or batch and live disagree:
/// the SQL side matches the CONFORMED value's rendering (the column is
/// conformed to its pin on the hot branch by [`crate::conform`] and
/// already typed on disk) while the live matcher only ever sees the wire
/// JSON. Stringifying the wire value is that same text only when the value
/// already reads as its pin — a JSON int under a BIGINT pin, a JSON bool
/// under a BOOLEAN pin. For every other shape the two diverge, so each
/// typed pin renders the value's own conformed reading:
///
/// - `"0404"` conforms to the BIGINT `404`, so `status=0*` matches the
///   wire text and misses the column, while `"accepted"` and `"1.5"`
///   conform to NULL where the wire text matches `a*` and `1*`.
///   [`Self::BigIntText`] renders [`conformed_bigint`] instead.
/// - `"true"` conforms to the BOOLEAN `true` and `"TRUE"` conforms to
///   NULL — the cast reads both, the round-trip guard keeps only the
///   spelling `DuckDB` writes back. [`Self::BooleanText`] renders
///   [`conformed_boolean`].
/// - `DuckDB` renders a TIMESTAMP space-separated and zoneless
///   (`2026-01-15 09:00:00`) where the event carries RFC 3339
///   (`2026-01-15T09:00:00.000000Z`), so `_time=/T09:/` would match live
///   and miss in batch. [`Self::Rfc3339Text`] pins both sides to the wire
///   form (see [`TIMESTAMP_PATTERN_SQL_FORMAT`] and
///   [`canonical_timestamp_text`]).
/// - `DuckDB` renders a DOUBLE with a mandatory fraction and a signed,
///   two-digit exponent (`200.0`, `0.0`, `1e-07`,
///   `1.2345678901234568e+17`) where the same wire value stringifies as
///   `200` / `0` / `1e-7` / `123456789012345680` — the conformed column
///   is DOUBLE whatever the wire number looked like, so `dur=/^200$/`
///   would match live and miss in batch. [`Self::DoubleText`] renders the
///   value's DOUBLE reading on the live side too (see
///   [`canonical_double_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternForm {
    /// Match against the column directly (unpinned or VARCHAR pin).
    Native,
    /// The BIGINT pin's canonical text: the conformed integer in decimal —
    /// `CAST(col AS VARCHAR)` on the SQL side (the column already IS
    /// BIGINT), the value's own [`conformed_bigint`] reading rendered
    /// through `i64`'s `Display` on the live side.
    BigIntText,
    /// The BOOLEAN pin's canonical text — `true`/`false`, lowercase:
    /// `CAST(col AS VARCHAR)` on the SQL side, the value's own
    /// [`conformed_boolean`] reading rendered through `bool`'s `Display`
    /// on the live side.
    BooleanText,
    /// The TIMESTAMP pin's canonical text: RFC 3339, UTC, always six
    /// fractional digits — `strftime` on the SQL side,
    /// [`canonical_timestamp_text`] on the live side.
    Rfc3339Text,
    /// The DOUBLE pin's canonical text: `DuckDB`'s own DOUBLE rendering —
    /// `CAST(col AS VARCHAR)` on the SQL side (the column already IS
    /// DOUBLE), [`canonical_double_text`] over the value's DOUBLE reading
    /// on the live side.
    DoubleText,
    /// The SEVERITY pin's canonical text: the `OTel` short name of the
    /// stored number (`17` → `error`, `18` → `error2`) —
    /// `crate::conform::severity_token_text_sql` on the SQL side,
    /// [`crate::severity::otel_name`] over the value's
    /// [`conformed_severity`] reading on the live side. Injective, so
    /// `_severity=warn*` matches exactly the WARN band (13-16).
    SeverityText,
}

/// The `DuckDB` `strftime` format producing a TIMESTAMP pin's canonical
/// pattern text — RFC 3339 with a fixed six-digit fraction and a literal
/// `Z`, the same shape ingest canonicalizes `_time`/`_ingested` into.
///
/// `%f` is `DuckDB`'s zero-padded microsecond field; the pairing of this
/// format with [`canonical_timestamp_text`] is executed in
/// `trawl-engine/tests/duckdb_probe.rs`, not assumed.
pub const TIMESTAMP_PATTERN_SQL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S.%fZ";

/// The zone NAMES that denote UTC for all time, exactly as
/// `pg_timezone_names()` spells them (matched case-insensitively, which is
/// how `DuckDB` matches them).
///
/// Deliberately the *definitionally* fixed set, not "every name whose
/// offset is zero today": `Africa/Abidjan` is +00:00 now and +00:16:08 in
/// 1800, so resolving it needs a zone HISTORY, not a table. See
/// [`canonical_timestamp_text`] for the residual that leaves.
const UTC_ZONE_NAMES: [&str; 18] = [
    "Etc/GMT",
    "Etc/GMT+0",
    "Etc/GMT-0",
    "Etc/GMT0",
    "Etc/Greenwich",
    "Etc/UCT",
    "Etc/UTC",
    "Etc/Universal",
    "Etc/Zulu",
    "GMT",
    "GMT+0",
    "GMT-0",
    "GMT0",
    "Greenwich",
    "UCT",
    "UTC",
    "Universal",
    "Zulu",
];

/// The instant a `DuckDB` TIMESTAMP holds — what a TIMESTAMP-pinned column
/// conformed a stored text to ([`conformed_timestamp`]), and what a query
/// literal reads as when it is compared against one ([`literal_timestamp`]).
///
/// `DuckDB`'s TIMESTAMP carries two values no calendar date can express,
/// and `strftime` renders them as the literal words `infinity` and
/// `-infinity` — texts a glob can match, so the mirror has to produce them
/// too.
///
/// The variant ORDER is the type's ordering: `-infinity` sorts below every
/// date and `infinity` above, which is what `ts>'2026-01-01'` answers for
/// a column holding them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Instant {
    NegInfinity,
    At(chrono::NaiveDateTime),
    Infinity,
}

impl Instant {
    /// This instant as `CAST(ts AS VARCHAR)` renders it — the SCALAR
    /// text, which is what `tostring()`, a `concat()` argument and a
    /// projected cell show.
    ///
    /// Deliberately NOT [`Self::pattern_text`]: that one is the TIMESTAMP
    /// pin's RFC 3339 `Z` form, the target a glob matches against a
    /// stored column. The two differ for every finite instant (`T` and a
    /// `Z` against a space and none), so a caller that reached for the
    /// wrong one would print a string `DuckDB` never prints. Both
    /// renderings are probed —
    /// `a_timestamp_casts_to_the_text_duckdb_prints` for this one — and
    /// the finite arm delegates to the renderer that already byte-matches
    /// the engine, exactly as [`canonical_double_text`] delegates.
    #[must_use]
    pub fn cast_text(self) -> String {
        match self {
            Self::Infinity => "infinity".to_owned(),
            Self::NegInfinity => "-infinity".to_owned(),
            Self::At(at) => crate::eval::timestamp_to_duckdb_text(&at),
        }
    }

    /// This instant in the TIMESTAMP pin's canonical pattern text — the
    /// in-memory mirror of `strftime(col, TIMESTAMP_PATTERN_SQL_FORMAT)`.
    #[must_use]
    pub fn pattern_text(self) -> String {
        match self {
            Self::Infinity => "infinity".to_owned(),
            Self::NegInfinity => "-infinity".to_owned(),
            Self::At(at) => render_pattern_text(at),
        }
    }
}

/// Whether a zone in the text is APPLIED or ignored — the one thing the
/// conform's cast and a bound literal's cast do differently.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ZoneRule {
    /// `TRY_CAST(TRY_CAST(text AS TIMESTAMPTZ) AS TIMESTAMP)`, the conform
    /// (ADR-0011 ruling #1): an offset shifts the instant and a zoneless
    /// text is read in the session zone, which is pinned to UTC.
    Apply,
    /// `TRY_CAST(text AS TIMESTAMP)`, what `DuckDB` casts a bound STRING
    /// parameter with when it is compared against a TIMESTAMP column: the
    /// WALL-CLOCK parse, which reads the same syntax and then throws the
    /// offset away (`'…T09:00:00+05:30'` is 09:00, probed).
    Ignore,
}

/// The instant a TIMESTAMP-pinned column CONFORMS a stored text to — the
/// live mirror of [`crate::conform::guarded_cast`]'s TIMESTAMP rung.
#[must_use]
pub fn conformed_timestamp(text: &str) -> Option<Instant> {
    parse_instant(text, ZoneRule::Apply)
}

/// The instant `DuckDB` reads a query LITERAL as when it is compared
/// against a TIMESTAMP column.
///
/// Deliberately not [`conformed_timestamp`]: the emitter binds the literal
/// as a string and leaves the cast to `DuckDB`, whose implicit
/// VARCHAR → TIMESTAMP conversion is the wall-clock parse. The two agree
/// on every zoneless text — which is every literal that does not spell an
/// offset out — and the SQL side is what decides, so the mirror follows it
/// rather than the reading it would prefer.
///
/// It also resolves a NARROWER set of zone names: exactly `UTC`, where the
/// conform's ICU-backed cast takes every name in `pg_timezone_names()`.
/// Probed, both ways.
#[must_use]
pub fn literal_timestamp(text: &str) -> Option<Instant> {
    parse_instant(text, ZoneRule::Ignore)
}

/// Render a live event's value in the TIMESTAMP pin's canonical pattern
/// text — the in-memory mirror of
/// `strftime(col, TIMESTAMP_PATTERN_SQL_FORMAT)` over the conformed
/// column.
///
/// `None` means the value has no timestamp reading, which is what the
/// batch side stores: [`crate::conform::guarded_cast`]'s TIMESTAMP rung is
/// NULL for it, and `strftime` of NULL is NULL — UNKNOWN, never a false
/// pattern miss that `NOT` could invert.
///
/// # The reading is `TRY_CAST(text AS TIMESTAMPTZ)` under a UTC session
///
/// Not the plain `TRY_CAST(… AS TIMESTAMP)` wall-clock parse: an offset in
/// the text is APPLIED (ADR-0011 ruling #1), which is also the only way
/// the mirror can agree with a corpus `read_json` typed for itself. Every
/// rule below is established by execution in
/// `trawl-engine/tests/duckdb_probe.rs`, never from a specification —
/// and that matrix is the CONTRACT: an input where this function and the
/// engine disagree is a bug HERE, to be added to the matrix and fixed,
/// not a tolerance to be absorbed at the call site. Four of the rules
/// below (`epoch`, the ` UTC` suffix, hour-24 rollover, and the
/// seconds-less form that used to fire live while batch stored NULL) were
/// missing precisely because an earlier version reasoned them out instead
/// of running them:
///
/// - **keywords**: `epoch` and `-epoch` → 1970-01-01, `infinity`/`inf`
///   and `-infinity`/`-inf` → the infinite instants, all
///   case-insensitive. The leading `-` is consumed before the keyword is
///   read, which is why `-epoch` IS epoch and `+infinity` is nothing.
///   Trailing whitespace is allowed after the FULL spellings only:
///   `'epoch '` and `'-infinity\t'` parse, `'inf '` does not;
/// - **date**: `[-]Y+{sep}M{1,2}{sep}D{1,2}` with `{sep}` either `-` or
///   `/` *and the same both times*. The year is any run of digits
///   (`02026` is 2026), month and day are one or two — a third digit is a
///   different shape, refused, never truncated. A date alone is midnight,
///   and then the text must END: `2026-01-15 ` is not a date;
/// - **separator**: one `T` or one ASCII space (then any run of further
///   whitespace), and a TIME is then mandatory;
/// - **time**: `H+:M{1,2}[:S{1,2}[.f*]]`. Hour ≤ 24, minute and second
///   ≤ 59; hour 24 is midnight of the NEXT day and only when everything
///   below it is zero. The fraction TRUNCATES to microseconds
///   (`.9999999` → `.999999`) and may be empty (`09:00:00.` parses);
/// - **zone, only when SECONDS are present**: `Z` (uppercase),
///   `±HH`, `±HHMM`, `±HH:MM`, `±HH:MM:SS` — each component EXACTLY two
///   digits, unvalidated in range (`+99:99` is a real offset) — or one
///   space and a zone name. A SPACE is the only whitespace that can close
///   a zoneless time, because it is where a zone name would start:
///   `09:00:00 ` and `09:00:00 \t` parse where `09:00:00\t` is NULL,
///   while past a zone any trailing whitespace goes (`09:00:00Z\t`).
///   Without seconds the text must end where the time does: `09:00Z`,
///   `09:00 UTC` and even `09:00 ` are all refused, the false-positive
///   direction the wall-clock mirror used to get wrong;
/// - **whitespace**: ASCII only (` \t\n\r\x0b\x0c`, never `\u{a0}`),
///   skipped before the value and after a complete time.
///
/// # Two residuals, both in the safe direction
///
/// Both make the mirror answer `None` where `DuckDB` has a reading, so a
/// live tail UNDER-matches; neither can invent a match the batch query
/// does not have. They are pinned as expected divergences in the probe
/// matrix rather than left to be rediscovered:
///
/// 1. **zone names other than the [`UTC_ZONE_NAMES`]**. `DuckDB` links ICU
///    and resolves all 638 of `pg_timezone_names()`, with DST rules and
///    pre-1970 local-mean-time offsets. Mirroring that means shipping a
///    zone database inside `trawl-core` — which compiles to wasm for the
///    SPA — and two tzdata versions that drift apart would be a *silent*
///    divergence in place of this loud one;
/// 2. **years outside chrono's calendar** (before `-262143-01-01` or
///    after `+262142-12-31`, its `NaiveDate` bounds in this build), where
///    `DuckDB`'s microsecond range reaches ±~290 000.
#[must_use]
pub fn canonical_timestamp_text(value: &str) -> Option<String> {
    conformed_timestamp(value).map(Instant::pattern_text)
}

/// `strftime`'s rendering of a finite timestamp under
/// [`TIMESTAMP_PATTERN_SQL_FORMAT`].
///
/// Built field by field rather than through chrono's own `%Y`, which
/// disagrees with `DuckDB`'s outside the four-digit years: chrono writes
/// `+10000` and `-0001` where `DuckDB` writes `10000` and `-1`.
fn render_pattern_text(at: chrono::NaiveDateTime) -> String {
    use chrono::{Datelike as _, Timelike as _};

    let year = at.year();
    let year = if year < 0 {
        year.to_string()
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z",
        month = at.month(),
        day = at.day(),
        hour = at.hour(),
        minute = at.minute(),
        second = at.second(),
        micros = at.nanosecond() / 1_000,
    )
}

/// The instant `DuckDB` reads out of this text, or `None`.
fn parse_instant(value: &str, zone: ZoneRule) -> Option<Instant> {
    let text = &value[leading_space(value.as_bytes(), 0)..];
    keyword_instant(text).or_else(|| parse_datetime(text, zone).map(Instant::At))
}

/// The keyword instants, case-insensitive.
///
/// The leading `-` is consumed before the keyword is matched, so `-epoch`
/// is epoch (only `infinity` reads the sign as a sign), and trailing
/// whitespace is tolerated after the FULL spellings ONLY: `'epoch '` and
/// `'-infinity\t'` parse where `'inf '` and `'-inf '` are NULL. Both rules
/// are the cast's, established by execution — an `inf` abbreviation with a
/// trailing space read as `infinity` here while the corpus held NULL,
/// which is the over-match direction this mirror forbids itself.
fn keyword_instant(text: &str) -> Option<Instant> {
    let token = text.trim_end_matches(is_c_space);
    if token.eq_ignore_ascii_case("epoch") || token.eq_ignore_ascii_case("-epoch") {
        return chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(Instant::At);
    }
    if token.eq_ignore_ascii_case("infinity") {
        return Some(Instant::Infinity);
    }
    if token.eq_ignore_ascii_case("-infinity") {
        return Some(Instant::NegInfinity);
    }
    if text.eq_ignore_ascii_case("inf") {
        return Some(Instant::Infinity);
    }
    if text.eq_ignore_ascii_case("-inf") {
        return Some(Instant::NegInfinity);
    }
    None
}

/// One parsed time of day. `has_seconds` is load-bearing, not bookkeeping:
/// a zone suffix is legal only after a `:SS` field.
struct TimeOfDay {
    hour: u32,
    minute: u32,
    second: u32,
    micros: u32,
    has_seconds: bool,
}

/// The calendar form: a date, optionally a time, optionally a zone.
fn parse_datetime(text: &str, zone: ZoneRule) -> Option<chrono::NaiveDateTime> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let date = parse_date(bytes, &mut pos)?;
    if pos == bytes.len() {
        return date.and_hms_opt(0, 0, 0);
    }
    // Whatever follows a date is a date/time SEPARATOR, which commits the
    // text to carrying a time: `2026-01-15 ` is not a date to DuckDB, so
    // trailing whitespace cannot be trimmed away before this point.
    if bytes[pos] != b'T' && !is_c_space(bytes[pos] as char) {
        return None;
    }
    pos += 1;
    pos = leading_space(bytes, pos);

    let time = parse_time(bytes, &mut pos)?;
    let offset = if time.has_seconds {
        parse_zone_offset(text, &mut pos, zone)?
    } else if pos == bytes.len() {
        0
    } else {
        // A seconds-less time takes no zone and no trailing anything.
        return None;
    };
    if leading_space(bytes, pos) != bytes.len() {
        return None;
    }

    let wall = if time.hour == 24 {
        // Hour 24 is the next midnight, and only when it IS midnight:
        // `24:00:01` has no reading at all.
        if time.minute != 0 || time.second != 0 || time.micros != 0 {
            return None;
        }
        date.succ_opt()?.and_hms_opt(0, 0, 0)?
    } else {
        date.and_hms_micro_opt(time.hour, time.minute, time.second, time.micros)?
    };
    if zone == ZoneRule::Ignore {
        // The wall-clock cast reads the offset's SYNTAX (a malformed one
        // is still NULL) and then discards its value.
        return Some(wall);
    }
    // The offset is seconds EAST of UTC, so the instant is the wall clock
    // minus it.
    wall.checked_sub_signed(chrono::TimeDelta::try_seconds(offset)?)
}

fn parse_date(bytes: &[u8], pos: &mut usize) -> Option<chrono::NaiveDate> {
    let negative = bytes.get(*pos) == Some(&b'-');
    if negative {
        *pos += 1;
    }
    let year = take_digits(bytes, pos, 1, usize::MAX)?;
    // The two date separators must be the SAME character: `2026-01/15` is
    // not a date.
    let separator = *bytes.get(*pos)?;
    if separator != b'-' && separator != b'/' {
        return None;
    }
    *pos += 1;
    let month = take_digits(bytes, pos, 1, 2)?;
    if *bytes.get(*pos)? != separator {
        return None;
    }
    *pos += 1;
    let day = take_digits(bytes, pos, 1, 2)?;

    let year = i32::try_from(if negative { -year } else { year }).ok()?;
    chrono::NaiveDate::from_ymd_opt(year, u32::try_from(month).ok()?, u32::try_from(day).ok()?)
}

fn parse_time(bytes: &[u8], pos: &mut usize) -> Option<TimeOfDay> {
    // The hour is a free run of digits where minute and second are one or
    // two: `T009:00:00` is 09:00:00, `T09:000:00` is nothing.
    let hour = take_digits(bytes, pos, 1, usize::MAX)?;
    if *bytes.get(*pos)? != b':' {
        return None;
    }
    *pos += 1;
    let minute = take_digits(bytes, pos, 1, 2)?;

    let mut second = 0;
    let mut micros = 0;
    let has_seconds = bytes.get(*pos) == Some(&b':');
    if has_seconds {
        *pos += 1;
        second = take_digits(bytes, pos, 1, 2)?;
        if bytes.get(*pos) == Some(&b'.') {
            *pos += 1;
            let start = *pos;
            while bytes.get(*pos).is_some_and(u8::is_ascii_digit) {
                *pos += 1;
            }
            micros = fraction_micros(&bytes[start..*pos]);
        }
    }
    if hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    Some(TimeOfDay {
        hour: u32::try_from(hour).ok()?,
        minute: u32::try_from(minute).ok()?,
        second: u32::try_from(second).ok()?,
        micros,
        has_seconds,
    })
}

/// The zone suffix, as SECONDS EAST of UTC. Absent is `Some(0)`; only a
/// malformed one is `None`.
fn parse_zone_offset(text: &str, pos: &mut usize, zone: ZoneRule) -> Option<i64> {
    let bytes = text.as_bytes();
    match bytes.get(*pos) {
        None => Some(0),
        Some(b'Z') => {
            *pos += 1;
            Some(0)
        }
        Some(&sign @ (b'+' | b'-')) => {
            *pos += 1;
            let hours = take_fixed_digits(bytes, pos, 2)?;
            let mut minutes = 0;
            let mut seconds = 0;
            if bytes.get(*pos) == Some(&b':') {
                *pos += 1;
                minutes = take_fixed_digits(bytes, pos, 2)?;
                if bytes.get(*pos) == Some(&b':') {
                    *pos += 1;
                    seconds = take_fixed_digits(bytes, pos, 2)?;
                }
            } else if bytes.get(*pos).is_some_and(u8::is_ascii_digit) {
                // The basic form: `+0530`, four digits and no colon.
                minutes = take_fixed_digits(bytes, pos, 2)?;
            }
            // Ranges are DuckDB's, which is to say none: `+99:99` is a
            // real (if absurd) offset, so no bound is imposed here either.
            let magnitude = hours * 3600 + minutes * 60 + seconds;
            Some(if sign == b'-' { -magnitude } else { magnitude })
        }
        // A SPACE is where a zone NAME would start, and it is therefore
        // the only whitespace that can close a zoneless time: `09:00:00 `
        // and `09:00:00 \t` parse where `09:00:00\t` is NULL. Past a zone
        // (`Z`, `+05:30`, a name) any trailing whitespace goes, which the
        // arms above already allow.
        Some(b' ') => {
            // The name runs to the end of the text, and only one space
            // introduces it (`  UTC` is refused by the recursion into
            // this same arm).
            //
            // The two casts resolve different sets: the conform's
            // `TIMESTAMPTZ` rung reaches ICU and takes every name, of
            // which the mirror keeps the definitionally-UTC ones
            // ([`UTC_ZONE_NAMES`]); the wall-clock cast a bound literal
            // gets takes exactly one, `UTC`, and NULLs even `GMT` and
            // `Etc/UTC` (probed).
            let name = text[*pos + 1..].trim_end_matches(is_c_space);
            let accepted: &[&str] = match zone {
                ZoneRule::Apply => &UTC_ZONE_NAMES,
                ZoneRule::Ignore => &["UTC"],
            };
            if accepted.iter().any(|zone| zone.eq_ignore_ascii_case(name)) {
                *pos = bytes.len();
                return Some(0);
            }
            // Otherwise this is trailing whitespace (or a zone the mirror
            // cannot resolve, which the caller's end-of-text check
            // refuses).
            Some(0)
        }
        Some(_) => None,
    }
}

/// The index of the first byte at or after `from` that is not ASCII
/// whitespace.
fn leading_space(bytes: &[u8], from: usize) -> usize {
    let mut pos = from;
    while bytes.get(pos).is_some_and(|&c| is_c_space(c as char)) {
        pos += 1;
    }
    pos
}

/// Read a run of ASCII digits of length `min..=max`.
///
/// The run must END inside the bound: a longer one is a different shape
/// and is refused, never silently truncated (`2026-011-15` is not a date).
fn take_digits(bytes: &[u8], pos: &mut usize, min: usize, max: usize) -> Option<i64> {
    let start = *pos;
    let mut value: i64 = 0;
    while bytes.get(*pos).is_some_and(u8::is_ascii_digit) {
        if *pos - start == max {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(i64::from(bytes[*pos] - b'0'))?;
        *pos += 1;
    }
    (*pos - start >= min).then_some(value)
}

/// Read EXACTLY `count` digits without caring what follows — the offset
/// fields, where `+0530` packs hours and minutes into one run.
fn take_fixed_digits(bytes: &[u8], pos: &mut usize, count: usize) -> Option<i64> {
    let mut value: i64 = 0;
    for _ in 0..count {
        let digit = bytes.get(*pos).filter(|c| c.is_ascii_digit())?;
        value = value * 10 + i64::from(digit - b'0');
        *pos += 1;
    }
    Some(value)
}

/// A fractional-seconds digit run as microseconds, TRUNCATED (never
/// rounded) past the sixth digit and zero-filled short of it.
fn fraction_micros(digits: &[u8]) -> u32 {
    digits
        .iter()
        .chain(std::iter::repeat(&b'0'))
        .take(6)
        .fold(0, |acc, digit| acc * 10 + u32::from(digit - b'0'))
}

/// Resolve the binding for a comparison (`=`, `!=`, ordered) or an IN-list
/// element under the field's pin.
///
/// `op` [`FilterOp::Glob`]/[`FilterOp::Regex`] never reach here (patterns
/// resolve through [`pattern_form`]); they fall through to the native
/// branch defensively.
///
/// # Errors
///
/// [`CompareError::UnknownSeverityToken`] when a `SEVERITY`-pinned field
/// is compared against a literal outside the ladder vocabulary.
pub fn compare_form(
    pin: Option<CanonicalType>,
    op: FilterOp,
    literal: &str,
) -> Result<CompareForm, CompareError> {
    form_over(pin, op, literal, || coerce_filter_value(literal))
}

/// Resolve the binding for a comparison whose literal arrives already
/// PARSED — the pipeline lane's door (`| where` / `| let`, ADR-0011 slice
/// A′), sharing one rule table with [`compare_form`].
///
/// `None` for [`LiteralValue::Null`]: `== null` is not a comparison this
/// table binds, and the caller takes the generic path.
///
/// Quote provenance is discarded, deliberately: the VARCHAR-pin rules read
/// the literal's CONTENT, and a string literal binds through the same
/// content coercion the search stage applies — so `where status == "400"`
/// is `status == 400`, exactly as `status="400"` is `status=400` in the
/// search stage (the two are one AST there). Honouring the AST's quoting
/// would make adjacent stages of one query disagree about the same
/// comparison.
///
/// That promise is why the literal's text comes from the AST and not from
/// re-rendering a parsed number: a float literal carries its source token
/// ([`crate::ast::FloatLiteral::text`]), so `where id > 9007199254740993.0` binds the
/// digits the user wrote instead of the `f64`-rounded `…992` — which would
/// match the ADJACENT identifier, and disagree with the quoted spelling of
/// the same literal. `i64` and the wire text are already exact.
///
/// A non-string literal binds unchanged under a typed pin (`flag == true`
/// stays `Bool(true)`, byte-identical SQL); only the live mirror conforms.
///
/// # Errors
///
/// [`CompareError::UnknownSeverityToken`], exactly as [`compare_form`].
pub fn compare_form_bound(
    pin: Option<CanonicalType>,
    op: FilterOp,
    literal: &LiteralValue,
) -> Result<Option<CompareForm>, CompareError> {
    // (the text every VARCHAR-pin rule reads — the same text the search
    // stage would have carried for this literal — and the value the
    // native/typed branches bind)
    let (text, native): (String, SqlValue) = match literal {
        LiteralValue::String(s) => (s.clone(), coerce_filter_value(s)),
        LiteralValue::Int(i) => (i.to_string(), SqlValue::Int(*i)),
        LiteralValue::Float(f) => (f.text().to_owned(), SqlValue::Float(f.value())),
        LiteralValue::Bool(b) => (b.to_string(), SqlValue::Bool(*b)),
        LiteralValue::Null => return Ok(None),
    };
    form_over(pin, op, &text, || native).map(Some)
}

/// The one rule table behind both doors: `text` is the literal's content,
/// `native` produces the value the native/typed branches bind.
fn form_over(
    pin: Option<CanonicalType>,
    op: FilterOp,
    text: &str,
    native: impl FnOnce() -> SqlValue,
) -> Result<CompareForm, CompareError> {
    match pin {
        None => return Ok(CompareForm::Native(native())),
        // The one pin with a closed literal vocabulary (ADR-0013).
        Some(CanonicalType::Severity) => return severity_form(op, text, native),
        // A typed pin binds the same literal and emits the same SQL; only
        // the live mirror changes, because only it has to conform first.
        Some(typed) if typed != CanonicalType::Varchar => {
            return Ok(CompareForm::Conformed {
                pin: typed,
                literal: native(),
            });
        }
        Some(_) => {}
    }
    Ok(match op {
        FilterOp::Eq | FilterOp::Ne if is_numeric_literal(text) => {
            CompareForm::TextOrNumeric(text.to_owned())
        }
        FilterOp::Eq | FilterOp::Ne => CompareForm::Text(text.to_owned()),
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte if is_numeric_literal(text) => {
            CompareForm::NumericOnText(text.to_owned())
        }
        FilterOp::Gt
        | FilterOp::Gte
        | FilterOp::Lt
        | FilterOp::Lte
        | FilterOp::Glob
        | FilterOp::Regex => CompareForm::Native(native()),
    })
}

/// The SEVERITY pin's rung (ADR-0013 §6), resolved in ONE order so every
/// lane reads the same answer:
///
/// 1. an INTEGER binds exactly, unclamped — `_severity>0` is "carries a
///    severity at all" and `_severity=99` honestly matches nothing (the
///    conform rung means no stored value is outside 1-24);
/// 2. a BAND token ([`crate::severity::number_for_token`], the ADR-0009
///    table with its aliases) is the band under `=`/`!=`/IN and the
///    token's own number under an ordered operator — `_severity=error`
///    is the band 17-20, `_severity>=warn` is `>= 13`;
/// 3. an `OTel` EXACT short name ([`crate::severity::number_for_exact`])
///    is that exact number, whatever the operator — `error2` is 18. The
///    band table is consulted first, so the bare base names (`error`,
///    `warn`) keep their band meaning;
/// 4. anything else — a word outside the vocabulary, a float, a boolean —
///    is an error naming the vocabulary, never a filter that quietly
///    matches nothing.
///
/// Glob and regex never reach here (they resolve through
/// [`pattern_form`]); the defensive arm keeps them literal-driven.
fn severity_form(
    op: FilterOp,
    text: &str,
    native: impl FnOnce() -> SqlValue,
) -> Result<CompareForm, CompareError> {
    if matches!(op, FilterOp::Glob | FilterOp::Regex) {
        return Ok(CompareForm::Native(native()));
    }
    if let Ok(n) = text.trim().parse::<i64>() {
        return Ok(CompareForm::SeverityExact(n));
    }
    let equality = matches!(op, FilterOp::Eq | FilterOp::Ne);
    if let Some(number) = crate::severity::number_for_token(text.trim()) {
        if equality {
            let (lo, hi) = crate::severity::band_of(number).expect("table numbers are in-ladder");
            return Ok(CompareForm::SeverityBand { lo, hi });
        }
        return Ok(CompareForm::SeverityExact(i64::from(number)));
    }
    if let Some(number) = crate::severity::number_for_exact(text.trim()) {
        return Ok(CompareForm::SeverityExact(i64::from(number)));
    }
    Err(CompareError::UnknownSeverityToken {
        token: text.to_owned(),
    })
}

/// Resolve the binding for a glob/regex pattern under the field's pin.
///
/// Exhaustive on purpose: a new [`CanonicalType`] must state which text
/// its patterns match, because "whatever `CAST(col AS VARCHAR)` says" is
/// only a *live-mirrorable* answer for types the wire form already
/// stringifies to identically.
#[must_use]
pub fn pattern_form(pin: Option<CanonicalType>) -> PatternForm {
    match pin {
        None | Some(CanonicalType::Varchar) => PatternForm::Native,
        Some(CanonicalType::Timestamp) => PatternForm::Rfc3339Text,
        Some(CanonicalType::Double) => PatternForm::DoubleText,
        Some(CanonicalType::BigInt) => PatternForm::BigIntText,
        Some(CanonicalType::Boolean) => PatternForm::BooleanText,
        Some(CanonicalType::Severity) => PatternForm::SeverityText,
    }
}

/// THE band→points expansion (issue #82): the ladder points a set of
/// `SEVERITY`-pinned equality forms accepts, as one sorted, deduplicated
/// list.
///
/// `Some(points)` exactly when the slice is non-empty and EVERY form is a
/// severity equality form — a [`CompareForm::SeverityBand`] contributing
/// its inclusive `lo..=hi`, or a [`CompareForm::SeverityExact`]
/// contributing its number UNCLAMPED. Anything else (a mixed list, an
/// empty one) is `None` and the caller keeps its per-element shape.
///
/// Unclamped is the point: `_severity=99` stays an honest matches-nothing
/// and a negative literal stays negative, because the exact rung binds
/// integers without consulting the ladder ([`compare_form`]'s rung 1).
/// The set is what lets a whole severity list render its subject ONCE, as
/// `subject IN (…)`, instead of once per band — a `sev()` subject is over
/// a kilobyte of SQL, so the repetition was 4-6x on natural queries.
///
/// The points are `i64` because the exact rung is: `u8` would have to
/// clamp, and clamping is exactly the silent meaning change this must not
/// make.
#[must_use]
pub fn severity_points(forms: &[CompareForm]) -> Option<Vec<i64>> {
    if forms.is_empty() {
        return None;
    }
    let mut points = std::collections::BTreeSet::new();
    for form in forms {
        match form {
            CompareForm::SeverityBand { lo, hi } => {
                points.extend(i64::from(*lo)..=i64::from(*hi));
            }
            CompareForm::SeverityExact(n) => {
                points.insert(*n);
            }
            _ => return None,
        }
    }
    Some(points.into_iter().collect())
}

/// Collapse sorted, deduplicated ladder points into MINIMAL CONTIGUOUS
/// RANGES — the shape a severity predicate actually renders (issue #82).
///
/// Points in, inclusive `(lo, hi)` runs out, in ascending order: a run of
/// one point is `(p, p)`, and two points are one run exactly when they are
/// adjacent integers. The whole ERROR band is therefore ONE run, the six
/// base bands together are ONE run (1-24), and a genuinely disjoint
/// selection like `warn,fatal` is two.
///
/// Ranges rather than the point set itself, because the SET is what the
/// comparison MEANS while the RANGE is what `DuckDB` executes cheaply: a
/// probe over 1M rows (`trawl-engine/tests/severity_set_bench.rs`) puts a
/// repeated-subject `BETWEEN` at ~6 ms against ~392 ms for the equivalent
/// `IN` over the same points — the engine takes an `IN` list over a
/// COMPUTED left-hand side off its fast path, and a `sev()` subject is
/// exactly that. Merging is what makes the two goals one: the natural
/// queries (a band, a contiguous run of bands) collapse to a SINGLE range,
/// so the expensive subject is written once AND the predicate stays on the
/// shape the engine likes.
///
/// # Out-of-ladder points collapse to ONE representative
///
/// Every non-adjacent point is its own run, and every run repeats the
/// subject — so an unbounded run count is an unbounded SQL amplifier. A
/// query is capped at 64 KB of TEXT, but `sev(level) in (1, 3, 5, …)`
/// turns each cheap literal into a fresh ~1.2 KB copy of the subject, so
/// the cap does not bound the SQL. The in-ladder points cannot amplify
/// (1-24 admits at most 12 non-adjacent points, hence ≤12 runs), but the
/// out-of-ladder ones are drawn from all of `i64`.
///
/// They are also all interchangeable, which is what makes the collapse
/// sound. A `SEVERITY`-typed subject evaluates to 1-24 or NULL and
/// nothing else — the conform rung guards a stored `_severity` or
/// SEVERITY-pinned column into that range
/// ([`crate::conform::guarded_cast`]'s ladder `CASE`), and the `sev()`
/// kernel's declared result has the same domain
/// ([`crate::severity::reading_text`]). So for any point `p` outside
/// 1-24, `subject = p` is FALSE for every non-NULL subject and UNKNOWN
/// for a NULL one — a contribution that depends on nothing but `p` being
/// unmatchable, and therefore identical for every such `p`. Keeping the
/// SMALLEST one (deterministic, so snapshots are stable) preserves the
/// predicate's three-valued answer exactly while bounding the render at
/// **≤13 runs**.
///
/// Dropping them entirely would NOT be equivalent: with no in-ladder
/// points left there would be no predicate at all, and the NULL subject
/// must still answer UNKNOWN rather than FALSE. The representative is
/// what carries that.
///
/// This is a RENDERING equivalence only — [`severity_points`] keeps the
/// exact semantic union, and the drift guard checks membership against
/// it over the ladder domain.
///
/// The input should come from [`severity_points`]; duplicates and unsorted
/// input are tolerated defensively, and a run's bounds are `i64` for the
/// same reason the points are — an out-of-ladder literal keeps its own
/// value.
#[must_use]
pub fn severity_ranges(points: &[i64]) -> Vec<(i64, i64)> {
    let mut kept: Vec<i64> = Vec::with_capacity(points.len());
    let mut representative: Option<i64> = None;
    for &p in points {
        if crate::severity::is_valid_number(p) {
            kept.push(p);
        } else {
            representative = Some(representative.map_or(p, |r: i64| r.min(p)));
        }
    }
    if let Some(r) = representative {
        kept.push(r);
    }
    kept.sort_unstable();

    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for p in kept {
        match ranges.last_mut() {
            // Already covered — a duplicate, or the representative landing
            // inside a run it is adjacent to.
            Some((_, hi)) if p <= *hi => {}
            // `checked_add` rather than `hi + 1`: the points are unclamped,
            // so `i64::MAX` is reachable by a literal and must not wrap.
            Some((_, hi)) if hi.checked_add(1) == Some(p) => *hi = p,
            _ => ranges.push((p, p)),
        }
    }
    ranges
}

/// The `SeverityNumber` a SEVERITY-pinned column CONFORMS a stored text to
/// — the live mirror of [`crate::conform::guarded_cast`]'s SEVERITY rung.
///
/// A one-line delegate to the ONE reader (ADR-0013 slice 2, ruling 9):
/// the rung and this mirror are the same kernel, so `"error"` reads 17 on
/// both engines and a text off the ladder — or off the vocabulary
/// entirely — has no reading at all, leaving the column NULL and the
/// value in `_raw`.
#[must_use]
pub fn conformed_severity(text: &str) -> Option<u8> {
    crate::severity::reading_text(text, crate::severity::Dialect::Otel)
}

/// The BIGINT a BIGINT-pinned column CONFORMS a stored text to — the live
/// mirror of [`crate::conform::guarded_cast`]'s BIGINT rung, which is
/// `TRY_CAST` under a `DECIMAL(38,6)` round-trip guard.
///
/// Derived from [`decimal_micros`] alone, because the guard leaves nothing
/// else to decide: it keeps the cast only where
/// `dec(text) = dec(TRY_CAST(text AS BIGINT))`, `dec` of a BIGINT is
/// exact, and so the guard passes exactly when the text's own DECIMAL
/// reading is a whole number of microsteps naming an integer `BIGINT` can
/// hold. Mirroring the CAST separately — the `f64` rung this replaces —
/// could only disagree with `DuckDB`'s exact decimal rounding above 2^53,
/// which it did: `'1.7356896001234568e+18'` and `'9007199254740993.0'`
/// conform in both batch lanes and read as nothing here.
///
/// Spelling drift is fine — `'0404'`, `'4.0'`, `'1e3'`, `' 200'`,
/// `'200_000'` all conform to the integer they denote — while a value the
/// cast would alter (`'1.5'`, `'2.5'`) conforms to NULL and is findable in
/// `_raw` instead.
///
/// `'0x10'` is the instructive rejection: `TRY_CAST` reads it as 16, and
/// the guard refuses it because `DECIMAL` does not read hex at all. That
/// is the guard working as specified — `16` is a value `DuckDB` would
/// never render back as `0x10` — not a mirror artefact; the SQL writes
/// NULL for it too.
///
/// Residual tolerance, shared with the SQL guard rather than added here: a
/// fraction below `DECIMAL(38,6)`'s half-microstep (`'4.0000001'`)
/// quantizes to the integer and conforms; `'4.0000005'` rounds away and is
/// refused.
#[must_use]
pub fn conformed_bigint(text: &str) -> Option<i64> {
    let micros = decimal_micros(text)?;
    (micros % MICROS_PER_UNIT == 0)
        .then_some(micros / MICROS_PER_UNIT)
        .and_then(|units| i64::try_from(units).ok())
}

/// The scale of [`crate::conform::DECIMAL_COMPARISON_SPACE`]: readings are
/// carried as whole microsteps.
const MICROS_PER_UNIT: i128 = 1_000_000;

/// `DECIMAL(38,6)`'s width — the total number of digits it holds.
const DECIMAL_DIGITS: usize = 38;

/// `DECIMAL(38,6)`'s scale, as a shift.
const DECIMAL_SCALE: i64 = 6;

/// The `DECIMAL(38,6)` reading `DuckDB` takes from a text, scaled by 10^6
/// — the live mirror of [`crate::conform::decimal_reading`], and so of
/// BOTH things that compare a text against a number:
/// [`conformed_bigint`]'s round-trip guard and the VARCHAR-pinned
/// comparison rungs ([`CompareForm::TextOrNumeric`],
/// [`CompareForm::NumericOnText`]).
///
/// Exact by construction (digit strings and `i128`, never `f64`): that is
/// the point of the DECIMAL space, which stays exact across the whole
/// BIGINT range and out to 10^32, where a DOUBLE comparison goes blind
/// above 2^53.
///
/// `None` is the NULL the cast writes — UNKNOWN on both engines, never a
/// false match.
///
/// The accepted syntax is the numeric-cast domain — C `isspace` trimmed
/// off both ends, `_` separators strictly between ASCII digits, optional
/// sign, digits with an optional fraction, optional `e`/`E` exponent —
/// minus the radix prefixes (`'0x10'` is NULL here and 16 to the BIGINT
/// cast) and minus `nan`/`inf`. Digits below the sixth decimal are
/// ROUNDED, half away from zero (`'4.0000005'` → `4.000001`,
/// `'4.0000001'` → `4.000000` — the documented tolerance that lets a
/// sub-microstep fraction conform as its integer), and a magnitude at or
/// above 10^32 overflows the type and reads NULL.
///
/// Three of the cast's own quirks are REPRODUCED rather than corrected,
/// because the SQL side has them and a tidier mirror is a divergence — all
/// three established by execution in `trawl-engine/tests/duckdb_probe.rs`:
///
/// 1. **a scan cut short by whitespace is forgiven** in states an
///    end-of-text refuses ([`terminated_scan`]): `'- '` reads 0 and
///    `'1e '` reads 1 where `'-'` and `'1e'` are NULL;
/// 2. **a negative exponent rounds on the leading SIGNIFICANT digit** when
///    its shift drops every digit the mantissa has ([`shift_digits`]), so
///    `'5e-8'` is one microstep while the same value spelled
///    `'0.00000005'` is zero;
/// 3. **the literal exponent is bounded** by the type's integer digits
///    plus the mantissa's own excessive decimals, so `'0.01e33'` is NULL
///    even though the 10^31 it denotes fits comfortably.
#[must_use]
pub fn decimal_micros(text: &str) -> Option<i128> {
    let parsed = NumericText::parse(text)?;
    if parsed.digits.is_empty() {
        return Some(0);
    }
    // The mantissa's decimals beyond the scale are `excessive_decimals` to
    // the cast, and it raises its exponent ceiling by exactly that many.
    let excess = (parsed.frac_len - DECIMAL_SCALE).max(0);
    let ceiling = i64::try_from(DECIMAL_DIGITS).ok()? - DECIMAL_SCALE + excess;
    if parsed.exponent > ceiling {
        return None;
    }

    let scaled = if parsed.exponent >= 0 {
        // One shift: the exponent and the scale are applied together, and
        // a mantissa carrying more decimals than the scale rounds on the
        // first dropped digit.
        let shift = parsed.exponent - parsed.frac_len + DECIMAL_SCALE;
        shift_digits(&parsed.digits, shift, Rounding::Standard)?
    } else {
        // Two shifts, because the cast takes them in two passes and they
        // round differently: the mantissa reaches the scale first, then
        // the negative exponent divides what is left.
        let mantissa = shift_digits(
            &parsed.digits,
            DECIMAL_SCALE - parsed.frac_len,
            Rounding::Standard,
        )?;
        shift_digits(&mantissa, parsed.exponent, Rounding::LeadingDigit)?
    };
    if scaled.len() > DECIMAL_DIGITS {
        return None;
    }
    let magnitude: i128 = scaled.parse().ok()?;
    Some(if parsed.negative {
        -magnitude
    } else {
        magnitude
    })
}

/// One numeric text, split the way `DuckDB`'s cast scans it.
struct NumericText {
    negative: bool,
    /// Every mantissa digit with leading zeros stripped; empty is zero.
    digits: String,
    /// How many of those digits sit after the decimal point.
    frac_len: i64,
    exponent: i64,
}

impl NumericText {
    fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim_matches(is_c_space);
        let completed = terminated_scan(text, trimmed);
        let normalized = if completed.contains('_') {
            std::borrow::Cow::Owned(strip_digit_separators(&completed)?)
        } else {
            completed
        };
        let (negative, unsigned) = match normalized.as_bytes().first() {
            Some(b'+') => (false, &normalized[1..]),
            Some(b'-') => (true, &normalized[1..]),
            _ => (false, &normalized[..]),
        };
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            // Exponents beyond i32 are not typos to reject: they are
            // simply far outside the type, so they saturate the same way
            // the magnitude checks already handle.
            Some(idx) => (
                &unsigned[..idx],
                i64::from(
                    unsigned[idx + 1..]
                        .parse::<i32>()
                        .ok()
                        .filter(|_| !unsigned[idx + 1..].is_empty())?,
                ),
            ),
            None => (unsigned, 0),
        };
        let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let all = format!("{int_part}{frac_part}");
        Some(Self {
            negative,
            digits: all.trim_start_matches('0').to_owned(),
            frac_len: i64::try_from(frac_part.len()).ok()?,
            exponent,
        })
    }
}

/// Complete a text whose numeric scan `DuckDB` cut short but still
/// finalized — the cast is lax where an end-of-text is strict, and the
/// mirror has to be lax in exactly the same places or a stored value the
/// batch query reads as a number is UNKNOWN to the live tail (which `!=`
/// then reports as a match).
///
/// Two terminators do it, both probed: ASCII whitespace anywhere in the
/// scan (the rest must be whitespace too, which trimming already
/// encodes), and a `.` immediately after exponent DIGITS. What the cast
/// forgives at that point is a missing sign body (`'- '` → 0), a missing
/// exponent body (`'1e '`, `'1e+ '` → the mantissa) and the dangling `.`
/// itself (`'1e0.'` → the mantissa). It forgives nothing else: `'-'`,
/// `'1e'`, `'. '`, `'1.5.'` and `'1e0.5'` are all NULL.
fn terminated_scan<'a>(text: &str, trimmed: &'a str) -> std::borrow::Cow<'a, str> {
    // `1e0.` — a `.` right after exponent digits ends the scan, with or
    // without trailing whitespace behind it.
    if let Some(head) = trimmed.strip_suffix('.')
        && let Some(marker) = head.rfind(['e', 'E'])
        && head[marker + 1..]
            .strip_prefix(['+', '-'])
            .is_none_or(|body| !body.is_empty())
        && head[marker + 1..]
            .trim_start_matches(['+', '-'])
            .bytes()
            .all(|b| b.is_ascii_digit())
        && !head[marker + 1..].is_empty()
    {
        return std::borrow::Cow::Owned(head.to_owned());
    }
    if trimmed.len() == text.trim_start_matches(is_c_space).len() {
        // Nothing was trimmed off the end, so no whitespace terminated the
        // scan and the strict reading stands.
        return std::borrow::Cow::Borrowed(trimmed);
    }
    if trimmed == "-" || trimmed == "+" {
        return std::borrow::Cow::Borrowed("0");
    }
    if trimmed
        .strip_suffix(['+', '-'])
        .unwrap_or(trimmed)
        .ends_with(['e', 'E'])
    {
        return std::borrow::Cow::Owned(format!("{trimmed}0"));
    }
    std::borrow::Cow::Borrowed(trimmed)
}

/// Which digit decides a shift that drops more digits than the mantissa
/// has — the one place the cast's two passes disagree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    /// The digit at the boundary, which past the end of the digit run is a
    /// leading ZERO: nothing rounds up. The mantissa pass, so
    /// `'0.00000005'` reads zero.
    Standard,
    /// The leading SIGNIFICANT digit, wherever the boundary fell. The
    /// negative-exponent pass, so `'5e-8'` reads one microstep — the same
    /// value, the other spelling, a different answer.
    LeadingDigit,
}

/// `digits` (leading-zero-free, empty for zero) multiplied by `10^shift`,
/// rounded half away from zero when `shift` drops digits.
///
/// Returns the result as a digit string so a mantissa far wider than
/// `i128` can still be shifted back into range; `None` only when padding
/// would exceed the type outright.
fn shift_digits(digits: &str, shift: i64, rounding: Rounding) -> Option<String> {
    if digits.is_empty() {
        return Some("0".to_owned());
    }
    if shift >= 0 {
        let pad = usize::try_from(shift).ok()?;
        if digits.len().checked_add(pad)? > DECIMAL_DIGITS {
            return None;
        }
        return Some(format!("{digits}{}", "0".repeat(pad)));
    }
    let dropped = usize::try_from(-shift).unwrap_or(usize::MAX);
    let kept_len = digits.len().saturating_sub(dropped);
    let boundary = if kept_len > 0 || digits.len() == dropped {
        digits.as_bytes().get(kept_len).copied()
    } else if rounding == Rounding::LeadingDigit {
        digits.as_bytes().first().copied()
    } else {
        None
    };
    let kept = &digits[..kept_len];
    Some(if boundary.is_some_and(|digit| digit >= b'5') {
        increment_digits(kept)
    } else if kept.is_empty() {
        "0".to_owned()
    } else {
        kept.to_owned()
    })
}

/// The decimal string one greater than `digits`, carrying into a new
/// leading digit when every kept digit was a 9 (`""` is zero, so `"1"`).
fn increment_digits(digits: &str) -> String {
    let mut out = digits.as_bytes().to_vec();
    for byte in out.iter_mut().rev() {
        if *byte == b'9' {
            *byte = b'0';
        } else {
            *byte += 1;
            return String::from_utf8(out).unwrap_or_else(|_| unreachable!());
        }
    }
    let mut carried = String::with_capacity(out.len() + 1);
    carried.push('1');
    carried.extend(out.iter().map(|_| '0'));
    carried
}

/// The BOOLEAN a BOOLEAN-pinned column CONFORMS a stored text to — the
/// live mirror of [`crate::conform::guarded_cast`]'s BOOLEAN rung.
///
/// Exactly two texts, lowercase, untrimmed. The CAST itself takes a wide
/// case-insensitive vocabulary (`'TRUE'`, `'t'`, `'yes'`, `'1'`, `'0'`,
/// `'no'`, …), and the round-trip guard — `CAST(cast AS VARCHAR) = text` —
/// keeps only the two spellings `DuckDB` writes back. Everything else
/// conforms to NULL and stays findable in `_raw`, which is what makes a
/// BOOLEAN pin mean the column holds booleans rather than a synonym table.
#[must_use]
pub fn conformed_boolean(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The BOOLEAN `DuckDB` reads out of a text it CASTS — the wide
/// vocabulary, which is what a query LITERAL gets when it is compared
/// against a BOOLEAN column (`flag=TRUE` and `flag=yes` both match a
/// stored `true`, probed).
///
/// Not what a pinned COLUMN holds: there the round-trip guard keeps only
/// the two spellings `DuckDB` writes back ([`conformed_boolean`]). The
/// asymmetry is deliberate — a stored `'yes'` is data the corpus must not
/// silently restate as `true`, while a literal is the user's own token and
/// `DuckDB` reads it generously.
///
/// A closed, case-insensitive vocabulary with NO whitespace trimming
/// (`' true '` is NULL, unlike the numeric casts) and no numeric texts
/// beyond `1`/`0` (`'2'`, `'1.0'` and `'on'`/`'off'` are all NULL) — every
/// member and every rejection probed by execution.
#[must_use]
pub fn try_cast_boolean(text: &str) -> Option<bool> {
    const TRUE_WORDS: [&str; 5] = ["true", "t", "yes", "y", "1"];
    const FALSE_WORDS: [&str; 5] = ["false", "f", "no", "n", "0"];
    if TRUE_WORDS.iter().any(|w| text.eq_ignore_ascii_case(w)) {
        return Some(true);
    }
    if FALSE_WORDS.iter().any(|w| text.eq_ignore_ascii_case(w)) {
        return Some(false);
    }
    None
}

/// Compare two DOUBLEs in `DuckDB`'s order — the ONE owner of that
/// domain, read by every lane that compares a double.
///
/// `DuckDB` orders DOUBLE TOTALLY, and in two places Rust's IEEE
/// operators do not:
///
/// - **every NaN is EQUAL to every other NaN**, whatever its sign bit,
///   and GREATER than every real value — above `inf`, and last before
///   SQL NULL in a sort. Rust answers `false` to `NaN == NaN` and `None`
///   to `partial_cmp`, so an IEEE mirror answers "no match" where the
///   batch query returns the row.
/// - `-0.0` TIES `0.0`, which Rust's `partial_cmp` already gets right.
///
/// Probed against the engine both ways round — two bound values
/// (`double_comparison_orders_nan_greatest_and_ties_the_two_zeros`) and a
/// stored parquet column against a bound literal
/// (`a_stored_double_column_compares_in_that_same_total_order`), because
/// a scalar answer can be constant-folded and a column read cannot.
///
/// NaN is ordinary reachable data: `0 / 0` in a pipeline expression, a
/// wire `"nan"` under a DOUBLE pin (the conform's round-trip guard keeps
/// both spellings), and `metric=nan` as a filter literal.
#[must_use]
pub fn double_total_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        // Neither side is NaN, so `partial_cmp` is total here — and it
        // already ties the two zeros.
        (false, false) => a
            .partial_cmp(&b)
            .expect("partial_cmp is total when neither operand is NaN"),
    }
}

/// Render a DOUBLE in the pattern text `DuckDB`'s `CAST(col AS VARCHAR)`
/// produces — the live mirror of the DOUBLE pin's pattern target, and the
/// same renderer `tostring()` uses in streaming eval (one renderer, so
/// `dur=/^200$/` and `tostring(dur)` cannot disagree about `200.0`).
///
/// The rendering rules (shortest round-trip digits, a mandatory `.0` on
/// integral values, sign-stripped zero, signed two-digit exponents,
/// lowercase `inf`/`nan`) are documented on the renderer itself and
/// executed against `DuckDB` in `trawl-engine/tests/duckdb_probe.rs`.
#[must_use]
pub fn canonical_double_text(x: f64) -> String {
    crate::eval::duckdb_double_to_string(x)
}

/// The DOUBLE `DuckDB` reads out of a stored text under
/// `TRY_CAST(col AS DOUBLE)` — the live mirror of the DOUBLE pin's own
/// cast, and deliberately NOT `str::parse::<f64>`.
///
/// Two callers, one cast domain: the DOUBLE pin's PATTERN text
/// ([`PatternForm::DoubleText`]) — what the conformed column holds, to be
/// rendered and globbed — and `crate::eval`'s `tonumber()` scalar, whose
/// SQL counterpart is the same `TRY_CAST(… AS DOUBLE)`. Nothing COMPARES
/// through it any more: that is [`decimal_micros`]' job (ADR-0011 ruling
/// #6).
///
/// `DuckDB`'s cast domain is strictly wider than Rust's float parser in
/// two ways (both executed in `trawl-engine/tests/duckdb_probe.rs`), and
/// both widen it in the direction that costs live matches: batch returns
/// the row, the stream drops it.
///
/// - ASCII whitespace is trimmed off BOTH ends — space, `\t`, `\n`,
///   `\r`, `\x0b`, `\x0c` (C `isspace`, so `\x0b` too, which
///   [`char::is_ascii_whitespace`] excludes), and never a non-ASCII
///   space like U+00A0. `' 200'` is 200 to `DuckDB`.
/// - `_` digit separators are accepted strictly BETWEEN ASCII digits:
///   `'200_000'` is 200000 and `'1e1_0'` is 1e10, while `'_200'`,
///   `'200_'`, `'1__0'`, `'1._5'` and `'1_e3'` are NULL.
///
/// Everything else the two engines already agree on, so the normalized
/// text goes to Rust's parser verbatim: `nan`/`inf`/`infinity` in any
/// case and sign, `+5`, `1.`, `.5`, `1e3`, `00200`, and out-of-range
/// exponents saturating to ±inf / ±0. `None` is the NULL `TRY_CAST`
/// writes — UNKNOWN, never a false FALSE that `NOT` could invert.
#[must_use]
pub fn try_cast_double(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(is_c_space);
    if trimmed.contains('_') {
        return strip_digit_separators(trimmed)?.parse().ok();
    }
    trimmed.parse().ok()
}

/// C `isspace` over ASCII — what `DuckDB` strips before a numeric cast.
fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Remove `_` digit separators, or `None` if any sits somewhere
/// `DuckDB` refuses it (anywhere but between two ASCII digits).
fn strip_digit_separators(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (idx, ch) in text.char_indices() {
        if ch != '_' {
            out.push(ch);
            continue;
        }
        // Byte-indexed neighbours: a multi-byte char's trailing byte is
        // never an ASCII digit, so a non-digit neighbour is rejected
        // whatever its encoding.
        let prev = idx.checked_sub(1).map(|i| bytes[i]);
        let next = bytes.get(idx + 1).copied();
        if !prev.is_some_and(|b| b.is_ascii_digit()) || !next.is_some_and(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(out)
}

/// Whether the literal's content is a number, over exactly the set
/// [`coerce_filter_value`]'s i64-then-f64 ladder answers numerically — so
/// the pinned and unpinned paths agree on what counts as one. (Every
/// i64-shaped text parses as `f64` too, so the ladder's two rungs are one
/// membership question; only the BINDING differed, and neither pinned
/// form binds through a float any more.)
///
/// It stays Rust's parser rather than the DECIMAL domain the readings use,
/// because this decides which RULE applies, not what the literal is worth.
/// `dur>1e40` is a numeric comparison whose answer happens to be UNKNOWN
/// for every row; routing it to the lexical rule instead would silently
/// turn it into a string comparison against `"1e40"` and return rows.
fn is_numeric_literal(s: &str) -> bool {
    s.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two doors, unwrapped: every case in this module binds a
    /// literal the rule table accepts, so the fallible signature is noise
    /// here. The refusal path has its own tests below.
    fn compare_form(pin: Option<CanonicalType>, op: FilterOp, literal: &str) -> CompareForm {
        super::compare_form(pin, op, literal).expect("literal binds under the pin")
    }

    fn compare_form_bound(
        pin: Option<CanonicalType>,
        op: FilterOp,
        literal: &LiteralValue,
    ) -> Option<CompareForm> {
        super::compare_form_bound(pin, op, literal).expect("literal binds under the pin")
    }

    const ORDERED: [FilterOp; 4] = [FilterOp::Gt, FilterOp::Gte, FilterOp::Lt, FilterOp::Lte];
    const EQ_CLASS: [FilterOp; 2] = [FilterOp::Eq, FilterOp::Ne];
    const TYPED_PINS: [CanonicalType; 4] = [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Timestamp,
        CanonicalType::Boolean,
    ];

    /// Literal shapes the matrix runs over: (literal, native coercion).
    fn literal_shapes() -> Vec<(&'static str, SqlValue)> {
        vec![
            ("200", SqlValue::Int(200)),
            ("-1", SqlValue::Int(-1)),
            ("0", SqlValue::Int(0)),
            // i64 overflow falls to the f64 rung, mirroring coerce_filter_value.
            (
                "9999999999999999999",
                SqlValue::Float(9_999_999_999_999_999_999.0),
            ),
            ("1.5", SqlValue::Float(1.5)),
            ("-0.5", SqlValue::Float(-0.5)),
            ("accepted", SqlValue::String("accepted".to_owned())),
            ("", SqlValue::String(String::new())),
        ]
    }

    fn is_numeric(native: &SqlValue) -> bool {
        matches!(native, SqlValue::Int(_) | SqlValue::Float(_))
    }

    // ── unpinned: everything stays literal-driven ─────────────────────

    #[test]
    fn unpinned_is_native_for_every_op_and_literal() {
        for (lit, native) in literal_shapes() {
            for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                assert_eq!(
                    compare_form(None, *op, lit),
                    CompareForm::Native(native.clone()),
                    "unpinned {op:?} {lit:?}"
                );
            }
        }
        assert_eq!(pattern_form(None), PatternForm::Native);
    }

    // ── the bound door (ADR-0011 slice A′) ────────────────────────────

    fn lit_str(s: &str) -> LiteralValue {
        LiteralValue::String(s.to_owned())
    }

    /// A float literal as the parser builds it: value plus source token.
    fn lit_float(text: &str) -> LiteralValue {
        LiteralValue::Float(crate::ast::FloatLiteral::new(
            text.parse::<f64>().expect("test literal parses"),
            text,
        ))
    }

    /// `== null` binds no comparison form — the caller falls through to
    /// the generic, pin-blind path.
    #[test]
    fn bound_null_literal_has_no_form() {
        for pin in [None, Some(CanonicalType::Varchar)]
            .into_iter()
            .chain(TYPED_PINS.into_iter().map(Some))
        {
            for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                assert_eq!(
                    compare_form_bound(pin, *op, &LiteralValue::Null),
                    None,
                    "{pin:?} {op:?}"
                );
            }
        }
    }

    /// A typed pin binds the AST's own literal unchanged — the SQL the
    /// pipeline lane emits for `flag == true` is byte-identical to the
    /// pin-blind emission; only the live mirror conforms.
    #[test]
    fn bound_typed_pin_binds_the_ast_literal_unchanged() {
        for pin in TYPED_PINS {
            assert_eq!(
                compare_form_bound(Some(pin), FilterOp::Eq, &LiteralValue::Int(400)),
                Some(CompareForm::Conformed {
                    pin,
                    literal: SqlValue::Int(400)
                }),
                "{pin:?}"
            );
        }
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Boolean),
                FilterOp::Eq,
                &LiteralValue::Bool(true)
            ),
            Some(CompareForm::Conformed {
                pin: CanonicalType::Boolean,
                literal: SqlValue::Bool(true)
            })
        );
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Timestamp),
                FilterOp::Gt,
                &lit_str("2026-01-01")
            ),
            Some(CompareForm::Conformed {
                pin: CanonicalType::Timestamp,
                literal: SqlValue::String("2026-01-01".to_owned())
            })
        );
    }

    /// Quote provenance is discarded: `where status == "400"` binds
    /// exactly as `status == 400` does, under every pin — content
    /// decides, matching the search stage where every literal arrives as
    /// text (`status="400"` and `status=400` are one AST).
    #[test]
    fn bound_string_literal_with_numeric_content_binds_like_the_number() {
        for pin in [None, Some(CanonicalType::Varchar)]
            .into_iter()
            .chain(TYPED_PINS.into_iter().map(Some))
        {
            for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                assert_eq!(
                    compare_form_bound(pin, *op, &lit_str("400")),
                    compare_form_bound(pin, *op, &LiteralValue::Int(400)),
                    "{pin:?} {op:?}"
                );
            }
        }
    }

    /// Quote-insensitivity holds ABOVE 2^53, where `f64` stops being able
    /// to name the number: a float literal binds its SOURCE TOKEN, so the
    /// unquoted spelling resolves to the same form the quoted one does.
    /// Re-rendering the parsed double would bind `…992` and answer for the
    /// ADJACENT identifier (ADR-0011 ruling #6).
    #[test]
    fn bound_float_literal_above_2_53_binds_its_source_token() {
        const HUGE: &str = "9007199254740993.0";
        // The premise: the parsed double cannot express this literal.
        assert_eq!(
            HUGE.parse::<f64>().unwrap().to_string(),
            "9007199254740992",
            "premise: f64 rounds this literal"
        );
        for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
            let quoted = compare_form_bound(Some(CanonicalType::Varchar), *op, &lit_str(HUGE));
            assert_eq!(
                compare_form_bound(Some(CanonicalType::Varchar), *op, &lit_float(HUGE)),
                quoted,
                "varchar {op:?} {HUGE}"
            );
        }
        assert_eq!(
            compare_form_bound(Some(CanonicalType::Varchar), FilterOp::Gt, &lit_float(HUGE)),
            Some(CompareForm::NumericOnText(HUGE.to_owned()))
        );
        assert_eq!(
            compare_form_bound(Some(CanonicalType::Varchar), FilterOp::Eq, &lit_float(HUGE)),
            Some(CompareForm::TextOrNumeric(HUGE.to_owned()))
        );
    }

    /// The bound door and the text door agree on every VARCHAR-pin rule:
    /// one rule table, however the literal arrives.
    #[test]
    fn bound_varchar_rules_mirror_compare_form() {
        let cases: &[(LiteralValue, &str)] = &[
            (LiteralValue::Int(200), "200"),
            (lit_float("1.5"), "1.5"),
            // A trailing zero is content, not noise: the token is what the
            // search stage would have carried for the same text.
            (lit_float("1.50"), "1.50"),
            (lit_str("accepted"), "accepted"),
            (lit_str("0200"), "0200"),
        ];
        for (bound, text) in cases {
            for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                assert_eq!(
                    compare_form_bound(Some(CanonicalType::Varchar), *op, bound),
                    Some(compare_form(Some(CanonicalType::Varchar), *op, text)),
                    "varchar {op:?} {bound:?}"
                );
            }
        }
        // A bool literal reads as its content text in the equality class;
        // the degenerate ordered-with-bool shape keeps the AST value and
        // falls through to generic (literal-driven) emission.
        for op in EQ_CLASS {
            assert_eq!(
                compare_form_bound(Some(CanonicalType::Varchar), op, &LiteralValue::Bool(true)),
                Some(compare_form(Some(CanonicalType::Varchar), op, "true")),
                "varchar {op:?} Bool(true)"
            );
        }
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Varchar),
                FilterOp::Gt,
                &LiteralValue::Bool(true)
            ),
            Some(CompareForm::Native(SqlValue::Bool(true)))
        );
        // The specific shapes, pinned:
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Varchar),
                FilterOp::Eq,
                &LiteralValue::Int(200)
            ),
            Some(CompareForm::TextOrNumeric("200".to_owned()))
        );
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Varchar),
                FilterOp::Gt,
                &LiteralValue::Int(400)
            ),
            Some(CompareForm::NumericOnText("400".to_owned()))
        );
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Varchar),
                FilterOp::Gt,
                &lit_str("alpha")
            ),
            Some(CompareForm::Native(SqlValue::String("alpha".to_owned())))
        );
        assert_eq!(
            compare_form_bound(
                Some(CanonicalType::Varchar),
                FilterOp::Eq,
                &LiteralValue::Bool(true)
            ),
            Some(CompareForm::Text("true".to_owned()))
        );
    }

    // ── VARCHAR pin ───────────────────────────────────────────────────

    #[test]
    fn varchar_eq_class_binds_text_for_every_literal() {
        for (lit, native) in literal_shapes() {
            // A numeric literal additionally carries its reading — the
            // stored text of a number is `read_json`'s inference rendered,
            // so exact text alone is unmirrorable live (see the module doc).
            let expected = if is_numeric(&native) {
                CompareForm::TextOrNumeric(lit.to_owned())
            } else {
                CompareForm::Text(lit.to_owned())
            };
            for op in EQ_CLASS {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    expected,
                    "varchar {op:?} {lit:?}"
                );
            }
        }
    }

    /// The equality rung's numeric half is decided by the SAME ladder the
    /// ordered rung uses: one literal is either a number to both or to
    /// neither, so `status=200` and `status>=200` cannot disagree about
    /// what `200` is.
    #[test]
    fn varchar_eq_and_ordered_agree_on_what_is_numeric() {
        for (lit, _) in literal_shapes()
            .into_iter()
            .chain([("accepted", SqlValue::String("accepted".to_owned()))])
        {
            let eq_numeric = matches!(
                compare_form(Some(CanonicalType::Varchar), FilterOp::Eq, lit),
                CompareForm::TextOrNumeric(_)
            );
            let ordered_numeric = matches!(
                compare_form(Some(CanonicalType::Varchar), FilterOp::Gt, lit),
                CompareForm::NumericOnText(_)
            );
            assert_eq!(eq_numeric, ordered_numeric, "{lit:?}");
        }
    }

    /// The ordered rung carries the literal's own TEXT, never a parsed
    /// number: the SQL binds that string and casts it with the same
    /// expression it casts the column with, so the literal never
    /// round-trips through `f64` (ADR-0011 ruling #6).
    #[test]
    fn varchar_ordered_numeric_literal_carries_the_literal_text() {
        for (lit, native) in literal_shapes() {
            if !is_numeric(&native) {
                continue;
            }
            for op in ORDERED {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    CompareForm::NumericOnText(lit.to_owned()),
                    "varchar {op:?} {lit:?}"
                );
            }
        }
        // The reported id: exact through the form, where the old f64
        // binding equated it with both neighbours.
        assert_eq!(
            compare_form(
                Some(CanonicalType::Varchar),
                FilterOp::Gt,
                "1737000000123456789"
            ),
            CompareForm::NumericOnText("1737000000123456789".to_owned())
        );
    }

    #[test]
    fn varchar_ordered_non_numeric_literal_stays_lexical() {
        for lit in ["accepted", "", "1.5.6", "10a"] {
            for op in ORDERED {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    CompareForm::Native(SqlValue::String(lit.to_owned())),
                    "varchar {op:?} {lit:?}"
                );
            }
        }
    }

    #[test]
    fn varchar_patterns_stay_native() {
        assert_eq!(
            pattern_form(Some(CanonicalType::Varchar)),
            PatternForm::Native
        );
    }

    // ── typed pins ────────────────────────────────────────────────────

    /// A typed pin binds the literal exactly as the unpinned path does —
    /// the emitted SQL stays byte-identical, since the column on disk IS
    /// the pinned type — and carries the pin so the LIVE matcher can
    /// conform the wire value before comparing.
    #[test]
    fn typed_pins_bind_natively_and_carry_the_pin() {
        for pin in TYPED_PINS {
            for (lit, native) in literal_shapes() {
                for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                    assert_eq!(
                        compare_form(Some(pin), *op, lit),
                        CompareForm::Conformed {
                            pin,
                            literal: native.clone()
                        },
                        "{pin:?} {op:?} {lit:?}"
                    );
                }
            }
        }
    }

    // --- the SEVERITY pin (ADR-0013) ---

    /// The equality class takes the BAND, ordered operators take the
    /// token's own number — the semantics the deleted `level=` alias
    /// carried, now bound by the pin instead of by a name.
    #[test]
    fn severity_band_tokens_bind_bands_for_equality_and_numbers_for_ordering() {
        let sev = Some(CanonicalType::Severity);
        for op in EQ_CLASS {
            assert_eq!(
                compare_form(sev, op, "error"),
                CompareForm::SeverityBand { lo: 17, hi: 20 },
                "{op:?}"
            );
        }
        // Aliases and case-insensitivity ride the ADR-0009 token table.
        assert_eq!(
            compare_form(sev, FilterOp::Eq, "ERR"),
            CompareForm::SeverityBand { lo: 17, hi: 20 }
        );
        assert_eq!(
            compare_form(sev, FilterOp::Eq, "notice"),
            CompareForm::SeverityBand { lo: 9, hi: 12 }
        );
        for op in ORDERED {
            assert_eq!(
                compare_form(sev, op, "warn"),
                CompareForm::SeverityExact(13),
                "{op:?}"
            );
        }
    }

    /// The `OTel` exact short names name ONE number, under every
    /// operator — the band table is consulted first, so the bare base
    /// names keep their band meaning.
    #[test]
    fn severity_exact_names_and_integers_bind_exactly() {
        let sev = Some(CanonicalType::Severity);
        for op in EQ_CLASS.into_iter().chain(ORDERED) {
            assert_eq!(
                compare_form(sev, op, "error2"),
                CompareForm::SeverityExact(18),
                "{op:?}"
            );
            assert_eq!(
                compare_form(sev, op, "17"),
                CompareForm::SeverityExact(17),
                "{op:?}"
            );
        }
        // Unclamped: `_severity>0` is "carries a severity at all", and an
        // out-of-ladder equality honestly matches nothing.
        assert_eq!(
            compare_form(sev, FilterOp::Gt, "0"),
            CompareForm::SeverityExact(0)
        );
        assert_eq!(
            compare_form(sev, FilterOp::Eq, "99"),
            CompareForm::SeverityExact(99)
        );
    }

    /// The drift guard for [`severity_points`] (issue #82): expanding a
    /// form to ladder points must accept EXACTLY the numbers the form's
    /// own rule accepts, for every literal the SEVERITY rung binds.
    ///
    /// Exhaustive and pure — every band token, every `OTel` exact short
    /// name, and the integers around the ladder's edges, each checked
    /// against every point on it. If the rule table ever grows a rung
    /// whose membership is not `lo..=hi` or `== n`, this fails rather
    /// than letting the `IN (…)` rendering quietly mean something else.
    #[test]
    fn severity_points_expands_exactly_the_forms_own_membership() {
        let sev = Some(CanonicalType::Severity);
        let literals = crate::severity::CANONICAL_TOKENS
            .iter()
            .map(|t| (*t).to_owned())
            .chain((1..=24).map(|n| {
                crate::severity::otel_name(n)
                    .expect("1-24 name the ladder")
                    .to_owned()
            }))
            .chain((-1..=26).map(|n: i64| n.to_string()));
        for literal in literals {
            let form = compare_form(sev, FilterOp::Eq, &literal);
            let points = severity_points(std::slice::from_ref(&form))
                .expect("a severity equality form always expands");
            // Sorted and deduplicated, as the renderer relies on.
            assert!(
                points.windows(2).all(|w| w[0] < w[1]),
                "{literal}: {points:?}"
            );
            // The RANGES the renderer actually emits must accept exactly
            // the numbers the points do (issue #82): merging is a
            // rendering choice, never a meaning change.
            let ranges = severity_ranges(&points);
            assert!(
                ranges.iter().all(|(lo, hi)| lo <= hi),
                "{literal}: {ranges:?}"
            );
            assert!(
                ranges.windows(2).all(|w| w[0].1 + 1 < w[1].0),
                "runs must be maximal and disjoint — {literal}: {ranges:?}"
            );
            // The amplification bound: ≤12 in-ladder runs plus at most one
            // out-of-ladder representative (review finding A).
            assert!(ranges.len() <= 13, "{literal}: {ranges:?}");
            for n in 1..=24_i64 {
                let live = match form {
                    CompareForm::SeverityBand { lo, hi } => {
                        i64::from(lo) <= n && n <= i64::from(hi)
                    }
                    CompareForm::SeverityExact(exact) => exact == n,
                    ref other => panic!("{literal} bound {other:?}"),
                };
                assert_eq!(points.contains(&n), live, "{literal} at {n}");
                assert_eq!(
                    ranges.iter().any(|(lo, hi)| *lo <= n && n <= *hi),
                    live,
                    "{literal} at {n} through the merged ranges {ranges:?}"
                );
            }
            // Unclamped: an out-of-ladder integer keeps its own value, so
            // `_severity=99` renders `= 99` and honestly matches nothing.
            if let CompareForm::SeverityExact(exact) = form {
                assert_eq!(points, vec![exact]);
            }
        }
    }

    /// `severity_points` is the SEVERITY set door and nothing else: a
    /// non-severity form, or no forms at all, keeps the caller on its
    /// per-element shape.
    #[test]
    fn severity_points_refuses_mixed_and_empty_form_sets() {
        assert_eq!(severity_points(&[]), None);
        assert_eq!(
            severity_points(&[
                CompareForm::SeverityExact(17),
                CompareForm::Text("error".to_owned())
            ]),
            None
        );
        assert_eq!(
            severity_points(&[CompareForm::TextOrNumeric("200".to_owned())]),
            None
        );
        // Overlapping bands and duplicated points collapse to one set.
        assert_eq!(
            severity_points(&[
                CompareForm::SeverityBand { lo: 17, hi: 20 },
                CompareForm::SeverityExact(18),
                CompareForm::SeverityBand { lo: 13, hi: 16 },
            ]),
            Some(vec![13, 14, 15, 16, 17, 18, 19, 20])
        );
    }

    /// The merge itself: maximal runs, in order, and adjacency is what
    /// joins them (issue #82).
    #[test]
    fn severity_ranges_merges_adjacent_points_into_maximal_runs() {
        // One band is one run; the six base bands together are ONE run,
        // which is the case the merge exists for.
        assert_eq!(severity_ranges(&[17, 18, 19, 20]), vec![(17, 20)]);
        assert_eq!(
            severity_ranges(&(1..=24).collect::<Vec<_>>()),
            vec![(1, 24)]
        );
        // A genuinely disjoint selection stays two runs…
        assert_eq!(
            severity_ranges(&[13, 14, 15, 16, 21, 22, 23, 24]),
            vec![(13, 16), (21, 24)]
        );
        // …and a point adjacent to a band extends it rather than splitting
        // (`warn,17` is the contiguous 13-17).
        assert_eq!(severity_ranges(&[13, 14, 15, 16, 17]), vec![(13, 17)]);
        // Singletons stay singletons.
        assert_eq!(severity_ranges(&[18]), vec![(18, 18)]);
        assert_eq!(
            severity_ranges(&[13, 14, 15, 16, 99]),
            vec![(13, 16), (99, 99)]
        );
        assert_eq!(severity_ranges(&[]), vec![]);
    }

    /// Out-of-ladder points are interchangeable, so exactly ONE survives
    /// the render — the amplification bound (issue #82, review finding A).
    #[test]
    fn severity_ranges_collapse_out_of_ladder_points_to_one_representative() {
        // All out of ladder: one representative, the smallest.
        assert_eq!(severity_ranges(&[-5, 99, 101]), vec![(-5, -5)]);
        assert_eq!(severity_ranges(&[25, 26, 4000]), vec![(25, 25)]);
        // Mixed: the in-ladder runs stand, plus the one representative.
        assert_eq!(
            severity_ranges(&[-5, 13, 14, 15, 16, 99, 101]),
            vec![(-5, -5), (13, 16)]
        );
        // A representative ADJACENT to an in-ladder run simply extends it
        // — 0 never matches a real value, so `BETWEEN 0 AND 4` accepts
        // exactly what `BETWEEN 1 AND 4` does.
        assert_eq!(severity_ranges(&[0, 1, 2, 3, 4]), vec![(0, 4)]);
        // `i64::MAX` is reachable by an unclamped literal: neither the
        // adjacency test nor the collapse may wrap.
        assert_eq!(
            severity_ranges(&[i64::MIN, i64::MAX]),
            vec![(i64::MIN, i64::MIN)]
        );
        // THE BOUND: the ladder admits at most 12 non-adjacent points, so
        // no input can render more than 13 runs however long it is.
        let adversarial: Vec<i64> = (1..=24)
            .step_by(2)
            .chain((100..2000).map(|n| n * 2))
            .collect();
        assert!(
            severity_ranges(&adversarial).len() <= 13,
            "{:?}",
            severity_ranges(&adversarial)
        );
        assert_eq!(severity_ranges(&adversarial).len(), 13);
    }

    /// Anything outside the vocabulary is an ERROR naming it — never a
    /// filter that quietly matches nothing.
    #[test]
    fn severity_refuses_literals_outside_the_ladder_vocabulary() {
        let sev = Some(CanonicalType::Severity);
        for literal in ["spicy", "gold", "1.5", "true", "error5", ""] {
            let err =
                super::compare_form(sev, FilterOp::Eq, literal).expect_err("must refuse {literal}");
            assert_eq!(
                err,
                CompareError::UnknownSeverityToken {
                    token: literal.to_owned()
                }
            );
            let msg = err.to_string();
            assert!(
                msg.contains("error"),
                "message must name the vocabulary: {msg}"
            );
            assert!(msg.contains("1-24"), "message must name the ladder: {msg}");
        }
        // The pipeline door refuses the same shapes, including a float
        // and a boolean the search stage can only spell as text.
        for literal in [
            LiteralValue::String("spicy".into()),
            LiteralValue::Bool(true),
            LiteralValue::Float(crate::ast::FloatLiteral::new(1.5, "1.5")),
        ] {
            assert!(
                super::compare_form_bound(sev, FilterOp::Eq, &literal).is_err(),
                "{literal:?} must refuse"
            );
        }
        // `== null` still declines rather than erroring.
        assert_eq!(
            super::compare_form_bound(sev, FilterOp::Eq, &LiteralValue::Null),
            Ok(None)
        );
    }

    /// Both doors agree, literal for literal — the property that keeps
    /// `_severity=error` and `| where _severity == "error"` one rule.
    #[test]
    fn severity_doors_agree() {
        let sev = Some(CanonicalType::Severity);
        for literal in ["error", "error2", "17", "warn"] {
            for op in EQ_CLASS.into_iter().chain(ORDERED) {
                let bound =
                    super::compare_form_bound(sev, op, &LiteralValue::String(literal.to_owned()))
                        .unwrap()
                        .unwrap();
                assert_eq!(compare_form(sev, op, literal), bound, "{literal} {op:?}");
            }
        }
        // An integer literal binds identically whichever door spelled it.
        assert_eq!(
            super::compare_form_bound(sev, FilterOp::Gte, &LiteralValue::Int(13))
                .unwrap()
                .unwrap(),
            compare_form(sev, FilterOp::Gte, "13")
        );
    }

    /// Every typed pin gets its OWN pattern text — none of them can share
    /// "stringify the wire value", because each conforms values the wire
    /// spells differently (`"0404"` → 404, `"TRUE"` → true, `200` → 200.0,
    /// `…T09:00:00Z` → the six-digit RFC 3339 form).
    #[test]
    fn typed_pins_each_take_their_own_pattern_text() {
        for pin in TYPED_PINS {
            let expected = match pin {
                CanonicalType::Timestamp => PatternForm::Rfc3339Text,
                CanonicalType::Double => PatternForm::DoubleText,
                CanonicalType::BigInt => PatternForm::BigIntText,
                CanonicalType::Boolean => PatternForm::BooleanText,
                CanonicalType::Severity => PatternForm::SeverityText,
                CanonicalType::Varchar => unreachable!("not a typed pin"),
            };
            assert_eq!(pattern_form(Some(pin)), expected, "{pin:?}");
        }
    }

    /// The DOUBLE pin's pattern text is `DuckDB`'s DOUBLE rendering, NOT
    /// the wire number's stringification — every expectation here is the
    /// string `CAST(v AS VARCHAR)` returns over a DOUBLE column (executed
    /// side by side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn canonical_double_text_mirrors_duckdb_rendering() {
        let cases = [
            // The reported divergence: a wire `200` stores as 200.0, and
            // `dur=/^200$/` must miss on both sides, not just in batch.
            (200.0, "200.0"),
            (0.0, "0.0"),
            // A stored -0.0 renders signed; only a SQL literal `-0.0`
            // folds to positive zero before it is ever rendered.
            (-0.0, "-0.0"),
            (-3.0, "-3.0"),
            (1.5, "1.5"),
            (1e-7, "1e-07"),
            (1e16, "1e+16"),
            (1.234_567_890_123_456_8e17, "1.2345678901234568e+17"),
            (1e100, "1e+100"),
            (1e-300, "1e-300"),
            (0.0001, "0.0001"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for (input, expected) in cases {
            assert_eq!(canonical_double_text(input), expected, "{input}");
        }
    }

    // ── the TIMESTAMP pin's canonical pattern text ────────────────────

    /// The shapes that HAVE a reading, each with the ONE canonical text —
    /// `T` separator, six fractional digits, `Z`.
    ///
    /// Every expectation is the string `DuckDB` returns for `strftime`
    /// over the same input under [`crate::conform::guarded_cast`]'s
    /// TIMESTAMP rung, executed side by side in
    /// `trawl-engine/tests/duckdb_probe.rs`, which is the CONTRACT: a case
    /// that disagrees there is a bug in this function, never a tolerance.
    const TIMESTAMP_RENDERINGS: &[(&str, &str)] = &[
        // The wire form ingest canonicalizes _time into.
        ("2026-01-15T09:00:00.000000Z", "2026-01-15T09:00:00.000000Z"),
        // Separator and whitespace variants of the same instant.
        ("2026-01-15T09:00:00Z", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00", "2026-01-15T09:00:00.000000Z"),
        ("  2026-01-15 09:00:00  ", "2026-01-15T09:00:00.000000Z"),
        ("\t2026-01-15T09:00:00Z\n", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15  09:00:00", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T 09:00:00", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15\t09:00:00", "2026-01-15T09:00:00.000000Z"),
        // An offset is APPLIED, not dropped (ADR-0011 ruling #1), in
        // every spelling DuckDB accepts — and the components are
        // unvalidated, so `+99:99` really is 99h99m.
        ("2026-01-15T09:00:00+05:30", "2026-01-15T03:30:00.000000Z"),
        ("2026-01-15T09:00:00-08:00", "2026-01-15T17:00:00.000000Z"),
        ("2026-01-15T09:00:00+02", "2026-01-15T07:00:00.000000Z"),
        ("2026-01-15T09:00:00-02", "2026-01-15T11:00:00.000000Z"),
        ("2026-01-15T09:00:00+0530", "2026-01-15T03:30:00.000000Z"),
        ("2026-01-15T09:00:00-0800", "2026-01-15T17:00:00.000000Z"),
        ("2026-01-15T09:00:00-0000", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00+00", "2026-01-15T09:00:00.000000Z"),
        (
            "2026-01-15T09:00:00+05:30:15",
            "2026-01-15T03:29:45.000000Z",
        ),
        (
            "2026-01-15T09:00:00-00:00:01",
            "2026-01-15T09:00:01.000000Z",
        ),
        ("2026-01-15T09:00:00+99:99", "2026-01-11T04:21:00.000000Z"),
        ("2026-01-15T09:00:00+24:00", "2026-01-14T09:00:00.000000Z"),
        ("2026-01-15T09:00:00-99:00", "2026-01-19T12:00:00.000000Z"),
        ("2026-01-15T09:00:00+05:30 ", "2026-01-15T03:30:00.000000Z"),
        (
            "2026-01-15T09:00:00.123456+05:30",
            "2026-01-15T03:30:00.123456Z",
        ),
        ("2026-01-15 9:0:0.5+05:30", "2026-01-15T03:30:00.500000Z"),
        // Fractions pad to six digits and TRUNCATE beyond them; the
        // `.` may carry no digits at all.
        ("2026-01-15T09:00:00.123Z", "2026-01-15T09:00:00.123000Z"),
        ("2026-01-15T09:00:00.1234567", "2026-01-15T09:00:00.123456Z"),
        (
            "2026-01-15T09:00:00.9999999Z",
            "2026-01-15T09:00:00.999999Z",
        ),
        (
            "2026-01-15T09:00:00.000000999Z",
            "2026-01-15T09:00:00.000000Z",
        ),
        (
            "2026-01-15T09:00:00.0000000000Z",
            "2026-01-15T09:00:00.000000Z",
        ),
        ("2026-01-15T09:00:00.Z", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00.-08:00", "2026-01-15T17:00:00.000000Z"),
        // Date-only, slash dates, and unpadded/overpadded components.
        ("2026-01-15", "2026-01-15T00:00:00.000000Z"),
        ("  2026-01-15", "2026-01-15T00:00:00.000000Z"),
        ("2026/01/15", "2026-01-15T00:00:00.000000Z"),
        ("2026/01/15 09:00:00", "2026-01-15T09:00:00.000000Z"),
        ("2026/01/15T09:00:00", "2026-01-15T09:00:00.000000Z"),
        ("2026-1-5", "2026-01-05T00:00:00.000000Z"),
        ("2026-1-5 9:0:0", "2026-01-05T09:00:00.000000Z"),
        ("02026-01-15", "2026-01-15T00:00:00.000000Z"),
        ("2026-01-15T009:00:00Z", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T0009:00:00Z", "2026-01-15T09:00:00.000000Z"),
        // Hour 24 is the next midnight — and only when it IS midnight.
        ("2026-01-15 24:00:00", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15T24:00:00", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15T24:00:00Z", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15 24:00", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15 24:00:00.000000", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15 24:00:00 UTC", "2026-01-16T00:00:00.000000Z"),
        ("2026-01-15 24:00:00+05:30", "2026-01-15T18:30:00.000000Z"),
        ("2026-12-31 24:00:00", "2027-01-01T00:00:00.000000Z"),
        // Keyword instants, case-insensitive, whitespace-tolerant.
        ("epoch", "1970-01-01T00:00:00.000000Z"),
        ("EpOcH", "1970-01-01T00:00:00.000000Z"),
        (" epoch ", "1970-01-01T00:00:00.000000Z"),
        ("infinity", "infinity"),
        ("INFINITY", "infinity"),
        ("inf", "infinity"),
        ("INF", "infinity"),
        (" infinity ", "infinity"),
        ("-infinity", "-infinity"),
        ("-inf", "-infinity"),
        // Zone NAMES that mean UTC for all time — one space, any case.
        ("2026-01-15 09:00:00 UTC", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00 UTC", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 uTc", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 GMT", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 gmt", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 Zulu", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 UCT", "2026-01-15T09:00:00.000000Z"),
        (
            "2026-01-15 09:00:00 Universal",
            "2026-01-15T09:00:00.000000Z",
        ),
        (
            "2026-01-15 09:00:00 Greenwich",
            "2026-01-15T09:00:00.000000Z",
        ),
        ("2026-01-15 09:00:00 GMT0", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 GMT+0", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 GMT-0", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 Etc/UTC", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 Etc/GMT", "2026-01-15T09:00:00.000000Z"),
        (
            "2026-01-15 09:00:00 Etc/Zulu",
            "2026-01-15T09:00:00.000000Z",
        ),
        ("2026-01-15 09:00:00 etc/utc", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15 09:00:00 UTC  ", "2026-01-15T09:00:00.000000Z"),
        (
            "2026-01-15T09:00:00.123456789 UTC",
            "2026-01-15T09:00:00.123456Z",
        ),
        // Years outside four digits: `%Y` pads a non-negative year to
        // four and writes a negative one bare, unlike chrono's own.
        ("0001-01-01 00:00:00", "0001-01-01T00:00:00.000000Z"),
        ("1-01-01", "0001-01-01T00:00:00.000000Z"),
        ("0-01-01", "0000-01-01T00:00:00.000000Z"),
        ("-0001-01-01 00:00:00", "-1-01-01T00:00:00.000000Z"),
        ("-0100-01-01 00:00:00", "-100-01-01T00:00:00.000000Z"),
        ("10000-01-01 00:00:00", "10000-01-01T00:00:00.000000Z"),
        ("100000-01-01 00:00:00", "100000-01-01T00:00:00.000000Z"),
        ("9999-12-31 24:00:00", "10000-01-01T00:00:00.000000Z"),
        ("9999-12-31T23:59:59.999999Z", "9999-12-31T23:59:59.999999Z"),
        ("1969-12-31T23:59:59Z", "1969-12-31T23:59:59.000000Z"),
        // The full keyword spellings tolerate trailing whitespace, and a
        // leading `-` is consumed before the keyword is read at all.
        ("epoch ", "1970-01-01T00:00:00.000000Z"),
        ("epoch\t", "1970-01-01T00:00:00.000000Z"),
        ("-epoch", "1970-01-01T00:00:00.000000Z"),
        ("-epoch ", "1970-01-01T00:00:00.000000Z"),
        ("-EPOCH", "1970-01-01T00:00:00.000000Z"),
        ("infinity ", "infinity"),
        ("-infinity\t", "-infinity"),
        (" inf", "infinity"),
        ("\tinf", "infinity"),
        // A space closes a zoneless time; further whitespace after it is
        // then trailing, and past a real zone anything goes.
        ("2026-01-15T09:00:00 ", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00  ", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00 \t", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00Z\t", "2026-01-15T09:00:00.000000Z"),
        ("2026-01-15T09:00:00+05:30\t", "2026-01-15T03:30:00.000000Z"),
        ("2026-01-15T09:00:00 UTC\t", "2026-01-15T09:00:00.000000Z"),
    ];

    #[test]
    fn canonical_timestamp_text_mirrors_duckdb_rendering() {
        for (input, expected) in TIMESTAMP_RENDERINGS {
            assert_eq!(
                canonical_timestamp_text(input).as_deref(),
                Some(*expected),
                "{input:?}"
            );
        }
    }

    /// No reading → `None`, the UNKNOWN that mirrors the NULL the conform
    /// wrote for the same value. Epoch numerals are not timestamps to
    /// `DuckDB` and must not become one here.
    ///
    /// The `HH:MM` rows are the false-POSITIVE direction the wall-clock
    /// mirror used to get wrong: a seconds-less time takes no zone and no
    /// trailing anything, so `09:00Z` fires live and NULLs in batch unless
    /// the mirror refuses it too.
    #[test]
    fn canonical_timestamp_text_is_none_without_a_reading() {
        for input in [
            "yesterday-ish",
            "",
            "accepted",
            "0404",
            "1737000000",
            "1737000000123",
            // Relative keywords are not timestamps; `epoch` is the only
            // word-shaped instant, and it takes no arithmetic.
            "now",
            "today",
            "tomorrow",
            "yesterday",
            "epoch+1",
            "+infinity",
            // A separator with no time after it: not a date to DuckDB
            // either, so trailing whitespace must not be trimmed away
            // before the split.
            "2026-01-15T",
            "2026-01-15 ",
            "2026-01-15\t",
            "2026-01-15\n",
            "2026-01-15T09",
            "2026-01-15 09",
            "2026-01-15Z",
            "2026-01-15+05:30",
            // A seconds-less time must END the text.
            "2026-01-15T09:00Z",
            "2026-01-15T09:00+05:30",
            "2026-01-15T09:00 UTC",
            "2026-01-15T09:00 ",
            "2026-01-15T09:00\t",
            "2026-01-15T09:00.5",
            "2026-01-15T09:00.",
            "2026-01-15 9:0Z",
            "2026-01-15T24:00Z",
            "2026-01-15T24:00 ",
            // Out-of-range clock fields, hour 24 included: it rolls over
            // only from an exact midnight.
            "2026-01-15 25:00:00",
            "2026-01-15 24:00:01",
            "2026-01-15 24:01:00",
            "2026-01-15T24:00:00.000001",
            "2026-01-15 23:59:60",
            "2026-01-15T09:60:00Z",
            "2026-01-15T09:00:60Z",
            // Malformed offsets: each component is EXACTLY two digits,
            // `Z` is uppercase, and nothing may follow the zone.
            "2026-01-15t09:00:00z",
            "2026-01-15T09:00:00z",
            "2026-01-15T09:00:00ZZ",
            "2026-01-15T09:00:00 +05:30",
            "2026-01-15T09:00:00+5:30",
            "2026-01-15T09:00:00+5",
            "2026-01-15T09:00:00+053",
            "2026-01-15T09:00:00+05:3",
            "2026-01-15T09:00:00+053015",
            "2026-01-15T09:00:00+100:00",
            "2026-01-15T09:00:00+005:30",
            "2026-01-15T09:00:00+999",
            "2026-01-15T09:00:00+05:30:1",
            "2026-01-15T09:00:00+05:30:155",
            "2026-01-15T09:00:00+",
            "2026-01-15T09:00:00-",
            "2026-01-15T09:00:00.123-",
            "2026-01-15T09:00:00,123Z",
            // A zone NAME takes exactly one space before it.
            "2026-01-15 09:00:00UTC",
            "2026-01-15 09:00:00  UTC",
            "2026-01-15 09:00:00\tUTC",
            "2026-01-15 09:00:00\nUTC",
            "2026-01-15 09:00:00 Z",
            "2026-01-15 09:00:00 UT",
            "2026-01-15 09:00:00 GMT+2",
            "2026-01-15 09:00:00 Narnia/Cair_Paravel",
            // Malformed dates: month and day are one or two digits, both
            // separators are the same character, and the year takes no
            // leading `+`.
            "2026-011-15",
            "2026-01-015",
            "2026-01/15",
            "2026/01-15",
            "20260115",
            "2026-02-30",
            "2026-13-01",
            "+2026-01-15",
            "2026-01-15T09:000:00Z",
            "2026-01-15T09:00:000Z",
            // Non-ASCII whitespace is not whitespace to DuckDB.
            "\u{a0}2026-01-15T09:00:00Z",
            // The `inf` abbreviations take NO trailing whitespace, where
            // the full spellings do — the over-match this mirror had.
            "inf ",
            "inf\t",
            "-inf ",
            "INF\n",
            " inf ",
            "- epoch",
            "--epoch",
            "epochx",
            "infx",
            "infinityx",
            // A zoneless time is closed by a SPACE (where a zone name
            // would start) and by nothing else.
            "2026-01-15T09:00:00\t",
            "2026-01-15T09:00:00\n",
            "2026-01-15T09:00:00\r",
            "2026-01-15T09:00:00\x0b",
            "2026-01-15T09:00:00\x0c",
            "2026-01-15 09:00:00\t",
            "2026-01-15T09:00:00.123\t",
            // Outside DuckDB's own microsecond range.
            "300000-01-01 00:00:00",
            "-290308-01-01 00:00:00",
        ] {
            assert_eq!(canonical_timestamp_text(input), None, "{input:?}");
        }
    }

    /// The two shapes the mirror deliberately does NOT read, asserted here
    /// so a later change to either has to say so out loud.
    ///
    /// `DuckDB` has a reading for all of them, so each is a live tail that
    /// UNDER-matches a batch query — never the reverse. The reasons are on
    /// [`canonical_timestamp_text`]; the divergence itself is pinned
    /// against the engine in `trawl-engine/tests/duckdb_probe.rs`, so it
    /// stays a known cost rather than becoming a rediscovery.
    #[test]
    fn canonical_timestamp_text_declines_the_documented_residuals() {
        for input in [
            // A zone whose offset needs a zone HISTORY, not a table.
            "2026-01-15 09:00:00 America/New_York",
            "2026-01-15 09:00:00 EST",
            "2026-01-15 09:00:00 Etc/GMT+5",
            "2026-01-15 09:00:00 UTC+2",
            "1800-01-01 00:00:00 Africa/Abidjan",
            // A year outside chrono's calendar but inside DuckDB's.
            "262144-01-01 00:00:00",
            "294247-01-01 00:00:00",
        ] {
            assert_eq!(canonical_timestamp_text(input), None, "{input:?}");
        }
    }

    // ── the stored-value cast domain mirrors DuckDB ───────────────────

    /// Every expectation here is the value `DuckDB`'s
    /// `TRY_CAST(v AS DOUBLE)` returns for the same text (executed side
    /// by side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn try_cast_double_mirrors_duckdb_cast_domain() {
        // ASCII whitespace is trimmed off both ends, `\x0b` included.
        for text in [
            " 200",
            "200 ",
            "\t200\n",
            "\r200\r",
            "\x0b200\x0c",
            "  200  ",
        ] {
            assert_eq!(try_cast_double(text), Some(200.0), "{text:?}");
        }
        // Non-ASCII spaces are not whitespace to DuckDB.
        for text in ["\u{a0}200", "\u{2000}200", "2 00", " ", ""] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
        // `_` separators, accepted only between ASCII digits.
        assert_eq!(try_cast_double("200_000"), Some(200_000.0));
        assert_eq!(try_cast_double("1_000.5"), Some(1000.5));
        assert_eq!(try_cast_double("1_0"), Some(10.0));
        assert_eq!(try_cast_double("-1_0"), Some(-10.0));
        assert_eq!(try_cast_double("1.0_0"), Some(1.0));
        assert_eq!(try_cast_double("1e1_0"), Some(1e10));
        assert_eq!(try_cast_double(" 1_0 "), Some(10.0));
        for text in [
            "_200", "200_", "1__0", "1._5", "1.5_", "1_.5", "1_e3", "1e_3", "_",
        ] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
        // Shapes both engines already agree on, unchanged.
        assert_eq!(try_cast_double("+5"), Some(5.0));
        assert_eq!(try_cast_double("1."), Some(1.0));
        assert_eq!(try_cast_double(".5"), Some(0.5));
        assert_eq!(try_cast_double("1e3"), Some(1000.0));
        assert_eq!(try_cast_double("00200"), Some(200.0));
        assert_eq!(try_cast_double("1e400"), Some(f64::INFINITY));
        assert!(try_cast_double("nan").is_some_and(f64::is_nan));
        assert!(try_cast_double("-NAN").is_some_and(f64::is_nan));
        assert_eq!(try_cast_double("infinity"), Some(f64::INFINITY));
        for text in ["0x10", "1,000", "1d", "true", "1.5e2.5", "1-"] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
    }

    /// The BIGINT reading is DERIVED from the comparison space, so the
    /// guard's algebra is the test: a text conforms exactly when its
    /// DECIMAL reading is a whole number of microsteps that `BIGINT` can
    /// hold. Every pairing is executed against the guard SQL in
    /// `trawl-engine/tests/duckdb_probe.rs`.
    #[test]
    fn conformed_bigint_is_the_whole_microsteps_of_the_decimal_reading() {
        for text in [
            "404",
            "0404",
            "4.0",
            "1e3",
            " 200",
            "200_000",
            "1.7356896001234568e+18",
            "9007199254740993.0",
            "9223372036854775807",
            "1.5",
            "0x10",
            "accepted",
            "1e19",
            "5e-8",
        ] {
            let expected = decimal_micros(text).and_then(|micros| {
                (micros % 1_000_000 == 0)
                    .then_some(micros / 1_000_000)
                    .and_then(|units| i64::try_from(units).ok())
            });
            assert_eq!(conformed_bigint(text), expected, "{text:?}");
        }
        // The `f64` cast rung this replaces went blind above 2^53: both
        // batch lanes hold these integers where the mirror read nothing.
        assert_eq!(
            conformed_bigint("1.7356896001234568e+18"),
            Some(1_735_689_600_123_456_800)
        );
        assert_eq!(
            conformed_bigint("9007199254740993.0"),
            Some(9_007_199_254_740_993)
        );
    }

    /// The conformed BIGINT reading keeps every SPELLING drift and refuses
    /// every VALUE change — the round-trip guard, mirrored (each
    /// expectation executed against the guard SQL in
    /// `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn conformed_bigint_preserves_value_not_spelling() {
        // Spelling drift: same value, different text.
        for (text, value) in [
            ("404", 404),
            ("0404", 404),
            ("00200", 200),
            ("+5", 5),
            ("-0", 0),
            (" 200", 200),
            ("\t404\n", 404),
            ("200_000", 200_000),
            ("1e3", 1000),
            ("1e1_0", 10_000_000_000),
            ("4.0", 4),
            ("0404.000000", 404),
            ("1.5e1", 15),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
            // Below DECIMAL(38,6)'s half-microstep: quantizes to the
            // integer, the documented residual tolerance.
            ("4.0000001", 4),
            ("4.0000004999", 4),
            // Above 2^53, where the deleted `f64` cast rung read nothing
            // and both batch lanes hold the integer.
            ("1.7356896001234568e+18", 1_735_689_600_123_456_800),
            ("1735689600123456800.0", 1_735_689_600_123_456_800),
            ("9007199254740993.0", 9_007_199_254_740_993),
        ] {
            assert_eq!(conformed_bigint(text), Some(value), "{text:?}");
        }
        // Value changes the cast would have made silently.
        for text in [
            "1.5",
            "1.4",
            "2.5",
            "-2.5",
            ".5",
            "1_0.5",
            // Half a microstep and up rounds away from zero, so the
            // DECIMAL reading is no longer the integer.
            "4.0000005",
            "-4.0000005",
            // Above 2^53, where a DOUBLE-space guard went blind.
            "1735689600123456710.7",
        ] {
            assert_eq!(conformed_bigint(text), None, "{text:?}");
        }
        // Radix prefixes: read by the CAST, refused by the guard —
        // DECIMAL does not read hex, and `16` is not a value DuckDB would
        // ever write back as `0x10`.
        for text in ["0x10", "0B101", "-0x10", " 0x10 "] {
            assert_eq!(conformed_bigint(text), None, "{text:?}");
        }
        // No reading at all, and out of BIGINT range.
        for text in [
            "accepted",
            "",
            "true",
            "nan",
            "inf",
            "1,000",
            "4 04",
            "9223372036854775808",
            "-9223372036854775809",
            "1e19",
            "1e400",
        ] {
            assert_eq!(conformed_bigint(text), None, "{text:?}");
        }
    }

    /// The guard's comparison space, exact by construction: `DuckDB`'s
    /// `TRY_CAST(v AS DECIMAL(38,6))` scaled by 10^6 (probe-pinned).
    #[test]
    fn decimal_micros_mirrors_duckdb_decimal_domain() {
        for (text, micros) in [
            ("200", Some(200_000_000)),
            (" 200 ", Some(200_000_000)),
            ("200_000", Some(200_000_000_000)),
            ("0404", Some(404_000_000)),
            ("+.5", Some(500_000)),
            ("1.", Some(1_000_000)),
            ("-0", Some(0)),
            ("-0.0", Some(0)),
            ("1e3", Some(1_000_000_000)),
            ("1E3", Some(1_000_000_000)),
            ("1e+3", Some(1_000_000_000)),
            ("0.5e1", Some(5_000_000)),
            ("1e-40", Some(0)),
            // Rounding at the sixth decimal, half away from zero.
            ("0.0000001", Some(0)),
            ("0.0000005", Some(1)),
            ("-0.0000005", Some(-1)),
            ("4.0000015", Some(4_000_002)),
            ("4.00000050000000001", Some(4_000_001)),
            ("4.0000004999", Some(4_000_000)),
            // Outside the syntax, or outside DECIMAL(38,6)'s magnitude.
            ("0x10", None),
            ("nan", None),
            ("inf", None),
            (" ", None),
            ("", None),
            ("1e", None),
            ("1,000", None),
            ("1.5.6", None),
            ("1e32", None),
            ("\u{a0}200", None),
            // The cast forgives a scan that whitespace cut short, in the
            // three states it can end in and nowhere else.
            ("- ", Some(0)),
            ("+\n", Some(0)),
            ("1e ", Some(1_000_000)),
            ("1e+ ", Some(1_000_000)),
            ("1e- ", Some(1_000_000)),
            ("1.e ", Some(1_000_000)),
            ("1e0.", Some(1_000_000)),
            ("1e0. ", Some(1_000_000)),
            ("1e5.", Some(100_000_000_000)),
            ("-", None),
            ("1e0.5", None),
            ("1e0..", None),
            ("1.5.", None),
            (". ", None),
            ("-. ", None),
            ("1e-. ", None),
            ("- x", None),
            // A negative exponent that drops every mantissa digit rounds
            // on the LEADING one; the same values without an exponent do
            // not, and neither does a positive exponent.
            ("5e-8", Some(1)),
            ("5e-30", Some(1)),
            ("0.5e-8", Some(1)),
            ("54e-9", Some(1)),
            ("45e-9", Some(0)),
            ("1.5e-8", Some(0)),
            ("5.5e-8", Some(1)),
            ("0.00000005", Some(0)),
            ("0.000000005", Some(0)),
            ("0.00000005e-1", Some(0)),
            ("0.000000005e+1", Some(0)),
            ("0.00000005e+1", Some(1)),
            // A negative exponent whose shift lands INSIDE the mantissa
            // rounds on the boundary digit, as everywhere else.
            ("15e-7", Some(2)),
            ("14e-7", Some(1)),
            ("1000005e-8", Some(10_000)),
            ("1000005e-13", Some(0)),
            // The exponent ceiling: the type's integer digits, raised by
            // the mantissa's own excessive decimals. Both of these name a
            // magnitude the type holds.
            ("0.01e33", None),
            ("0.000000005e40", None),
            ("0.5e32", Some(5 * 10i128.pow(37))),
            ("0.000000005e35", Some(5 * 10i128.pow(32))),
        ] {
            assert_eq!(decimal_micros(text), micros, "{text:?}");
        }
        // The exact edges of the type: 10^32 - 10^-6 fits, 10^32 does not.
        assert_eq!(
            decimal_micros("99999999999999999999999999999999.999999"),
            Some(10i128.pow(38) - 1)
        );
        assert_eq!(decimal_micros("1e31"), Some(10i128.pow(37)));
        assert_eq!(
            decimal_micros("99999999999999999999999999999999999999"),
            None
        );
    }

    /// Exactly two conformed texts, lowercase and untrimmed: the CAST's
    /// vocabulary is wide, and the round-trip guard keeps only what
    /// `DuckDB` renders back (probe-pinned).
    #[test]
    fn conformed_boolean_keeps_only_the_rendered_spellings() {
        assert_eq!(conformed_boolean("true"), Some(true));
        assert_eq!(conformed_boolean("false"), Some(false));
        for text in [
            "TRUE", "tRuE", "t", "T", "yes", "yEs", "Y", "1", "FALSE", "f", "F", "no", "nO", "N",
            "0", "on", "off", "", " true ", "\ttrue\n", "accepted", "2", "-1", "1.0", "01", "+1",
            "true1",
        ] {
            assert_eq!(conformed_boolean(text), None, "{text:?}");
        }
    }

    /// The comparison space is EXACT where a DOUBLE one collapses: the
    /// ids from the reported finding read as three distinct values, and
    /// `2^53 ± 1` are distinguishable at all (executed against `DuckDB`
    /// in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn decimal_micros_separates_ids_a_double_would_equate() {
        let ids = [
            "1737000000123456788",
            "1737000000123456789",
            "1737000000123456790",
            "9007199254740992",
            "9007199254740993",
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(decimal_micros(a), decimal_micros(b), "{a} vs {b}");
            }
        }
        // The premise of the finding: the DOUBLE reading these used to be
        // compared through equates each pair, so the collision was in the
        // comparison space and not in either engine.
        for (a, b) in [
            ("1737000000123456788", "1737000000123456789"),
            ("1737000000123456789", "1737000000123456790"),
            ("9007199254740992", "9007199254740993"),
        ] {
            assert_eq!(try_cast_double(a), try_cast_double(b), "{a} vs {b}");
        }
        // Both spellings of the same value still meet — the enum-shaped
        // case the numeric arm exists for.
        assert_eq!(decimal_micros("200"), decimal_micros("200.0"));
    }

    // ── the numeric-literal ladder mirrors coerce_filter_value ────────

    #[test]
    fn numeric_detection_follows_the_coercion_ladder() {
        // i64 rung.
        assert!(is_numeric_literal("42"));
        assert!(is_numeric_literal("-7"));
        // f64 rung (i64 overflow, fractions).
        assert!(is_numeric_literal("1.5"));
        assert!(is_numeric_literal("9999999999999999999"));
        // Numeric to the ladder, unreadable in the comparison space: the
        // rule still applies, and both engines answer UNKNOWN through it
        // rather than falling back to a lexical comparison.
        for lit in ["nan", "inf", "-inf", "1e40"] {
            assert!(is_numeric_literal(lit), "{lit:?}");
            assert_eq!(decimal_micros(lit), None, "{lit:?}");
        }
        // Non-numeric.
        assert!(!is_numeric_literal("accepted"));
        assert!(!is_numeric_literal(""));
        assert!(!is_numeric_literal("1.5s"));
        // Whitespace is NOT trimmed — mirrors coerce_filter_value, which
        // would bind " 200 " as a string.
        assert!(!is_numeric_literal(" 200 "));
    }
}
