// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{BTreeMap, BTreeSet};
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
/// The crate's name in any spelling: a path, a `use`, or an alias.
const AUTOMATA: &str = concat!("regex_", "automata");
const PREPARER_NEW: &str = concat!("Preparer", "::new");
const PREPARER_CHECK: &str = concat!("Preparer", "::check");
const PARSE_TURBOFISH: &str = concat!("parse", "::<…Regex…>");
const QUALIFIED: &str = concat!("<…Reg", "ex… as …>::from_str or ::try_from");
const LET_PARSE: &str = concat!("let …: …Reg", "ex… = ….parse()");
const AUTOMATA_BUILD: &str = concat!("regex_", "automata build");
const POOL: &str = concat!("util::", "pool");

const NEEDLES: [&str; 10] = [
    REGEX_NEW,
    BUILDER_NEW,
    SET_NEW,
    SET_BUILDER_NEW,
    FROM_STR,
    TRY_FROM,
    AUTOMATA,
    PREPARER_NEW,
    PREPARER_CHECK,
    POOL,
];

/// A bare `.build(` is too common a name to count; the builder it closes is
/// named in a path the [`AUTOMATA`] needle counts, and clippy resolves the
/// call itself.
const AUTOMATA_BUILDERS: [&str; 5] = [
    concat!("build_from", "_hir("),
    concat!("build_many_from", "_hir("),
    concat!("build_many", "("),
    concat!("build_from", "_nfa("),
    concat!("new_from", "_nfa("),
];

fn automata_builds(text: &str) -> usize {
    AUTOMATA_BUILDERS
        .iter()
        .map(|needle| text.matches(needle).count())
        .sum()
}

/// Three tests build outside the owner's build site on purpose, each
/// under an `#[expect]` of its own. `pattern/tests.rs` builds a twin with
/// the lazy DFA off, to show the owner's build kept it on.
/// `tests/regex_memory.rs` measures regex-automata itself, the facts the
/// owner's charge relies on. The server's gate test checks a regex that
/// lives in Vector config, not query text. `pattern/charge.rs` names the
/// crate to read a profile off an NFA the build site compiled, and builds
/// nothing.
const INVENTORY: &[(&str, &str, usize)] = &[
    ("crates/trawl-core/src/filter.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/parser/mod.rs", PREPARER_CHECK, 1),
    (
        "crates/trawl-core/src/parser/primitives.rs",
        PREPARER_CHECK,
        1,
    ),
    ("crates/trawl-core/src/pattern/charge.rs", AUTOMATA, 2),
    ("crates/trawl-core/src/pattern/mod.rs", AUTOMATA, 5),
    ("crates/trawl-core/src/pattern/mod.rs", AUTOMATA_BUILD, 2),
    ("crates/trawl-core/src/pattern/table.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/pattern/tests.rs", PREPARER_CHECK, 2),
    ("crates/trawl-core/src/pattern/tests.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/src/pattern/tests.rs", AUTOMATA_BUILD, 1),
    ("crates/trawl-core/src/stream.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/tests/eval_no_panic.rs", PREPARER_NEW, 2),
    ("crates/trawl-core/tests/filter_parity.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-core/tests/pin_stage_parity.rs",
        PREPARER_NEW,
        1,
    ),
    ("crates/trawl-core/tests/regex_memory.rs", AUTOMATA, 4),
    ("crates/trawl-core/tests/regex_memory.rs", PREPARER_NEW, 1),
    ("crates/trawl-core/tests/regex_memory.rs", AUTOMATA_BUILD, 4),
    ("crates/trawl-core/tests/scalar_parity.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-core/tests/tree_depth_stack.rs",
        PREPARER_NEW,
        1,
    ),
    ("crates/trawl-engine/src/executor.rs", PREPARER_NEW, 4),
    ("crates/trawl-engine/src/post_process.rs", PREPARER_NEW, 1),
    ("crates/trawl-engine/tests/duckdb_probe.rs", PREPARER_NEW, 1),
    ("crates/trawl-engine/tests/glob_parity.rs", PREPARER_NEW, 1),
    ("crates/trawl-engine/tests/re2_memory.rs", PREPARER_NEW, 3),
    ("crates/trawl-engine/tests/regex_cost.rs", PREPARER_NEW, 1),
    ("crates/trawl-server/src/handlers.rs", PREPARER_NEW, 1),
    ("crates/trawl-server/src/live.rs", PREPARER_NEW, 1),
    (
        "crates/trawl-server/tests/live_chunk_calibration.rs",
        PREPARER_NEW,
        1,
    ),
    (
        "crates/trawl-server/tests/query_resources.rs",
        PREPARER_NEW,
        1,
    ),
    (
        "crates/trawl-server/tests/vector_service_gate_contract.rs",
        AUTOMATA,
        1,
    ),
    (
        "crates/trawl-server/tests/vector_service_gate_contract.rs",
        REGEX_NEW,
        1,
    ),
];

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("trawl-core sits two levels under the workspace root")
}

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

fn turbofish_regex_parses(text: &str) -> usize {
    text.match_indices(concat!("parse", "::<"))
        .filter(|(at, opener)| {
            let generic = &text[at + opener.len()..];
            generic[..generic.find('>').unwrap_or(generic.len())].contains("Regex")
        })
        .count()
}

fn qualified_regex_conversions(text: &str) -> usize {
    [">::from_str", ">::try_from"]
        .into_iter()
        .flat_map(|tail| text.match_indices(tail))
        .filter(|&(at, _)| qualified_self_type(&text[..=at]).is_some_and(|ty| ty.contains("Regex")))
        .count()
}

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

fn char_literal_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().skip(1);
    let (_, first) = chars.next()?;
    let (end, close) = match first {
        '\'' => return None,
        '\\' => {
            chars.next()?;
            chars.find(|&(_, c)| !(c.is_ascii_hexdigit() || c == '{' || c == '}'))?
        }
        _ => chars.next()?,
    };
    (close == '\'').then_some(end + 1)
}

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

fn lone_colon(binding: &str) -> Option<usize> {
    let bytes = binding.as_bytes();
    (0..bytes.len()).find(|&i| {
        bytes[i] == b':'
            && bytes.get(i + 1) != Some(&b':')
            && i.checked_sub(1).map(|p| bytes[p]) != Some(b':')
    })
}

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
    sites.insert(AUTOMATA_BUILD, automata_builds(&code));
    sites.retain(|_, n| *n > 0);
    sites
}

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

#[test]
fn the_inventory_counts_automata_spellings() {
    let sites: &[(&str, &[(&str, usize)])] = &[
        (
            concat!("let re = regex_", "automata::meta::Reg", "ex::new(body)?;"),
            &[(AUTOMATA, 1), (REGEX_NEW, 1)],
        ),
        (concat!("use regex_", "automata as ra;"), &[(AUTOMATA, 1)]),
        (
            concat!(
                "meta::Builder::new().configure(c).build_from",
                "_hir(&hir)?"
            ),
            &[(AUTOMATA_BUILD, 1)],
        ),
        (
            concat!("meta::Builder::new().build_", "many(&patterns)?"),
            &[(AUTOMATA_BUILD, 1)],
        ),
        (
            concat!("thompson::Compiler::new().build_many_from", "_hir(&hirs)?"),
            &[(AUTOMATA_BUILD, 1)],
        ),
        (
            concat!("hybrid::dfa::Builder::new().build_from", "_nfa(nfa)?"),
            &[(AUTOMATA_BUILD, 1)],
        ),
        (
            concat!("PikeVM::new_from", "_nfa(nfa)?"),
            &[(AUTOMATA_BUILD, 1)],
        ),
        (
            concat!("let pool = util::po", "ol::Pool::new(create);"),
            &[(POOL, 1)],
        ),
    ];
    for (source, spellings) in sites {
        assert_eq!(
            construction_sites(source),
            spellings.iter().copied().collect::<BTreeMap<_, _>>(),
            "{source}"
        );
    }
}

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

fn parse_counting_builds(
    input: &str,
) -> (Result<crate::ast::Query, Vec<parser::ParseError>>, usize) {
    let before = thread_builds();
    let result = parser::parse(input);
    (result, thread_builds() - before)
}

fn copies_of(body: &str, copies: u64) -> String {
    let element = format!("/{body}/");
    let copies = usize::try_from(copies).expect("a small count");
    format!("f={}", vec![element.as_str(); copies].join(","))
}

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

/// `{0,1}` is longer to write, so 100 of its wrappers fill the body limit.
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

fn batch_view(body: &str) -> String {
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(body)
        .expect("the body parses");
    re2::view(&ast)
}

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
    let hir = parse_hir(r"\pL{4}", PatternKind::Regex).expect("parses");
    assert!(compile(&hir, rust).is_ok());
    assert!(compile(&hir, rust - 1).is_err());
}

/// Bodies whose program fits 1 MiB in Rust and whose batch-engine view
/// compiles past 512 KiB.
const BATCH_TOO_LARGE: [&str; 3] = [
    r"\B(?:\pL?){23}\PL",
    r"\B(?:\pL|(?:\pL)?|(?:(?:\pL){0,2}){0,2}){0,2}\PL",
    r"\B(?:\pL?){15}\PL",
];

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

    assert!(parser::parse(r"msg=/\pL{12}/").is_ok());
    let errors = parser::parse(r"msg=/\pL{13}/").expect_err(r"\pL{13}");
    assert_eq!(errors[0].message, MSG_BATCH_TOO_LARGE);
}

#[test]
fn a_failed_batch_build_stops_construction() {
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

    let over = format!("{at_cap} | extract \"(a)\"");
    let error = pattern_cap_refusal(&over);
    assert!(error.message.contains(&format!("({} ", n + 1)), "{error:?}");
    assert_eq!(&over[error.span], "extract \"(a)\"");

    let globs: Vec<String> = (0..n - 2).map(|i| format!("g{i}*")).collect();
    let globs = format!("f={}", globs.join(","));
    let two = "| extract \"(a)(?P<x>b)(c)(?P<y>d)\"";
    assert!(parser::parse(&format!("{globs} {two}")).is_ok());
    let three = "| extract \"(a)(?P<x>b)(?P<y>c)(?P<z>d)\"";
    let over = format!("{globs} {three}");
    let error = pattern_cap_refusal(&over);
    assert_eq!(&over[error.span], &three[2..]);
    assert!(parser::parse(&format!("{globs} {two} | extract kv")).is_ok());
}

/// An `extract` body, its batch copy, and each named group's index there.
type MixedExtract = (&'static str, &'static str, &'static [(&'static str, u32)]);

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
                pattern
                    .captures(subject)
                    .expect("never cancelled")
                    .map(|captures| {
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

#[test]
fn a_ledger_builds_each_pattern_once() {
    let mut ledger = preparer();
    let before = thread_builds();
    assert!(ledger.build(PatternKind::Regex, "a+").is_ok());
    assert!(ledger.build(PatternKind::Regex, "a+").is_ok());
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

#[test]
fn a_poisoned_cache_starts_afresh() {
    let mut ledger = preparer();
    let pattern = ledger
        .build(PatternKind::Regex, r"(?P<user>\w+)@(?P<host>[a-z]+)\.org")
        .expect("builds");
    let other = ledger
        .build(PatternKind::Regex, r"(?P<digit>\d)")
        .expect("builds");
    let held = pattern.clone();
    let panicked = std::thread::spawn(move || {
        let mut cache = held.cache();
        *cache = other.0.regex.create_cache();
        panic!("a search panics while it holds the cache");
    })
    .join();
    assert!(panicked.is_err());
    assert!(pattern.0.cache.is_poisoned());

    let subject = "mail ana@example.org today";
    assert_eq!(pattern.is_match(subject), Ok(true));
    assert!(!pattern.0.cache.is_poisoned());
    assert_eq!(pattern.is_match("no address here"), Ok(false));
    let captures = pattern
        .captures(subject)
        .expect("never cancelled")
        .expect("matches");
    assert_eq!(captures.name("user").map(|g| g.as_str()), Some("ana"));
    assert_eq!(captures.name("host").map(|g| g.range()), Some(9..16));
    assert!(captures.name("nope").is_none());
}

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

fn unmetered() -> Preparer {
    let mut ledger = preparer();
    ledger.budget = usize::MAX;
    ledger
}

fn calibration_bodies() -> impl Iterator<Item = &'static str> {
    charge::LOG_REGEXES
        .iter()
        .chain(charge::UNICODE_REGEXES)
        .copied()
}

#[expect(
    clippy::disallowed_methods,
    reason = "a twin with the lazy DFA off, built only to tell whether the owner's build kept it on"
)]
fn without_lazy_dfa(body: &str, capacity: usize) -> meta::Regex {
    let hir = parse_hir(&batch_view(body), PatternKind::Regex).expect("the view parses");
    meta::Builder::new()
        .configure(meta_config(MAX_REGEX_COMPILED_BYTES, capacity).hybrid(false))
        .build_from_hir(&hir)
        .expect("the view compiles under 1 MiB")
}

/// Below a lazy DFA's minimum capacity meta turns it off without an
/// error, and the matching falls back to the Pike VM. A fresh cache sums
/// every engine's cache, and one with the lazy DFA off holds none of its
/// own, so a fresh cache larger than its twin's, built with the lazy DFA
/// off, shows it on (`tests/regex_memory.rs`,
/// `hybrid_below_minimum_is_silently_disabled`).
#[test]
fn lazy_dfa_stays_enabled() {
    for body in calibration_bodies() {
        let pattern = preparer()
            .build(PatternKind::Regex, body)
            .expect("one pattern fits a query");
        let capacity = pattern.0.charge.capacity;
        let fresh = pattern.0.regex.create_cache().memory_usage();
        let twin = without_lazy_dfa(body, capacity)
            .create_cache()
            .memory_usage();
        assert!(
            fresh > twin,
            "{body}: the lazy DFA is off at capacity {capacity} (fresh cache {fresh}, twin {twin})"
        );
    }
}

/// Seven distinct bodies of the `(\pL{12})` class, each its own batch-engine
/// view, near the 512 KiB batch compiled-size limit, and each charging
/// about 4.3 MiB (4 474 264 bytes on the reference host). Six fit one
/// query's budget beside a build transient, and the seventh does not.
const HEAVY: [&str; 7] = [
    r"(\pL{12})0",
    r"(\pL{12})1",
    r"(\pL{12})2",
    r"(\pL{12})3",
    r"(\pL{12})4",
    r"(\pL{12})5",
    r"(\pL{12})6",
];

#[test]
fn query_budget_exact_and_one_over() {
    let bodies = charge::LOG_REGEXES;
    let (last, rest) = bodies.split_last().expect("the corpus has patterns");
    let mut measure = unmetered();
    for body in bodies {
        measure.build(PatternKind::Regex, body).expect("builds");
    }
    let sum = measure.charged();
    let last_charge = measure
        .build(PatternKind::Regex, last)
        .expect("built")
        .0
        .charge
        .total();
    assert!(
        sum + BUILD_TRANSIENT <= REGEX_QUERY_BUDGET,
        "the corpus is one query: {sum} bytes and a transient, budget {REGEX_QUERY_BUDGET}"
    );
    let mut shipped = preparer();
    for body in bodies {
        assert!(shipped.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(shipped.charged(), sum);

    let refused = PatternError::Resources(RegexRefusal::Budget);
    let (over, under) = HEAVY.split_last().expect("HEAVY has bodies");
    let mut heavy = preparer();
    for body in under {
        assert_eq!(batch_view(body), *body, "a heavy body runs as written");
        assert!(heavy.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(
        heavy.build(PatternKind::Regex, over).err(),
        Some(refused.clone()),
        "the last heavy body is over the budget"
    );

    let mut exact = preparer();
    exact.budget = sum + BUILD_TRANSIENT;
    for body in bodies {
        assert!(exact.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(exact.charged(), sum);

    let mut short = preparer();
    short.budget = sum + BUILD_TRANSIENT - 1;
    for body in rest {
        assert!(short.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(
        short.build(PatternKind::Regex, last).err(),
        Some(refused.clone())
    );
    assert_eq!(short.builds(), bodies.len());
    assert_eq!(short.charged(), sum - last_charge);
    assert_eq!(
        short.build(PatternKind::Regex, "a").err(),
        Some(refused.clone())
    );
    assert_eq!(
        short.build(PatternKind::Regex, last).err(),
        Some(refused.clone())
    );
    assert!(short.build(PatternKind::Regex, rest[0]).is_ok());
    assert_eq!(
        short.builds(),
        bodies.len(),
        "nothing builds after the stop"
    );

    let sentence = refused.to_string();
    assert_eq!(sentence, MSG_REGEX_BUDGET);
    assert!(bodies.iter().all(|body| !sentence.contains(body)));
}

#[test]
fn check_mode_meters_nothing() {
    use crate::filter::CompiledFilter;
    use crate::schema::FieldTypes;

    let (over, under) = HEAVY.split_last().expect("HEAVY has bodies");
    let mut metered = preparer();
    for body in under {
        assert!(metered.build(PatternKind::Regex, body).is_ok(), "{body}");
    }
    assert_eq!(
        metered.build(PatternKind::Regex, over).err(),
        Some(PatternError::Resources(RegexRefusal::Budget))
    );
    let mut check = Preparer::check();
    for body in HEAVY {
        assert_eq!(check.check(PatternKind::Regex, body), Ok(()), "{body}");
    }
    assert_eq!(check.builds(), HEAVY.len());

    let query =
        parser::parse(&format!("msg=/{}/", HEAVY.join("/,/"))).expect("parse meters no budget");
    let refused = CompiledFilter::compile(&query.search, &FieldTypes::new()).err();
    assert_eq!(
        refused,
        Some(crate::filter::FilterError::RegexResources(
            RegexRefusal::Budget
        ))
    );
    assert_eq!(
        refused.map(|e| e.to_string()).as_deref(),
        Some(MSG_REGEX_BUDGET),
        "the fixed sentence, quoting no pattern"
    );

    let sixty_four_words = r"\w".repeat(64);
    let mut bodies: Vec<String> = calibration_bodies().map(str::to_owned).collect();
    bodies.extend(BATCH_TOO_LARGE.map(str::to_owned));
    bodies.extend(class_set_bodies());
    bodies.extend(
        [
            r"\w{64}",
            r"\w{3,30}",
            r"\p{Nope}",
            "(",
            r"\pL{12}",
            r"\pL{13}",
        ]
        .map(str::to_owned),
    );
    bodies.push(sixty_four_words);
    for kind in [
        PatternKind::Regex,
        PatternKind::Like,
        PatternKind::ILike,
        PatternKind::Glob,
    ] {
        for body in &bodies {
            let checked = Preparer::check().check(kind, body);
            let built = unmetered().build(kind, body).map(|_| ());
            assert_eq!(checked, built, "{kind:?} {body}");
        }
    }
}

/// The most any pattern of the corpus or the Unicode table may charge for
/// its program, in bytes. The largest on the reference host is
/// `\pL{12}`'s, 590 160 bytes. A prefilter is a small part of any
/// program here, so another CPU's choice of one stays well under the
/// ceiling, and a change that makes programs grow does not.
const PROGRAM_CEILING: usize = 3 << 19;

/// The program term is not in the snapshot. It is the program's
/// `memory_usage`, which counts the prefilter meta picked, and that choice
/// follows the CPU's vector support: AVX2 on the reference host, and
/// perhaps not on a CI runner. The program term is held under
/// [`PROGRAM_CEILING`] instead.
#[test]
fn corpus_charges() {
    use std::fmt::Write as _;

    let mut table = String::from("capacity  min_fwd     lazy    pike captures  pattern\n");
    for body in calibration_bodies() {
        let pattern = preparer()
            .build(PatternKind::Regex, body)
            .expect("one pattern fits a query");
        let charge = pattern.0.charge;
        let (_, profile) = compile(
            &parse_hir(&batch_view(body), PatternKind::Regex).expect("the view parses"),
            MAX_BATCH_COMPILED_BYTES,
        )
        .expect("the view compiles");
        writeln!(
            table,
            "{:>8} {:>8} {:>8} {:>7} {:>8}  {body}",
            charge.capacity, profile.min_forward, charge.lazy, charge.pike, charge.captures,
        )
        .expect("writes to a String");
        assert!(
            charge.program <= PROGRAM_CEILING,
            "{body}: its program charges {} bytes, over the ceiling of {PROGRAM_CEILING}",
            charge.program
        );
        assert!(charge.program > charge::PROGRAM_OVERHEAD, "{body}");
    }
    insta::assert_snapshot!(table);
}

fn bytes(n: usize) -> u64 {
    u64::try_from(n).expect("a size fits in u64")
}

fn reserving(limit: u64) -> (Preparer, &'static memory::RegexReservation) {
    let reservation = memory::private_reservation(limit);
    let mut ledger = unmetered();
    ledger.reservation = reservation;
    (ledger, reservation)
}

#[test]
fn the_re2_allowance_is_held_until_the_ledger_drops() {
    let (mut ledger, pool) = reserving(1000);
    ledger.cap = 600;
    assert_eq!(ledger.hold_re2(0), Ok(()));
    assert_eq!(pool.holds(), 0, "no allowance holds nothing");
    assert_eq!(ledger.hold_re2(500), Ok(()));
    assert_eq!((ledger.re2_held(), pool.reserved()), (500, 500));
    assert_eq!(ledger.hold_re2(601), Err(RegexRefusal::Budget));
    assert_eq!((ledger.re2_held(), pool.reserved()), (500, 500));
    let other = pool.try_hold(450).expect("fits beside the allowance");
    assert_eq!(ledger.hold_re2(600), Err(RegexRefusal::Reservation));
    assert_eq!((ledger.re2_held(), pool.reserved()), (500, 950));
    assert_eq!(ledger.hold_re2(300), Ok(()));
    assert_eq!((ledger.re2_held(), pool.reserved()), (300, 750));
    drop(other);
    drop(ledger);
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (2, 2));
}

#[test]
fn the_query_cap_counts_the_allowance_beside_the_charges() {
    let body = r"\d{1,3}(\.\d{1,3}){3}";
    let charge = bytes(
        unmetered()
            .build(PatternKind::Regex, body)
            .expect("builds")
            .charge()
            .total(),
    );
    let exact = 100 + charge + bytes(BUILD_TRANSIENT);
    for (cap, admitted) in [(exact, true), (exact - 1, false)] {
        let (mut ledger, pool) = reserving(u64::MAX);
        ledger.cap = cap;
        ledger.hold_re2(100).expect("the allowance fits the cap");
        let built = ledger.build(PatternKind::Regex, body);
        if admitted {
            assert!(built.is_ok(), "cap {cap}: {built:?}");
            assert_eq!(pool.reserved(), 100 + charge);
        } else {
            assert_eq!(
                built.err(),
                Some(PatternError::Resources(RegexRefusal::Budget)),
                "cap {cap}"
            );
            assert_eq!(pool.reserved(), 100);
        }
    }
}

#[test]
fn a_build_past_the_cap_is_refused_before_it_holds_a_transient() {
    let transient = bytes(BUILD_TRANSIENT);
    let cap = 100 + transient - 1;
    for limit in [u64::MAX, cap] {
        let (mut ledger, pool) = reserving(limit);
        ledger.cap = cap;
        ledger.hold_re2(100).expect("the allowance fits the cap");
        let (holds, builds) = (pool.holds(), thread_builds());
        let built = ledger.build(PatternKind::Regex, "a+");
        assert_eq!(
            built.err(),
            Some(PatternError::Resources(RegexRefusal::Budget)),
            "limit {limit}"
        );
        assert_eq!(pool.holds(), holds, "limit {limit}: no transient held");
        assert_eq!(thread_builds(), builds, "limit {limit}: nothing built");
        assert_eq!(pool.reserved(), 100, "limit {limit}");
    }
}

#[test]
fn a_hold_is_released_once_on_drop() {
    let (mut ledger, pool) = reserving(u64::MAX);
    let pattern = ledger
        .build(PatternKind::Regex, r"\d{1,3}(\.\d{1,3}){3}")
        .expect("builds");
    let charge = bytes(pattern.charge().total());
    assert_eq!(pool.reserved(), charge);
    assert_eq!(bytes(ledger.charged()), charge);
    let clone = pattern.clone();
    drop(ledger);
    drop(pattern);
    assert_eq!(pool.reserved(), charge, "a clone still holds the charge");
    drop(clone);
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (1, 1));
}

#[test]
fn a_hold_is_released_on_unwind() {
    let (mut ledger, pool) = reserving(u64::MAX);
    let pattern = ledger.build(PatternKind::Regex, "a+").expect("builds");
    drop(ledger);
    assert_eq!(pool.reserved(), bytes(pattern.charge().total()));
    let panicked = std::thread::spawn(move || {
        let _cache = pattern.cache();
        panic!("a search panics while it holds the cache");
    })
    .join();
    assert!(panicked.is_err());
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (1, 1));
}

#[test]
fn a_refused_build_leaves_the_reservation_where_it_started() {
    let transient = bytes(BUILD_TRANSIENT);
    let reservation = PatternError::Resources(RegexRefusal::Reservation);

    let (mut ledger, pool) = reserving(transient - 1);
    assert_eq!(
        ledger.build(PatternKind::Regex, "a+").err(),
        Some(reservation.clone())
    );
    assert_eq!(ledger.builds(), 0, "no build ran");
    assert_eq!(
        ledger.build(PatternKind::Regex, "b+").err(),
        Some(reservation.clone()),
        "the ledger stopped"
    );
    assert_eq!((pool.reserved(), pool.holds()), (0, 0));

    let heavy = r"(\pL{12})";
    let charge = unmetered()
        .build(PatternKind::Regex, heavy)
        .expect("builds")
        .charge()
        .total();
    assert!(charge > BUILD_TRANSIENT, "{heavy} charges {charge}");
    let (mut ledger, pool) = reserving(transient);
    assert_eq!(
        ledger.build(PatternKind::Regex, heavy).err(),
        Some(reservation.clone())
    );
    assert_eq!((ledger.builds(), ledger.charged()), (1, 0));
    assert_eq!(
        ledger.build(PatternKind::Regex, "a").err(),
        Some(reservation.clone())
    );
    assert_eq!(ledger.builds(), 1, "nothing builds after the stop");
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (1, 1));

    let (mut ledger, pool) = reserving(u64::MAX);
    let kept = ledger.build(PatternKind::Regex, r"\w{20}").expect("fits");
    let before = pool.reserved();
    ledger.budget = ledger.charged() + BUILD_TRANSIENT;
    assert_eq!(
        ledger.build(PatternKind::Regex, r"\w{19}").err(),
        Some(PatternError::Resources(RegexRefusal::Budget))
    );
    assert_eq!(pool.reserved(), before);
    drop((kept, ledger));
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (2, 2));

    let (mut ledger, pool) = reserving(u64::MAX);
    assert!(matches!(
        ledger.build(PatternKind::Regex, r"\p{Nope}"),
        Err(PatternError::Syntax(_))
    ));
    assert_eq!(
        ledger.build(PatternKind::Regex, r"\w{64}").err(),
        Some(PatternError::CompiledSize)
    );
    assert_eq!(
        ledger.build(PatternKind::Regex, "[a&&b]").err(),
        Some(PatternError::ClassSetSyntax)
    );
    assert_eq!(pool.reserved(), 0);
    assert_eq!(
        (pool.holds(), pool.releases()),
        (2, 2),
        "the two builds held a transient each; the class set refusal held none"
    );
}

#[test]
fn one_ledger_holds_a_shared_pattern_once() {
    use crate::filter::CompiledFilter;
    use crate::pin_scope::PinScope;
    use crate::schema::FieldTypes;
    use crate::stream::compile_stream_plan_with;

    let query = parser::parse(
        r#"message=/time(out)?/ | where message matches "time(out)?" | where host like "web-%""#,
    )
    .expect("the query parses");
    let (mut ledger, pool) = reserving(u64::MAX);
    let filter = CompiledFilter::compile_with(&query.search, &FieldTypes::new(), &mut ledger)
        .expect("the filter compiles");
    let plan = compile_stream_plan_with(&query.pipeline, &PinScope::unpinned(), None, &mut ledger)
        .expect("the plan compiles");
    assert_eq!(ledger.builds(), 2, "the shared regex once, the like once");
    let shared = ledger
        .build(PatternKind::Regex, "time(out)?")
        .expect("held")
        .charge()
        .total();
    let like = ledger
        .build(PatternKind::Like, "web-%")
        .expect("held")
        .charge()
        .total();
    assert_eq!(ledger.charged(), shared + like);
    assert_eq!(pool.reserved(), bytes(shared + like));
    drop(ledger);
    assert_eq!(
        pool.reserved(),
        bytes(shared + like),
        "the query holds what it charged"
    );
    drop((filter, plan));
    assert_eq!(pool.reserved(), 0);
    assert_eq!((pool.holds(), pool.releases()), (2, 2));
}

#[test]
fn regex_class_resource_ownership() {
    let body = r"\w{20}";
    let view = batch_view(body);
    assert_ne!(view, body);
    let charge = unmetered()
        .build(PatternKind::Regex, body)
        .expect("builds")
        .charge();
    let of_view = unmetered()
        .build(PatternKind::Regex, &view)
        .expect("the view builds")
        .charge();
    assert_eq!(charge, of_view, "the body charges for its view's program");
    let (regex, profile) = compile(
        &parse_hir(body, PatternKind::Regex).expect("the body parses"),
        MAX_REGEX_COMPILED_BYTES,
    )
    .expect("the body compiles under 1 MiB");
    let as_written = Charge::new(&profile, regex.memory_usage());
    drop(regex);
    assert!(
        charge.total() < as_written.total(),
        "the view charges {} bytes, the body as written {}",
        charge.total(),
        as_written.total()
    );

    let (mut ledger, pool) = reserving(u64::MAX);
    let start = pool.reserved();
    let first = ledger.build(PatternKind::Regex, body).expect("builds");
    let again = ledger.build(PatternKind::Regex, body).expect("held");
    assert_eq!(ledger.builds(), 1, "one build for the shared body");
    assert_eq!(ledger.charged(), charge.total());
    assert_eq!(pool.reserved(), start + bytes(charge.total()));
    assert_eq!(pool.holds(), 1);
    drop((ledger, first, again));
    assert_eq!(pool.reserved(), start);
    assert_eq!((pool.holds(), pool.releases()), (1, 1));

    let (ledger, pool) = reserving(u64::MAX);
    let stop = crate::cancel::Stop::new();
    stop.request();
    let mut ledger = ledger.with_stop(stop);
    let before = thread_builds();
    assert_eq!(
        ledger.build(PatternKind::Regex, body).err(),
        Some(PatternError::Cancelled)
    );
    assert_eq!(
        (ledger.builds(), thread_builds()),
        (0, before),
        "no build ran"
    );
    assert_eq!((pool.reserved(), pool.holds()), (0, 0), "nothing held");
}

#[test]
fn a_build_pauses_after_its_charge() {
    let (mut ledger, pool) = reserving(u64::MAX);
    let pause = pool.pause_after_charge();
    let build = std::thread::spawn(move || {
        ledger
            .build(PatternKind::Regex, "a+")
            .map(|pattern| pattern.charge().total())
    });
    while pause.arrivals() == 0 {
        std::thread::yield_now();
    }
    let held = pool.reserved();
    assert!(!build.is_finished(), "the build waits at the gate");
    pause.release();
    let charge = build.join().expect("the build goes on").expect("builds");
    assert_eq!(held, bytes(charge));
    assert!(charge < BUILD_TRANSIENT, "{charge}");
    assert_eq!(pool.reserved(), 0, "the thread dropped the pattern");
    assert_eq!((pool.holds(), pool.releases()), (1, 1));
}

#[test]
fn preparation_observes_cancellation_between_builds() {
    let (ledger, pool) = reserving(u64::MAX);
    let stop = crate::cancel::Stop::new();
    let mut ledger = ledger.with_stop(stop.clone());
    let pause = pool.pause_after_charge();
    let preparation = std::thread::spawn(move || {
        let first = ledger.build(PatternKind::Regex, "time(out)?").map(|_| ());
        let rest: Vec<_> = [
            (PatternKind::Regex, r"\d{1,3}(\.\d{1,3}){3}"),
            (PatternKind::Like, "web-%"),
            (PatternKind::Regex, "time(out)?"),
        ]
        .into_iter()
        .map(|(kind, text)| ledger.build(kind, text).err())
        .collect();
        (first, rest, ledger.builds())
    });
    while pause.arrivals() == 0 {
        std::thread::yield_now();
    }
    stop.request();
    pause.release();
    let (first, rest, builds) = preparation.join().expect("the preparation ends");
    assert_eq!(first, Ok(()), "the build the stop came during answers");
    assert_eq!(rest, vec![Some(PatternError::Cancelled); 3]);
    assert_eq!(builds, 1, "no build after the stop");
    assert_eq!(pause.arrivals(), 1, "no charge after the stop");
    assert_eq!(pool.reserved(), 0);
    assert_eq!(
        (pool.holds(), pool.releases()),
        (1, 1),
        "a cancelled build holds no transient"
    );
}

#[test]
fn a_build_reads_its_stop_before_it_compiles() {
    let body = r"\w{64}";
    let view = batch_view(body);
    assert_ne!(view, body);
    let never = crate::cancel::Stop::never();
    assert_eq!(
        prepare(PatternKind::Regex, body, None, &never).err(),
        Some(PatternError::CompiledSize)
    );
    let stop = crate::cancel::Stop::new();
    stop.request();
    for view in [None, Some(body), Some(view.as_str())] {
        assert_eq!(
            prepare(PatternKind::Regex, body, view, &stop).err(),
            Some(PatternError::Cancelled),
            "{view:?}"
        );
    }
}

#[test]
fn a_requested_stop_cancels_every_search() {
    let stop = crate::cancel::Stop::new();
    let mut ledger = preparer().with_stop(stop.clone());
    let pattern = ledger
        .build(PatternKind::Regex, "(?<user>[a-z]+)@")
        .expect("builds");
    stop.request();
    let before = thread_regex_calls();
    assert_eq!(pattern.is_match("ana@"), Err(PatternError::Cancelled));
    assert!(matches!(
        pattern.captures("ana@"),
        Err(PatternError::Cancelled)
    ));
    assert_eq!(thread_regex_calls(), before, "no search ran");
}

#[test]
fn a_stop_between_searches_ends_at_the_next() {
    let stop = crate::cancel::Stop::new();
    let mut ledger = preparer().with_stop(stop.clone());
    let pattern = ledger.build(PatternKind::Regex, "b+").expect("builds");
    let before = thread_regex_calls();
    assert_eq!(pattern.is_match("abbc"), Ok(true));
    stop.request();
    assert_eq!(pattern.is_match("abbc"), Err(PatternError::Cancelled));
    assert_eq!(thread_regex_calls() - before, 1, "only the first searched");
}

#[test]
fn each_kind_matches_its_dialect() {
    let mut ledger = preparer();
    let mut is_match = |kind, text: &str, subject: &str| {
        ledger
            .build(kind, text)
            .expect("builds")
            .is_match(subject)
            .expect("never cancelled")
    };
    assert!(is_match(PatternKind::Regex, "b+", "abbc"));
    assert!(is_match(PatternKind::Like, "a%c", "abbc"));
    assert!(!is_match(PatternKind::Like, "b%", "abbc"));
    assert!(!is_match(PatternKind::Like, "A%", "abbc"));
    assert!(is_match(PatternKind::ILike, "A_B%", "axbc"));
    assert!(is_match(PatternKind::Glob, "a*c", "ab\nc"));
    assert!(!is_match(PatternKind::Glob, "b*", "abc"));
}

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
    assert_eq!(glob_to_regex("*.txt"), r"(?s-u:^.*\x2E\x74\x78\x74$)");
    assert_eq!(glob_to_regex("a**"), r"(?s-u:^\x61.*.*$)");
}

#[test]
fn glob_conversion_question() {
    assert_eq!(
        glob_to_regex("file?.log"),
        r"(?s-u:^\x66\x69\x6C\x65.\x2E\x6C\x6F\x67$)"
    );
}

#[test]
fn glob_conversion_character_class() {
    assert_eq!(glob_to_regex("[abc]"), r"(?s-u:^[\x61-\x63]$)");
    assert_eq!(glob_to_regex("[^4]"), r"(?s-u:^[\x34\x5E]$)");
    assert_eq!(glob_to_regex("[é]"), r"(?s-u:^[\xA9\xC3]$)");
}

#[test]
fn glob_conversion_negated_class() {
    assert_eq!(glob_to_regex("[!abc]"), r"(?s-u:^[\x00-\x60\x64-\xFF]$)");
}

#[test]
fn glob_conversion_escapes_every_byte() {
    assert_eq!(
        glob_to_regex("foo.bar"),
        r"(?s-u:^\x66\x6F\x6F\x2E\x62\x61\x72$)"
    );
    assert_eq!(glob_to_regex(r"\*é"), r"(?s-u:^\x2A\xC3\xA9$)");
    assert_eq!(glob_to_regex(""), "(?s-u:^$)");
}

/// The repeat walk counts a glob's bytes: `é` is two positions, so 30 of them
/// and a `*` are `^`, 60 bytes, `.*` and `$`, 64 positions, at the budget,
/// and 31 are 66, over it. Read as characters, 31 would be 35 and pass.
#[test]
fn glob_byte_mode_is_priced() {
    let at = format!("{}*", "é".repeat(30));
    let over = format!("{}*", "é".repeat(31));
    assert!(parser::parse(&format!("f={at}")).is_ok(), "{at}");
    let errors = parser::parse(&format!("f={over}")).expect_err("over the budget");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(
        errors[0].message,
        format!("{MSG_REPEAT_BUDGET} (66 positions, max 64)")
    );
    let errors = parser::parse(&format!("f={at}a")).expect_err("one past the budget");
    assert_eq!(
        errors[0].message,
        format!("{MSG_REPEAT_BUDGET} (65 positions, max 64)")
    );

    for glob in ["[!4]*", "[*"] {
        let pattern = preparer()
            .build(PatternKind::Glob, glob)
            .unwrap_or_else(|e| panic!("{glob} builds: {e}"));
        assert!(pattern.charge().total() > 0, "{glob}");
    }
}

#[test]
fn glob_conversion_malformed_matches_nothing() {
    for glob in [r"x\", "[", "[a-]", "[]", "[!]", "a[z-a]*", "*[", "[a-]]"] {
        assert_eq!(glob_to_regex(glob), GLOB_NEVER, "{glob}");
    }
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

#[test]
fn capture_names_lists_named_groups_in_order() {
    let before = thread_builds();
    assert_eq!(
        capture_names(r"(?P<code>\d+) (\w+) (?<path>/\S*)"),
        Some(vec!["code".to_owned(), "path".to_owned()])
    );
    assert_eq!(capture_names(r"(\d+)-(\d+)"), Some(Vec::new()));
    assert_eq!(capture_names(r"(?P<code>\d+"), None);
    assert_eq!(thread_builds(), before, "capture_names built a regex");
}

const PREMISE_SUBJECTS: [&str; 29] = [
    "\u{663}",
    "\u{e9}",
    "\u{200a}",
    "\u{b}",
    "\u{c}",
    "\u{212a}",
    "\u{17f}",
    "3",
    "e",
    "_",
    "K",
    "s",
    " ",
    "\t",
    "\n",
    "\r",
    "",
    "x\u{e9}y",
    "x\u{e9}",
    "\u{e9}x",
    "x\u{663}y",
    "x\u{212a}y",
    "x\u{17f}y",
    "\u{e9}\u{e9}",
    "a\n",
    "a\r\n",
    "x y",
    "Ka",
    "aK",
];

fn code_points(text: &str) -> String {
    if text.is_empty() {
        return "(empty)".to_owned();
    }
    text.chars()
        .map(|c| format!("U+{:04X}", u32::from(c)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn premise_case_forms(body: &str) -> [String; 3] {
    [
        body.to_owned(),
        format!("(?i){body}"),
        format!("(?i:{body}){body}"),
    ]
}

fn premise_bodies() -> Vec<(String, String)> {
    let mut bodies = Vec::new();
    for class in [r"\d", r"\D", r"\w", r"\W", r"\s", r"\S"] {
        for form in [
            class.to_owned(),
            format!("^{class}$"),
            format!("{class}+"),
            format!("[{class}]"),
            format!("[x{class}]"),
            format!("[^{class}]"),
        ] {
            for body in premise_case_forms(&form) {
                bodies.push((class.to_owned(), body));
            }
        }
    }
    for (family, forms) in [
        (r"\b", &[r"\b", r"x\b", r"\bx", r"\b\w+\b"][..]),
        (r"\B", &[r"\B", r"x\B", r"\Bx", r"x*\B"][..]),
    ] {
        for form in forms {
            for body in premise_case_forms(form) {
                bodies.push((family.to_owned(), body));
            }
        }
    }
    for body in [r"(?i)k", r"(?i)s", r"(?i)\w", r"(?i)\W"] {
        bodies.push(("case fold".to_owned(), body.to_owned()));
    }
    for body in ["a$", "(?m)a$", "^.$", "(?s)^.$"] {
        bodies.push(("anchor and dot".to_owned(), body.to_owned()));
    }
    for body in [r"\pN", r"\p{Nd}", r"\pL"] {
        bodies.push(("Unicode class control".to_owned(), body.to_owned()));
    }
    bodies
}

/// `body`'s view, built as `prepare` builds a view.
fn premise_view_program(body: &str) -> Result<(String, meta::Regex), String> {
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(body)
        .map_err(|e| format!("the body does not parse: {e}"))?;
    let view = re2::view(&ast);
    let hir = parse_hir(&view, PatternKind::Regex)
        .map_err(|e| format!("the view {view:?} does not parse: {e}"))?;
    let (regex, _) = compile(&hir, MAX_BATCH_COMPILED_BYTES)
        .map_err(|e| format!("the view {view:?} does not compile: {e}"))?;
    Ok((view, regex))
}

fn duckdb_version(conn: &duckdb::Connection) -> String {
    conn.query_row("SELECT version()", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|e| format!("(version unknown: {e})"))
}

const EMPTY_NON_BOUNDARY_FORMS: [&str; 2] = [r"\B", r"x*\B"];

const SPLIT_WORD_SUBJECTS: [&str; 4] = ["x\u{e9}y", "x\u{663}y", "x\u{212a}y", "x\u{17f}y"];

fn empty_non_boundary_rows() -> BTreeSet<(String, &'static str)> {
    EMPTY_NON_BOUNDARY_FORMS
        .iter()
        .flat_map(|form| premise_case_forms(form))
        .flat_map(|body| SPLIT_WORD_SUBJECTS.map(|subject| (body.clone(), subject)))
        .collect()
}

#[test]
fn regex_class_view_premise() {
    let split_word = |subject: &str| {
        let chars: Vec<char> = subject.chars().collect();
        matches!(chars.as_slice(), [a, c, b]
            if a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric() && c.len_utf8() > 1)
    };
    let split_subjects: Vec<&str> = PREMISE_SUBJECTS
        .into_iter()
        .filter(|subject| split_word(subject))
        .collect();
    assert_eq!(
        split_subjects, SPLIT_WORD_SUBJECTS,
        "SPLIT_WORD_SUBJECTS must name every premise subject of that shape"
    );
    let accepted = empty_non_boundary_rows();
    let mut differing = BTreeSet::new();
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    let mut mismatches = Vec::new();
    let mut answers: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let mut rows = 0usize;
    for (family, body) in premise_bodies() {
        let program = premise_view_program(&body);
        for subject in PREMISE_SUBJECTS {
            rows += 1;
            let duck = conn
                .prepare_cached("SELECT regexp_matches(?::VARCHAR, ?)")
                .and_then(|mut stmt| {
                    stmt.query_row(duckdb::params![subject, body], |row| row.get::<_, bool>(0))
                })
                .map_err(|e| format!("DuckDB error: {e}"));
            let (view, ours) = match &program {
                Ok((view, regex)) => {
                    let mut cache = regex.create_cache();
                    let input = Input::new(subject).earliest(true);
                    (
                        view.clone(),
                        Ok(regex.search_half_with(&mut cache, &input).is_some()),
                    )
                }
                Err(e) => ("(none)".to_owned(), Err(e.clone())),
            };
            if let Ok(answer) = duck {
                let seen = answers.entry(family.clone()).or_default();
                if answer {
                    seen.0 = true;
                } else {
                    seen.1 = true;
                }
            }
            let row = (body.clone(), subject);
            let expected = accepted.contains(&row);
            if duck != ours {
                differing.insert(row);
            }
            let unexpected = if expected {
                (&duck, &ours) != (&Ok(true), &Ok(false))
            } else {
                duck != ours
            };
            if unexpected {
                mismatches.push(format!(
                    "body {body:?} | view {view:?} | subject {} | duckdb {duck:?} | view {ours:?}{}",
                    code_points(subject),
                    if expected {
                        " | accepted as DuckDB true, view false"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    for (family, (any_true, any_false)) in &answers {
        assert!(
            *any_true && *any_false,
            "{family}: DuckDB answered only {}",
            if *any_true { "true" } else { "false" }
        );
    }
    assert!(
        mismatches.is_empty(),
        "{} of {rows} rows answer other than expected, DuckDB {}:\n{}",
        mismatches.len(),
        duckdb_version(&conn),
        mismatches.join("\n")
    );
    assert_eq!(
        differing, accepted,
        "the disagreeing rows are the accepted set"
    );
}

fn premise_view_captures(
    regex: &meta::Regex,
    batch: &BatchExtract,
    subject: &str,
) -> Result<Vec<Option<String>>, String> {
    let mut cache = regex.create_cache();
    let mut groups = regex.create_captures();
    regex.search_captures_with(&mut cache, &Input::new(subject), &mut groups);
    batch
        .captures
        .iter()
        .map(|capture| {
            if !groups.is_match() {
                return Ok(None);
            }
            let Some(span) = groups.get_group_by_name(&capture.name) else {
                return Ok(None);
            };
            if !subject.is_char_boundary(span.start) || !subject.is_char_boundary(span.end) {
                return Err(format!(
                    "group {} span {}..{} splits a character",
                    capture.name, span.start, span.end
                ));
            }
            let text = &subject[span.range()];
            Ok((!text.is_empty()).then(|| text.to_owned()))
        })
        .collect()
}

#[test]
fn regex_class_view_capture_premise() {
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    let mut mismatches = Vec::new();
    let (mut rows, mut some, mut none) = (0usize, 0usize, 0usize);
    let mut cells = 0usize;
    for body in [
        r"(?P<n>\d+)",
        r"(?P<n>\D+)",
        r"(\w)(?P<n>\s)",
        r"(?P<n>\S+)",
        r"(?P<a>\w+)\b(?P<b>\W*)",
        r"(?P<n>\B)",
        r"x(?P<n>\B)",
        r"(?P<n>\b)",
        r"(?P<a>\b\w)(?P<b>.)",
        r"(?i:(?P<n>\w))",
        r"(?i)(?P<n>[\w\s]+)",
        r"(?P<n>[^\w])",
    ] {
        let batch = batch_extract(body).expect("the body parses");
        let reads: Vec<String> = batch
            .captures
            .iter()
            .map(|capture| format!("nullif(struct_extract(x, 'g{}'), '')", capture.index))
            .collect();
        let fields: Vec<String> = (1..=batch.captures.len())
            .map(|k| format!("g{k}"))
            .collect();
        let sql = format!(
            "SELECT {} FROM (SELECT regexp_extract(?::VARCHAR, ?, ?::VARCHAR[]) AS x)",
            reads.join(", ")
        );
        let names = format!("[{}]", fields.join(", "));
        let program = premise_view_program(body);
        for subject in PREMISE_SUBJECTS {
            rows += 1;
            let duck: Result<Vec<Option<String>>, String> = conn
                .prepare_cached(&sql)
                .and_then(|mut stmt| {
                    stmt.query_row(duckdb::params![subject, batch.regex, names], |row| {
                        (0..batch.captures.len())
                            .map(|i| row.get::<_, Option<String>>(i))
                            .collect()
                    })
                })
                .map_err(|e| format!("DuckDB error: {e}"));
            let (view, ours) = match &program {
                Ok((view, regex)) => (view.clone(), premise_view_captures(regex, &batch, subject)),
                Err(e) => ("(none)".to_owned(), Err(e.clone())),
            };
            for (i, _) in batch.captures.iter().enumerate() {
                cells += 1;
                match duck.as_ref().map(|read| read[i].is_some()) {
                    Ok(true) => some += 1,
                    Ok(false) => none += 1,
                    Err(_) => {}
                }
            }
            if duck != ours {
                let names: Vec<&str> = batch.captures.iter().map(|c| c.name.as_str()).collect();
                mismatches.push(format!(
                    "body {body:?} | batch {:?} | view {view:?} | groups {names:?} | subject {} | duckdb {duck:?} | view {ours:?}",
                    batch.regex,
                    code_points(subject)
                ));
            }
        }
    }
    assert!(
        some > 0 && none > 0,
        "captures: {some} present, {none} null"
    );
    assert!(
        mismatches.is_empty(),
        "{} of {rows} rows ({cells} group cells) disagree, DuckDB {}:\n{}",
        mismatches.len(),
        duckdb_version(&conn),
        mismatches.join("\n")
    );
}

/// RE2 takes the empty alternative between the bytes of the character, so
/// the named group takes no part. The view cannot match there, so it goes
/// on and captures `y`.
#[test]
fn regex_class_view_capture_differs_on_an_empty_non_boundary() {
    let body = r"(?:\B|(?P<n>y))";
    let subject = "x\u{e9}y";
    let batch = batch_extract(body).expect("the body parses");
    let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
    let duck: Option<String> = conn
        .query_row(
            "SELECT nullif(struct_extract(x, 'g1'), '') \
             FROM (SELECT regexp_extract(?::VARCHAR, ?, ?::VARCHAR[]) AS x)",
            duckdb::params![subject, batch.regex, "[g1]"],
            |row| row.get(0),
        )
        .expect("DuckDB extracts");
    assert_eq!(duck, None, "DuckDB {}", duckdb_version(&conn));
    let (_, regex) = premise_view_program(body).expect("the view compiles");
    assert_eq!(
        premise_view_captures(&regex, &batch, subject),
        Ok(vec![Some("y".to_owned())])
    );
}
