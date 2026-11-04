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
/// `<…Regex… as …>::from_str` or `::try_from`, counted by
/// [`qualified_regex_conversions`].
const QUALIFIED: &str = concat!("<…Reg", "ex… as …>::from_str or ::try_from");
/// A `.parse()` in a `let` whose type names a `Regex`, counted by
/// [`let_typed_regex_parses`].
const LET_PARSE: &str = concat!("let …: …Reg", "ex… = ….parse()");

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
    ("crates/trawl-core/src/filter.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/parser/mod.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-core/src/parser/primitives.rs",
        PREPARER_NEW,
        1,
    ),
    ("crates/trawl-core/src/pattern/mod.rs", BUILDER_NEW, 1),
    ("crates/trawl-core/src/pattern/table.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/pattern/tests.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/stream.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/tests/eval_no_panic.rs", PREPARER_NEW, 2),
    ("crates/trawl-core/tests/filter_parity.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-core/tests/pin_stage_parity.rs",
        PREPARER_NEW,
        1,
    ),
    ("crates/trawl-core/tests/scalar_parity.rs", PREPARER_NEW, 1),
    ("crates/trawl-engine/tests/regex_cost.rs", PREPARER_NEW, 1),
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

/// How many `parse::<T>` turbofishes in `text` name a `Regex` in `T`.
fn turbofish_regex_parses(text: &str) -> usize {
    text.match_indices(concat!("parse", "::<"))
        .filter(|(at, opener)| {
            let generic = &text[at + opener.len()..];
            generic[..generic.find('>').unwrap_or(generic.len())].contains("Regex")
        })
        .count()
}

/// How many fully qualified `from_str` and `try_from` calls in `text`
/// name a `Regex` as their self type: `<regex::Regex as FromStr>::from_str`,
/// `<regex::bytes::Regex as TryFrom<String>>::try_from`, or `<Regex>::…`.
fn qualified_regex_conversions(text: &str) -> usize {
    [">::from_str", ">::try_from"]
        .into_iter()
        .flat_map(|tail| text.match_indices(tail))
        .filter(|&(at, _)| qualified_self_type(&text[..=at]).is_some_and(|ty| ty.contains("Regex")))
        .count()
}

/// The self type of the qualified path that the last `>` of `head` closes:
/// `T` in `<T as Trait>` or in `<T>`. `None` when no `<` opens it inside
/// the statement.
fn qualified_self_type(head: &str) -> Option<&str> {
    let bytes = head.as_bytes();
    let mut depth = 0usize;
    for (i, &byte) in bytes.iter().enumerate().rev() {
        match byte {
            // The arrow of a function type, not a bracket.
            b'>' if i > 0 && bytes[i - 1] == b'-' => {}
            b'>' => depth += 1,
            b'<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let path = &head[i + 1..];
                    return path.split(" as ").next();
                }
            }
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
    }
    None
}

/// How many `.parse()` calls in `text` sit in a `let` statement whose type
/// annotation names a `Regex`, such as `let re: regex::Regex = s.parse()?;`
/// or `let re: Result<Regex, _> = s.parse();`. The statement may span
/// lines.
fn let_typed_regex_parses(text: &str) -> usize {
    text.match_indices("let ")
        .filter(|&(at, _)| {
            !text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
        .filter_map(|(at, keyword)| {
            let statement = statement(&text[at + keyword.len()..]);
            let (binding, init) = statement.split_at(assignment(statement)?);
            let annotation = &binding[lone_colon(binding)?..];
            annotation
                .contains("Regex")
                .then(|| init.matches(".parse()").count())
        })
        .sum()
}

/// `rest` up to the `;` that ends its statement, or up to the bracket that
/// closes the block around it. Nested brackets, string literals and
/// character literals do not end it.
fn statement(rest: &str) -> &str {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            // A character literal, such as `')'`, `';'` or `'"'`, neither
            // opens a bracket nor a string nor ends the statement.
            '\'' => {
                if let Some(len) = char_literal_len(&rest[i..]) {
                    while chars.next_if(|&(j, _)| j < i + len).is_some() {}
                }
            }
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => match depth.checked_sub(1) {
                Some(inner) => depth = inner,
                None => return &rest[..i],
            },
            ';' if depth == 0 => return &rest[..i],
            _ => {}
        }
    }
    rest
}

/// The byte length of the character literal that opens `text`, which
/// starts at a `'`: `'x'`, or an escape such as `'\''`, `'\\'`, `'\x3b'`
/// or `'\u{29}'`. `None` for a lifetime such as `'a` or `'static`, which
/// has no closing quote after one character.
fn char_literal_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().skip(1);
    let (_, first) = chars.next()?;
    let (end, close) = match first {
        '\'' => return None,
        // The escaped character, then any hex digits and braces of a
        // `\x` or `\u{…}` escape, then the closing quote.
        '\\' => {
            chars.next()?;
            chars.find(|&(_, c)| !(c.is_ascii_hexdigit() || c == '{' || c == '}'))?
        }
        _ => chars.next()?,
    };
    (close == '\'').then_some(end + 1)
}

/// Where the `=` that assigns a `let` statement's value sits: not part of
/// `==`, `=>`, `<=`, `>=` or `!=`.
fn assignment(statement: &str) -> Option<usize> {
    let bytes = statement.as_bytes();
    (0..bytes.len()).find(|&i| {
        bytes[i] == b'='
            && !matches!(bytes.get(i + 1), Some(b'=' | b'>'))
            && !matches!(
                i.checked_sub(1).map(|p| bytes[p]),
                Some(b'=' | b'!' | b'<' | b'>')
            )
    })
}

/// Where the `:` that opens a binding's type annotation sits: a colon
/// that is not half of a `::` path separator.
fn lone_colon(binding: &str) -> Option<usize> {
    let bytes = binding.as_bytes();
    (0..bytes.len()).find(|&i| {
        bytes[i] == b':'
            && bytes.get(i + 1) != Some(&b':')
            && i.checked_sub(1).map(|p| bytes[p]) != Some(b':')
    })
}

/// The construction sites in one source file, by spelling. Comment lines
/// are skipped. A trait-based build names no builder, so the scan counts
/// the spellings that reach `FromStr` or `TryFrom` for a `Regex` too: the
/// contiguous `Regex::from_str` and `Regex::try_from`, a `parse::<…>`
/// turbofish, a fully qualified call, and a `.parse()` whose `let` names
/// the type. Clippy's `disallowed-methods` cannot see any of these.
fn construction_sites(text: &str) -> BTreeMap<&'static str, usize> {
    let code: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut sites = BTreeMap::new();
    for needle in NEEDLES {
        sites.insert(needle, code.matches(needle).count());
    }
    sites.insert(PARSE_TURBOFISH, turbofish_regex_parses(&code));
    sites.insert(QUALIFIED, qualified_regex_conversions(&code));
    sites.insert(LET_PARSE, let_typed_regex_parses(&code));
    sites.retain(|_, n| *n > 0);
    sites
}

/// The scanner counts every spelling that builds a `Regex` through a
/// trait, for `regex::Regex` and `regex::bytes::Regex` alike, and nothing
/// that builds another type. Each fixture splits the type's name, so this
/// file's own text never matches one.
#[test]
fn the_inventory_counts_every_trait_based_build() {
    let sites: &[(&str, &str)] = &[
        (
            concat!("let re: regex::Reg", "ex = body.parse()?;"),
            LET_PARSE,
        ),
        (
            concat!("let re: regex::bytes::Reg", "ex = body.parse()?;"),
            LET_PARSE,
        ),
        (
            concat!("let re: Result<Reg", "ex, regex::Error> = body.parse();"),
            LET_PARSE,
        ),
        (
            concat!(
                "let re: regex::Reg",
                "ex = body\n        .trim()\n        .parse()\n        .expect(\"compiles\");"
            ),
            LET_PARSE,
        ),
        // A character literal that holds a bracket or a `;` neither
        // closes nor ends the statement around it.
        (
            concat!("let re: regex::Reg", "ex = s.trim_matches(')').parse()?;"),
            LET_PARSE,
        ),
        (
            concat!("let re: regex::Reg", "ex = ';'.to_string().parse()?;"),
            LET_PARSE,
        ),
        (
            concat!("<regex::Reg", "ex as std::str::FromStr>::from_str(body)?"),
            QUALIFIED,
        ),
        (
            concat!(
                "<regex::bytes::Reg",
                "ex as std::str::FromStr>::from_str(body)?"
            ),
            QUALIFIED,
        ),
        (
            concat!("<regex::Reg", "ex as TryFrom<&str>>::try_from(body)?"),
            QUALIFIED,
        ),
        (
            concat!(
                "<regex::bytes::Reg",
                "ex as TryFrom<String>>::try_from(body.into())?"
            ),
            QUALIFIED,
        ),
        (concat!("<Reg", "ex>::from_str(body)?"), QUALIFIED),
        (concat!("regex::Reg", "ex::from_str(body)?"), FROM_STR),
        (
            concat!("regex::bytes::Reg", "ex::try_from(body)?"),
            TRY_FROM,
        ),
        (
            concat!("body.parse::<regex::bytes::Reg", "ex>()?"),
            PARSE_TURBOFISH,
        ),
    ];
    for (source, spelling) in sites {
        assert_eq!(
            construction_sites(source),
            BTreeMap::from([(*spelling, 1)]),
            "{source}"
        );
    }

    let not_sites = [
        "let n: u32 = body.parse()?;",
        "<u32 as std::str::FromStr>::from_str(body)?",
        "<u64 as TryFrom<i64>>::try_from(n)?",
        concat!("let re: regex::Reg", "ex = other.clone();"),
        concat!(
            "let re: Option<regex::Reg",
            "ex> = None; let n: u32 = body.parse()?;"
        ),
        concat!("    // let re: regex::Reg", "ex = body.parse()?;"),
        "let regex_count: usize = body.parse()?;",
    ];
    for source in not_sites {
        assert_eq!(construction_sites(source), BTreeMap::new(), "{source}");
    }
}

/// An escaped character literal is one literal, and a lifetime is not a
/// literal at all, so neither throws the `let` scan off the statement
/// around it. Each fixture splits the type's name, so this file's own text
/// never matches one.
#[test]
fn the_inventory_reads_escaped_literals_and_lifetimes() {
    let sites = [
        concat!(
            "let re: regex::Reg",
            "ex = s.trim_matches(['\\'', ')', '\\\\']).parse()?;"
        ),
        concat!(
            "let re: regex::Reg",
            "ex = s.trim_matches(['\\u{29}', '\\x3b']).parse()?;"
        ),
        concat!(
            "let re: regex::Reg",
            "ex = Cow::<'_, str>::from(s).parse()?;"
        ),
    ];
    for source in sites {
        assert_eq!(
            construction_sites(source),
            BTreeMap::from([(LET_PARSE, 1)]),
            "{source}"
        );
    }
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
        for (spelling, n) in construction_sites(&text) {
            found.insert((rel.clone(), spelling), n);
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
    let same = vec![r"/\w{64}/".to_string(); n];
    // Each distinct body is `\w{61}` and a numeral of at most three
    // digits, inside the repeat budget, so it reaches the build that fails.
    let distinct: Vec<String> = (0..n).map(|i| format!(r"/\w{{61}}{i}/")).collect();
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
/// The attacks: 63 bodies of `(?i)` and 64 `\p{Any}` or `\p{Ll}`,
/// full-range brackets, and 63 bodies of 40 `(?i)[^\x00-\x{10FFFE}]`, a
/// negated range whose complement is one character. Each is as many
/// bodies as the query cap holds.
///
/// A body both budgets admit is built, and the compiled-size limit can
/// still refuse it: `dsl.md` says `\w{3,30}` does not fit, and neither
/// do 64 `\w`. Every copy of such a body reports the one failed build.
///
/// The walk prices a nested class's fold, so the calibration still names
/// X8, 64 `(?i)[a[^b]]`, as a translation attack. The parser refuses its
/// nested classes first (`class_set_syntax_is_refused`), so it never
/// reaches the budget.
#[test]
fn translation_budget() {
    use super::price::calibration::{Verdict, calibration_cases};
    use super::price::{MAX_REPEAT_POSITIONS, MAX_TRANSLATION_UNITS};

    let sixty_four_words = r"\w".repeat(64);
    let compiles_too_large = [r"\w{3,30}", sixty_four_words.as_str()];
    for case in calibration_cases() {
        let input = copies_of(&case.body, case.copies);
        assert!(input.len() <= parser::MAX_QUERY_LEN, "{}", case.name);
        let (result, builds) = parse_counting_builds(&input);
        let label = format!("{} x{}", case.name, case.copies);
        let ast = regex_syntax::ast::parse::Parser::new()
            .parse(&case.body)
            .expect("every calibration body parses");
        if re2::class_set_refusal(&ast).is_err() {
            let errors = result.expect_err(&label);
            assert_eq!(errors[0].message, MSG_CLASS_SET_SYNTAX, "{label}");
            assert_eq!(builds, 0, "{label}");
            continue;
        }
        match case.expect {
            Verdict::Admitted if compiles_too_large.contains(&case.body.as_str()) => {
                let errors = result.expect_err(&label);
                // The parser reports at most eight errors.
                let reported = usize::try_from(case.copies).expect("a small count").min(8);
                assert_eq!(errors.len(), reported, "{label}: {errors:?}");
                for error in &errors {
                    assert!(
                        error.message.starts_with(MSG_REGEX_TOO_LARGE),
                        "{label}: {error:?}"
                    );
                }
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

/// The repeat budget refuses a pattern that repeats to more than 64
/// positions, at its own site and with a hint, before anything builds,
/// and admits the ordinary counted repeats. Each atom is one position, so
/// 64 literals are the limit, spelled out or counted.
#[test]
fn repeat_budget() {
    for body in [
        "(?:a|b)[abc]{1000}(?:a|d)d",
        "[A-Za-z]{1000}",
        "a{65}",
        &"a".repeat(65),
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
        assert!(hint.contains("64 positions"), "{body}: {hint}");
    }
    assert_eq!(
        parser::parse("f=/a{65}/")
            .expect_err("over")
            .remove(0)
            .message,
        format!("{MSG_REPEAT_BUDGET} (65 positions, max 64)")
    );

    for body in [
        "(?:a|b)[abc]{30}(?:a|d)d",
        r"\d{1,3}(\.\d{1,3}){3}",
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        "[0-9a-f]{64}",
        "a{64}",
        &"a".repeat(64),
    ] {
        let result = parser::parse(&format!("f=/{body}/"));
        assert!(result.is_ok(), "{body}: {result:?}");
    }
}

/// The repeat budget holds at every pattern site, for the regex a glob or
/// `like` operand stands for too: 65 positions are refused over the
/// operand as written, and 64 are admitted.
#[test]
fn repeat_budget_holds_at_every_site() {
    type Site = fn(&str) -> (String, String);
    // How many characters each site holds at 64 positions: an `extract`
    // body's named group is one of them.
    let regex_sites: [(&str, Site, usize); 3] = [
        (
            "matches /re/",
            |b| (format!("* | where m matches /{b}/"), format!("/{b}/")),
            64,
        ),
        (
            "matches \"re\"",
            |b| (format!("* | where m matches \"{b}\""), format!("\"{b}\"")),
            64,
        ),
        (
            "extract",
            |b| {
                (
                    format!("* | extract \"(?P<x>{b})\""),
                    format!("\"(?P<x>{b})\""),
                )
            },
            63,
        ),
    ];
    for (site, spell, limit) in regex_sites {
        let (dsl, _) = spell(&"a".repeat(limit));
        assert!(parser::parse(&dsl).is_ok(), "{site}");
        let (dsl, covered) = spell(&"a".repeat(limit + 1));
        let errors = parser::parse(&dsl).expect_err(site);
        assert_eq!(errors.len(), 1, "{site}: {errors:?}");
        assert!(errors[0].message.starts_with(MSG_REPEAT_BUDGET), "{site}");
        assert_eq!(dsl[errors[0].span.clone()], covered, "{site}");
    }
    // `%` and a glob's `*` are `.*`, two positions: the `.` and the repeat
    // around it. Each other character is one. The translation anchors the
    // whole value, `^…$`, and the two anchors count one each.
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
        let (dsl, _) = spell(&"a".repeat(60));
        assert!(
            parser::parse(&dsl).is_ok(),
            "{site}: {:?}",
            parser::parse(&dsl)
        );
        let (dsl, covered) = spell(&"a".repeat(61));
        let errors = parser::parse(&dsl).expect_err(site);
        assert_eq!(errors.len(), 1, "{site}: {errors:?}");
        assert!(errors[0].message.starts_with(MSG_REPEAT_BUDGET), "{site}");
        assert_eq!(dsl[errors[0].span.clone()], covered, "{site}");
    }
}

/// Every node that compiles to work counts toward the repeat budget, so a
/// pattern cannot hide its cost in nodes that match no character: eight
/// `\B` before each of 62 classes, or 490 empty capture groups before 32
/// positions. Each is refused at every regex site before anything
/// builds, with its whole count in the message.
#[test]
fn assertions_and_capture_groups_count_toward_the_repeat_budget() {
    let assertions = format!("𐐨(?:{}[𐐨𐐩𐐪]){{62}}𐐪", r"\B".repeat(8));
    let captures = format!(r"\B{}𐐨[𐐨𐐩𐐪]{{30}}𐐪", "()".repeat(490));
    assert!(captures.len() + "(?P<x>)".len() <= MAX_REGEX_LEN);
    for (body, positions) in [(assertions, 560), (captures, 523)] {
        let sites = [
            (format!("message=/{body}/"), positions),
            (format!("* | where m matches /{body}/"), positions),
            // The named group around the body counts one more.
            (format!("* | extract \"(?P<x>{body})\""), positions + 1),
        ];
        for (dsl, positions) in sites {
            let (result, builds) = parse_counting_builds(&dsl);
            let errors = result.expect_err(&dsl);
            assert_eq!(builds, 0, "{dsl}");
            assert_eq!(errors.len(), 1, "{dsl}: {errors:?}");
            assert_eq!(
                errors[0].message,
                format!("{MSG_REPEAT_BUDGET} ({positions} positions, max 64)"),
                "{dsl}"
            );
        }
    }
}

/// A repeat is a node of its own and counts one position more than its
/// operand, so wrapping a class in `(?:…)*` or `(?:…)?` again and again
/// cannot hide the splits each wrapper compiles to. 120 such wrappers
/// around a 4-byte class, counted 62 times after a `\B`, once priced at
/// 64 positions: over 65 536 Deseret letters one call took about 4.5 s
/// with `*` and about 2.4 s with `?`. Each spelling, lazy ones included,
/// is refused before anything builds. `{0,1}` is longer to write, so 100
/// of its wrappers fill the body limit.
#[test]
fn nested_repetitions_count_toward_the_repeat_budget() {
    for (op, depth) in [
        ("*", 120),
        ("?", 120),
        ("*?", 120),
        ("??", 120),
        ("{0,1}", 100),
    ] {
        let mut inner = String::from("[𐐨𐐩𐐪]");
        for _ in 0..depth {
            inner = format!("(?:{inner}){op}");
        }
        let body = format!(r"\B(?:{inner}){{62}}𐐪");
        assert!(body.len() <= MAX_REGEX_LEN, "{op}: {} bytes", body.len());
        let input = format!("message=/{body}/");
        let (result, builds) = parse_counting_builds(&input);
        let errors = result.expect_err(op);
        assert_eq!(builds, 0, "{op}");
        assert_eq!(errors.len(), 1, "{op}: {errors:?}");
        // Each of the 62 units is the class and its wrappers, and the
        // `\B` and the last letter count one each.
        assert_eq!(
            errors[0].message,
            format!(
                "{MSG_REPEAT_BUDGET} ({} positions, max 64)",
                1 + 62 * (depth + 1) + 1
            ),
            "{op}"
        );
    }
}

/// An alternation of `n` branches compiles to `n - 1` splits, which an
/// enclosing repeat copies, so each `|` counts one position. Review round
/// 3's body (F7) hid three of them under `{0,2}` and once priced at 64
/// positions: over 65 536 Deseret letters one `DuckDB` call took about
/// 450 ms. It is refused before anything builds, at 70.
#[test]
fn alternation_splits_count_toward_the_repeat_budget() {
    let body = super::price::calibration::unequal_alternation();
    assert!(body.len() <= MAX_REGEX_LEN, "{} bytes", body.len());
    let input = format!("message=/{body}/");
    let (result, builds) = parse_counting_builds(&input);
    let errors = result.expect_err(&body);
    assert_eq!(builds, 0);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(
        errors[0].message,
        format!("{MSG_REPEAT_BUDGET} (70 positions, max 64)")
    );
    assert_eq!(&input[errors[0].span.clone()], format!("/{body}/"));
}

/// Bodies whose classes the `regex` crate and RE2 read differently: a
/// class nested in a bracketed class, or a class set operation. The first
/// four admitted the repeat budget before the refusal, and RE2 compiled
/// them into a larger program than the walk priced (review round 4):
/// over 65 536 Deseret letters one `DuckDB` call took 0.40 to 0.94 s.
fn class_set_bodies() -> Vec<String> {
    vec![
        r"\B(?:[\pL~~\pL]?){30}\PL".to_string(),
        r"\B(?:[\pL&&a]?){30}\PL".to_string(),
        format!(r"\B(?:[{}]?){{8}}\PL", r"[\pL]".repeat(12)),
        format!("a(?:[{}]?){{31}}[^abc]", "[abc]".repeat(8)),
        "[a[b]]".to_string(),
        "[a-z--b]".to_string(),
        r"[\pL&&\p{Greek}]".to_string(),
        "[^a-z&&[^aeiou]]".to_string(),
    ]
}

/// Every regex site, as the query that writes a body there and the text
/// its error covers.
type BodySite = (&'static str, fn(&str) -> (String, String));

const REGEX_BODY_SITES: [BodySite; 4] = [
    ("search /re/", |b| {
        (format!("msg=a,/{b}/"), format!("/{b}/"))
    }),
    ("matches /re/", |b| {
        (format!("* | where msg matches /{b}/"), format!("/{b}/"))
    }),
    ("matches \"re\"", |b| {
        let q = format!("\"{}\"", b.replace('\\', r"\\").replace('"', "\\\""));
        (format!("* | where msg matches {q}"), q)
    }),
    ("extract", |b| {
        let q = format!("\"{b}\"");
        (format!("* | extract {q} from msg"), q)
    }),
];

/// A class nested in a bracketed class and the class set operators `&&`,
/// `--` and `~~` are refused in every regex body, at its own site, by
/// `parse` and `parse_unbuilt` alike, before anything builds. RE2, which
/// runs every batch regex, reads them as plain characters: the first `]`
/// closes the class, and each operand joins one union. The two lanes would
/// then match different text, and RE2 would compile a program the walk
/// never priced.
#[test]
fn class_set_syntax_is_refused() {
    for body in class_set_bodies() {
        for (site, spell) in REGEX_BODY_SITES {
            let (dsl, covered) = spell(&body);
            let (result, builds) = parse_counting_builds(&dsl);
            let errors = result.expect_err(&dsl);
            assert_eq!(builds, 0, "{site} {body}");
            assert_eq!(errors.len(), 1, "{site} {body}: {errors:?}");
            assert_eq!(errors[0].message, MSG_CLASS_SET_SYNTAX, "{site} {body}");
            assert_eq!(dsl[errors[0].span.clone()], covered, "{site} {body}");
            let hint = errors[0].hint.as_deref().unwrap_or_default();
            assert!(hint.contains("[a-c]"), "{site} {body}: {hint}");

            let before = thread_builds();
            assert_eq!(parser::parse_unbuilt(&dsl).expect_err(&dsl), errors);
            assert_eq!(thread_builds(), before, "{site} {body}");
        }
    }
}

/// A class that only escapes a `&`, `-`, `[` or `]`, or holds a POSIX
/// class, nests nothing and operates on nothing, and both engines read it
/// alike. Each parses at every regex site.
#[test]
fn escaped_and_posix_classes_are_not_class_set_syntax() {
    for body in [
        r"[a\&&b]",
        r"[\[]",
        r"[\]]",
        r"[a\-z]",
        r"[\~a]",
        "[[:alpha:]]",
        "[^[:space:]]",
        "[[:^digit:]x]",
        "[a-z&]",
        "[-~]",
        r"[\pL\p{Greek}]",
    ] {
        for (site, spell) in REGEX_BODY_SITES {
            let (dsl, _) = spell(body);
            let result = parser::parse(&dsl);
            assert!(result.is_ok(), "{site} {body}: {result:?}");
        }
    }
}

/// The live plan refuses class set syntax for an operand no parser
/// checked, as it refuses a computed one, and builds nothing for it.
#[test]
fn the_live_compile_refuses_class_set_syntax() {
    use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned};
    use crate::pattern::table::PatternTable;

    let span = |node| Spanned { node, span: 0..0 };
    for body in class_set_bodies() {
        let expr = span(Expr::Binary {
            lhs: Box::new(span(Expr::FieldRef("message".into()))),
            op: BinaryOp::Matches,
            rhs: Box::new(span(Expr::Literal(LiteralValue::String(body.clone())))),
        });
        let mut ledger = preparer();
        let before = thread_builds();
        assert_eq!(
            PatternTable::prepare([&expr], &mut ledger).err(),
            Some(PatternError::ClassSetSyntax),
            "{body}"
        );
        assert_eq!(
            ledger.build(PatternKind::Regex, &body).err(),
            Some(PatternError::ClassSetSyntax),
            "{body}"
        );
        assert_eq!(ledger.builds(), 0, "{body}");
        assert_eq!(thread_builds(), before, "{body}");
    }
}

/// A `like` or `ilike` operand never becomes class set syntax: its
/// translation escapes every `[` and `]`, so it holds no class at all.
/// A glob's `[…]` copies through to a class, and a glob may nest one
/// (`[a[b]]`), but the batch lane runs a glob as `DuckDB`'s GLOB, never
/// RE2, so the refusal is for regex bodies alone.
#[test]
fn like_operands_never_become_class_set_syntax() {
    let operand = "[a[b]]&&x--y~~[c&&a]%_";
    for kind in [PatternKind::Like, PatternKind::ILike] {
        let regex = kind.regex_source(operand);
        let ast = regex_syntax::ast::parse::Parser::new()
            .parse(&regex)
            .expect("the translation parses");
        assert_eq!(re2::class_set_refusal(&ast), Ok(()), "{kind:?}: {regex}");
        assert!(preparer().build(kind, operand).is_ok(), "{kind:?}");
    }
    assert!(parser::parse(&format!("* | where host like \"{operand}\"")).is_ok());
    assert!(parser::parse("host=[a[b]]*").is_ok());
}

/// `body`'s batch-engine view.
fn batch_view(body: &str) -> String {
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(body)
        .expect("the body parses");
    re2::view(&ast)
}

/// The view writes `\w`, `\d` and `\s` as RE2's ASCII classes, negated as
/// written and inside a bracket too, and each word assertion under
/// `(?-u:…)`, so the `regex` crate compiles it the way RE2 compiles the
/// body. Every other node prints back as written. A nested class in the
/// view is the `regex` crate's union: the view is measured, never run.
#[test]
fn the_batch_view_reads_perl_classes_and_word_assertions_as_ascii() {
    const WORD: &str = r"[\x30-\x39\x41-\x5A\x5F\x61-\x7A]";
    for (body, view) in [
        (r"\w{20}", format!("{WORD}{{20}}")),
        (
            r"(?i)\W\d[\s\D]",
            format!(
                r"(?i)[^{}][\x30-\x39][[\x09-\x0A\x0C-\x0D\x20][^\x30-\x39]]",
                &WORD[1..WORD.len() - 1]
            ),
        ),
        (
            r"\bx\B\b{start}",
            r"(?-u:\b)x(?-u:\B)(?-u:\b{start})".to_string(),
        ),
        // `(?x)` whitespace and comments are no nodes, so they do not
        // print back.
        (r"(?x) a \w # c", format!("(?x)a{WORD}")),
    ] {
        assert_eq!(batch_view(body), view, "{body}");
    }
    for body in [r"\pL[a-z]\x{41}(?P<n>x)", r"(?i)\p{Greek}+?$", "(?s:.)*a|b"] {
        assert_eq!(batch_view(body), body);
    }
}

/// What the view compiles to is what makes it a measure: `\w{20}` is
/// about 1 MiB in Rust and about 2 KiB as RE2 reads it, and a body with
/// no Perl class or word assertion is its own view.
#[test]
fn the_batch_view_compiles_to_what_re2_compiles() {
    let mut ledger = preparer();
    let rust = ledger
        .compiled_bytes(PatternKind::Regex, r"\w{20}")
        .expect("measures");
    let view = ledger.batch_compiled_bytes(r"\w{20}").expect("measures");
    assert!(rust > 900 << 10, "{rust}");
    assert!(view < 4 << 10, "{view}");
    let rust = ledger
        .compiled_bytes(PatternKind::Regex, r"\pL{4}")
        .expect("measures");
    assert_eq!(ledger.batch_compiled_bytes(r"\pL{4}"), Ok(rust));
    // The measure is the smallest limit the build fits.
    assert!(compile(r"\pL{4}", rust).is_ok());
    assert!(compile(r"\pL{4}", rust - 1).is_err());
}

/// Bodies whose program fits 1 MiB in Rust and whose batch-engine view
/// compiles past 512 KiB. FIX6 timed each in RE2's NFA over 65 536
/// Deseret letters: 0.71 s, 0.46 s (review round 3's F7 core under 0
/// wrappers, beside this twin) and 0.45 s a call.
const BATCH_TOO_LARGE: [&str; 3] = [
    r"\B(?:\pL?){23}\PL",
    r"\B(?:\pL|(?:\pL)?|(?:(?:\pL){0,2}){0,2}){0,2}\PL",
    r"\B(?:\pL?){15}\PL",
];

/// A regex body whose program the batch engine compiles past
/// `MAX_BATCH_COMPILED_BYTES` is refused at every regex site, with its
/// own message and hint, by `parse` and by the live plan, after one build.
/// `parse_unbuilt` builds nothing, so it cannot measure the program and
/// admits the body, as it admits one only the 1 MiB limit refuses.
#[test]
fn batch_engine_cap() {
    use crate::pin_scope::PinScope;
    use crate::stream::{StreamPlanError, compile_stream_plan};

    for body in BATCH_TOO_LARGE {
        let mut measure = preparer();
        assert!(
            measure
                .compiled_bytes(PatternKind::Regex, body)
                .expect("measures")
                <= MAX_REGEX_COMPILED_BYTES,
            "{body}"
        );
        assert!(
            measure.batch_compiled_bytes(body).expect("measures") > MAX_BATCH_COMPILED_BYTES,
            "{body}"
        );
        for (site, spell) in REGEX_BODY_SITES {
            let (dsl, covered) = spell(body);
            let (result, builds) = parse_counting_builds(&dsl);
            let errors = result.expect_err(&dsl);
            assert_eq!(builds, 1, "{site} {body}");
            assert_eq!(errors.len(), 1, "{site} {body}: {errors:?}");
            assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE, "{site} {body}");
            assert_eq!(dsl[errors[0].span.clone()], covered, "{site} {body}");
            let hint = errors[0].hint.as_deref().unwrap_or_default();
            assert!(hint.contains(r"\pL"), "{site} {body}: {hint}");
            assert!(parser::parse_unbuilt(&dsl).is_ok(), "{site} {body}");
        }
        for dsl in [
            format!("* | where msg matches /{body}/"),
            format!("* | extract \"(?P<x>{body})\" from msg"),
        ] {
            let query = parser::parse_unbuilt(&dsl).expect("admitted unbuilt");
            let refused = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None).err();
            assert!(
                matches!(
                    refused,
                    Some(StreamPlanError::Pattern(PatternError::BatchCompiledSize))
                ),
                "{dsl}: {refused:?}"
            );
            assert_eq!(
                refused.map(|e| e.to_string()).as_deref(),
                Some(MSG_BATCH_TOO_LARGE)
            );
        }
    }
}

/// The cap sits between counts of one family: eleven `\pL?` compile to
/// a view under 512 KiB and are admitted, twelve are over and refused.
/// A body over 1 MiB in Rust keeps the 1 MiB message, whatever its view.
#[test]
fn batch_engine_cap_edge() {
    let mut measure = preparer();
    let at = r"\B(?:\pL?){11}\PL";
    let over = r"\B(?:\pL?){12}\PL";
    assert!(measure.batch_compiled_bytes(at).expect("measures") <= MAX_BATCH_COMPILED_BYTES);
    assert!(measure.batch_compiled_bytes(over).expect("measures") > MAX_BATCH_COMPILED_BYTES);
    assert!(parser::parse(&format!("msg=/{at}/")).is_ok());
    let errors = parser::parse(&format!("msg=/{over}/")).expect_err(over);
    assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE);

    for body in [r"\B\pL{26}\PL", r"\w{3,30}"] {
        let errors = parser::parse(&format!("msg=/{body}/")).expect_err(body);
        assert_eq!(errors[0].message, MSG_REGEX_TOO_LARGE, "{body}");
    }

    // The plain repeat dsl.md uses: twelve `\pL` fit, thirteen are over.
    assert!(parser::parse(r"msg=/\pL{12}/").is_ok());
    let errors = parser::parse(r"msg=/\pL{13}/").expect_err(r"\pL{13}");
    assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE);
}

/// A batch-view failure stops construction as a 1 MiB failure does:
/// distinct bodies, each over 512 KiB in its view, cost one failed
/// build, and only the first is reported.
#[test]
fn a_failed_batch_build_stops_construction() {
    // `\pL{13}` and a numeral: distinct bodies, each refused by the view.
    let sites: Vec<String> = (0..8).map(|i| format!(r"/\pL{{13}}{i}/")).collect();
    let input = format!("f={}", sites.join(","));
    let (result, builds) = parse_counting_builds(&input);
    let errors = result.expect_err("every site is over the view cap");
    assert_eq!(builds, 1, "one failed build");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE);
    let first = "f=".len()..("f=".len() + sites[0].len());
    assert_eq!(errors[0].span, first);
}

/// Every example FIX6 pinned at the cap is admitted: what dsl.md and the
/// calibration corpus write, `\w` and `\d` included, because RE2 reads
/// them as ASCII. `\w{3,30}` is not: its Rust program is over 1 MiB,
/// whatever RE2 compiles.
#[test]
fn every_pinned_example_fits_the_batch_engine() {
    for body in [
        "[0-9a-f]{64}",
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        r"\d{1,3}(\.\d{1,3}){3}",
        r"(?i)^\w{3}\s+\d+\s+[\d:]+\s+(\S+)\s+(\w+)\[(\d+)\]",
        r"(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}",
        r"\w{10}",
        r"\w+@\w+\.\w{2,10}",
        r"[\w.+-]+@[\w-]+\.[\w.]{2,10}",
        "[A-Za-z0-9_]{3,30}",
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})",
        "([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}",
        "(?i)(error|fail(ed|ure)?|exception|timeout|refused|denied)",
        r#"(?P<key>\w+)=(?P<val>"[^"]*"|\S+)"#,
        r"^(?P<ip>\d{1,3}(\.\d{1,3}){3}) - (?P<user>\S+) \[(?P<ts>[^\]]+)\] .(?P<method>[A-Z]+) (?P<path>\S+) HTTP\d\.\d. (?P<status>\d{3}) (?P<bytes>\d+|-)",
        r"(\w+\s+){5}\w+",
        r"(?P<ip>\d+\.\d+\.\d+\.\d+)",
        r"\w+",
        r"\w{20}",
        r"\pL",
        r"\pL{12}",
    ] {
        let result = parser::parse(&format!("msg=/{body}/"));
        assert!(result.is_ok(), "{body}: {result:?}");
    }
    let errors = parser::parse(r"msg=/\w{3,30}/").expect_err("over 1 MiB in Rust");
    assert_eq!(errors[0].message, MSG_REGEX_TOO_LARGE);
}

/// One build prepares one pattern. A regex body whose view differs from it
/// compiles twice, the program and the view, and both counters count the
/// pair once. A view over the cap stops the ledger as a program over
/// 1 MiB does.
#[test]
fn a_build_compiles_the_program_and_its_view_once() {
    let mut ledger = preparer();
    let before = thread_builds();
    for body in [r"\w{20}", r"(?P<code>\d+)", "a+", r"\bx\b"] {
        assert!(ledger.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(ledger.builds(), 4);
    assert_eq!(thread_builds() - before, 4);

    let mut refusal = |text: &str| ledger.build(PatternKind::Regex, text).err();
    assert_eq!(
        refusal(BATCH_TOO_LARGE[2]),
        Some(PatternError::BatchCompiledSize)
    );
    assert_eq!(refusal("b"), Some(PatternError::Stopped));
    assert_eq!(
        refusal(BATCH_TOO_LARGE[2]),
        Some(PatternError::BatchCompiledSize)
    );
    assert_eq!(ledger.builds(), 5);
}

/// The stop after a size failure covers the batch-engine cap: 256
/// distinct bodies that each compile past it in RE2 cost one failed
/// build, and the first carries the error.
#[test]
fn a_batch_size_failure_stops_construction() {
    let n = parser::MAX_QUERY_PATTERNS;
    let distinct: Vec<String> = (0..n)
        .map(|i| format!(r"/\B(?:\pL?){{15}}\PL{i}/"))
        .collect();
    let input = format!("f={}", distinct.join(","));
    assert!(input.len() <= parser::MAX_QUERY_LEN);
    let (result, builds) = parse_counting_builds(&input);
    let errors = result.expect_err("over the cap");
    assert_eq!(builds, 1);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE);
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

/// An `extract` body of `n` named groups, numbered from `from`, each two
/// positions, the group and its atom: 32 of them fill the repeat budget
/// and fit the body limit with room to spare.
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
/// their own toward the cap. Exactly the cap, spread across bodies that
/// each fit the body limit and the repeat budget, is admitted, and one charge more is refused over the
/// stage that carries it.
#[test]
fn extract_groups_count_toward_the_pattern_cap() {
    let n = parser::MAX_QUERY_PATTERNS;
    let stages: Vec<String> = (0..8)
        .map(|k| format!("| extract \"{}\"", named_groups(k * 32, 32)))
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

/// An `extract` body, its batch copy, and each named group's index there.
type MixedExtract = (&'static str, &'static str, &'static [(&'static str, u32)]);

/// Mixed `extract` bodies: unnamed groups before, around and beside named
/// ones.
const MIXED_EXTRACTS: &[MixedExtract] = &[
    (
        "(a|b)(?P<x>b)-(?P<y>c)",
        "(?:a|b)(?P<x>b)-(?P<y>c)",
        &[("x", 1), ("y", 2)],
    ),
    (
        "((((((((((?P<x>j))))))))))",
        "(?:(?:(?:(?:(?:(?:(?:(?:(?:(?P<x>j))))))))))",
        &[("x", 1)],
    ),
    (
        "(a)(b)(c)(d)(e)(f)(g)(h)(i)(?P<x>j)",
        "(?:a)(?:b)(?:c)(?:d)(?:e)(?:f)(?:g)(?:h)(?:i)(?P<x>j)",
        &[("x", 1)],
    ),
    (
        r"(?<x>(\d)|(?i:k))(?P<y>(?:c|(d)))+",
        r"(?<x>(?:\d)|(?i:k))(?P<y>(?:c|(?:d)))+",
        &[("x", 1), ("y", 2)],
    ),
];

/// `DuckDB`'s scalar `regexp_extract` reads one group by index, 0 to 9.
/// The batch copy of an `extract` body has every unnamed group made
/// non-capturing, so its named groups are numbered 1 to n in order,
/// whatever unnamed groups sit before or around them. A body with no
/// unnamed group binds as written.
#[test]
fn a_batch_extract_numbers_named_groups_among_themselves() {
    for (body, regex, named) in MIXED_EXTRACTS {
        let batch = batch_extract(body).expect("the body parses");
        assert_eq!(batch.regex, *regex, "{body}");
        let got: Vec<(&str, u32)> = batch
            .captures
            .iter()
            .map(|c| (c.name.as_str(), c.index))
            .collect();
        assert_eq!(got, *named, "{body}");
    }
    for body in [
        r"(?P<ip>\d+\.\d+\.\d+\.\d+)",
        r#"(?P<key>\w+)=(?P<val>"[^"]*"|\S+)"#,
        r"(?i)(?P<code>[A-Z]{2,}) \x{1F600}\pL\p{Greek}[^\]a-z\w]+?$",
    ] {
        assert_eq!(batch_extract(body).expect("parses").regex, body);
    }
    assert!(batch_extract("(").is_err());
}

/// A group's kind never changes what a regex matches, so the batch copy
/// matches what the body matches, and each named group reads the same
/// text.
#[test]
fn a_batch_extract_matches_what_its_body_matches() {
    let subjects = ["ab-c", "j", "abcdefghij", "7cdcd", "Kc", "x7c", "", "bb-cc"];
    let mut ledger = preparer();
    for (body, _, named) in MIXED_EXTRACTS {
        let batch = batch_extract(body).expect("the body parses");
        let written = ledger.build(PatternKind::Regex, body).expect("builds");
        let bound = ledger
            .build(PatternKind::Regex, &batch.regex)
            .expect("builds");
        for subject in subjects {
            let read = |pattern: &Pattern| {
                pattern.captures(subject).map(|captures| {
                    named
                        .iter()
                        .map(|(name, _)| captures.name(name).map(|m| m.range()))
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(read(&written), read(&bound), "{body} over {subject:?}");
        }
    }
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
    assert_eq!(refusal(r"\w{64}"), Some(PatternError::CompiledSize));
    assert_eq!(refusal("b"), Some(PatternError::Stopped));
    assert_eq!(refusal(r"\w{64}"), Some(PatternError::CompiledSize));
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
