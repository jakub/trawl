# Advice on a valid query is a local mark that never gates

status: accepted (2026-11-14) — prep ruling record for the DSL footgun hints (0.9.5 RC survey, bug 3)

The 0.9.5 RC survey on the homelab found three query texts that misled
the person who wrote them. Each one behaves as the DSL reference says:

| Text | What the author meant | What trawl does |
| --- | --- | --- |
| `"10.244.0.83" -service=coredns` | drop the coredns service | drops events whose `message` or `_raw` contains the text `service=coredns` |
| `service=trawld event_type in (a, b)` | `event_type` is `a` or `b` | text searches for `event_type` and `in`, then a bracketed group of text searches `a,` and `b` |
| `\| where m matches "^\s+at "` | the regex `\s` | parse error `found 's', expected '\' or '"' or 'n' or 't' or 'r'` |

The first two run and return a plausible answer, so nothing tells the
author that the question changed. The third fails, but its message names
the parser's expected tokens, not the mistake. `host="a\sb"` fails the
same way with a different, wrong message: `a value that opens with '"'
must be a complete quoted string`, hint `close the quote`.

Trawl had no way to say anything about a query that parses. A parse
error carries a hint, and every surface renders it. A valid query
carries nothing. The browser and the TUI lint the draft locally with
trawl-core and never call `/validate` (ADR-0039), and `trawl query` and
MCP's execute mode never call it either.

## Decision

**Owner rulings (2026-11-13).** Hints only: no query that runs today
changes its parse, its results or its formatting. One issue covers all
three texts. The two valid-query hints trigger on the shape of the text
alone, for any word shaped like a field name. They never consult a list
of known fields or the catalog, so a custom field such as `event_type`
is covered, and a false hit costs one warning the author can ignore.

**Advice is a third kind of mark.** Errors refuse a query. Refusals come
from the server. Advice describes a query that is valid, and it never
changes whether the query is valid, whether it runs, an exit code, MCP's
`valid` or its next actions. When an MCP reply runs over its byte
budget, advice is dropped before anything else. A text with any error shows only its
errors.

**One owner computes advice, and each surface calls it.** A pure
trawl-core function takes an admitted query and returns at most 8
findings in source order, each with a kind, a byte span into the text
that was parsed, and fixed copy. When there are more, the surface says
how many it left out. The function reads only the search stage, walks it
with an explicit work list, and never parses, formats, clones or
compares the tree. The browser draws findings as CodeMirror warnings,
named as advice and not as syntax errors. The TUI shows them in a region
that wraps. `trawl validate` and `trawl query` print them to stderr, and
`trawl query` prints them before the query runs. MCP adds them to the
reply's computed hints in both modes. The server, `/validate`, `/query`
and the API types do not change: every surface links trawl-core and
computes advice from the text it holds, so no surface takes advice from
two sources. Under ADR-0039 this is a local mark: the client's
trawl-core decides it, and a version skew between client and server
changes only the advice.

**Advice names a different query, and says so.** ADR-0060's rule that a
hint never offers a rewrite that changes the meaning governs parse-error
hints. Advice exists to name the query the author probably meant, which
has a different meaning. Each spelling it offers says what it does:
`f!=…` keeps events that lack the field, `NOT f=…` drops them, and
`NOT "f=…"` keeps the text search the author wrote and silences the
advice. Any other operator gets `NOT f<op>…` and its quoted form. Copy
interpolates the field name only, and never a value, which is always
printed as `…`. A field name in an offered field filter goes through
`quote_dsl_field`, or `backtick_dsl_field` directly under `NOT`. Inside
the quoted text-search form it stays exactly as the author wrote it,
because there it is text, not a field reference.

**The two shapes.**
- A negated text term whose text is a field-shaped name
  (`is_bare_field_name`), then one of `=`, `!=`, `<`, `<=`, `>`, `>=`,
  then at least one more byte. This fires anywhere in the search stage,
  under `NOT` and inside brackets.
- In one AND sequence, a positive text term that is a field-shaped name,
  then the word `in` or `IN` and a bracketed group (`F in (`), or
  `not in` (`F not in (`), or `NOT` before `in` (`F NOT in (`). The group's
  contents do not matter. Time clauses are hoisted out of the search
  stage before this runs, so a hoisted clause between the field word and
  the group cancels the match.

**An unknown escape is reported where it is lexed.** Quoted strings keep
their five escapes (`\\`, `\"`, `\n`, `\t`, `\r`). Any other backslash
pair emits a parse error that spans the backslash and the character, and
keeps both characters in the value, so the string still closes. Later
checks then see the text that the hinted spelling `\\s` decodes to. A
second error they report is therefore true for the corrected query too,
and it reports after the escape error. A backslash at the end of the
input still reports an unterminated string. The hint names the doubled
backslash, the `/regex/` spelling for regex positions, and extract's
literal backslashes. Raw strings (extract and rex bodies, `sep=`) do not
change.

## Considered, not adopted

- **An `advisories` field on `/validate`.** It reaches raw HTTP clients.
  But every first-party surface computes advice locally anyway, so
  connected `trawl validate` and MCP validate would read the server while
  `trawl query` and MCP execute read the client, and the two disagree
  under version skew. Add it when a raw-API consumer asks for it.
- **A list of known fields.** It misses custom fields, and the catalog is
  server state that the editors do not have.
- **Concrete rewrites built from the author's values.** The quoting,
  pattern and null rules for an arbitrary value make a correct rewrite a
  large surface. A field name alone, rendered by the existing renderers,
  covers the lesson.
- **One finding per kind.** A second mistaken term of the same kind would
  get no mark until the first was fixed.
- **Reporting the escape inside an unclosed filter value.** `host="a\sb`
  with no closing quote falls back to the bare-value scanner and keeps
  today's `close the quote` error, which is true. The escape error
  appears once the quote is closed. Reporting it earlier needs a second
  escape table in that scanner. An unclosed quoted search or expression
  string has no fallback and reports `unterminated string literal`.

## Premise checks (read at aa8b8355)

- The negated term keeps its whole text: `text_search`'s negated arm,
  `crates/trawl-core/src/parser/search.rs:347-363`, stores
  `TextSearch { term: "service=coredns", negated: true }`.
- `(` is not a word byte (`search.rs:375-380`), so `in (a, b)` parses as
  the word `in` followed by `SearchToken::Group` (`search.rs:484-516`).
- `Hoist::take` (`search.rs:569`) removes time clauses from each
  top-level group, and `SearchStage.hoisted` (`crates/trawl-core/src/ast.rs`)
  keeps their spans in source order.
- `admit` runs the tree-depth check before it returns a query
  (`crates/trawl-core/src/parser/mod.rs:352-410`).
- `quoted_string` accepts five escapes (`primitives.rs:100-116`), and
  `raw_quoted_string` decodes nothing (`primitives.rs:122-132`).
- The browser lints with `trawl_core::parser::parse`
  (`crates/trawl-web-ui/src/components/editor.rs:41`), and the TUI parses
  in `maybe_validate` (`crates/trawl-cli/src/tui/state.rs:2192`).
- MCP's budget fitter drops the whole hints vector, and records the drop
  only for replies with a result view
  (`crates/trawl-mcp/src/render/budget.rs:570-577`).
- The installed 0.9.5 binary (04603b8) reports the texts in the table
  above, with the caret on the `s` byte only.
