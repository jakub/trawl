# Two-valued text containment: negated text search is total

status: accepted (2026-08-18) — prep ruling record for #103

A bare or quoted search term asks "does this event contain the text" over
`message` OR `_raw`. The DSL has two negation spellings a user reads as
synonyms — `-term` and `NOT term` — and they diverged on sources without a
`_raw` column (embedded `--data` over foreign parquet):

- `-term` emits the total form
  `("message" NOT ILIKE ? AND COALESCE("_raw" NOT ILIKE ?, TRUE))` and
  answers correctly.
- `NOT term` wraps the positive leaf: `NOT ("message" ILIKE ? OR NULL)` is
  NULL for every non-matching row — a **silent empty result** that reads as
  "no data" rather than "broken query".

On the server corpus the spellings agree only by accident of
`envelope::canonicalize` stamping `_raw` on every event. Evidence — emitted
SQL diff, 7-row truth table, DuckDB differential proving both lanes agree
per spelling (a semantic split between spellings, not a lane bug) — lives in
the #83 journal; the draft was staged on PR #102 and acked at /flow:land.

## Decision

**Text-search containment is two-valued.** "Does this event contain the
term" is a property of the event with a plain answer when a column is
absent: it does not. The containment leaf therefore answers true or false,
never unknown — an absent `message`/`_raw` contributes *does not contain*,
not NULL — and every composition over it (`-x`, `NOT x`, `NOT "x"`, terms
inside OR groups) inherits totality for free. All negation spellings emit
the same total form in the search stage.

This **partially supersedes ADR-0011's strictness note**: the clause
extending strict SQL null logic to `NOT <bare term>` is withdrawn. The rest
of that note stands unchanged — field comparisons (`NOT f=x`, `f!=x`,
`NOT _severity=...`) remain three-valued exactly as ADR-0011 rules; this ADR
touches only the containment predicate for bare/quoted text terms.

Rationale for two-valued over the consistent alternative (making `-term`
strict too): strictness would make *excluding a word* silently drop every
row on `_raw`-free data — the trap grows. Containment is not a comparison
between two values where one is unknown; unknown-contains has no useful
reading for a search predicate.

## Scope

- `trawl-core/src/emitter/search.rs` search-token arm: one total form for
  positive and negated containment (the positive leaf's top-level behavior
  is unchanged — NULL and false are both filtered — so the only observable
  change is under negation on `_raw`-free sources).
- `trawl-core/src/filter.rs` `CompiledFilter` mirror: text-term matchers
  answer `Some(true)`/`Some(false)`, never `None`.
- `filter_parity` gains rows for absent-`message` / absent-`_raw` /
  both-absent events under each spelling.
- `docs/reference/dsl.md`: the `NOT <bare term>` sentence in the
  three-valued-logic section moves here.
