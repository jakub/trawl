# A search-stage filter keeps the operator its author wrote

status: accepted (2026-10-30), prep record for the search-stage pattern fix; human rulings of 2026-10-30 recorded below

Two parser defects give a search-stage field filter a meaning its author did not write, and nothing reports either one.

- **A regex before `)` is read as text.** After a closing `/`, the regex arm of the search-stage value parser admits only ASCII whitespace, `|` or the end of the query. Parenthesised groups came later (65e1a056), and `(` and `)` joined the bare-value stop set but never this lookahead. So `(a OR msg=/x/)` falls back to a bare value and matches the text `/x/`, or the glob `/x.*/` when the body holds `*` or `?`. The formatter then quotes that literal faithfully, so `Composed::verify` (ADR-0058) sees the same wrong reading on both sides and passes it. A real regex that a group closes, written `(msg=/x/ )`, formats tight as `(msg=/x/)`, reparses as a literal, and comes back as `correlation_invalid`.
- **A detected pattern replaces the written operator.** `final_op = if auto_op == Eq { op } else { auto_op }` keeps a glob or regex whatever operator was typed. `host!=web*` therefore matches the hosts it was written to exclude, `host!=*` means "has host", and `host<a*` is silently a glob. The reference documented the override as the rule ("a glob whatever operator you typed").

Comma lists carry a third form of the same defect. A list never auto-detects, so `host!=web*,db` excludes only the exact text `web*`, while the reference reads `f!=a,b` as `f!=a AND f!=b`.

The root of the second defect is that `FilterOp` holds two facts, the comparison the author typed and the kind of value, and the parser has to drop one of them. The parser tests never caught either defect: each one compares the parser with itself (format then reparse, idempotence, and a correlate property that counts `Reparse` as an allowed outcome), and a misreading the parser makes the same way twice passes all of them.

## Decision

**The operator is what the author typed, and the value carries its own kind.** A search-stage field filter holds a comparison (`=`, `!=`, `<`, `<=`, `>`, `>=`) and a value. The value is a scalar or a comma list, and each scalar is an exact value, a glob or a regex. A quoted scalar is always exact. An unquoted scalar holding `*` or `?` is a glob, and a recognised `/…/` (below) is a regex. Nothing in the parser, the formatter or a consumer derives one fact from the other. Function placement and type names belong to the implementation; the rule is that pattern-ness is never an operator.

**Patterns take `=` and `!=` only.** An ordered comparison with a glob or a regex, in a scalar or in a list, is a parse error. Its span runs from the operator through the end of the value, and its hint names `=`/`!=` and, for a glob only, quoting the value to compare text. A hint never offers a rewrite that changes the meaning. `host<"a*"` stays the ordered comparison of the text `a*`. Ordered comparisons with a list keep their existing error.

**`f!=P` is the complement of `f=P` under the search-stage `!=` rule.** It is true unless the field has canonical pattern text (`compare::pattern_form` under its pin) that P matches. An event without the field, or whose value is shelved to NULL by its pin, matches `f!=P`, as it matches `f!=x` (`NullPolicy::NeMatchesNull`). The SQL is `NOT COALESCE(<pattern predicate over pattern_target>, FALSE)`, so it does not depend on every target renderer turning a NULL field into a NULL target. The live lane answers the same way, so `f=P` and `f!=P` divide every event between them in both lanes. `NOT f=P` keeps three-valued logic and drops events missing the field, which the reference already says of `NOT f=x`. As a result `f!=*` selects events that lack `f`.

**Lists honour each element's kind (human ruling, 2026-10-30).** `f=a,b*,/c/` means `f=a OR f=b* OR f=/c/`, and `f!=a,b*,/c/` means `f!=a AND f!=b* AND f!=/c/` under the `!=` rule above. A quoted element is exact. A list of exact values may still emit as one membership test; the meaning is what is fixed, not the SQL spelling. The prep's two designers had proposed a parse error for pattern elements. The human pointed out that the reference already reads `f!=a,b` as `f!=a AND f!=b`, and an author writing `host!=web*,db` means "neither": an error would refuse the sentence the reference promises.

**A regex ends where a bare value ends.** After the closing `/` of a candidate `/body/` (body: one or more bytes, none of them `/`; the body rule is unchanged):

| Next byte | Reading |
| --- | --- |
| ASCII whitespace (`\t \n \x0c \r` and space), `\|`, `)`, `(`, backtick, end of query | A regex. `(` and backtick begin the next search token, as they already do after a bare value. |
| `,` | A regex list element; the list continues. |
| `"` | A parse error. A complete `/x/` never silently becomes text. |
| `#` | The existing ADR-0014 opener error, through the bare-value path. |
| Anything else (letters, digits, `/`, `]`, `=`, `*`, `?`, non-ASCII whitespace) | Not a regex. The whole run is an ordinary bare value, so `path=/api/*` is a glob and `path=/var/log` is exact. |

A regex body may hold a `,`: `/a,b/` is one regex, because the regex is recognised before the list is split. Regexes take no trailing flags. `msg=/err.*/i` is a glob on that text (the `i` continues the word), and the reference says to write `(?i)` inside the pattern.

**A recognised regex that does not compile is an error.** The order is shape, then next byte, then body validation. A candidate whose next byte makes it a regex, but whose body fails to compile or is longer than 1024 bytes, is a parse error that covers the whole `/…/`. It never falls back to a literal, a glob or a text term. A candidate whose next byte makes it a bare value is never validated, so `/[a/]/` and `/tmp/s` raise no false error. `msg=/*/`, today a glob that formats quoted and reparses as an exact value, becomes an invalid-regex error. The length message says bytes, which is what it counts. The pipeline `matches /re/` shares this body validation and keeps its own expression-grammar ending.

**Errors are emitted, never returned from an alternative.** The parser recovers from a failed alternative by trying the next one, which is how both defects stayed silent. Every new error comes from a branch that succeeds structurally and emits a diagnostic, like the existing ordered-list error and the ADR-0014 opener error. When a value already carries an ADR-0014 opener error, the parser adds no pattern or operator error for it, so `f>#a*` still reports exactly one error.

**The formatter spells kinds, not operators.** It writes the operator as typed, a regex as `/body/`, a glob bare, and an exact value quoted when its bare spelling would read as anything else. Group rendering stays tight: `(msg=/x/)` now reparses as the regex it was. `Composed::verify` keeps its AST-equality check unchanged. The search-url `parenthesize()` branch that adds a space before `)` after a trailing `/` is deleted; its test keeps guarding the tight form. One gap stays open and named: a glob whose bare spelling needs quoting (it holds `"`, `#` or whitespace, as in `host="*`) has no spelling, because quoting makes it exact. #321's `host<"*` example becomes an ordered-pattern parse error. The `=`/`!=` form is the remaining case, outside this decision.

**The grammar's contract is what queries mean.** The tests that guard this decision check meaning, not the parser against itself:
- a table from query text to an expected meaning, in a notation written in the test (spans ignored), never produced by `format`;
- a context-invariance property: each table filter means the same alone, in a group on either side of `OR`, doubly grouped, under `NOT`, before a pipe and before a comment;
- a nested round trip over generated ASTs, `parse(format(ast)) == ast` without spans, over a stated domain (valid slash-free regex bodies, lists of at least two elements, globs that need no quoting, no glob spelled like a regex);
- complement and parity tests across the SQL and live lanes for every pin class and for missing, null, shelved, matching and non-matching values;
- a correlate property over well-formed filters in which `Reparse` is a failure.

**Stored query text changes meaning on upgrade.** Saved queries, schedules (through their saved query), investigation steps and captured results hold DSL text. After this decision `service!=nginx*` excludes nginx, `(… msg=/re/)` is a regex, `host!=*` selects hosts that are missing, and `host<a*`, `msg=/[/` and `msg=/*/` no longer parse. Trawl is forward-only (AGENTS.md, "Stored data"). Restoring the old meaning means restoring a consistent pair, the Postgres database and `[server] results_dir`, taken together with trawld stopped. ADR-0057 commits capture rows and their files as a pair, and ADR-0059 evidence lives in the same root.

Considered and rejected:

- **`NotGlob` and `NotRegex` operator variants.** The diff is smaller, but the operator still holds the value's kind, every pattern check becomes four-way, and the quoting rule stays operator-dependent.
- **A negation flag.** It gives `!=` two spellings.
- **Rewriting `!=` patterns as `NOT`.** The null rule is wrong, and the formatter would rewrite what the author wrote.
- **`(NOT pred OR field IS NULL)` for negated patterns.** It matches `NOT COALESCE` only while every pattern target is NULL exactly when its field is. The severity renderer's `CASE` has no `ELSE`, so the two differ on values outside the ladder. The conform invariant excludes those values, but the contract should not lean on it.
- **Loud errors after `(` and backtick.** The grammar already reads both as the start of the next token after a bare value, and a rule that holds only after a regex would make a value's kind depend on its neighbour.
- **`"` as a literal continuation.** It is consistent with `host=a"b`, but a complete `/x/` silently becoming text is the defect this decision removes.
- **Lists stay exact only, or pattern elements are an error.** The first keeps the inverted alert. The second refuses what the reference already promises.

The reference (`docs/src/content/docs/reference/dsl.md`, also served by the MCP server) changes with the code. ADR-0014 ruling 6 is amended by this decision: the quoting rule no longer depends on the operator, and format then reparse is proven over generated nested ASTs, not a fixed table.
