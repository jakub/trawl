// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shipped Debian Vector configs and trawld's service rule (ADR-0053).
//!
//! VRL has no functions, so every transform that derives `service` from
//! event data carries its own copy of the gate regex and falls back to a
//! marked name when no candidate passes. These tests keep every copy equal
//! to one pinned literal, keep that literal equal to
//! `trawl_config::is_valid_service_name`, and notice a transform that
//! derives `service` without a gate. The behaviour of the configs runs
//! under the real Vector binary in `scripts/test-vector-collector.py`.
//!
//! The scanner is conservative. It lexes each remap, finds every write to
//! `service` (any position, any whitespace, quoted paths, destructuring,
//! merge-assignment, whole-event assignment, and calls to `set` or `merge`),
//! and accepts only a literal name trawld takes or the gated `service`
//! variable. A write it cannot classify fails the test.

use std::collections::BTreeSet;

use trawl_config::is_valid_service_name;

/// The gate, as it appears in every deriving transform. VRL `match` and
/// this test use the same `regex` crate.
const GATE: &str = r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,127}$";

const SHIPPED: [(&str, &str); 9] = [
    (
        "apache.toml",
        include_str!("../../../config/vector/debian/apache.toml"),
    ),
    (
        "base.toml",
        include_str!("../../../config/vector/debian/base.toml"),
    ),
    (
        "docker.toml",
        include_str!("../../../config/vector/debian/docker.toml"),
    ),
    (
        "fail2ban.toml",
        include_str!("../../../config/vector/debian/fail2ban.toml"),
    ),
    (
        "mysql.toml",
        include_str!("../../../config/vector/debian/mysql.toml"),
    ),
    (
        "nginx.toml",
        include_str!("../../../config/vector/debian/nginx.toml"),
    ),
    (
        "postgresql.toml",
        include_str!("../../../config/vector/debian/postgresql.toml"),
    ),
    (
        "redis.toml",
        include_str!("../../../config/vector/debian/redis.toml"),
    ),
    (
        "unifi-syslog.toml",
        include_str!("../../../config/vector/debian/unifi-syslog.toml"),
    ),
];

/// The VRL source of every remap transform: `(file, transform, source)`.
fn remap_sources() -> Vec<(&'static str, String, String)> {
    let mut sources = Vec::new();
    for (file, text) in SHIPPED {
        let parsed: toml::Table = toml::from_str(text).unwrap_or_else(|e| panic!("{file}: {e}"));
        let Some(transforms) = parsed.get("transforms").and_then(|t| t.as_table()) else {
            continue;
        };
        for (name, transform) in transforms {
            if transform.get("type").and_then(|t| t.as_str()) != Some("remap") {
                continue;
            }
            let source = transform
                .get("source")
                .and_then(|s| s.as_str())
                .unwrap_or_else(|| panic!("{file}: transform {name} has no source"));
            sources.push((file, name.clone(), source.to_owned()));
        }
    }
    sources
}

/// A lexed VRL token. Comments and whitespace are dropped.
#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    /// A `"..."` string, escapes unprocessed.
    Str(String),
    /// A `r'...'`, `s'...'` or `t'...'` literal.
    Raw(String),
    Punct(String),
}

struct Token {
    tok: Tok,
    /// 1-based line the token starts on.
    line: usize,
    start: usize,
    end: usize,
}

const OPERATORS: [&str; 9] = ["??=", "==", "!=", "<=", ">=", "??", "|=", "&&", "||"];

/// Assignment operators. `==` and `!=` are comparisons and lex apart.
const ASSIGN: [&str; 3] = ["=", "|=", "??="];

fn lex(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < bytes.len() {
        let (start, start_line, b) = (i, line, bytes[i]);
        let tok = match b {
            b'\n' => {
                line += 1;
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' | b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    if bytes.get(i) == Some(&b'\n') {
                        line += 1;
                    }
                    i += 1;
                }
                assert!(i < bytes.len(), "unterminated string at line {start_line}");
                let content = source[start + 1..i].to_owned();
                i += 1;
                if b == b'"' {
                    Tok::Str(content)
                } else {
                    Tok::Raw(content)
                }
            }
            b if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                if matches!(&source[start..i], "r" | "s" | "t") && bytes.get(i) == Some(&b'\'') {
                    continue;
                }
                if bytes.get(i) == Some(&b'!') && bytes.get(i + 1) != Some(&b'=') {
                    i += 1;
                }
                Tok::Ident(source[start..i].to_owned())
            }
            _ => {
                let op = OPERATORS
                    .iter()
                    .find(|op| source[i..].starts_with(**op))
                    .map_or_else(|| (b as char).to_string(), |op| (*op).to_owned());
                i += op.len();
                Tok::Punct(op)
            }
        };
        tokens.push(Token {
            tok,
            line: start_line,
            start,
            end: i,
        });
    }
    tokens
}

fn is_punct(tokens: &[Token], i: usize, text: &str) -> bool {
    matches!(tokens.get(i), Some(Token { tok: Tok::Punct(p), .. }) if p == text)
}

/// Token `i` starts where token `i - 1` ends, with no whitespace between.
fn adjacent(tokens: &[Token], i: usize) -> bool {
    i > 0 && tokens.get(i).is_some_and(|t| t.start == tokens[i - 1].end)
}

/// The dot at `i` continues a path (`foo.service`, `.a.service`, `x().y`)
/// rather than starting one at the event root.
fn continues_path(tokens: &[Token], i: usize) -> bool {
    adjacent(tokens, i)
        && match &tokens[i - 1].tok {
            Tok::Ident(_) | Tok::Str(_) => true,
            Tok::Punct(p) => p == ")" || p == "]",
            Tok::Raw(_) => false,
        }
}

/// The index after the path segments (`.name`, `."name"`, `[..]`) that
/// follow, with no whitespace, from `j`.
fn path_end(tokens: &[Token], mut j: usize) -> usize {
    loop {
        if adjacent(tokens, j)
            && is_punct(tokens, j, ".")
            && matches!(
                tokens.get(j + 1).map(|t| &t.tok),
                Some(Tok::Ident(_) | Tok::Str(_))
            )
        {
            j += 2;
        } else if adjacent(tokens, j) && is_punct(tokens, j, "[") {
            let mut depth = 0;
            while let Some(token) = tokens.get(j) {
                j += 1;
                match &token.tok {
                    Tok::Punct(p) if p == "[" => depth += 1,
                    Tok::Punct(p) if p == "]" => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
            }
        } else {
            return j;
        }
    }
}

/// `, target = ...` after a path: the second target of `.service, err = f()`.
/// `foo(.service, err == 1)` and `[.service, a]` are not.
fn destructures(tokens: &[Token], comma: usize) -> bool {
    let mut k = comma + 1;
    while let Some(token) = tokens.get(k) {
        let target_part = match &token.tok {
            Tok::Ident(_) | Tok::Str(_) => true,
            Tok::Punct(p) => matches!(p.as_str(), "." | "[" | "]"),
            Tok::Raw(_) => false,
        };
        if !target_part || token.line != tokens[comma].line {
            break;
        }
        k += 1;
    }
    k > comma + 1 && is_punct(tokens, k, "=")
}

#[derive(Debug, PartialEq)]
enum Write {
    /// `.service = "name"`
    Literal(String),
    /// `.service = service`, the variable the transform's gate checks.
    Derived,
    /// A write the scanner cannot classify, and why.
    Refused(&'static str),
}

#[derive(Debug)]
struct ServiceWrite {
    line: usize,
    write: Write,
}

const NOT_CLASSIFIABLE: &str = "`.service` is assigned something other than a string literal \
     or the gated `service` variable";

/// The right-hand side starting at token `k`: a lone string literal or a
/// lone `service`, ending its statement.
fn classify_rhs(tokens: &[Token], k: usize) -> Write {
    let Some(first) = tokens.get(k) else {
        return Write::Refused(NOT_CLASSIFIABLE);
    };
    let ends = tokens.get(k + 1).is_none_or(|next| {
        next.line > first.line || matches!(&next.tok, Tok::Punct(p) if p == "}" || p == ";")
    });
    match &first.tok {
        Tok::Str(literal) if ends => Write::Literal(literal.clone()),
        Tok::Ident(name) if name == "service" && ends => Write::Derived,
        _ => Write::Refused(NOT_CLASSIFIABLE),
    }
}

/// Every write to the event's `service` field in a remap `source`, and every
/// construct that could write it without the scanner seeing which field.
/// Comparisons, reads and writes to other fields are not writes.
fn service_writes(source: &str) -> Vec<ServiceWrite> {
    let tokens = lex(source);
    let mut writes = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let line = token.line;
        let write = match &token.tok {
            Tok::Ident(name)
                if matches!(name.trim_end_matches('!'), "set" | "merge")
                    && is_punct(&tokens, i + 1, "(") =>
            {
                Some(Write::Refused(
                    "calls a function that can write any field of the event",
                ))
            }
            Tok::Punct(p) if p == "." && !continues_path(&tokens, i) => {
                let field = tokens
                    .get(i + 1)
                    .filter(|_| adjacent(&tokens, i + 1))
                    .and_then(|t| match &t.tok {
                        Tok::Ident(name) | Tok::Str(name) => Some(name.as_str()),
                        _ => None,
                    });
                let (whole_event, end) = match field {
                    Some("service") => (false, path_end(&tokens, i + 2)),
                    Some(_) => continue,
                    None if is_punct(&tokens, i + 1, "[") && adjacent(&tokens, i + 1) => {
                        writes.push(ServiceWrite {
                            line,
                            write: Write::Refused("indexes the whole event by expression"),
                        });
                        continue;
                    }
                    None => (true, i + 1),
                };
                match tokens.get(end).map(|t| &t.tok) {
                    Some(Tok::Punct(op)) if op == "=" && !whole_event => {
                        Some(classify_rhs(&tokens, end + 1))
                    }
                    Some(Tok::Punct(op)) if ASSIGN.contains(&op.as_str()) => {
                        Some(Write::Refused(if whole_event {
                            "assigns the whole event, which can set `service`"
                        } else {
                            "assigns `.service` with an operator the scanner cannot classify"
                        }))
                    }
                    Some(Tok::Punct(op)) if op == "," && destructures(&tokens, end) => {
                        Some(Write::Refused(
                            "assigns `.service` by destructuring, which the scanner cannot classify",
                        ))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(write) = write {
            writes.push(ServiceWrite { line, write });
        }
    }
    writes
}

/// The contents of every `r'...'` regex literal in `source`.
fn regex_literals(source: &str) -> Vec<String> {
    let literal = regex::Regex::new(r"r'([^']*)'").expect("literal pattern");
    literal
        .captures_iter(source)
        .map(|c| c[1].to_owned())
        .collect()
}

#[test]
fn every_service_deriving_transform_ends_in_the_one_gate() {
    let mut deriving = BTreeSet::new();
    let mut remaps = 0;
    for (file, name, source) in remap_sources() {
        remaps += 1;
        let at = format!("{file}: transform {name}");
        let mut derives = false;
        for ServiceWrite { line, write } in service_writes(&source) {
            match write {
                Write::Literal(literal) => {
                    assert!(
                        is_valid_service_name(&literal),
                        "{at}: line {line}: literal service {literal:?} is a name trawld refuses"
                    );
                    assert_ne!(
                        literal, "unknown",
                        "{at}: line {line}: `unknown` is a host fallback, never a service"
                    );
                }
                Write::Derived => derives = true,
                Write::Refused(why) => panic!(
                    "{at}: line {line}: {why}: {}",
                    source.lines().nth(line - 1).unwrap_or_default().trim()
                ),
            }
        }
        for line in source.lines().map(str::trim) {
            if line.contains("\"unknown\"") {
                assert!(
                    line.starts_with(".host ="),
                    "{at}: `\"unknown\"` may only fall back a host: {line}"
                );
            }
        }

        let gates: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("gate = r'"))
            .collect();
        if derives {
            assert_eq!(
                gates.len(),
                1,
                "{at}: a transform deriving service holds exactly one gate: {gates:?}"
            );
            assert_eq!(
                gates[0],
                format!("gate = r'{GATE}'"),
                "{at}: the gate differs from trawld's service rule"
            );
            deriving.insert(name);
        } else {
            assert!(gates.is_empty(), "{at}: a gate without a derived service");
        }

        for literal in regex_literals(&source) {
            if literal.contains("{0,127}") {
                assert_eq!(literal, GATE, "{at}: a stray copy of the gate differs");
            }
        }
    }
    assert!(remaps > 0, "no remap transform was parsed");

    let expected: BTreeSet<String> = [
        "journal_enriched",
        "trawl_docker",
        "trawl_unifi",
        "trawl_varlog",
    ]
    .map(String::from)
    .into();
    assert_eq!(
        deriving, expected,
        "the transforms deriving service from event data changed; give each its gate \
         and fallback, then list it here"
    );
}

fn writes(source: &str) -> Vec<Write> {
    service_writes(source)
        .into_iter()
        .map(|w| w.write)
        .collect()
}

fn refused(source: &str) -> bool {
    writes(source)
        .iter()
        .any(|w| matches!(w, Write::Refused(_)))
}

#[test]
fn the_scanner_refuses_every_write_it_cannot_classify() {
    for source in [
        "if exists(.app) { .service = to_string!(.app) }",
        ".service=x",
        ".service = .x",
        ".service = to_string!(.app)",
        ".service = \"a\" + .x",
        ".service = if .a { \"x\" } else { \"y\" }",
        ".service = 'raw'",
        ".service = service ?? \"x\"",
        ".service = replace(service, \"a\", \"b\")",
        "\n\n  .service   =   .x",
        ".\"service\" = .x",
        ".\"service\"=.x",
        ".service, err = parse_regex(.message, r'x')",
        ".service |= \"a\"",
        ".service ??= .x",
        ".service[0] = .x",
        "if .a { .b = 1 } else { .service = .x }",
        ". = merge(., {\"service\": .x})",
        ". |= {\"service\": .x}",
        ". = parse_json!(.message)",
        ".[\"service\"] = .x",
        "set!(., [\"service\"], .x)",
        "merge!(., {\"service\": .x})",
        "ok, .service = f()",
        "for_each(.m) -> |k, v| { .service = v }",
    ] {
        assert!(refused(source), "the scanner let this through: {source}");
    }
}

#[test]
fn the_scanner_accepts_the_shipped_shapes() {
    let literal = |name: &str| vec![Write::Literal(name.to_owned())];
    assert_eq!(writes(".service = \"nginx\""), literal("nginx"));
    assert_eq!(writes("  .service = \"ufw\"  # firewall"), literal("ufw"));
    assert_eq!(writes("if .a { .service = \"ufw\" }"), literal("ufw"));
    assert_eq!(writes(".service = service"), vec![Write::Derived]);
    assert_eq!(
        writes(
            "service = \"\"\nif !match(service, gate) {\n  service = \"varlog-unidentified\"\n}\n\
             .service = service\n.host = \"x\""
        ),
        vec![Write::Derived]
    );
    assert_eq!(
        service_writes("a = 1\n\n.service = service")[0].line,
        3,
        "line numbers count from 1"
    );
}

#[test]
fn the_scanner_ignores_reads_comparisons_and_other_fields() {
    for source in [
        "svc = to_string(.service) ?? \"\"",
        "if .service == \"x\" { return false }",
        "if .service != \"x\" { return false }",
        "foo(.service, err == 1)",
        "x = [.service, a]\nb = 1",
        "del(.service)",
        "x = .",
        "parsed, err = parse_regex(.message, r'^(?P<service>.+)$')",
        ".label.\"com.docker.compose.service\"",
        "x = .label.\"com.docker.compose.service\" ?? \"\"",
        ".docker_compose_service = compose_svc",
        ".a.service = \"x\"",
        "foo.service = \"x\"",
        "service = \"x\"",
        "# .service = .x",
        "msg = \"a .service = b\"",
        "re = r'.service = x'",
        "unit = replace(unit, r'\\.(?:service|scope)$', \"\")",
        "settings = 1\nmerged = 2",
        "x = 1.5",
    ] {
        assert_eq!(writes(source), vec![], "the scanner flagged this: {source}");
    }
}

#[test]
fn the_gate_agrees_with_is_valid_service_name() {
    let bytes = regex::bytes::Regex::new(GATE).expect("gate compiles");
    for b in 0..=u8::MAX {
        let gate = bytes.is_match(&[b]);
        let rule = std::str::from_utf8(&[b]).is_ok_and(is_valid_service_name);
        assert_eq!(gate, rule, "byte {b:#04x}");
    }

    let text = regex::Regex::new(GATE).expect("gate compiles");
    for c in '\u{80}'..='\u{ff}' {
        let s = c.to_string();
        assert_eq!(
            text.is_match(&s),
            is_valid_service_name(&s),
            "char {:#06x}",
            c as u32
        );
    }

    let mut samples: Vec<String> = ["a", "-", "_"]
        .iter()
        .flat_map(|unit| [0, 1, 127, 128, 129].map(|n| unit.repeat(n)))
        .collect();
    samples.extend(
        [
            ".", "..", ".x", "-a", "_a", "a.", "a\n", "a b", "a/b", "a.b-c_d",
        ]
        .map(String::from),
    );
    for s in samples {
        assert_eq!(
            text.is_match(&s),
            is_valid_service_name(&s),
            "{:?} (len {})",
            if s.len() > 16 { &s[..16] } else { &s },
            s.len()
        );
    }
}
