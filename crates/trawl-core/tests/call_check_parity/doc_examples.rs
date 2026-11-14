// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every query the documentation shows, as an explicit manifest.
//!
//! A row is one complete query occurrence: where it is written, the exact
//! source text that anchors it, the query it is, and what the call check
//! must answer. [`candidates`] scans each source for the places a query
//! can be written: every line of a fenced block that is not a shell
//! block, every `trawl query`, `trawl validate` or `trawl tail` command in
//! a shell block or in prose, every backticked prose span that
//! [`is_query_shaped`], and, in `dsl::ESSENTIALS`, every backticked span
//! and every indented example line. Each candidate must be accounted for
//! by a row or by an [`Exclusion`] that says why it is not a query, so a
//! new example cannot appear unaccounted.
//!
//! `dsl::ESSENTIALS` lives in `trawl-mcp`, which depends on this crate, so
//! the rows carry copies anchored in that file's text through
//! [`essentials`], which decodes the string literal's escapes explicitly.

use trawl_core::parser::suggest::KNOWN_PIPE_STAGES;

/// A document that shows queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    DslReference,
    CliReference,
    CliTuiGuide,
    QueryTutorial,
    FirstQuery,
    Essentials,
}

impl Source {
    pub const ALL: [Self; 6] = [
        Self::DslReference,
        Self::CliReference,
        Self::CliTuiGuide,
        Self::QueryTutorial,
        Self::FirstQuery,
        Self::Essentials,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Self::DslReference => "docs/src/content/docs/reference/dsl.md",
            Self::CliReference => "docs/src/content/docs/reference/cli.md",
            Self::CliTuiGuide => "docs/src/content/docs/use/cli-tui.md",
            Self::QueryTutorial => "docs/src/content/docs/use/query-tutorial.md",
            Self::FirstQuery => "docs/src/content/docs/getting-started/first-query.md",
            Self::Essentials => "crates/trawl-mcp/src/dsl.rs (ESSENTIALS)",
        }
    }

    /// The text the rows are anchored in: a markdown page, or the decoded
    /// `ESSENTIALS` literal.
    pub fn text(self) -> String {
        match self {
            Self::DslReference => {
                include_str!("../../../../docs/src/content/docs/reference/dsl.md").to_owned()
            }
            Self::CliReference => {
                include_str!("../../../../docs/src/content/docs/reference/cli.md").to_owned()
            }
            Self::CliTuiGuide => {
                include_str!("../../../../docs/src/content/docs/use/cli-tui.md").to_owned()
            }
            Self::QueryTutorial => {
                include_str!("../../../../docs/src/content/docs/use/query-tutorial.md").to_owned()
            }
            Self::FirstQuery => {
                include_str!("../../../../docs/src/content/docs/getting-started/first-query.md")
                    .to_owned()
            }
            Self::Essentials => essentials(),
        }
    }
}

/// How a row's excerpt sits in its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// Whole lines of a fenced block (or of `ESSENTIALS`' indented
    /// examples). The query is the excerpt, or a lone pipe stage under
    /// `* | `.
    Fenced,
    /// A quoted shell argument, quotes included. The query is its
    /// unquoted text.
    Shell,
    /// A backticked span. The query is the span, or a lone pipe stage
    /// under `* | `.
    Inline,
}

/// What the call check must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Parses, and `validate_pipeline` accepts it.
    Valid,
    /// A deliberate negative the parser refuses.
    Parser,
    /// A deliberate negative `validate_pipeline` refuses, with this
    /// sentence.
    Validate(&'static str),
}

pub struct Example {
    pub source: Source,
    pub site: Site,
    pub excerpt: &'static str,
    pub query: &'static str,
    pub expect: Expect,
}

/// Why a candidate is not a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Excluded {
    /// A grammar line, with placeholders.
    Grammar,
    /// A command synopsis, with placeholders.
    Synopsis,
    /// Output a command prints.
    Transcript,
    /// An operator token named in prose.
    Operator,
    /// Part of a query, such as a stage name or a clause, named in prose.
    Fragment,
    /// A query shape with placeholders such as `<expr>` or `...`.
    Placeholder,
}

/// A candidate that is not a query. In a fenced block, the excerpt is one
/// of the block's lines and the exclusion covers every line of that block
/// no row covers; elsewhere it covers the one candidate equal to it.
pub struct Exclusion {
    pub source: Source,
    pub excerpt: &'static str,
    pub kind: Excluded,
    pub reason: &'static str,
}

/// One place a query can be written.
#[derive(Debug)]
pub struct Candidate {
    /// The fenced block it sits in, by index; `None` outside fences.
    pub block: Option<usize>,
    /// A backticked prose span.
    pub span: bool,
    /// The trimmed line, or a shell command with its continuations
    /// joined.
    pub text: String,
}

/// The `ESSENTIALS` string literal, decoded. Only the escapes it uses are
/// accepted: a line continuation (`\` then a line break, which drops the
/// next line's leading whitespace), `\"`, `\\` and `\n`. Any other escape
/// fails, so a new one is handled here on purpose rather than misread.
pub fn essentials() -> String {
    const SOURCE: &str = include_str!("../../../trawl-mcp/src/dsl.rs");
    const OPEN: &str = "pub const ESSENTIALS: &str = \"";
    let start = SOURCE.find(OPEN).expect("dsl.rs declares ESSENTIALS") + OPEN.len();
    let mut out = String::new();
    let mut chars = SOURCE[start..].chars().peekable();
    loop {
        match chars.next().expect("the ESSENTIALS literal closes") {
            '"' => return out,
            '\\' => match chars.next().expect("an escape has a second character") {
                '\n' => while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {},
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                other => {
                    panic!("ESSENTIALS uses an escape this decoder does not handle: \\{other}")
                }
            },
            c => out.push(c),
        }
    }
}

const SHELL_LABELS: &[&str] = &["bash", "sh", "shell", "console"];

/// A shell command that runs a query: `trawl` with a `query`, `validate`
/// or `tail` word.
pub fn is_query_command(command: &str) -> bool {
    command.contains("trawl")
        && command
            .split_whitespace()
            .any(|word| matches!(word, "query" | "validate" | "tail"))
}

/// A prose line that writes a query command with a quoted argument.
fn is_prose_query_command(line: &str) -> bool {
    is_query_command(line)
        && ["query", "validate", "tail"]
            .iter()
            .any(|word| line.contains(&format!("{word} '")) || line.contains(&format!("{word} \"")))
}

/// Whether a backticked prose span is written like a query, and so must
/// be a row or an exclusion: it starts with `|`, it starts with a pipe
/// stage keyword and a space, or it holds ` | `. This is deliberately
/// broad; a fragment or placeholder it catches is excluded by name.
pub fn is_query_shaped(span: &str) -> bool {
    span.starts_with('|')
        || span.contains(" | ")
        || KNOWN_PIPE_STAGES.iter().any(|stage| {
            span.strip_prefix(stage)
                .is_some_and(|rest| rest.starts_with(' '))
        })
}

/// The backticked spans of `source`'s prose. In a Markdown page these are
/// the code spans outside fenced blocks; in `ESSENTIALS`, the text between
/// paired backticks on a line.
pub fn spans(source: Source) -> Vec<String> {
    let text = source.text();
    if source == Source::Essentials {
        return text
            .lines()
            .filter(|line| !line.starts_with("  "))
            .flat_map(|line| line.split('`').skip(1).step_by(2).map(str::to_owned))
            .collect();
    }
    let mut found = Vec::new();
    let mut paragraph = String::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if fenced {
            fenced = trimmed != "```";
            continue;
        }
        // A span never crosses a blank line, a fence, a heading, a table
        // row or a list item, so each of these ends the text it is read
        // from.
        let table_row = trimmed.starts_with('|');
        let starts_block = trimmed.is_empty()
            || trimmed.starts_with("```")
            || trimmed.starts_with('#')
            || table_row
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if starts_block {
            code_spans(&paragraph, &mut found);
            paragraph.clear();
        }
        if trimmed.starts_with("```") {
            fenced = true;
        } else if table_row {
            // A table cell writes a literal pipe as `\|`, also in a span.
            code_spans(&trimmed.replace("\\|", "|"), &mut found);
        } else {
            paragraph.push_str(trimmed);
            paragraph.push('\n');
        }
    }
    code_spans(&paragraph, &mut found);
    found
}

/// The code spans of one Markdown block, as the Markdown spec reads them:
/// a run of n backticks opens a span that the next run of exactly n
/// closes, a line break inside a span reads as a space, and one space is
/// stripped from each end when both ends have one. An opening run with no
/// closing run is literal text, and so is a backslash-escaped backtick.
fn code_spans(block: &str, found: &mut Vec<String>) {
    let bytes = block.as_bytes();
    let run_end = |mut i: usize| {
        while bytes.get(i) == Some(&b'`') {
            i += 1;
        }
        i
    };
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if bytes.get(i + 1) == Some(&b'`') => i += 2,
            b'`' => {
                let open = run_end(i);
                let width = open - i;
                let mut j = open;
                let mut close = None;
                while j < bytes.len() {
                    if bytes[j] == b'`' {
                        let end = run_end(j);
                        if end - j == width {
                            close = Some((j, end));
                            break;
                        }
                        j = end;
                    } else {
                        j += 1;
                    }
                }
                let Some((start, end)) = close else {
                    i = open;
                    continue;
                };
                let content = block[open..start].replace('\n', " ");
                let content = match content.strip_prefix(' ').and_then(|c| c.strip_suffix(' ')) {
                    Some(inner) if !content.trim().is_empty() => inner.to_owned(),
                    _ => content,
                };
                found.push(content);
                i = end;
            }
            _ => i += 1,
        }
    }
}

/// Every place `source` can write a query.
pub fn candidates(source: Source) -> Vec<Candidate> {
    let text = source.text();
    let prose = spans(source).into_iter();
    if source == Source::Essentials {
        let examples = text.lines().filter_map(|line| line.strip_prefix("  "));
        return examples
            .map(|example| Candidate {
                block: Some(0),
                span: false,
                text: example.trim().to_owned(),
            })
            .chain(prose.map(|text| Candidate {
                block: None,
                span: true,
                text,
            }))
            .collect();
    }

    let mut found = Vec::new();
    let mut lines = text.lines();
    let mut block = 0;
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let Some(label) = trimmed.strip_prefix("```") else {
            if is_prose_query_command(trimmed) {
                found.push(Candidate {
                    block: None,
                    span: false,
                    text: trimmed.to_owned(),
                });
            }
            continue;
        };
        let shell = SHELL_LABELS.contains(&label.trim());
        let mut command = String::new();
        for line in lines.by_ref() {
            let trimmed = line.trim();
            if trimmed == "```" {
                break;
            }
            if shell {
                // Join a command's `\` continuations into one line.
                if let Some(head) = trimmed.strip_suffix('\\') {
                    command.push_str(head.trim_end());
                    command.push(' ');
                    continue;
                }
                command.push_str(trimmed);
                if is_query_command(&command) {
                    found.push(Candidate {
                        block: Some(block),
                        span: false,
                        text: std::mem::take(&mut command),
                    });
                }
                command.clear();
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                found.push(Candidate {
                    block: Some(block),
                    span: false,
                    text: trimmed.to_owned(),
                });
            }
        }
        block += 1;
    }
    found.extend(
        prose
            .filter(|span| is_query_shaped(span))
            .map(|text| Candidate {
                block: None,
                span: true,
                text,
            }),
    );
    found
}

/// The DSL a quoted shell argument passes. Single quotes pass their text
/// verbatim; double quotes are accepted only with nothing the shell would
/// expand or unescape inside.
pub fn unquote(argument: &str) -> &str {
    if let Some(inner) = argument
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        assert!(!inner.contains('\''), "{argument}: one quoted word");
        return inner;
    }
    let inner = argument
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("{argument}: a shell row's excerpt is a quoted argument"));
    assert!(
        !inner.contains(['"', '\\', '$', '`']),
        "{argument}: handle the shell's double-quote expansion before admitting this row"
    );
    inner
}

pub const EXAMPLES: &[Example] = &[
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity>=error last=1h    # only the last hour\n| stats count() by service\n| where count > 10",
        query: "_severity>=error last=1h    # only the last hour\n| stats count() by service\n| where count > 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "service=nginx",
        query: "service=nginx",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "status=200,301,404",
        query: "status=200,301,404",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "status>=400",
        query: "status>=400",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "path=/api/*",
        query: "path=/api/*",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "message=/error.*/",
        query: "message=/error.*/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "host=\"db host\"",
        query: "host=\"db host\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | where host like \"a b%\"",
        query: "* | where host like \"a b%\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "(service=api OR message=/timeout|refused/)",
        query: "(service=api OR message=/timeout|refused/)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "message=/(?i)timeout/",
        query: "message=/(?i)timeout/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "message=/connection refused/",
        query: "message=/connection refused/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "path=/tmp* | where x / 2 == 1",
        query: "path=/tmp* | where x / 2 == 1",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "status=200,\"301\",404",
        query: "status=200,\"301\",404",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "host=\"db host\",web-01",
        query: "host=\"db host\",web-01",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "tag=\"a#b\",plain",
        query: "tag=\"a#b\",plain",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where status > 400",
        query: "| where status > 400",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where status == 200",
        query: "| where status == 200",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where status in (200, \"accepted\")",
        query: "| where status in (200, \"accepted\")",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "`http-status`=500",
        query: "`http-status`=500",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "`last`=5 last=2h",
        query: "`last`=5 last=2h",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| table `a``b`",
        query: "| table `a``b`",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity=error",
        query: "_severity=error",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity!=info",
        query: "_severity!=info",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity=warn,error",
        query: "_severity=warn,error",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity=warn,fatal",
        query: "_severity=warn,fatal",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity>=warn",
        query: "_severity>=warn",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity=error2",
        query: "_severity=error2",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity=warn*",
        query: "_severity=warn*",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where sev(level) >= \"error\"",
        query: "| where sev(level) >= \"error\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where sev(level) == \"error\"",
        query: "| where sev(level) == \"error\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| where sev(syslog_severity, \"syslog\") == \"error\"",
        query: "| where sev(syslog_severity, \"syslog\") == \"error\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "error",
        query: "error",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "-debug",
        query: "-debug",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "\"connection refused\"",
        query: "\"connection refused\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "message=/debug/",
        query: "message=/debug/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "message=*debug*",
        query: "message=*debug*",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "last=2h",
        query: "last=2h",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "earliest=\"2026-03-14T03:00:00Z\"",
        query: "earliest=\"2026-03-14T03:00:00Z\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "latest=\"2026-03-14T03:15:00Z\"",
        query: "latest=\"2026-03-14T03:15:00Z\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity>=error | stats count() by service",
        query: "_severity>=error | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "earliest=\"2026-03-14T01:55:00.000000Z\" latest=\"2026-03-14T02:55:00.000000Z\" _severity>=error | stats count() by service",
        query: "earliest=\"2026-03-14T01:55:00.000000Z\" latest=\"2026-03-14T02:55:00.000000Z\" _severity>=error | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "service=nginx OR service=apache",
        query: "service=nginx OR service=apache",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "a b OR c d",
        query: "a b OR c d",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "(service=nginx OR service=apache) status>=400",
        query: "(service=nginx OR service=apache) status>=400",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "stats count()",
        query: "* | stats count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "stats count() by host",
        query: "* | stats count() by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "stats count(), avg(duration) by service, host",
        query: "* | stats count(), avg(duration) by service, host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "where status == 200 and count > 5",
        query: "* | where status == 200 and count > 5",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "where host matches /prod-.*/ and x in (1, 2, 3)",
        query: "* | where host matches /prod-.*/ and x in (1, 2, 3)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "sort count",
        query: "* | sort count",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "sort -count, status",
        query: "* | sort -count, status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "limit 20",
        query: "* | limit 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "head 20",
        query: "* | head 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "tail 5",
        query: "* | tail 5",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "table host, status",
        query: "* | table host, status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "fields host, status",
        query: "* | fields host, status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "top 10 host",
        query: "* | top 10 host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "top 5 status by service",
        query: "* | top 5 status by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "rare 5 status",
        query: "* | rare 5 status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "rare 3 host by service",
        query: "* | rare 3 host by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "drop message",
        query: "* | drop message",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "drop host, raw",
        query: "* | drop host, raw",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "let duration_ms = duration * 1000",
        query: "* | let duration_ms = duration * 1000",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "eval status_class = floor(status / 100)",
        query: "* | eval status_class = floor(status / 100)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: r#"extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#,
        query: r#"* | extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "rex \"(?P<code>[A-Z]+)\" from raw",
        query: "* | rex \"(?P<code>[A-Z]+)\" from raw",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "extract kv sep=\":\" from raw",
        query: "* | extract kv sep=\":\" from raw",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "rename service as svc",
        query: "* | rename service as svc",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "rename service as svc, host as hostname",
        query: "* | rename service as svc, host as hostname",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "dedup",
        query: "* | dedup",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "dedup host, service",
        query: "* | dedup host, service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "timechart span=5m count()",
        query: "* | timechart span=5m count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "timechart span=1h count() by service",
        query: "* | timechart span=1h count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| from saved daily_errors run=all | timechart on _run_time span=1h sum(count)",
        query: "| from saved daily_errors run=all | timechart on _run_time span=1h sum(count)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "pivot count() on status",
        query: "* | pivot count() on status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "pivot avg(duration) on service by host",
        query: "* | pivot avg(duration) on service by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "sample 10%",
        query: "* | sample 10%",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "sample 1000",
        query: "* | sample 1000",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | eventstats count() as total",
        query: "* | eventstats count() as total",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | eventstats max(duration) as host_max by host | where duration > host_max / 2",
        query: "* | eventstats max(duration) as host_max by host | where duration > host_max / 2",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| from saved daily_errors",
        query: "| from saved daily_errors",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| from saved daily_errors run=all | stats sum(count) by _run_id",
        query: "| from saved daily_errors run=all | stats sum(count) by _run_id",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| stats count() by count",
        query: "| stats count() by count",
        // A deliberate negative: two producers naming one column.
        expect: Expect::Validate(
            "invalid aggregation: `stats` would project two columns named `count`: the group key `count` and the aggregate `count()` — give one an explicit name with `as`",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| stats count() as n, sum(x) as N",
        query: "| stats count() as n, sum(x) as N",
        // A deliberate negative: two producers naming one column.
        expect: Expect::Validate(
            "invalid aggregation: `stats` would project two columns named `n`: the aggregate `count() as n` and the aggregate `sum(x) as N` — give one an explicit name with `as`",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| timechart span=1h count() by _time",
        query: "| timechart span=1h count() by _time",
        // A deliberate negative: two producers naming one column.
        expect: Expect::Validate(
            "invalid aggregation: `timechart` would project two columns named `_time`: the `timechart` time bucket `_time` and the group key `_time` — `timechart` always projects its bucket as `_time`, so `_time` cannot also be a group key",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| top 5 count",
        query: "| top 5 count",
        // A deliberate negative: two producers naming one column.
        expect: Expect::Validate(
            "invalid aggregation: `top` would project two columns named `count`: the counted field `count` and the `count` column the stage mints — `top` always mints its own `count` column and cannot spell `as` — write it as `stats count() as <name> by <field>` instead",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "| let a0 = status + status | let a1 = a0 + a0 | let a2 = a1 + a1",
        query: "| let a0 = status + status | let a1 = a0 + a0 | let a2 = a1 + a1",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "service=kubelet | eval pod = json_extract_string(k8s, \"$.pod\") | where isnotnull(pod)",
        query: "service=kubelet | eval pod = json_extract_string(k8s, \"$.pod\") | where isnotnull(pod)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | let last_hop = json(hops, \"$[-1]\"), app = json(labels, \"$.\\\"app.kubernetes.io/name\\\"\")",
        query: "* | let last_hop = json(hops, \"$[-1]\"), app = json(labels, \"$.\\\"app.kubernetes.io/name\\\"\")",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | let v = json(message, path_field)",
        query: "* | let v = json(message, path_field)",
        expect: Expect::Validate(
            "json() argument 2 must be a string literal JSON path (a JSON path is $ for the root, $ then steps such as .key, .\"quoted key\", [0], [-1] or [#-1], a pointer such as /key/0, or one bare key)",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity>=error last=1h | stats count() by service | sort -count",
        query: "_severity>=error last=1h | stats count() by service | sort -count",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10",
        query: "status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "status>=400 last=2h | stats count() by host, status | where count > 10",
        query: "status>=400 last=2h | stats count() by host, status | where count > 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: r#""connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count"#,
        query: r#""connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count"#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "_severity>=error | timechart span=5m count() by service",
        query: "_severity>=error | timechart span=5m count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "service=monitoring | dedup host, alert_name",
        query: "service=monitoring | dedup host, alert_name",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "last=1h | pivot count() on status by host",
        query: "last=1h | pivot count() on status by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10",
        query: "* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | stats values(_severity), first(message) by service | head 10",
        query: "* | stats values(_severity), first(message) by service | head 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "'last=15m | head 20'",
        query: "last=15m | head 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "\"_severity>=error last=1h | stats count() by service\"",
        query: "_severity>=error last=1h | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "\"last=1h | stats count() by service\"",
        query: "last=1h | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "\"* | stats count() by service\"",
        query: "* | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "\"_severity>=error | stats count() by host\"",
        query: "_severity>=error | stats count() by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliReference,
        site: Site::Shell,
        excerpt: "'* | let k = json(message, \"$[\")'",
        query: "* | let k = json(message, \"$[\")",
        expect: Expect::Validate(
            "json(): invalid JSON path \"$[\" in argument 2: expected an array index after [ (a JSON path is $ for the root, $ then steps such as .key, .\"quoted key\", [0], [-1] or [#-1], a pointer such as /key/0, or one bare key)",
        ),
    },
    Example {
        source: Source::CliTuiGuide,
        site: Site::Shell,
        excerpt: "'service=nginx last=15m | head 20'",
        query: "service=nginx last=15m | head 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliTuiGuide,
        site: Site::Shell,
        excerpt: "'service=nginx last=15m | stats count() as events by host'",
        query: "service=nginx last=15m | stats count() as events by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::CliTuiGuide,
        site: Site::Shell,
        excerpt: "'service=nginx last=15m | stats count() by host'",
        query: "service=nginx last=15m | stats count() by host",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial",
        query: "service=tutorial",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | table _time, level, _severity, message, duration",
        query: "service=tutorial | table _time, level, _severity, message, duration",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial _severity>=error | table message, duration",
        query: "service=tutorial _severity>=error | table message, duration",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial \"connection refused\"",
        query: "service=tutorial \"connection refused\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | where duration > 500 | sort -duration | table message, duration",
        query: "service=tutorial | where duration > 500 | sort -duration | table message, duration",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | stats count() by service",
        query: "service=tutorial | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | stats avg(duration) as mean_duration, count() as events by service",
        query: "service=tutorial | stats avg(duration) as mean_duration, count() as events by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | stats count() as events by service | where events >= 3",
        query: "service=tutorial | stats count() as events by service | where events >= 3",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Fenced,
        excerpt: "service=tutorial | timechart span=5m count()",
        query: "service=tutorial | timechart span=5m count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'service=checkout _severity>=error | stats count() as errors by service'",
        query: "service=checkout _severity>=error | stats count() as errors by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'* | head 20'",
        query: "* | head 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'* | stats count() by service'",
        query: "* | stats count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'_severity>=error | stats count() as errors by service | sort -errors | head 10'",
        query: "_severity>=error | stats count() as errors by service | sort -errors | head 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'service=web _severity>=warn | timechart span=5m count()'",
        query: "service=web _severity>=warn | timechart span=5m count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Shell,
        excerpt: "'service=tutorial'",
        query: "service=tutorial",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "service=nginx status>=500",
        query: "service=nginx status>=500",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "(service=nginx OR service=apache) NOT status=200",
        query: "(service=nginx OR service=apache) NOT status=200",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "status=401,403",
        query: "status=401,403",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "path=/api/*",
        query: "path=/api/*",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "message=/timeout|refused/",
        query: "message=/timeout|refused/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "host!=web*",
        query: "host!=web*",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "host=\"db host\"",
        query: "host=\"db host\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "\"connection refused\"",
        query: "\"connection refused\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "last=15m",
        query: "last=15m",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "earliest=\"2026-03-14T03:00:00Z\" latest=\"2026-03-14T04:00:00Z\"",
        query: "earliest=\"2026-03-14T03:00:00Z\" latest=\"2026-03-14T04:00:00Z\"",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "last=1h | where message matches /conn.*refused/",
        query: "last=1h | where message matches /conn.*refused/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Inline,
        excerpt: "last=1h | eval m = replace(message, \"[0-9]\", \"N\")",
        query: "last=1h | eval m = replace(message, \"[0-9]\", \"N\")",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Fenced,
        excerpt: "_severity>=error last=1h | stats count() by service | sort -count",
        query: "_severity>=error last=1h | stats count() by service | sort -count",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Fenced,
        excerpt: "service=nginx status>=500 last=24h | stats count() by host, status",
        query: "service=nginx status>=500 last=24h | stats count() by host, status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Fenced,
        excerpt: "\"connection refused\" last=2h | sort -_time | head 20",
        query: "\"connection refused\" last=2h | sort -_time | head 20",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Fenced,
        excerpt: "last=6h | timechart span=5m count() by service",
        query: "last=6h | timechart span=5m count() by service",
        expect: Expect::Valid,
    },
    Example {
        source: Source::Essentials,
        site: Site::Fenced,
        excerpt: "status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10",
        query: "status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10",
        expect: Expect::Valid,
    },
    // Queries written in prose spans.
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "extract kv",
        query: "* | extract kv",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "* | where host matches /^a b/",
        query: "* | where host matches /^a b/",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "path=/tmp* | where x / 2 == 1",
        query: "path=/tmp* | where x / 2 == 1",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where f != x",
        query: "* | where f != x",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"where f != "x""#,
        query: r#"* | where f != "x""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where isnull(f)",
        query: "* | where isnull(f)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "top 10 f",
        query: "* | top 10 f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "rare 10 f",
        query: "* | rare 10 f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "dedup f",
        query: "* | dedup f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "table f",
        query: "* | table f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "fields f",
        query: "* | fields f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let y = f",
        query: "* | let y = f",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "rename f as y",
        query: "* | rename f as y",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let a = 1, b = a",
        query: "* | let a = 1, b = a",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let web1 = web1 + 1",
        query: "* | let web1 = web1 + 1",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "timechart on f count()",
        query: "* | timechart on f count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"where status == "400""#,
        query: r#"* | where status == "400""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where status == 400",
        query: "* | where status == 400",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where a == b",
        query: "* | where a == b",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"where lower(status) == "a""#,
        query: r#"* | where lower(status) == "a""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where status * 2 > 400",
        query: "* | where status * 2 > 400",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where f == null",
        query: "* | where f == null",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "rename status as st",
        query: "* | rename status as st",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let s2 = status",
        query: "* | let s2 = status",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let ms = 1000, total = ms * 2",
        query: "* | let ms = 1000, total = ms * 2",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let a = status, b = a",
        query: "* | let a = status, b = a",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "rename _time as t",
        query: "* | rename _time as t",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let t2 = _time",
        query: "* | let t2 = _time",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"let d = date_trunc("day", _time)"#,
        query: r#"* | let d = date_trunc("day", _time)"#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let t = coalesce(_time, x)",
        query: "* | let t = coalesce(_time, x)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| where `true` == 1",
        query: "| where `true` == 1",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| let s = sev(level) | stats count() by s",
        query: "| let s = sev(level) | stats count() by s",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"| let s = sev(lower(level)) | where s == "error""#,
        query: r#"| let s = sev(lower(level)) | where s == "error""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| sort -timestamp",
        query: "| sort -timestamp",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| let x = 0.0 / 0 | where x == x",
        query: "| let x = 0.0 / 0 | where x == x",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "stats count()",
        query: "* | stats count()",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| stats count() as hits by count",
        query: "| stats count() as hits by count",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| stats count() as hits by count | sort -hits | head 5",
        query: "| stats count() as hits by count | sort -hits | head 5",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| where count == 1",
        query: "| where count == 1",
        expect: Expect::Valid,
    },
    // A scalar head in an aggregation slot runs in batch SQL, so
    // `validate_pipeline` admits it; live tail and the kv tail refuse it
    // when they compile, as the reference says.
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| stats lower(x)",
        query: "| stats lower(x)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "extract kv | sort -bytes | head 2",
        query: "* | extract kv | sort -bytes | head 2",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "stats lower(x)",
        query: "* | stats lower(x)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "stats coalesce(sum(x), 0)",
        query: "* | stats coalesce(sum(x), 0)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::QueryTutorial,
        site: Site::Inline,
        excerpt: "| head 10",
        query: "| head 10",
        expect: Expect::Valid,
    },
    Example {
        source: Source::FirstQuery,
        site: Site::Inline,
        excerpt: "service=checkout _severity>=error | stats count() as errors by service",
        query: "service=checkout _severity>=error | stats count() as errors by service",
        expect: Expect::Valid,
    },
    // Escapes, negated field-shaped terms and the search stage's lack of
    // `in (…)`.
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: r"* | where m matches /^\s+at /",
        query: r"* | where m matches /^\s+at /",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: r#"* | where m matches "^\\s+at ""#,
        query: r#"* | where m matches "^\\s+at ""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "-service=coredns",
        query: "-service=coredns",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "service!=coredns",
        query: "service!=coredns",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "NOT service=coredns",
        query: "NOT service=coredns",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"NOT "service=coredns""#,
        query: r#"NOT "service=coredns""#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "-status>=500",
        query: "-status>=500",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "NOT status>=500",
        query: "NOT status>=500",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "event_type in (a, b)",
        query: "event_type in (a, b)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "event_type NOT in (a, b)",
        query: "event_type NOT in (a, b)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "event_type=a,b",
        query: "event_type=a,b",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "event_type!=a,b",
        query: "event_type!=a,b",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"| where event_type in ("a", "b")"#,
        query: r#"| where event_type in ("a", "b")"#,
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"| where not event_type in ("a", "b")"#,
        query: r#"| where not event_type in ("a", "b")"#,
        expect: Expect::Valid,
    },
    // Deliberate negatives the reference states in prose.
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"* | where m matches "^\s+at ""#,
        query: r#"* | where m matches "^\s+at ""#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"host="*"#,
        query: r#"host="*"#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "msg=/err | warn/",
        query: "msg=/err | warn/",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "(error last=1h)",
        query: "(error last=1h)",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "NOT last=1h",
        query: "NOT last=1h",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"earliest="yesterday""#,
        query: r#"earliest="yesterday""#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"latest="10000-01-01""#,
        query: r#"latest="10000-01-01""#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "f>=a,b",
        query: "f>=a,b",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"f=a,"b"#,
        query: r#"f=a,"b"#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let _foo = 1",
        query: "* | let _foo = 1",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "rename x as _foo",
        query: "* | rename x as _foo",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"where "x" matches f"#,
        query: r#"* | where "x" matches f"#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| where m matches /a | b/",
        query: "| where m matches /a | b/",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| where count(x) > 1",
        query: "| where count(x) > 1",
        expect: Expect::Validate(
            "invalid aggregation: count() is an aggregation; use it in stats, timechart, eventstats or pivot",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| stats sum(count(x))",
        query: "| stats sum(count(x))",
        expect: Expect::Validate("invalid aggregation: count() cannot be nested inside sum()"),
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"| where lower(count(x)) == "a""#,
        query: r#"| where lower(count(x)) == "a""#,
        expect: Expect::Validate(
            "invalid aggregation: count() is an aggregation; use it in stats, timechart, eventstats or pivot",
        ),
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "| stats coalesce(sum(x), 0)",
        query: "| stats coalesce(sum(x), 0)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "where m matches f",
        query: "* | where m matches f",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: r#"where m like lower("A%")"#,
        query: r#"* | where m like lower("A%")"#,
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Inline,
        excerpt: "let `_foo` = 1",
        query: "* | let `_foo` = 1",
        expect: Expect::Parser,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | where status in (500, 502, 503, 504)",
        query: "* | where status in (500, 502, 503, 504)",
        expect: Expect::Valid,
    },
    Example {
        source: Source::DslReference,
        site: Site::Fenced,
        excerpt: "* | where status >= 500 | where host == \"web-1\" | where bytes > 0",
        query: "* | where status >= 500 | where host == \"web-1\" | where bytes > 0",
        expect: Expect::Valid,
    },
];

pub const EXCLUSIONS: &[Exclusion] = &[
    Exclusion {
        source: Source::DslReference,
        excerpt: "[search stage] | [pipe stage] | [pipe stage] ...",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "<field><operator><value>",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "stats <agg>[ as <name>][, <agg> ...] [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "where <expression>",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "sort [-]<field>[, [-]<field> ...]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "limit <n>",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "tail <n>",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "table <field>[, <field> ...]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "top <n> <field> [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "rare <n> <field> [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "drop <field>[, <field> ...]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "let <name> = <expression>[, <name> = <expression> ...]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "extract \"<regex>\" [from <field>]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "rename <field> as <name>[, <field> as <name> ...]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "dedup [<field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "timechart [on <field>] [span=<duration>] <agg>[ as <name>][, <agg> ...] [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "pivot <agg> on <field> [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "sample <n>%",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "eventstats <agg> as <name>[, <agg> as <name> ...] [by <field>[, <field> ...]]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "from saved <name> [run=latest|all|<id>]",
        kind: Excluded::Grammar,
        reason: "the grammar, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "pipeline stage 1 (`let`) goes over this query's alias-expansion budget of",
        kind: Excluded::Transcript,
        reason: "refusal sentences the query engine prints",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "expression is deeper than 64 levels",
        kind: Excluded::Transcript,
        reason: "refusal sentences the parser prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl [--driver [PATH]]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl query <QUERY> [--data GLOB] [-f FORMAT] [-o PATH]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "note: results may be incomplete — degraded field(s): duration (see: trawl schema field duration)",
        kind: Excluded::Transcript,
        reason: "output the command prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl validate <QUERY>",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl preview-ingest [FILE|-] [--peer-ip IP] [--json]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "peer 192.0.2.1 (placeholder, no --peer-ip given; an event without a host depends on the sender), not a trusted relay; arrival 2026-10-01T12:00:00.000000Z",
        kind: Excluded::Transcript,
        reason: "output the command prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "5 event(s): 3 accepted, 2 rejected; 2 depend on the sender's address (pass --peer-ip)",
        kind: Excluded::Transcript,
        reason: "output the command prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema <SUBCOMMAND>",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema fields [--service NAME] [--last WINDOW] [--limit N] [--data GLOB] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema field <NAME> [--limit N] [--after CURSOR] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema conflicts [--field NAME] [--service NAME] [--last WINDOW] [--limit N] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema repin <FIELD> --to TYPE [--dialect otel|syslog] [--dry-run] [--force]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema repin-status [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema repin-cancel [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema gc-pins [--dry-run] [--older-than WINDOW] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl schema ack <FIELD> [--note TEXT | --clear] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl doctor --url URL [--token-env NAME | --token-file PATH] [--insecure] [--web-url ORIGIN] [-f FORMAT]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "target: https://trawl.example.com:5514 (CLI profile `prod` in ~/.config/trawl/config.toml)",
        kind: Excluded::Transcript,
        reason: "output the command prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl trial <VERB>",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl trial up [--api-port PORT] [--web-port PORT] [--image REFERENCE] [--no-sample-data]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl trial down [--yes]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl mcp --profile NAME [--token-file PATH]",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "trawl driver [--socket PATH] <SUBCOMMAND>",
        kind: Excluded::Synopsis,
        reason: "a command synopsis, with placeholders",
    },
    Exclusion {
        source: Source::QueryTutorial,
        excerpt: "Samples",
        kind: Excluded::Transcript,
        reason: "the trial summary the TUI prints",
    },
    Exclusion {
        source: Source::FirstQuery,
        excerpt: "The trial is up.",
        kind: Excluded::Transcript,
        reason: "output the trial command prints",
    },
    Exclusion {
        source: Source::FirstQuery,
        excerpt: "Samples",
        kind: Excluded::Transcript,
        reason: "output the trial command prints",
    },
    Exclusion {
        source: Source::FirstQuery,
        excerpt: "{\"errors\":20,\"service\":\"checkout\"}",
        kind: Excluded::Transcript,
        reason: "a result row",
    },
    Exclusion {
        source: Source::Essentials,
        excerpt: "|",
        kind: Excluded::Operator,
        reason: "an operator token, not a query",
    },
    Exclusion {
        source: Source::Essentials,
        excerpt: "!=",
        kind: Excluded::Operator,
        reason: "an operator token, not a query",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "|",
        kind: Excluded::Operator,
        reason: "the pipe operator named in prose",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: r"drop the spaces or write \s",
        kind: Excluded::Transcript,
        reason: "a hint the parser prints",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "stats ... by f",
        kind: Excluded::Placeholder,
        reason: "`...` stands for the aggregations",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "timechart ... by f",
        kind: Excluded::Placeholder,
        reason: "`...` stands for the aggregations",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "let y = <expr of f>",
        kind: Excluded::Placeholder,
        reason: "`<expr of f>` stands for an expression",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "| where",
        kind: Excluded::Fragment,
        reason: "a stage named in prose",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "| let",
        kind: Excluded::Fragment,
        reason: "a stage named in prose",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "where field <op> literal",
        kind: Excluded::Grammar,
        reason: "a comparison shape, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "where literal <op> field",
        kind: Excluded::Grammar,
        reason: "a comparison shape, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "where field matches|like|ilike <pattern>",
        kind: Excluded::Grammar,
        reason: "a comparison shape, with placeholders",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "let status = <expr>",
        kind: Excluded::Placeholder,
        reason: "`<expr>` stands for an expression",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "stats ... by status",
        kind: Excluded::Placeholder,
        reason: "`...` stands for the aggregations",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "extract <regex>",
        kind: Excluded::Placeholder,
        reason: "`<regex>` stands for a pattern",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "from saved",
        kind: Excluded::Fragment,
        reason: "the stage named without a saved query",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "| from saved",
        kind: Excluded::Fragment,
        reason: "the stage named without a saved query",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "from <field>",
        kind: Excluded::Grammar,
        reason: "an extract option, with a placeholder",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "sample <n>",
        kind: Excluded::Grammar,
        reason: "a sample size form, with a placeholder",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "stats ... by",
        kind: Excluded::Placeholder,
        reason: "`...` stands for the aggregations and the keys",
    },
    Exclusion {
        source: Source::DslReference,
        excerpt: "tail is not supported after extract kv; use sort and head instead",
        kind: Excluded::Transcript,
        reason: "a refusal sentence the kv tail prints",
    },
    Exclusion {
        source: Source::CliReference,
        excerpt: "|",
        kind: Excluded::Operator,
        reason: "the pipe operator named in prose",
    },
];
