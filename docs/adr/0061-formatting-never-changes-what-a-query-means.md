# Formatting never changes what a query means

status: accepted (2026-11-03), prep record for the format round-trip fix; human rulings of 2026-11-03 recorded below

Trawl rewrites query text in several places. The web and TUI editors' Format button writes the formatted text back into the buffer. `/api/v1/validate` returns it as `formatted`, and the CLI client and the MCP server print it. `Composed::new` formats the compare and sightings queries it builds. The MCP server offers formatted queries as suggestions. ADR-0014 ruling 6, as amended by ADR-0060, promises that `format → reparse` gives the same query, proven by a property over generated ASTs.

The promise does not hold on main (`06d672f2`). Scouts for this prep reproduced each of these:

- `` `OR.a`="" `` formats as `OR.a=""`, which reparses as the group separator `OR`, a text search for `.a=`, and an empty phrase. The field filter disappears. `quote_dsl_field` compares the whole name with the keyword lists, while `keyword()` treats `.` as a word boundary.
- In `where` and `let`, `` `true.a` ``, `` `false.a` ``, `` `null.a` `` and `` `not.a` `` format bare and no longer parse.
- `` NOT `@x`=1 `` formats as `NOT @x=1`, which reads `NOT` as a search word and drops the negation.
- `matches "a/b"`, `matches ""` and `matches "["` format as `/body/`, which does not parse or means something else.
- `(a == b) == c`, `(a in (1)) == 1` and `(a == b) in (1)` lose their parentheses and no longer parse. `(a + b) in (1)` and `a + (b in (1))` format to the same text but emit different SQL.
- `- -1` formats as `--1`, a different AST.
- `(last=1h)` and `(OR)` format as `()`, which does not parse, and `/validate` answers `valid: true` with that text.
- `last=1h NOT` formats as `NOT last=1h`, which negates the time clause.
- `NOT-` followed by a backticked field formats as `NOT- `, which re-lexes as `NOT -`.
- `host="* f=/x` formats as `host="* f="/x"`, an unterminated string.
- `path=/tmp*` followed by a pipeline line holding `/` formats into one regex that runs across the line break.
- A 56 KB `* | head 1 | …` chain grows past the 64 KiB query cap when formatted.

The property never found these, because its generator covered only search-stage field filters and excluded the failing names. `hostile_names_round_trip_through_the_formatter` checks that formatting is idempotent, not that it keeps the AST.

Three neighbours of the same kind are parse defects, not formatting defects, and change meaning silently: `NOT (last=1h)` and `NOT NOT last=1h` drop the time window, and `NOT last=1h` ignores the `NOT`. Ruling 2 below removes all three.

## Decision

**A formatted query means what its source meant.** The canonical form of a parsed query is its AST with every span zeroed and the search stage's `hoisted` list emptied. `hoisted` records where time clauses sat in the source, as spans do. Nothing else is normalised: token order, group nesting, scalar kind, operator, list order, float source text, `Int` against `Neg`, and `TextSearch` against `QuotedSearch` all count. For every query that `parse` accepts, formatting returns either text whose reparse has an equal canonical form, or a typed refusal. Formatting that text again returns the same text.

**One function owns the canonical form.** `Composed::verify`, the checked renderer, and the splice proofs in `report_window.rs` and `results/bounds.rs` compare canonical forms. A proof that a rewrite kept a query's body does not compare formatted strings, because a renderer collision would certify a changed body.

**The checked renderer is the only public way to format.** It renders, reparses through the ordinary `parse`, compares canonical forms, and returns text only when they are equal. The raw renderer is private to `trawl-core`. Every caller that shows formatted text to a person or parses it again uses the checked renderer: `/api/v1/validate`, both editors' Format, `Composed`, and the MCP server's suggestions, hints and field queries. The checked renderer has two refusals:

- **Too long.** The renderer tries the readable layout first. If that exceeds `MAX_QUERY_LEN`, it tries a compact layout. If the compact layout also exceeds the limit, it refuses. The query still parses and runs. Parse admission does not depend on the formatted size.
- **Unfaithful.** The reparse failed or its canonical form differed. This is a formatter defect. The server logs it at error level with a hash of the query, and the generated tests treat it as a failure.

On a refusal, `/api/v1/validate` answers `valid: true`, omits `formatted`, and returns a typed format error that clients can tell apart from a query diagnostic. The editors keep the buffer and the undo history and show the reason. The MCP server omits a suggestion it cannot format.

**The formatter spells each token so its neighbours cannot change it.**

- A field name is backticked when its whole name or its first dot segment is a word the grammar reads with `keyword()` where a field may start. A drift-guard test scans the parser for every `keyword("…")` literal and requires each one either in that set or in a list of words that the grammar reads only in a fixed position (`by`, `as`, `on`, `from`, `sep`, `kv`, `span`, `run`, and the like). The field name keeps its backticks where the next context requires them, as after `NOT`. This changes rendering only: unquoted `OR.a` keeps its current reading.
- The right side of `matches` prints as `/body/` only when the parser's own regex-literal check accepts that spelling. Otherwise it prints as an escaped quoted string.
- Both operands of a comparison print at one precedence level above it, because the grammar takes one comparison suffix per expression. An `in` list is a comparison-level node.
- A unary minus whose operand prints with a leading `-` gets a space.
- Time clauses print first, in the order `last`, `earliest`, `latest`, so the token after each search term is the same as in the source.
- Quoted output escapes control characters, so a decoded tab or line break cannot end a regex that an earlier value opened.

**Parse admission narrows where a spelling cannot be stable (human rulings, 2026-11-03).** Each of these is a parse error with a span and a hint. Before this decision, each one parsed into something its author did not mean or that the formatter could not spell.

1. **A regex body holds no raw line break and no `|` after whitespace.** A new pipe stage always ends what came before it. `path=/tmp*` followed by `| where x / 2 == 1` is the glob `/tmp*` and a pipeline, whether the stage is on the same line or a new one. `/err|warn/` is still a regex. A `/…/` run that would close only by crossing a line break, such as `msg=/a` on one line and `b/` on the next, is a parse error whose hint says to write `\n`. A run that `|` after whitespace cuts, such as `msg=/err | warn/`, is a parse error whose hint says to drop the spaces or write `\s`. This amends ADR-0060, whose regex body was "one or more bytes, none of them `/`".
2. **A time clause is legal only as a top-level search token.** `last`, `earliest` and `latest` are refused inside parentheses and under `NOT`, with the hint "time clauses apply to the whole search; write it outside the group". `a last=1h OR b` stays legal and keeps its hoisting order. This removes `(last=1h)` groups that hoisting leaves empty, which match in batch and match nothing in the live lane today. It also removes the three silent window defects above.
3. **A bare filter value that opens with `"` is an error**, with the hint "close the quote". Quoting makes a glob exact, so a glob such as `host="*` had no spelling. This reverses the gap that ADR-0060 left open.
4. **A group that holds only `OR` separators, such as `(OR)`, is an error**, as `()` already is.
5. **`NOT-` directly before a backtick is an error**, next to the existing refusal of `-` before a backtick.

**Stored query text may stop parsing on upgrade.** Saved queries, schedules (through their saved query), investigation steps and captured results hold DSL text. A stored query that hits one of the five errors fails its next validation or run with that diagnostic. Trawl is forward-only (AGENTS.md, "Stored data"). Restoring the old behaviour means restoring a consistent pair, the Postgres database and `[server] results_dir`, taken together with trawld stopped. The pull request's Revert section names that pair.

**Evidence covers the parser's whole image, not a chosen domain.**

- A text-driven property generates query text from a token set: dotted and keyword-prefixed names, `@` names and backticked names; every value kind, including slash-opening globs and quote-bearing values; text terms such as `NOT`, `NOT-` and `-`; quoted phrases; time clauses; groups; and every pipeline stage with expressions. It keeps the inputs that parse and asserts a faithful format or a too-long refusal.
- The AST property extends to text, quoted phrases, time clauses and pipelines. An exclusion is allowed only where a parse error makes the AST unreachable, and the exclusion names that error.
- A `format_roundtrip` cargo-fuzz target asserts the same, with a seed corpus of every defect above and a bounded CI smoke run beside the existing fuzz smoke.
- A cross-product test pairs risky tokens with every following context: commas, `)`, backticks, comments and appended pipeline text.
- The canonical form has negative tests: pairs that differ only in a predicate, a scalar kind, a negation, a time slot, stage order, a float token or the bare-`*` wildcard compare unequal.
- Meaning tests compare SQL results and live verdicts before and after formatting within each lane, so existing differences between lanes stay visible and are not counted as formatting defects.

New whole-tree walks (canonical form, rendering, the checked renderer's comparison) use explicit stacks, because a flat chain can be deeper than `MAX_EXPR_DEPTH` allows on a worker-sized stack.

Considered and rejected:

- **Quote only `OR.` and `or.` field names.** Five more defects of the same class would remain, and the ADR-0014 promise would stay false.
- **Change `keyword()` so `.` is not a boundary.** It would silently change what existing text `OR.a=x` means.
- **Treat a bare text term and an equal quoted phrase as one query.** It is true of today's search code, but it would write a meaning rule into the formatting contract. If bare words ever become token matches, formatting would change meaning and no test would fail.
- **Make the formatted size a parse admission rule.** Whether a query runs would then depend on how it is displayed.
- **A formatter with one layout.** A 49 KB `where x in (…)` list of short ids formats to 66 KB, so Format, compare and MCP suggestions would refuse a realistic query that is well under the cap.
- **Let hoisting delete the empty group and the `NOT` residue.** The live matcher treats an empty group as false and the SQL emitter drops it, so deleting the node would change live results inside a formatting change.
- **Refuse a slash-opening glob that a later `/` could close.** `path=/tmp*` would be legal or illegal depending on unrelated later text.
- **Leave `format_query` public and infallible.** Every caller that formats would keep the failure class this decision removes.

Out of scope, and not filed by this prep: `extract kv sep="ab"` silently falls back to `=`, and unquoted `NOT @x=1` reads `NOT` as a search word. Both are parse-level meaning defects that format faithfully.

The reference (`docs/src/content/docs/reference/dsl.md`, also served by the MCP server) changes with the code: the regex body rule, the time-clause placement rule, the three other new errors, and a sentence that Format removes comments. ADR-0014 ruling 6 and ADR-0060 are amended by this decision.
