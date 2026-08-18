# Grammar-owned comments: retire the pre-parse scanner, drop `//`

status: accepted (2026-08-17) — prep ruling record for #83

Comments entered the DSL as a pre-parse byte walker
(`trawl-core/src/parser/mod.rs::strip_comments`): blank every `#`/`//` run to
end-of-line, preserving byte length so error spans still point into the
original input, then hand the blanked text to the grammar. Whether a `#` is a
comment depends on whether it sits inside a double-quoted string, a regex
literal, or a backtick-quoted name — so the walker has to re-derive the
parser's own decisions without being the parser. ADR-0013 ruling 7 (backtick
identifiers) forced it from two states to three; each round added another
grammar fact restated in byte-walking form.

The class does not converge. A probe of the shipped scanner (2026-08-17)
found that ordinary query shapes parse **successfully** as silently different
queries — no error, no warning:

| typed | executed |
|---|---|
| `message=/a#b/` | `message == "/a"` — an equality on a literal, not a regex |
| `url=https://example.com/x` | `url == "https:"` |
| `referrer=https://a.b/c status=200` | `referrer == "https:"` — **`status=200` is deleted** |
| `color=#ff0000` | text search for `color=` |
| `path=/api//v1` | `path == "/api"` |
| `foo#bar` | text search for `foo` |

A `//` does not merely truncate its own token: it blanks to end-of-line, so
sibling filters on the same line disappear and the query silently widens.
`https://` is ubiquitous in log data, which makes this the everyday case
rather than the pathological one.

This ADR records the redesign. As in ADR-0013, there is **no
backward-compatibility requirement** — no deployed corpus and no supported
public grammar to migrate — so a breaking language change is priced at zero
and no dual-path machinery appears anywhere below.

## Decisions

### 1. A comment is a grammar production, not a pre-pass

`comment = '#' ~ (!'\n')*`, admitted wherever the grammar skips whitespace.
The parser never asks "am I inside a string / regex / backtick name" — it
*is* inside them, so `#` is only ever tested at a position the grammar
already knows is between tokens. One owner, zero re-derivation: the same
move ADR-0013 applied to the alias rule, the field-name quoting helper and
the projection collision check.

`strip_comments` and its two helper predicates (`opens_quoted_name`,
`quoted_name_end`) are deleted outright. Nothing else calls them.

Consequence, load-bearing: parse-error spans become natively correct.
Today they are only correct because the scanner is byte-length-preserving,
an invariant five consumers depend on without stating it — the web UI's
CodeMirror squiggles, the TUI editor's caret regions, the CLI's caret
renderer, and the wire `ErrorSpan` the server sends to both. Chumsky spans
point into the real input, so the invariant stops needing to exist.

### 2. Inside an unquoted token, a comment opener is an error — never data, never a comment

A comment opens only where the grammar sits between tokens: start of input,
or after whitespace. ("After a delimiter" was in the original wording and
is dropped: the delimiter set is unenumerable — `=` cannot qualify, or
`color=#ff0000` stops erroring — so the rule is encoded structurally
instead, as `layout = (whitespace+ comment?)*` plus one leading-comment
site at the start of the input. The consequence is strictly the loud
direction: `count(),# x` and `(#x` are parse errors rather than
comments.) A `#` **inside** an unquoted token is a parse error carrying
the byte position and a hint to quote:

```
foo#bar        → error: '#' inside a search term — quote it ("foo#bar") to
                 search for it, or put whitespace before it to start a comment
color=#ff0000  → same error on the value
a=1# note      → same error on the value
a=1 # note     → filter a=1, plus a comment          (unchanged)
```

Quoted contexts carry `#` verbatim and always have — a double-quoted string,
a backtick-quoted name (ADR-0013 ruling 7), and a regex body. Those
productions contain no whitespace-skip site, so the comment rule cannot reach
inside them, which is what retires the regex residual: `message=/a#b/` is the
regex `/a#b/`.

The alternative rules were both rejected for being quiet (see *Rejected*).
This is the third application of the rule ADR-0013 established for backticks
in `bare_value` and the search-stage bare word: **loud, or correct — never
something silently different.**

### 3. `#` is the only comment opener; `//` is dropped

`//` collides with the data far harder than `#` does: `https://`, `//cdn…`,
`/api//v1` are routine in log corpora, while `#`-bearing values (hex colours,
`foo#123`) are rare and, under ruling 2, fail loudly with a hint. `#` cannot
collide with a URL scheme at all.

With `//` gone, a value may carry it freely and needs no quoting:

```
url=https://example.com/x          → value https://example.com/x
referrer=https://a.b/c status=200  → BOTH filters, correctly
path=/api//v1                      → value /api//v1
```

The loud half of the removal: a **search-stage bare term that starts with
`//`** is a parse error naming `#`, so an existing `// note` line fails
instead of silently becoming two AND-ed text terms that narrow the result set
to nothing. The restriction is on a term at a token boundary only — a *value*
after an operator is not a comment site, so `url=//cdn.example.com/x` is an
ordinary value.

The evidence that this costs nothing: the DSL reference never documented
comments. `#`/`//` appear there only incidentally, inside the backtick
discussion. There is no taught syntax to break.

### 4. One padding owner, enforced by lint

There is no shared padding combinator today — the parser calls chumsky's
built-in `.padded()` at 84 sites across `expr.rs`, `pipe.rs` and `search.rs`.
The comment rule is introduced as a single project-owned padding parser that
every site adopts, and `chumsky::Parser::padded` is added to clippy's
`disallowed_methods` so a missed site, or a new one added later, fails the
build rather than silently becoming a place comments do not work.

`.padded()` is also what separates search-stage tokens and pipeline stages,
so the single substitution covers `a=1 # note\nhost=x` and
`| stats count()\n# note\n| sort -count` without a special case.

### 5. Scope: one correctness walker moves, two display walkers stay and say so

Four other hand-rolled DSL walkers exist. Exactly one of them can change an
answer, and only that one is unified here:

- **moves** — `trawl-web-ui/src/query_merge.rs::scan_outside_quotes`. It
  decides where the search stage splits and whether a `last=` clause is
  already present, so the date-range popover rewrites a span chosen by this
  walk. It reads a bare `/` as a regex open and has no comment case at all.
  It consumes a scan primitive exported from `trawl-core` instead.
- **stays, scoped** — `trawl-cli/src/tui/highlight.rs::tokenize` (no `#`, no
  backtick awareness) and `trawl-cli/src/tui/autocomplete.rs`'s
  `open_backtick_prefix` / `inside_string_or_regex`. These drive colours and
  ghost text. They are already divergent, this change does not worsen them,
  and unifying them needs a partial-input policy that is its own design
  problem. Each gains a doc comment stating the honest scope: display only,
  never correctness.

### 6. The formatter's quoting rule is re-derived, not inherited

`format.rs::needs_quoting` exists to keep `format → reparse` yielding the
same AST, and it currently quotes on `#` **and** on `//` because that is what
the scanner blanked. Under rulings 2 and 3 the `#` half stands and the `//`
half is obsolete for a value. It is re-derived against the new grammar rather
than left as harmless over-quoting, because a stale mirror of a deleted rule
is exactly the drift this ADR is retiring.

## Rejected

- **Terminator (zero behaviour delta)** — `#` ends a bare token and opens a
  comment, reproducing the blanking exactly. Ordinary queries stay
  bit-identical and the regex and quoted-context bugs are still fixed, but
  `foo#bar` still silently means `foo` and `color=#ff0000` still silently
  means a text search for `color=`. It preserves the silent-truncation class
  it was brought in to remove.

- **Absorb (comment only between tokens, tokens greedy)** — the literal
  reading of "comments wherever whitespace is". `foo#bar` becomes a real text
  term and both residuals named in #83 parse as working queries. But
  `a=1# note` silently flips from `a=1` to a value `1#` plus a text term
  `note`: it removes one silent-change class by introducing another.

- **Keep both openers under ruling 2** — nothing silent in either direction,
  but `url=https://example.com/x` becomes a permanent parse error requiring
  quotes on the single most common URL filter shape. Correct and consistent;
  rejected for daily friction against a feature (`//` comments) nobody was
  taught.

- **Drop `#` and keep `//`** — frees `#` for hex colours and issue refs but
  retains the opener that deletes sibling filters and collides with every
  URL scheme. Trades a rare papercut away to keep a common one.

- **Drop comments entirely** — every collision class disappears and the
  grammar gets simpler than either single-opener option, but multi-line
  queries in the TUI and web editors and saved scheduled reports lose their
  only annotation mechanism. A larger behavioural cut than the problem needs.

- **Unify all four walkers** — the complete one-owner answer #83 gestures at.
  Deferred within this design, not to a follow-up ticket: the autocomplete
  walkers need a partial-input policy for an unterminated token at the
  cursor, which is a separate design problem, and folding it in doubles a
  PR that is already the whole parser's padding surface.

## Consequences

- Breaking language change, twice over: `//` stops being a comment opener,
  and a comment opener inside an unquoted token stops being tolerated. Both
  halves fail loudly. Under the project's no-back-compat stance this needs no
  migration path, but it does need the DSL reference to gain the comments
  section it never had.
- Every query shape whose meaning changes moves from a silently wrong answer
  to either a correct answer or a parse error with a hint. There is no shape
  that goes from working to silently different.
- The byte-length-preserving span invariant is retired rather than
  maintained; the five consumers that leaned on it are served by real spans.
- Residual: the two TUI display walkers keep their own reading of the query
  text, now documented as display-scoped.
