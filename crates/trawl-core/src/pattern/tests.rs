// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::path::Path;

use super::*;
use crate::parser;

/// What a construction site spells. Each needle is split with `concat!`
/// so this file's own text never matches it, and the scan below reads
/// this file like any other.
const REGEX_NEW: &str = concat!("Regex", "::new");
const BUILDER_NEW: &str = concat!("RegexBuilder", "::new");
const SET_NEW: &str = concat!("RegexSet", "::new");
const SET_BUILDER_NEW: &str = concat!("RegexSetBuilder", "::new");
const FROM_STR: &str = concat!("Regex", "::from_str");
const TRY_FROM: &str = concat!("Regex", "::try_from");
const AUTOMATA: &str = concat!("regex_", "automata::");
const PREPARER_NEW: &str = concat!("Preparer", "::new");
/// `parse::<…Regex…>`, counted by [`turbofish_regex_parses`].
const PARSE_TURBOFISH: &str = concat!("parse", "::<…Regex…>");

const NEEDLES: [&str; 8] = [
    REGEX_NEW,
    BUILDER_NEW,
    SET_NEW,
    SET_BUILDER_NEW,
    FROM_STR,
    TRY_FROM,
    AUTOMATA,
    PREPARER_NEW,
];

/// Every place in the workspace that builds a regex or opens a ledger,
/// by file and spelling.
///
/// The clippy `disallowed-methods` rule bans the builders outside
/// `pattern/mod.rs`, which holds the one build. It cannot see `FromStr`
/// or `TryFrom`, and a `Preparer` is legal anywhere, so this list is what
/// shows where ledgers begin: a ledger opened per event or per call
/// builds again each time. The server's gate test checks a regex that
/// lives in Vector config, not query text, so it builds its own under an
/// `#[expect]` of its own.
const INVENTORY: &[(&str, &str, usize)] = &[
    ("crates/trawl-core/src/emitter/pipeline.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/emitter/validate.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/eval.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/filter.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/parser/mod.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-core/src/parser/primitives.rs",
        PREPARER_NEW,
        1,
    ),
    ("crates/trawl-core/src/pattern/mod.rs", BUILDER_NEW, 1),
    ("crates/trawl-core/src/pattern/tests.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/pin_scope.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/stream.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-server/tests/vector_service_gate_contract.rs",
        REGEX_NEW,
        2,
    ),
];

/// The workspace root: two levels above this crate's manifest.
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("trawl-core sits two levels under the workspace root")
}

/// Every `.rs` file under `dir`, skipping build output, vendored
/// JavaScript and dot directories (`.git`, other worktrees, scratch).
fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// How many `parse::<T>` turbofishes on `line` name a `Regex` in `T`.
fn turbofish_regex_parses(line: &str) -> usize {
    line.match_indices(concat!("parse", "::<"))
        .filter(|(at, opener)| {
            let generic = &line[at + opener.len()..];
            generic[..generic.find('>').unwrap_or(generic.len())].contains("Regex")
        })
        .count()
}

/// The construction sites in the workspace sources: `src`, `tests`,
/// `benches`, `examples`, build scripts, `xtask`, the fuzz targets and
/// scripts, comment lines excluded. Any of them builds through a crate
/// the lint does not cover the moment it is compiled, so the walk takes
/// every Rust file there is.
#[test]
fn construction_inventory() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(root, &mut files);
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("crates/trawl-core/src/pattern/mod.rs")),
        "the walk reaches the owner"
    );

    let mut found: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let rel = file
            .strip_prefix(root)
            .expect("under the root")
            .to_string_lossy()
            .replace('\\', "/");
        for line in text.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for needle in NEEDLES {
                let n = line.matches(needle).count();
                if n > 0 {
                    *found.entry((rel.clone(), needle)).or_default() += n;
                }
            }
            let n = turbofish_regex_parses(line);
            if n > 0 {
                *found.entry((rel.clone(), PARSE_TURBOFISH)).or_default() += n;
            }
        }
    }

    let expected: BTreeMap<(String, &str), usize> = INVENTORY
        .iter()
        .map(|&(file, needle, n)| ((file.to_string(), needle), n))
        .collect();
    let report = |map: &BTreeMap<(String, &str), usize>| {
        map.iter()
            .map(|((file, needle), n)| format!("  {file}: {needle} x{n}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        found == expected,
        "the regex construction sites changed. A new site builds outside \
         the owner, or a ledger moved; route it through \
         crate::pattern::Preparer, then update INVENTORY.\nfound:\n{}\n\
         expected:\n{}",
        report(&found),
        report(&expected)
    );
}

/// A query of `MAX_QUERY_PATTERNS` regexes that each compile past the
/// size limit costs one failed build, not one per site, and still
/// reports the size error over the first.
///
/// The same body at every site builds once because the ledger
/// remembers it, and each site reports it. Distinct bodies show the
/// stop: after the first fails for size, the rest are never built, so
/// only the first is reported.
#[test]
fn a_failed_build_stops_construction() {
    let n = parser::MAX_QUERY_PATTERNS;
    let same = vec![r"/\w{100}/".to_string(); n];
    // Each distinct body is `\w{100}` and a numeral, short of the repeat
    // budget, so it reaches the build that fails.
    let distinct: Vec<String> = (0..n).map(|i| format!(r"/\w{{100}}{i}/")).collect();
    for (label, sites, reported) in [("same", same, 8), ("distinct", distinct, 1)] {
        let input = format!("f={}", sites.join(","));
        assert!(input.len() <= parser::MAX_QUERY_LEN, "{label}");

        let before = thread_builds();
        let errors = parser::parse(&input).expect_err(label);
        assert_eq!(thread_builds() - before, 1, "{label}: one failed build");

        assert_eq!(errors.len(), reported, "{label}: {errors:?}");
        assert!(
            errors[0].message.starts_with(MSG_REGEX_TOO_LARGE),
            "{label}: {errors:?}"
        );
        let first = "f=".len()..("f=".len() + sites[0].len());
        assert_eq!(errors[0].span, first, "{label}");
    }
}

/// `parse(input)`, and how many regexes it built.
fn parse_counting_builds(
    input: &str,
) -> (Result<crate::ast::Query, Vec<parser::ParseError>>, usize) {
    let before = thread_builds();
    let result = parser::parse(input);
    (result, thread_builds() - before)
}

/// A search list of `copies` copies of the regex `body`.
fn copies_of(body: &str, copies: u64) -> String {
    let element = format!("/{body}/");
    let copies = usize::try_from(copies).expect("a small count");
    format!("f={}", vec![element.as_str(); copies].join(","))
}

/// Every pattern ADR-0063 calibrates lands where the calibration put it
/// once the parser reads it: the translation budget refuses the measured
/// attacks, summed over the query, with nothing built, and admits the
/// ordinary patterns, the `log_regexes` corpus and every regex in
/// `dsl.md`.
///
/// The attacks: 63 bodies of `(?i)` and `\p{Any}` repeated, 64 KiB of
/// `(?i)\p{Ll}` and of `\w`, full-range brackets, and 63 bodies of 40
/// `(?i)[^\x00-\x{10FFFE}]`, a negated range whose complement is one
/// character. Each is as many bodies as the query cap holds.
///
/// A body both budgets admit is built, and the compiled-size limit can
/// still refuse it: `dsl.md` says `\w{3,30}` does not fit.
#[test]
fn translation_budget() {
    use super::price::calibration::{Verdict, calibration_cases};
    use super::price::{MAX_REPEAT_POSITIONS, MAX_TRANSLATION_UNITS};

    let compiles_too_large = [r"\w{3,30}"];
    for case in calibration_cases() {
        let input = copies_of(&case.body, case.copies);
        assert!(input.len() <= parser::MAX_QUERY_LEN, "{}", case.name);
        let (result, builds) = parse_counting_builds(&input);
        let label = format!("{} x{}", case.name, case.copies);
        match case.expect {
            Verdict::Admitted if compiles_too_large.contains(&case.body.as_str()) => {
                let errors = result.expect_err(&label);
                assert_eq!(errors.len(), 1, "{label}: {errors:?}");
                assert_eq!(errors[0].message, MSG_REGEX_TOO_LARGE, "{label}");
                assert_eq!(builds, 1, "{label}");
            }
            Verdict::Admitted => {
                assert!(result.is_ok(), "{label}: {result:?}");
                // One ledger: every copy after the first is answered from
                // what it built.
                assert_eq!(builds, 1, "{label}");
            }
            Verdict::RefusedByTranslation => {
                let errors = result.expect_err(&label);
                assert_eq!(errors.len(), 1, "{label}: {errors:?}");
                assert!(
                    errors[0].message.starts_with(MSG_TRANSLATION_BUDGET),
                    "{label}: {errors:?}"
                );
                assert!(
                    errors[0]
                        .message
                        .ends_with(&format!("max {MAX_TRANSLATION_UNITS})")),
                    "{label}: {errors:?}"
                );
                assert!(errors[0].hint.is_some(), "{label}");
                assert_eq!(builds, 0, "{label}");
            }
            Verdict::RefusedByRepeat => {
                let errors = result.expect_err(&label);
                assert!(
                    errors[0].message.starts_with(MSG_REPEAT_BUDGET),
                    "{label}: {errors:?}"
                );
                assert!(
                    errors[0]
                        .message
                        .ends_with(&format!("max {MAX_REPEAT_POSITIONS})")),
                    "{label}: {errors:?}"
                );
                assert_eq!(builds, 0, "{label}");
            }
        }
    }
}

/// The budget is per query: two bodies that together cost exactly the
/// limit are admitted, and one unit more is refused over the body that
/// takes the sum past it, with the whole query's cost in the message.
#[test]
fn translation_budget_sums_the_query_exactly_to_its_limit() {
    use super::price::MAX_TRANSLATION_UNITS;

    // 14 case-insensitive full-range brackets cost 1 131 757 units each;
    // `(?i)[\x41-\x{N}]` costs 17 + S + 6·2938 for S = N - 0x40 code
    // points, so N = 0xDF65D brings the two bodies to the limit.
    let full = format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(14));
    let at = r"(?i)[\x41-\x{DF65D}]";
    let over = r"(?i)[\x41-\x{DF65E}]";
    let units = |body: &str| {
        let ast = regex_syntax::ast::parse::Parser::new()
            .parse(body)
            .expect("the body parses");
        super::price::shape(&ast).units
    };
    assert_eq!(units(&full) + units(at), MAX_TRANSLATION_UNITS);
    assert_eq!(units(&full) + units(over), MAX_TRANSLATION_UNITS + 1);

    let (result, builds) = parse_counting_builds(&format!("f=/{full}/ g=/{at}/"));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(builds, 2);

    let input = format!("f=/{full}/ g=/{over}/");
    let (result, builds) = parse_counting_builds(&input);
    let errors = result.expect_err("one unit over");
    assert_eq!(builds, 0);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(
        errors[0].message,
        format!(
            "{MSG_TRANSLATION_BUDGET} ({} units, max {MAX_TRANSLATION_UNITS})",
            MAX_TRANSLATION_UNITS + 1
        )
    );
    assert_eq!(&input[errors[0].span.clone()], format!("/{over}/"));
}

/// The repeat budget refuses a pattern that repeats to more than 256
/// positions, at its own site and with a hint, before anything builds,
/// and admits the ordinary counted repeats. Each atom is one position, so
/// 256 literals are the limit, spelled out or counted.
#[test]
fn repeat_budget() {
    use super::price::MAX_REPEAT_POSITIONS;

    for body in [
        "(?:a|b)[abc]{1000}(?:a|d)d",
        "[A-Za-z]{1000}",
        "a{257}",
        &"a".repeat(257),
        r"\d{1000}",
        ".{1000}",
    ] {
        let input = format!("f=/{body}/");
        let (result, builds) = parse_counting_builds(&input);
        let errors = result.expect_err(body);
        assert_eq!(builds, 0, "{body}");
        assert_eq!(errors.len(), 1, "{body}: {errors:?}");
        assert!(
            errors[0].message.starts_with(MSG_REPEAT_BUDGET),
            "{errors:?}"
        );
        assert_eq!(&input[errors[0].span.clone()], format!("/{body}/"));
        let hint = errors[0].hint.as_deref().unwrap_or_default();
        assert!(hint.contains('+'), "{body}: {hint}");
    }
    assert_eq!(
        parser::parse("f=/a{257}/")
            .expect_err("over")
            .remove(0)
            .message,
        format!("{MSG_REPEAT_BUDGET} (257 positions, max {MAX_REPEAT_POSITIONS})")
    );

    for body in [
        "(?:a|b)[abc]{30}(?:a|d)d",
        r"\d{1,3}(\.\d{1,3}){3}",
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        "[0-9a-f]{64}",
        "a{256}",
        &"a".repeat(256),
    ] {
        let result = parser::parse(&format!("f=/{body}/"));
        assert!(result.is_ok(), "{body}: {result:?}");
    }
}

/// The repeat budget holds at every pattern site, for the regex a glob or
/// `like` operand stands for too: 257 positions are refused over the
/// operand as written, and 256 are admitted.
#[test]
fn repeat_budget_holds_at_every_site() {
    type Site = fn(&str) -> (String, String);
    let regex_sites: [(&str, Site); 3] = [
        ("matches /re/", |b| {
            (format!("* | where m matches /{b}/"), format!("/{b}/"))
        }),
        ("matches \"re\"", |b| {
            (format!("* | where m matches \"{b}\""), format!("\"{b}\""))
        }),
        ("extract", |b| {
            (
                format!("* | extract \"(?P<x>{b})\""),
                format!("\"(?P<x>{b})\""),
            )
        }),
    ];
    for (site, spell) in regex_sites {
        let (dsl, _) = spell(&"a".repeat(256));
        assert!(parser::parse(&dsl).is_ok(), "{site}");
        let (dsl, covered) = spell(&"a".repeat(257));
        let errors = parser::parse(&dsl).expect_err(site);
        assert_eq!(errors.len(), 1, "{site}: {errors:?}");
        assert!(errors[0].message.starts_with(MSG_REPEAT_BUDGET), "{site}");
        assert_eq!(dsl[errors[0].span.clone()], covered, "{site}");
    }
    // `%` is `.*`, one position, and so is each other character.
    let operand_sites: [(&str, Site); 3] = [
        ("glob", |t| (format!("host=*{t}"), format!("*{t}"))),
        ("like", |t| {
            (format!("* | where host like \"%{t}\""), format!("\"%{t}\""))
        }),
        ("ilike", |t| {
            (
                format!("* | where host ilike \"%{t}\""),
                format!("\"%{t}\""),
            )
        }),
    ];
    for (site, spell) in operand_sites {
        let (dsl, _) = spell(&"a".repeat(255));
        assert!(
            parser::parse(&dsl).is_ok(),
            "{site}: {:?}",
            parser::parse(&dsl)
        );
        let (dsl, covered) = spell(&"a".repeat(256));
        let errors = parser::parse(&dsl).expect_err(site);
        assert_eq!(errors.len(), 1, "{site}: {errors:?}");
        assert!(errors[0].message.starts_with(MSG_REPEAT_BUDGET), "{site}");
        assert_eq!(dsl[errors[0].span.clone()], covered, "{site}");
    }
}

/// The one error `input` parses to, which must be the pattern cap's.
fn pattern_cap_refusal(input: &str) -> parser::ParseError {
    let errors = parser::parse(input).expect_err("over the cap");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].message.starts_with("too many patterns"),
        "{errors:?}"
    );
    errors[0].clone()
}

/// An `extract` body of `n` named groups, numbered from `from`, each one
/// position: 64 of them fit the body limit with room to spare.
fn named_groups(from: usize, n: usize) -> String {
    use std::fmt::Write as _;
    (from..from + n).fold(String::new(), |mut body, i| {
        write!(body, "(?P<g{i}>a)").expect("writing to a String");
        body
    })
}

/// An `extract` charges `max(1, named groups)` toward
/// `MAX_QUERY_PATTERNS` (ADR-0063), because the batch lane emits one
/// `regexp_extract` per named group. Unnamed groups charge nothing of
/// their own. Exactly the cap, spread across bodies that each fit the
/// body limit, is admitted, and one charge more is refused over the
/// stage that carries it.
#[test]
fn extract_groups_count_toward_the_pattern_cap() {
    let n = parser::MAX_QUERY_PATTERNS;
    let stages: Vec<String> = (0..4)
        .map(|k| format!("| extract \"{}\"", named_groups(k * 64, 64)))
        .collect();
    for stage in &stages {
        assert!(stage.len() < MAX_REGEX_LEN, "{stage:.40}");
    }
    let at_cap = format!("* {}", stages.join(" "));
    assert!(parser::parse(&at_cap).is_ok(), "{at_cap:.80}");

    // A body with no named group still charges one.
    let over = format!("{at_cap} | extract \"(a)\"");
    let error = pattern_cap_refusal(&over);
    assert!(error.message.contains(&format!("({} ", n + 1)), "{error:?}");
    assert_eq!(&over[error.span], "extract \"(a)\"");

    // Beside other patterns: the extract that crosses the cap carries
    // the error, and its unnamed groups add nothing.
    let globs: Vec<String> = (0..n - 2).map(|i| format!("g{i}*")).collect();
    let globs = format!("f={}", globs.join(","));
    let two = "| extract \"(a)(?P<x>b)(c)(?P<y>d)\"";
    assert!(parser::parse(&format!("{globs} {two}")).is_ok());
    let three = "| extract \"(a)(?P<x>b)(?P<y>c)(?P<z>d)\"";
    let over = format!("{globs} {three}");
    let error = pattern_cap_refusal(&over);
    assert_eq!(&over[error.span], &three[2..]);
    // `extract kv` builds no regex and charges nothing.
    assert!(parser::parse(&format!("{globs} {two} | extract kv")).is_ok());
}

fn preparer() -> Preparer {
    Preparer::new()
}

/// A ledger builds each `(kind, text)` once and counts what it built, a
/// failure included.
#[test]
fn a_ledger_builds_each_pattern_once() {
    let mut ledger = preparer();
    let before = thread_builds();
    assert!(ledger.build(PatternKind::Regex, "a+").is_ok());
    assert!(ledger.build(PatternKind::Regex, "a+").is_ok());
    // The same text as another kind is another pattern.
    assert!(ledger.build(PatternKind::Like, "a+").is_ok());
    let unknown = ledger.build(PatternKind::Regex, r"\p{Nope}").err();
    assert!(
        matches!(unknown, Some(PatternError::Syntax(_))),
        "{unknown:?}"
    );
    assert_eq!(ledger.build(PatternKind::Regex, r"\p{Nope}").err(), unknown);
    assert_eq!(ledger.builds(), 3);
    assert_eq!(thread_builds() - before, 3);
}

/// After a build fails for size, a new pattern is not built, but one the
/// ledger already holds still answers.
#[test]
fn a_ledger_stops_after_a_size_failure() {
    let mut ledger = preparer();
    let mut refusal = |text: &str| ledger.build(PatternKind::Regex, text).err();
    assert_eq!(refusal("a"), None);
    assert_eq!(refusal(r"\w{100}"), Some(PatternError::CompiledSize));
    assert_eq!(refusal("b"), Some(PatternError::Stopped));
    assert_eq!(refusal(r"\w{100}"), Some(PatternError::CompiledSize));
    assert_eq!(refusal("a"), None);
    assert_eq!(ledger.builds(), 2);
}

/// Each kind matches as its lane always has: `like` and `ilike` anchor
/// the whole value, `ilike` ignores case, a glob crosses newlines.
#[test]
fn each_kind_matches_its_dialect() {
    let mut ledger = preparer();
    let mut is_match = |kind, text: &str, subject: &str| {
        ledger.build(kind, text).expect("builds").is_match(subject)
    };
    assert!(is_match(PatternKind::Regex, "b+", "abbc"));
    assert!(is_match(PatternKind::Like, "a%c", "abbc"));
    assert!(!is_match(PatternKind::Like, "b%", "abbc"));
    assert!(!is_match(PatternKind::Like, "A%", "abbc"));
    assert!(is_match(PatternKind::ILike, "A_B%", "axbc"));
    assert!(is_match(PatternKind::Glob, "a*c", "ab\nc"));
    assert!(!is_match(PatternKind::Glob, "b*", "abc"));
}

/// The like translation, byte for byte: `%` and `_` become `.*` and `.`,
/// every regex metacharacter is escaped, and `ilike` prefixes `(?i)`.
#[test]
fn like_translation_is_unchanged() {
    assert_eq!(like_to_regex("a%b_c", false), "^a.*b.c$");
    assert_eq!(
        like_to_regex(r".^$*+?()[]{}|\", false),
        r"^\.\^\$\*\+\?\(\)\[\]\{\}\|\\$"
    );
    assert_eq!(like_to_regex("a%", true), "(?i)^a.*$");
}

#[test]
fn glob_conversion_star() {
    assert_eq!(glob_to_regex("*.txt"), r"(?s)^.*\.txt$");
}

#[test]
fn glob_conversion_question() {
    assert_eq!(glob_to_regex("file?.log"), r"(?s)^file.\.log$");
}

#[test]
fn glob_conversion_character_class() {
    assert_eq!(glob_to_regex("[abc]"), "(?s)^[abc]$");
}

#[test]
fn glob_conversion_negated_class() {
    assert_eq!(glob_to_regex("[!abc]"), "(?s)^[^abc]$");
}

#[test]
fn glob_conversion_escapes_dots() {
    assert_eq!(glob_to_regex("foo.bar"), r"(?s)^foo\.bar$");
}

#[test]
fn fold_sites_are_charged() {
    super::price::fold_site_inventory_check();
}

#[test]
fn only_a_string_literal_is_a_pattern_operand() {
    use crate::ast::{Expr, LiteralValue};

    let literal = Expr::Literal(LiteralValue::String("a.*b".into()));
    assert_eq!(literal_operand(&literal), Ok("a.*b"));

    let field = Expr::FieldRef("message".into());
    assert_eq!(literal_operand(&field), Err(PatternError::Computed));
    let number = Expr::Literal(LiteralValue::Int(1));
    assert_eq!(literal_operand(&number), Err(PatternError::Computed));

    assert_eq!(
        PatternKind::of_operator(crate::ast::BinaryOp::Matches),
        Some(PatternKind::Regex)
    );
    assert_eq!(PatternKind::of_operator(crate::ast::BinaryOp::Eq), None);
}
