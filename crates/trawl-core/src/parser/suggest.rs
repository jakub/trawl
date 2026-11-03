// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! "Did you mean?" suggestions, and the DSL field-name renderer.
//!
//! Levenshtein typo correction for pipe stage and function names, behind
//! the contextual hints in parse errors, plus [`quote_dsl_field`] — the
//! one renderer every surface that offers a field name goes through, so a
//! name trawl prints is a name trawl can parse back.

/// All recognized pipe stage keywords.
pub const KNOWN_PIPE_STAGES: &[&str] = &[
    "from",
    "stats",
    "timechart",
    "where",
    "sort",
    "limit",
    "head",
    "tail",
    "let",
    "eval",
    "extract",
    "rex",
    "table",
    "fields",
    "top",
    "rare",
    "dedup",
    "drop",
    "rename",
    "pivot",
    "sample",
    "eventstats",
];

/// Known function names — single source of truth, also used by emitter validation.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    // aggregates
    "count",
    "avg",
    "sum",
    "min",
    "max",
    "dc",
    "distinct_count",
    "p50",
    "p90",
    "p95",
    "p99",
    "first",
    "last",
    "values",
    "list",
    "median",
    "stddev",
    // scalars
    "lower",
    "upper",
    "length",
    "len",
    "coalesce",
    "if",
    "replace",
    "substr",
    "trim",
    "ltrim",
    "rtrim",
    "isnull",
    "isnotnull",
    "abs",
    "ceil",
    "ceiling",
    "floor",
    "round",
    "now",
    "typeof",
    "tonumber",
    "tostring",
    "sev",
    // string
    "contains",
    "startswith",
    "endswith",
    "split",
    "concat",
    // date/time
    "date_part",
    "date_trunc",
    "date_diff",
    "strftime",
    "strptime",
    // conditional
    "case",
    // json
    "json",
    "json_extract",
    "json_extract_string",
    "json_valid",
    "json_keys",
    "json_array_length",
];

/// The grammar's closed keyword set: three words the search stage reads
/// unconditionally, before any field filter (`last=2h`, `earliest="…"`,
/// `latest="…"`).
///
/// A field of one of these names is reachable only through backticks, so
/// [`quote_dsl_field`] always quotes them (ADR-0013 ruling 7).
pub const GRAMMAR_KEYWORDS: &[&str] = &["last", "earliest", "latest"];

/// The words an EXPRESSION position reads before it tries a field
/// reference: the literal words (`true`/`false`/`null`) and the operator
/// words (`and`/`or`/`not`/`in`/`matches`/`like`/`ilike`).
///
/// Bare, these do not name a column inside `| where` / `| let`: a field
/// called `true` renders as the boolean literal — a silent change of
/// meaning — and one called `not` turns the surrounding text into a
/// parse error. A rendered name must be pasteable in every field
/// position, so [`quote_dsl_field`] backticks the whole set, exactly as
/// it does [`GRAMMAR_KEYWORDS`].
pub const EXPRESSION_KEYWORDS: &[&str] = &[
    "true", "false", "null", "and", "or", "not", "in", "matches", "like", "ilike",
];

/// The SEARCH stage's group separator, in the two spellings the grammar
/// reads (`search.rs`: `keyword("OR").or(keyword("or"))`) before it tries
/// a field filter.
///
/// `or` is also an expression keyword; `OR` is reachable as a field name
/// only through backticks, and rendered bare it would not merely change
/// meaning — `OR=x` splits the search stage into two groups and leaves
/// the filter as a text search for `=x`, so the predicate vanishes.
pub const SEARCH_KEYWORDS: &[&str] = &["OR", "or"];

/// The keywords a parser tries at the START of a field reference's
/// position, before the field: `OR`/`or`/`NOT` at a search token start,
/// and `true`/`false`/`null`/`not` at an expression atom or a `not`
/// operand.
///
/// `keyword()` ends a match at any character that cannot continue an
/// identifier, and `.` is one, so these words claim the first dot
/// segment as well as the whole name: bare, `true.a` lexes as the literal
/// `true` followed by `.a`, and `OR.a=x` as the OR separator followed by
/// text. [`quote_dsl_field`] therefore backticks a name whose whole name
/// or first dot segment is one of these. An `@`-name is exempt: the
/// `@` is the first character, so no keyword can claim its start (but
/// see [`backtick_dsl_field`] for a bare `@`-name directly under `NOT`).
pub const FIELD_START_KEYWORDS: &[&str] = &["OR", "or", "NOT", "true", "false", "null", "not"];

/// The keywords the parser reads only in a fixed position AFTER some
/// other token, never where a field reference starts: the binary and
/// list operators, the clause words of individual stages, and the stage
/// names themselves (read right after `|`).
///
/// A field named `by.x` or `in.a` is not shadowed by these, so
/// [`quote_dsl_field`] does not quote them as a first segment. Whole
/// names that need quoting for another reason stay in
/// [`GRAMMAR_KEYWORDS`] and [`EXPRESSION_KEYWORDS`]. A drift-guard test
/// requires every `keyword("…")` in the parser to be in this list or in
/// [`FIELD_START_KEYWORDS`].
pub const POSITIONAL_KEYWORDS: &[&str] = &[
    // operators read after a left operand
    "and", "in", "matches", "like", "ilike", // stage clause words
    "by", "as", "on", "from", "sep", "kv", "span", "run", "saved", "all", "latest",
];

/// Whether `name` lexes as a BARE field name — the unquoted production
/// (`[A-Za-z_][A-Za-z0-9_]*`, dot-joined, optionally `@`-prefixed).
///
/// A pure char walk mirroring `primitives::ident`/`system_field`, pinned
/// against the real grammar by a drift-guard test.
#[must_use]
pub fn is_bare_field_name(name: &str) -> bool {
    let body = name.strip_prefix('@').unwrap_or(name);
    !body.is_empty() && body.split('.').all(is_bare_segment)
}

fn is_bare_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Render a field name as DSL text: bare when the bare production can
/// spell it and it is not a keyword of the grammar
/// ([`GRAMMAR_KEYWORDS`]), of the search stage's group separator
/// ([`SEARCH_KEYWORDS`]) or of an expression position
/// ([`EXPRESSION_KEYWORDS`]), and its first dot segment is not a
/// field-start keyword ([`FIELD_START_KEYWORDS`]); else backtick-quoted
/// with embedded backticks doubled (ADR-0013 ruling 7).
///
/// The one renderer every suggestion surface goes through — autocomplete
/// insertions and the query formatter — so a name trawl offers is a name
/// trawl can parse back.
///
/// Returns `None` when the field-name grammar cannot represent `name` at
/// all: the empty name, or one carrying a control/invisible display
/// character. Catalog-backed callers must decline those names rather than
/// manufacture invalid DSL or splice the raw client-chosen key.
#[must_use]
pub fn quote_dsl_field(name: &str) -> Option<String> {
    if is_bare_field_name(name)
        && !GRAMMAR_KEYWORDS.contains(&name)
        && !SEARCH_KEYWORDS.contains(&name)
        && !EXPRESSION_KEYWORDS.contains(&name)
        && !starts_with_field_start_keyword(name)
    {
        return Some(name.to_string());
    }
    backtick_dsl_field(name)
}

/// Whether a field-start keyword claims `name`'s whole name or its first
/// dot segment. `@`-names are exempt (see [`FIELD_START_KEYWORDS`]).
fn starts_with_field_start_keyword(name: &str) -> bool {
    if name.starts_with('@') {
        return false;
    }
    let first = name.split('.').next().unwrap_or(name);
    FIELD_START_KEYWORDS.contains(&first)
}

/// Render a field name backtick-quoted whatever its shape, with embedded
/// backticks doubled; `None` exactly when [`quote_dsl_field`] declines.
///
/// For a context that needs the quoted spelling even where the bare one
/// is a good field name. The search stage's `NOT` negates the next token
/// only when that token starts with an identifier character, a quote, a
/// `-`, a `(` or a backtick, so a bare `@`-name directly under `NOT`
/// (`NOT @host=x`) leaves `NOT` as a text search; `` NOT `@host`=x ``
/// negates the filter.
#[must_use]
pub fn backtick_dsl_field(name: &str) -> Option<String> {
    if name.is_empty() || name.chars().any(crate::sanitize::is_unsafe_display_char) {
        return None;
    }
    Some(format!("`{}`", name.replace('`', "``")))
}

/// Compute the Levenshtein edit distance between two strings.
///
/// Two rows rather than the full matrix, so the working space is
/// proportional to `b` alone.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (a_len, b_len) = (a_chars.len(), b_chars.len());

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = usize::from(a_chars[i - 1] != b_chars[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

/// Find the closest match in `candidates` for `input`, within `max_distance`.
///
/// Returns `None` if no candidate is within range or if the input is empty.
pub fn suggest_closest<'a>(
    input: &str,
    candidates: &[&'a str],
    max_distance: usize,
) -> Option<&'a str> {
    if input.is_empty() {
        return None;
    }

    let input_lower = input.to_lowercase();
    let mut best: Option<(&str, usize)> = None;

    for &candidate in candidates {
        let dist = levenshtein(&input_lower, candidate);
        if dist <= max_distance && best.as_ref().is_none_or(|(_, d)| dist < *d) {
            best = Some((candidate, dist));
        }
    }

    best.map(|(name, _)| name)
}

/// Suggest a pipe stage name for a typo.
pub fn suggest_pipe_stage(input: &str) -> Option<&'static str> {
    suggest_closest(input, KNOWN_PIPE_STAGES, 2)
}

/// Suggest a function name for a typo.
pub fn suggest_function(input: &str) -> Option<&'static str> {
    suggest_closest(input, KNOWN_FUNCTIONS, 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DSL field-name quoting (ADR-0013 ruling 7) ─────────────────────

    #[test]
    fn a_bare_lexable_name_renders_bare() {
        for name in [
            "host",
            "host_name",
            "host.name",
            "@timestamp",
            "_time",
            "a1",
        ] {
            assert_eq!(quote_dsl_field(name).as_deref(), Some(name), "{name}");
        }
    }

    #[test]
    fn a_name_the_bare_production_cannot_spell_is_backticked() {
        assert_eq!(
            quote_dsl_field("x-request-id").as_deref(),
            Some("`x-request-id`")
        );
        assert_eq!(
            quote_dsl_field("request id").as_deref(),
            Some("`request id`")
        );
        assert_eq!(quote_dsl_field("1st").as_deref(), Some("`1st`"));
        assert_eq!(quote_dsl_field("über").as_deref(), Some("`über`"));
        assert_eq!(quote_dsl_field("trailing.").as_deref(), Some("`trailing.`"));
    }

    /// The closed keyword set is three, and the rule is uniform: a
    /// rendered name is pasteable in every position, so these are always
    /// quoted even where a bare spelling would happen to parse.
    #[test]
    fn the_three_grammar_keywords_are_always_quoted() {
        assert_eq!(GRAMMAR_KEYWORDS, ["last", "earliest", "latest"]);
        for kw in GRAMMAR_KEYWORDS {
            assert_eq!(quote_dsl_field(kw), Some(format!("`{kw}`")));
        }
    }

    /// An expression position reads its keywords BEFORE it tries a field
    /// reference, so a bare rendering there is either a silent change of
    /// meaning (`true`, `false`, `null` lex as literals) or a parse error
    /// (`not`). Every one of them round-trips as a field reference only
    /// through backticks.
    #[test]
    fn expression_keywords_round_trip_in_expression_positions() {
        use crate::ast::{Expr, PipeStage};

        for kw in EXPRESSION_KEYWORDS {
            let rendered = quote_dsl_field(kw).expect("keyword is representable");
            assert_eq!(rendered, format!("`{kw}`"), "{kw} must be quoted");

            for query in [
                format!("| where {rendered} == 1"),
                format!("| let x = {rendered}"),
                format!("| where a == 1 and {rendered} == 2"),
            ] {
                let parsed = crate::parser::parse(&query)
                    .unwrap_or_else(|e| panic!("{query} must parse: {e:?}"));
                let names = crate::field_refs::referenced_fields(&parsed);
                assert!(
                    names.contains(*kw),
                    "{query} must bind the field {kw:?}, bound {names:?}"
                );
            }

            // …and the same name bare does NOT reach the AST as a field.
            let bare = crate::parser::parse(&format!("| where {kw} == 1"));
            let bare_is_field = bare.is_ok_and(|q| match &q.pipeline[0].node {
                PipeStage::Where(w) => matches!(
                    &w.condition.node,
                    Expr::Binary { lhs, .. } if lhs.node == Expr::FieldRef((*kw).to_string())
                ),
                _ => false,
            });
            assert_eq!(
                bare_is_field,
                !matches!(*kw, "true" | "false" | "null" | "not"),
                "{kw:?}: expression-keyword hazard drifted from the grammar"
            );
        }
    }

    /// The search stage reads its group separator in TWO spellings before
    /// it tries a field filter, so both are quoted. Rendered bare, `OR=x`
    /// is not a changed predicate but a VANISHED one: an OR separator
    /// followed by a text search for `=x`.
    #[test]
    fn the_or_separator_round_trips_in_the_search_stage() {
        use crate::ast::SearchToken;

        for kw in SEARCH_KEYWORDS {
            let rendered = quote_dsl_field(kw).expect("keyword is representable");
            assert_eq!(rendered, format!("`{kw}`"), "{kw} must be quoted");

            let parsed = crate::parser::parse(&format!("{rendered}=x"))
                .unwrap_or_else(|e| panic!("{rendered}=x must parse: {e:?}"));
            let groups = &parsed.search.groups;
            assert_eq!(groups.len(), 1, "{kw:?}: {rendered}=x must be one group");
            match &groups[0][..] {
                [term] => match &term.node {
                    SearchToken::FieldFilter(ff) => assert_eq!(&ff.field, kw),
                    other => panic!("{kw:?}: expected a field filter, got {other:?}"),
                },
                other => panic!("{kw:?}: expected one term, got {other:?}"),
            }

            // …and bare, the same text is the separator: the filter is
            // gone entirely, not merely reinterpreted.
            let bare = crate::parser::parse(&format!("{kw}=x"))
                .unwrap_or_else(|e| panic!("{kw}=x must parse: {e:?}"));
            assert!(
                !bare.search.all_tokens().any(|t| matches!(
                    &t.node,
                    SearchToken::FieldFilter(ff) if ff.field == *kw
                )),
                "{kw:?}: separator hazard drifted from the grammar"
            );
        }
    }

    /// A field-start keyword claims the first dot segment of a bare name,
    /// so `quote_dsl_field` quotes it there as well as whole. The words
    /// are written out, not read from [`FIELD_START_KEYWORDS`], so
    /// dropping one from the constant fails here.
    #[test]
    fn every_field_start_keyword_is_quoted_as_a_first_segment() {
        assert_eq!(
            FIELD_START_KEYWORDS,
            ["OR", "or", "NOT", "true", "false", "null", "not"]
        );
        for kw in ["OR", "or", "NOT", "true", "false", "null", "not"] {
            for name in [kw.to_string(), format!("{kw}.a"), format!("{kw}.a.b")] {
                assert_eq!(
                    quote_dsl_field(&name),
                    Some(format!("`{name}`")),
                    "{name:?} must be quoted"
                );
            }
            // the keyword must be the whole first segment…
            for name in [format!("{kw}x.a"), format!("{kw}_x.a"), format!("a.{kw}")] {
                assert_eq!(
                    quote_dsl_field(&name).as_deref(),
                    Some(name.as_str()),
                    "{name:?}"
                );
            }
            // …and an `@`-name is exempt from the first-segment rule.
            let at = format!("@{kw}.a");
            assert_eq!(quote_dsl_field(&at).as_deref(), Some(at.as_str()), "{at:?}");
        }
    }

    #[test]
    fn backtick_dsl_field_always_quotes_and_declines_like_quote_dsl_field() {
        use crate::ast::SearchToken;

        assert_eq!(backtick_dsl_field("host").as_deref(), Some("`host`"));
        assert_eq!(backtick_dsl_field("@host").as_deref(), Some("`@host`"));
        assert_eq!(backtick_dsl_field("a`b").as_deref(), Some("`a``b`"));
        for name in ["", "a\u{1b}b", "a\u{200b}b"] {
            assert_eq!(backtick_dsl_field(name), None, "{name:?}");
        }

        // Bare under NOT, an `@`-name leaves NOT as text; backticked, the
        // filter is negated.
        let negated = |q: &str| {
            let parsed = crate::parser::parse(q).unwrap_or_else(|e| panic!("{q}: {e:?}"));
            parsed.search.all_tokens().any(|t| {
                matches!(&t.node, SearchToken::Not(inner)
                    if matches!(&inner.node, SearchToken::FieldFilter(f) if f.field == "@host"))
            })
        };
        assert!(
            !negated("NOT @host=x"),
            "the hazard drifted from the grammar"
        );
        let quoted = backtick_dsl_field("@host").unwrap();
        assert!(negated(&format!("NOT {quoted}=x")));
    }

    /// The parser sources the keyword guard reads. A `read_dir` check
    /// keeps the list complete: a new parser file must join it.
    const PARSER_SOURCES: &[(&str, &str)] = &[
        ("comment.rs", include_str!("comment.rs")),
        ("expr.rs", include_str!("expr.rs")),
        ("mod.rs", include_str!("mod.rs")),
        ("pipe.rs", include_str!("pipe.rs")),
        ("primitives.rs", include_str!("primitives.rs")),
        ("scan.rs", include_str!("scan.rs")),
        ("search.rs", include_str!("search.rs")),
    ];

    /// `suggest.rs` holds no grammar; its tests spell `keyword(` in
    /// strings, so the guard does not read itself.
    const UNSCANNED_PARSER_SOURCES: &[&str] = &["suggest.rs"];

    /// The `keyword(<variable>)` calls the guard allows, each with the
    /// keywords the variable can hold. `extract_like_stage(kw)` is called
    /// with the two stage names it parses.
    const NON_LITERAL_KEYWORD_CALLS: &[(&str, &str, &[&str])] =
        &[("pipe.rs", "kw", &["extract", "rex"])];

    /// Every `keyword(` call in the source, as its argument: `Ok(word)`
    /// for a string literal, `Err(expression)` for anything else. Lines
    /// that are `//` comments are skipped.
    fn keyword_calls(source: &str) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for line in source.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(at) = rest.find("keyword(") {
                let before = rest[..at].chars().next_back();
                let after = &rest[at + "keyword(".len()..];
                rest = after;
                if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    continue; // another function's name, e.g. `is_keyword(`
                }
                if let Some(lit) = after.strip_prefix('"') {
                    let end = lit.find('"').expect("a keyword literal closes on its line");
                    out.push(Ok(lit[..end].to_string()));
                } else {
                    let end = after.find(')').unwrap_or(after.len());
                    out.push(Err(after[..end].trim().to_string()));
                }
            }
        }
        out
    }

    /// The drift guard for [`FIELD_START_KEYWORDS`]: every keyword the
    /// parser matches is either quoted as a first segment or named
    /// positional. A new `keyword("x")` forces a decision about whether
    /// `x.a` still renders bare.
    #[test]
    fn parser_keywords_are_quoted_or_positional() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/parser");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("the parser directory is readable")
            .map(|e| {
                e.expect("a directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        on_disk.sort();
        let mut listed: Vec<String> = PARSER_SOURCES
            .iter()
            .map(|(name, _)| (*name).to_string())
            .chain(UNSCANNED_PARSER_SOURCES.iter().map(|n| (*n).to_string()))
            .collect();
        listed.sort();
        assert_eq!(
            on_disk, listed,
            "a parser file is missing from PARSER_SOURCES"
        );

        let classified = |word: &str| {
            FIELD_START_KEYWORDS.contains(&word)
                || POSITIONAL_KEYWORDS.contains(&word)
                || KNOWN_PIPE_STAGES.contains(&word)
        };
        let mut seen = 0;
        for (file, source) in PARSER_SOURCES {
            for call in keyword_calls(source) {
                seen += 1;
                match call {
                    Ok(word) => assert!(
                        classified(&word),
                        "{file}: keyword({word:?}) is in neither FIELD_START_KEYWORDS \
                         nor POSITIONAL_KEYWORDS nor KNOWN_PIPE_STAGES"
                    ),
                    Err(arg) => {
                        let allowed = NON_LITERAL_KEYWORD_CALLS
                            .iter()
                            .find(|(f, a, _)| f == file && *a == arg)
                            .unwrap_or_else(|| {
                                panic!(
                                    "{file}: keyword({arg}) is not a literal and not allowlisted"
                                )
                            });
                        for word in allowed.2 {
                            assert!(classified(word), "{file}: keyword({arg}) may hold {word:?}");
                        }
                    }
                }
            }
        }
        // the scanner found the grammar: a broken scanner passes vacuously.
        assert!(seen > 40, "only {seen} keyword( calls found");
    }

    #[test]
    fn the_keyword_scanner_reads_literals_and_variables() {
        let src = "let a = keyword(\"OR\").or(keyword(\"or\"));\n\
                   // keyword(\"commented\")\n\
                   let b = keyword(kw).then(is_keyword(x));";
        assert_eq!(
            keyword_calls(src),
            vec![Ok("OR".into()), Ok("or".into()), Err("kw".into())]
        );
    }

    #[test]
    fn an_embedded_backtick_is_doubled() {
        assert_eq!(quote_dsl_field("a`b").as_deref(), Some("`a``b`"));
    }

    #[test]
    fn an_inexpressible_catalog_name_is_declined() {
        for name in ["", "a\u{1b}b", "a\u{200b}b", "a\u{202e}b", "a\u{00ad}b"] {
            assert_eq!(quote_dsl_field(name), None, "{name:?}");
        }
    }

    /// The drift guard: `is_bare_field_name` claims to mirror the bare
    /// production, and `quote_dsl_field` claims its output parses back to
    /// the same name. Both claims are checked against the REAL parser
    /// over a hostile corpus — a grammar change that isn't mirrored here
    /// fails loudly instead of emitting suggestions that don't parse.
    #[test]
    fn quoting_agrees_with_the_real_grammar() {
        for name in [
            "host",
            "host.name",
            "@timestamp",
            "_time",
            "x-request-id",
            "request id",
            "last",
            "earliest",
            "latest",
            "where",
            "Dur",
            "a`b",
            "a#b",
            "a//b",
            "1st",
            "über",
            "a.b.c",
            "trailing.",
            "with\"quote",
            "with'quote",
            "count",
            "|pipe",
        ] {
            let rendered = quote_dsl_field(name).expect("fixture name is representable");
            let query = crate::parser::parse(&format!("| table {rendered}"))
                .unwrap_or_else(|e| panic!("{name:?} rendered as {rendered} must parse: {e:?}"));
            match &query.pipeline[0].node {
                crate::ast::PipeStage::Table(t) => assert_eq!(
                    t.fields,
                    vec![name.to_string()],
                    "{name:?} rendered as {rendered} must round-trip"
                ),
                other => panic!("expected Table, got {other:?}"),
            }

            // …and the bare claim is exactly the bare production, keyword
            // exclusion aside.
            let bare_parses = crate::parser::parse(&format!("| table {name}"))
                .ok()
                .and_then(|q| match &q.pipeline[0].node {
                    crate::ast::PipeStage::Table(t) => Some(t.fields == vec![name.to_string()]),
                    _ => None,
                })
                .unwrap_or(false);
            assert_eq!(
                is_bare_field_name(name),
                bare_parses,
                "{name:?}: is_bare_field_name drifted from the grammar"
            );
        }
    }

    // ── the round trip, over every field position ──────────────────────

    /// Names chosen to break a naive helper: every grammar keyword and
    /// literal, the three unconditional search spellings, stage names,
    /// and the shapes only backticks can express.
    ///
    /// The keyword half is written out here rather than read from
    /// [`GRAMMAR_KEYWORDS`]/[`SEARCH_KEYWORDS`]/[`EXPRESSION_KEYWORDS`] on
    /// purpose: drawing the corpus from the constants under test would
    /// make dropping a keyword invisible, since the name would leave the
    /// corpus with it. This list comes from grepping `keyword(...)` and
    /// `just("...=")` out of the grammar.
    fn adversarial_names() -> Vec<&'static str> {
        vec![
            // shapes the bare grammar cannot express at all
            "request id",
            "http-status",
            "x-request-id",
            "a`b",
            "a\"b",
            "a'b",
            "a b`c",
            "日本語",
            "über",
            "2fast",
            "1st",
            "a-b.c",
            "a.b.c",
            "trailing.",
            " ",
            "a=b",
            "a,b",
            "a|b",
            "a#b",
            "a//b",
            "http://x",
            "(a)",
            "*",
            // shapes it can — including the ones the FOLD makes equal
            "host",
            "host.name",
            "@timestamp",
            "_time",
            "Dur",
            "dur",
            "count",
            "stats",
            "where",
            "table",
            "sort",
            "limit",
            "eventstats",
            "timechart",
            // every keyword spelling the grammar matches
            "true",
            "false",
            "null",
            "and",
            "or",
            "not",
            "OR",
            "NOT",
            "in",
            "matches",
            "like",
            "ilike",
            "as",
            "by",
            "on",
            "from",
            "sep",
            "kv",
            "span",
            "run",
            "saved",
            "all",
            "last",
            "earliest",
            "latest",
            "head",
            "tail",
            "drop",
            "dedup",
            "rare",
            "top",
            "let",
            "eval",
            "rename",
            "pivot",
            "sample",
            "fields",
            // a field-start keyword as the first dot segment
            "OR.a",
            "or.a.b",
            "NOT.a",
            "not.a",
            "true.a",
            "false.a",
            "null.a",
            "by.a",
            "in.a",
        ]
    }

    /// A field position for each door the helper's output must survive.
    /// Read positions only: a write position adds the sealed-prefix
    /// policy, which is not a quoting question.
    ///
    /// The `NOT` position takes the backticked spelling of an `@`-name:
    /// the one context [`backtick_dsl_field`] exists for.
    fn read_positions(rendered: &str) -> Vec<String> {
        let under_not = if rendered.starts_with('@') {
            backtick_dsl_field(rendered).expect("a rendered name is representable")
        } else {
            rendered.to_string()
        };
        vec![
            format!("{rendered}=1"),
            format!("NOT {under_not}=1"),
            format!("* | where {rendered} == 1"),
            format!("* | stats count() by {rendered}"),
            format!("* | table {rendered}"),
            format!("* | sort -{rendered}"),
            format!("* | top 5 {rendered}"),
        ]
    }

    fn walk_expr(expr: &crate::ast::Expr, out: &mut Vec<String>) {
        use crate::ast::Expr;
        match expr {
            Expr::FieldRef(name) => out.push(name.clone()),
            Expr::FunctionCall { args, .. } => {
                for a in args {
                    walk_expr(&a.node, out);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk_expr(&lhs.node, out);
                walk_expr(&rhs.node, out);
            }
            Expr::Unary { operand, .. } => walk_expr(&operand.node, out),
            Expr::InList { expr, list } => {
                walk_expr(&expr.node, out);
                for item in list {
                    walk_expr(&item.node, out);
                }
            }
            Expr::Literal(_) => {}
        }
    }

    fn walk_token(token: &crate::ast::SearchToken, out: &mut Vec<String>) {
        use crate::ast::SearchToken;
        match token {
            SearchToken::FieldFilter(f) => out.push(f.field.clone()),
            SearchToken::Not(inner) => walk_token(&inner.node, out),
            SearchToken::Group(groups) => {
                for group in groups {
                    for t in group {
                        walk_token(&t.node, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn walk_aggs(aggs: &[crate::ast::AggExpr], out: &mut Vec<String>) {
        for agg in aggs {
            for a in &agg.args {
                walk_expr(&a.node, out);
            }
            if let Some(alias) = &agg.alias {
                out.push(alias.clone());
            }
        }
    }

    /// Every name the query carries in a field position, whatever door it
    /// came through.
    fn field_positions(query: &crate::ast::Query) -> Vec<String> {
        use crate::ast::PipeStage;
        let mut out = Vec::new();
        for group in &query.search.groups {
            for token in group {
                walk_token(&token.node, &mut out);
            }
        }
        for stage in &query.pipeline {
            match &stage.node {
                PipeStage::Stats(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::EventStats(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::Timechart(s) => {
                    walk_aggs(&s.aggregations, &mut out);
                    out.extend(s.group_by.iter().cloned());
                }
                PipeStage::Pivot(s) => {
                    walk_aggs(std::slice::from_ref(&s.aggregation), &mut out);
                    out.push(s.on_field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Where(s) => walk_expr(&s.condition.node, &mut out),
                PipeStage::Let(s) => {
                    for (target, value) in &s.assignments {
                        out.push(target.clone());
                        walk_expr(&value.node, &mut out);
                    }
                }
                PipeStage::Sort(s) => out.extend(s.fields.iter().map(|f| f.field.clone())),
                PipeStage::Table(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Drop(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Dedup(s) => out.extend(s.fields.iter().cloned()),
                PipeStage::Top(s) => {
                    out.push(s.field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Rare(s) => {
                    out.push(s.field.clone());
                    out.extend(s.by.iter().cloned());
                }
                PipeStage::Rename(s) => {
                    for (from, to) in &s.renames {
                        out.push(from.clone());
                        out.push(to.clone());
                    }
                }
                PipeStage::Extract(s) => out.extend(s.source_field.iter().cloned()),
                _ => {}
            }
        }
        out
    }

    /// The helper is the grammar's inverse: whatever it renders parses
    /// back to the name it was given, in EVERY field position — that is
    /// the contract every suggestion surface leans on, and one position
    /// cannot stand in for the rest (`| sort -{name}` reads the tick
    /// after an operator, `{name}=1` reads it at a token start).
    #[test]
    fn quote_dsl_field_round_trips_in_every_position() {
        for original in adversarial_names() {
            let Some(rendered) = quote_dsl_field(original) else {
                // refused names are the ones the grammar cannot express;
                // a caller declines to offer them rather than inventing one.
                panic!("{original:?} was refused but is expressible");
            };
            for dsl in read_positions(&rendered) {
                let query = crate::parser::parse(&dsl)
                    .unwrap_or_else(|e| panic!("{original:?} rendered as {dsl:?}: {e:?}"));
                assert!(
                    field_positions(&query).iter().any(|n| n == original),
                    "{original:?} rendered as {dsl:?} parsed as {:?}",
                    field_positions(&query)
                );
            }
        }
    }

    /// The half a forgotten keyword breaks: when the helper renders a name
    /// bare, the bare spelling must mean that name in every position.
    /// `true` and `last` are good identifiers that some position reads as
    /// something else first — the boolean literal inside `| where`, the
    /// time keyword in the search stage — which is why `quote_dsl_field`
    /// backticks them even though [`is_bare_field_name`] calls them
    /// lexable.
    #[test]
    fn a_bare_rendering_means_that_name_in_every_position() {
        for original in adversarial_names() {
            if quote_dsl_field(original).as_deref() != Some(original) {
                continue;
            }
            for dsl in read_positions(original) {
                let query = crate::parser::parse(&dsl)
                    .unwrap_or_else(|e| panic!("{original:?} rendered bare: {dsl:?}: {e:?}"));
                assert!(
                    field_positions(&query).iter().any(|n| n == original),
                    "{original:?} rendered bare but {dsl:?} parsed as {:?}",
                    field_positions(&query)
                );
            }
        }
    }

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("stats", "stats"), 0);
    }

    #[test]
    fn levenshtein_one_edit() {
        assert_eq!(levenshtein("stats", "staats"), 1);
        assert_eq!(levenshtein("stats", "stat"), 1);
        assert_eq!(levenshtein("stats", "statx"), 1);
    }

    #[test]
    fn levenshtein_two_edits() {
        assert_eq!(levenshtein("stats", "staatz"), 2);
    }

    #[test]
    fn levenshtein_empty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn suggest_pipe_stage_typo() {
        assert_eq!(suggest_pipe_stage("staats"), Some("stats"));
        assert_eq!(suggest_pipe_stage("whre"), Some("where"));
        assert_eq!(suggest_pipe_stage("sortt"), Some("sort"));
        assert_eq!(suggest_pipe_stage("limt"), Some("limit"));
    }

    #[test]
    fn suggest_pipe_stage_exact() {
        assert_eq!(suggest_pipe_stage("stats"), Some("stats"));
    }

    #[test]
    fn suggest_pipe_stage_no_match() {
        assert_eq!(suggest_pipe_stage("zzzzzzz"), None);
        assert_eq!(suggest_pipe_stage(""), None);
    }

    #[test]
    fn suggest_function_typo() {
        assert_eq!(suggest_function("countt"), Some("count"));
        assert_eq!(suggest_function("avgg"), Some("avg"));
        assert_eq!(suggest_function("summ"), Some("sum"));
    }

    #[test]
    fn suggest_function_no_match() {
        assert_eq!(suggest_function("totallyunknown"), None);
    }

    #[test]
    fn suggest_case_insensitive() {
        assert_eq!(suggest_pipe_stage("STATS"), Some("stats"));
        assert_eq!(suggest_pipe_stage("Stats"), Some("stats"));
    }

    /// Ensures every entry in `KNOWN_PIPE_STAGES` actually parses as a valid
    /// pipe stage. Catches drift between this list and the parser's `choice()`.
    #[test]
    fn known_pipe_stages_all_parse() {
        for &stage in KNOWN_PIPE_STAGES {
            // Build a minimal valid query for each stage.
            let query = match stage {
                "stats" => "| stats count()".to_string(),
                "timechart" => "| timechart span=1h count()".to_string(),
                "where" => "| where x > 1".to_string(),
                "sort" => "| sort host".to_string(),
                "limit" | "head" | "tail" => format!("| {stage} 10"),
                "let" => "| let x = 1".to_string(),
                "eval" => "| eval x = 1".to_string(),
                "extract" => r#"| extract "(?P<ip>\d+)" from message"#.to_string(),
                "rex" => r#"| rex "(?P<ip>\d+)" from message"#.to_string(),
                "table" | "fields" | "drop" => format!("| {stage} host"),
                "top" => "| top 5 host".to_string(),
                "rare" => "| rare 5 host".to_string(),
                "dedup" => "| dedup host".to_string(),
                "rename" => "| rename host as hostname".to_string(),
                "pivot" => "| pivot count() on status".to_string(),
                "sample" => "| sample 10%".to_string(),
                "eventstats" => "| eventstats count()".to_string(),
                "from" => "| from saved daily_errors".to_string(),
                _ => panic!("unhandled stage '{stage}' in test — add a case"),
            };
            assert!(
                crate::parser::parse(&query).is_ok(),
                "KNOWN_PIPE_STAGES entry '{stage}' failed to parse with query: {query}"
            );
        }
    }

    /// Ensures every entry in `KNOWN_FUNCTIONS` is recognized by the emitter
    /// (catches drift between the list and the emitter's match arms).
    #[test]
    fn known_functions_all_validate() {
        for &func in KNOWN_FUNCTIONS {
            // Build a query that uses the function in a stats stage.
            // Functions with 0 args: count, now. Others: pass one dummy arg.
            let query = match func {
                "count" | "now" => format!("| stats {func}()"),
                "if" | "replace" => format!("| let x = {func}(a, b, c)"),
                "coalesce" | "concat" => format!("| let x = {func}(a, b)"),
                "substr"
                | "contains"
                | "startswith"
                | "endswith"
                | "date_part"
                | "date_trunc"
                | "strftime"
                | "strptime"
                | "json"
                | "json_extract_string"
                | "json_extract" => {
                    format!(r#"| let x = {func}(a, "b")"#)
                }
                "split" | "date_diff" => format!(r#"| let x = {func}(a, "b", 1)"#),
                "case" => format!(r#"| let x = {func}(a > 1, "yes", "no")"#),
                "round" => format!("| let x = {func}(a)"),
                // Aggregates use stats, scalars use let/eval
                _ if crate::emitter::is_aggregate_function(func) => {
                    format!("| stats {func}(x)")
                }
                _ => format!("| let y = {func}(x)"),
            };
            let ast = crate::parser::parse(&query)
                .unwrap_or_else(|e| panic!("'{func}' failed to parse: {e:?}"));
            let result = crate::emitter::validate_pipeline(&ast.pipeline);
            assert!(
                !matches!(
                    result,
                    Err(crate::emitter::EmitError::UnknownFunction { .. })
                ),
                "KNOWN_FUNCTIONS entry '{func}' rejected as unknown by emitter"
            );
        }
    }
}
