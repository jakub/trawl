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
//! | typed pins | everything else | [`CompareForm::Native`] — unchanged (already correct) |
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
//! prefixes refused — executed side by side in
//! `trawl-engine/tests/duckdb_probe.rs`. [`try_cast_double`] survives only
//! for the DOUBLE pin's PATTERN text, which renders what the column stores
//! instead of comparing anything.

use crate::ast::FilterOp;
use crate::emitter::SqlValue;
use crate::emitter::coerce_filter_value;
use crate::schema::CanonicalType;

/// How one search-stage comparison binds its literal.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareForm {
    /// Today's literal-driven binding — the unpinned/typed-pin default.
    Native(SqlValue),
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
}

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

/// The instant a TIMESTAMP-pinned column holds for one stored text.
///
/// `DuckDB`'s TIMESTAMP carries two values no calendar date can express,
/// and `strftime` renders them as the literal words `infinity` and
/// `-infinity` — texts a glob can match, so the mirror has to produce them
/// too.
enum Instant {
    At(chrono::NaiveDateTime),
    Infinity,
    NegInfinity,
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
/// - **keywords**: `epoch` → 1970-01-01, `infinity`/`inf` and
///   `-infinity`/`-inf` → the infinite instants — all case-insensitive,
///   surrounding whitespace allowed. `+infinity` is not one of them;
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
///   space and a zone name. Without seconds the text must end where the
///   time does: `09:00Z`, `09:00 UTC` and even `09:00 ` are all refused,
///   the false-positive direction the wall-clock mirror used to get
///   wrong;
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
/// 2. **years outside chrono's calendar** (below -262144 or above
///    262143), where `DuckDB`'s microsecond range reaches ±~290 000.
#[must_use]
pub fn canonical_timestamp_text(value: &str) -> Option<String> {
    match parse_instant(value)? {
        Instant::Infinity => Some("infinity".to_owned()),
        Instant::NegInfinity => Some("-infinity".to_owned()),
        Instant::At(at) => Some(render_pattern_text(at)),
    }
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
fn parse_instant(value: &str) -> Option<Instant> {
    let text = &value[leading_space(value.as_bytes(), 0)..];
    keyword_instant(text).or_else(|| parse_datetime(text).map(Instant::At))
}

/// The keyword instants, case-insensitive, trailing whitespace allowed.
fn keyword_instant(text: &str) -> Option<Instant> {
    let token = text.trim_end_matches(is_c_space);
    if token.eq_ignore_ascii_case("epoch") {
        return chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(Instant::At);
    }
    if token.eq_ignore_ascii_case("infinity") || token.eq_ignore_ascii_case("inf") {
        return Some(Instant::Infinity);
    }
    if token.eq_ignore_ascii_case("-infinity") || token.eq_ignore_ascii_case("-inf") {
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
fn parse_datetime(text: &str) -> Option<chrono::NaiveDateTime> {
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
        parse_zone_offset(text, &mut pos)?
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
fn parse_zone_offset(text: &str, pos: &mut usize) -> Option<i64> {
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
        Some(&c) if is_c_space(c as char) => {
            // A zone NAME is separated by EXACTLY one space (`  UTC` and
            // `\tUTC` are both refused) and runs to the end of the text.
            if c == b' ' {
                let name = text[*pos + 1..].trim_end_matches(is_c_space);
                if UTC_ZONE_NAMES
                    .iter()
                    .any(|zone| zone.eq_ignore_ascii_case(name))
                {
                    *pos = bytes.len();
                    return Some(0);
                }
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
#[must_use]
pub fn compare_form(pin: Option<CanonicalType>, op: FilterOp, literal: &str) -> CompareForm {
    if pin != Some(CanonicalType::Varchar) {
        return CompareForm::Native(coerce_filter_value(literal));
    }
    match op {
        FilterOp::Eq | FilterOp::Ne if is_numeric_literal(literal) => {
            CompareForm::TextOrNumeric(literal.to_owned())
        }
        FilterOp::Eq | FilterOp::Ne => CompareForm::Text(literal.to_owned()),
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte
            if is_numeric_literal(literal) =>
        {
            CompareForm::NumericOnText(literal.to_owned())
        }
        FilterOp::Gt
        | FilterOp::Gte
        | FilterOp::Lt
        | FilterOp::Lte
        | FilterOp::Glob
        | FilterOp::Regex => CompareForm::Native(coerce_filter_value(literal)),
    }
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
    }
}

/// The BIGINT a BIGINT-pinned column CONFORMS a stored text to — the live
/// mirror of [`crate::conform::guarded_cast`]'s BIGINT rung, which is
/// `TRY_CAST` under a `DECIMAL(38,6)` round-trip guard.
///
/// The guard is the whole point: the cast alone ROUNDS (`'1.5'` → 2), so
/// the conformed reading exists only where the cast PRESERVED the value.
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
/// Residual: the cast's fractional rung reads through `f64`, so a
/// fractional text within a half-ULP of ±2^63 is refused where `DuckDB`'s
/// exact decimal rounding keeps it — invisible through the guard, which
/// rejects every fractional text anyway. Integer texts are exact across
/// the whole BIGINT range.
#[must_use]
pub fn conformed_bigint(text: &str) -> Option<i64> {
    let value = try_cast_bigint(text)?;
    // `dec(text) = dec(cast)`: NULL on either side is UNKNOWN, which the
    // CASE answers with NULL — so a text outside DECIMAL's domain has no
    // conformed reading even when the cast produced one.
    decimal_micros(text)
        .filter(|micros| *micros == i128::from(value) * 1_000_000)
        .map(|_| value)
}

/// The `DECIMAL(38,6)` reading `DuckDB` takes from a text, scaled by 10^6
/// — the live mirror of [`crate::conform::decimal_reading`], and so of
/// BOTH things that compare a text against a number:
/// [`conformed_bigint`]'s round-trip guard and the VARCHAR-pinned
/// comparison rungs ([`CompareForm::TextOrNumeric`],
/// [`CompareForm::NumericOnText`]).
///
/// Exact by construction (i128 over the digit string, never `f64`): that
/// is the point of the DECIMAL space, which stays exact across the whole
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
/// above 10^32 overflows the type and reads NULL. All probed by execution.
#[must_use]
pub fn decimal_micros(text: &str) -> Option<i128> {
    /// `DECIMAL(38,6)` holds magnitudes strictly below this, scaled.
    const LIMIT: i128 = 10i128.pow(38);

    let trimmed = text.trim_matches(is_c_space);
    let normalized = if trimmed.contains('_') {
        std::borrow::Cow::Owned(strip_digit_separators(trimmed)?)
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };
    let (negative, unsigned) = match normalized.as_bytes().first() {
        Some(b'+') => (false, &normalized[1..]),
        Some(b'-') => (true, &normalized[1..]),
        _ => (false, &normalized[..]),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        // Exponents beyond i32 are not typos to reject: they are simply
        // far outside the type, so they saturate the same way the
        // magnitude checks below already handle.
        Some(idx) => (
            &unsigned[..idx],
            unsigned[idx + 1..]
                .parse::<i32>()
                .ok()
                .filter(|_| !unsigned[idx + 1..].is_empty())?,
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

    // value = digits * 10^shift, so the scaled-by-10^6 target is
    // digits * 10^(shift + 6).
    let digits = format!("{int_part}{frac_part}");
    let digits = digits.trim_start_matches('0');
    let shift = i64::from(exponent) - i64::try_from(frac_part.len()).ok()? + 6;
    let magnitude = if digits.is_empty() {
        0
    } else if shift >= 0 {
        let width = i64::try_from(digits.len()).ok()? + shift;
        if width > 38 {
            return None;
        }
        digits.parse::<i128>().ok()? * 10i128.pow(u32::try_from(shift).ok()?)
    } else {
        let dropped = usize::try_from(-shift).ok()?;
        // A digit at or above 5 in the first dropped place rounds the
        // magnitude up — half away from zero, matching DuckDB.
        let round_up = |first_dropped: u8| i128::from(first_dropped >= b'5');
        match digits.len().checked_sub(dropped) {
            Some(0) | None => {
                // Every digit is below the microstep: the reading is 0
                // unless the very first one rounds it up to one step.
                if digits.len() == dropped {
                    round_up(digits.as_bytes()[0])
                } else {
                    0
                }
            }
            Some(kept_len) => {
                if kept_len > 38 {
                    return None;
                }
                digits[..kept_len].parse::<i128>().ok()? + round_up(digits.as_bytes()[kept_len])
            }
        }
    };
    if magnitude >= LIMIT {
        return None;
    }
    Some(if negative { -magnitude } else { magnitude })
}

/// The BIGINT `DuckDB` reads out of a stored text under
/// `TRY_CAST(col AS BIGINT)` — the unguarded cast, and deliberately NOT
/// `str::parse::<i64>`. [`conformed_bigint`] is what a pinned column
/// actually stores; this is the ingredient its guard filters.
///
/// `DuckDB`'s VARCHAR → BIGINT domain is wider than an integer parse in
/// four ways (all executed in `trawl-engine/tests/duckdb_probe.rs`):
///
/// - leading/trailing ASCII whitespace is trimmed and `_` separators are
///   accepted between digits, exactly as for DOUBLE ([`try_cast_double`]);
/// - `0x`/`0b` prefixes are read as hex/binary — but ONLY as an exact
///   prefix, so no sign and no surrounding whitespace (`' 0x10 '` and
///   `'-0x10'` are both NULL to `DuckDB`);
/// - fractional and exponent forms ROUND rather than fail, half AWAY FROM
///   ZERO (`'1.5'` → 2, `'2.5'` → 3, `'-2.5'` → -3);
/// - the result must fit BIGINT: an integer-syntax literal too large is
///   NULL, never a saturated approximation.
fn try_cast_bigint(text: &str) -> Option<i64> {
    // Radix prefixes bind on the RAW text: DuckDB reads '0x10' but not
    // ' 0x10 ', so trimming has to come after this.
    for (prefix, radix) in [("0x", 16u32), ("0X", 16), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            // `from_str_radix` would take a sign here; `DuckDB` does not.
            if digits.starts_with(['+', '-']) {
                return None;
            }
            return i64::from_str_radix(digits, radix).ok();
        }
    }
    let trimmed = text.trim_matches(is_c_space);
    let normalized = if trimmed.contains('_') {
        std::borrow::Cow::Owned(strip_digit_separators(trimmed)?)
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };
    if let Ok(i) = normalized.parse::<i64>() {
        return Some(i);
    }
    // Integer syntax `i64` cannot hold is out of BIGINT range, and the f64
    // rung below would answer with a saturated approximation instead of
    // the NULL `DuckDB` writes.
    if !normalized.contains(['.', 'e', 'E']) {
        return None;
    }
    bigint_in_range(normalized.parse::<f64>().ok()?.round())
}

/// A rounded double narrowed to BIGINT, or `None` outside its range.
/// ±2^63 is exactly representable, so the bounds are exact.
#[allow(clippy::cast_possible_truncation)] // guarded by the range check
fn bigint_in_range(rounded: f64) -> Option<i64> {
    (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
        .contains(&rounded)
        .then_some(rounded as i64)
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
/// Its remaining caller is the DOUBLE pin's PATTERN text
/// ([`PatternForm::DoubleText`]): what the conformed column holds, to be
/// rendered and globbed. Nothing COMPARES through it any more — that is
/// [`decimal_micros`]' job (ADR-0011 ruling #6).
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

    #[test]
    fn typed_pins_keep_native_comparisons() {
        for pin in TYPED_PINS {
            for (lit, native) in literal_shapes() {
                for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                    assert_eq!(
                        compare_form(Some(pin), *op, lit),
                        CompareForm::Native(native.clone()),
                        "{pin:?} {op:?} {lit:?}"
                    );
                }
            }
        }
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

    /// Every expectation here is the value `DuckDB`'s
    /// `TRY_CAST(v AS BIGINT)` returns for the same text — the UNGUARDED
    /// cast, which [`conformed_bigint`] then filters (executed side by
    /// side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn try_cast_bigint_mirrors_duckdb_cast_domain() {
        // Plain integers, including the leading-zero text whose stored
        // value is spelled differently — the reported divergence.
        assert_eq!(try_cast_bigint("404"), Some(404));
        assert_eq!(try_cast_bigint("0404"), Some(404));
        assert_eq!(try_cast_bigint("00200"), Some(200));
        assert_eq!(try_cast_bigint("+5"), Some(5));
        assert_eq!(try_cast_bigint("-0"), Some(0));
        assert_eq!(try_cast_bigint("9223372036854775807"), Some(i64::MAX));
        assert_eq!(try_cast_bigint("-9223372036854775808"), Some(i64::MIN));
        // Whitespace and `_` separators, as for the DOUBLE domain.
        assert_eq!(try_cast_bigint(" 404 "), Some(404));
        assert_eq!(try_cast_bigint("\t404\n"), Some(404));
        assert_eq!(try_cast_bigint("404_000"), Some(404_000));
        assert_eq!(try_cast_bigint("1e1_0"), Some(10_000_000_000));
        // Fractional / exponent texts ROUND, half AWAY FROM ZERO.
        assert_eq!(try_cast_bigint("1.5"), Some(2));
        assert_eq!(try_cast_bigint("1.4"), Some(1));
        assert_eq!(try_cast_bigint("2.5"), Some(3));
        assert_eq!(try_cast_bigint("-2.5"), Some(-3));
        assert_eq!(try_cast_bigint(".5"), Some(1));
        assert_eq!(try_cast_bigint("1e3"), Some(1000));
        assert_eq!(try_cast_bigint("1_0.5"), Some(11));
        // Radix prefixes bind on the RAW text: no sign, no whitespace.
        assert_eq!(try_cast_bigint("0x10"), Some(16));
        assert_eq!(try_cast_bigint("0B101"), Some(5));
        for text in ["-0x10", " 0x10 ", "0x+10", "0xzz", "0x1.5", "0o17"] {
            assert_eq!(try_cast_bigint(text), None, "{text:?}");
        }
        // No reading, and out of range — NULL, never a saturated value.
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
            assert_eq!(try_cast_bigint(text), None, "{text:?}");
        }
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
