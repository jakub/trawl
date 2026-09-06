# The search URL contract: readable where the grammar is closed, opaque where it is open, and a link that does not parse does not run

status: accepted (2026-09-06) — prep ruling record for #100 slice B

The search page keeps its whole state in the address bar: `q` (the DSL
text), `page`, `mode`, `f` (sidebar filters) and `r` (time range). One
producer writes the URL and one reader parses it back, so bookmarks, Back
and Forward and "copy link" are one mechanism. The browser decodes each
query value exactly once, before the app sees it, and reads a bare `+` as
a space. Two defects followed from ignoring that: the absolute range was
written as `abs:<from>:<to>` with percent-escaped colons that the browser
un-escaped, so the reader split inside the timestamp and every date-picker
range became an HTTP 500 (browser audit, 2026-09-04); and the pre-#85
plain-text filter reader, kept for old bookmarks, double-decoded its values
while its documented include form (`+host=`) had never survived the
browser at all. A malformed `f` or `r` silently ran a wider query than the
link claimed.

## Decision

**`q` and `r` are human-readable and stable. `f` is opaque and versioned.
A link whose structured state does not parse is shown, not run.**

- **`r` is `<from>..<to>` for an absolute range, the quick label
  (`1h`) otherwise.** Both bounds are RFC 3339 normalized to UTC `Z`; the
  right bound may be the literal `now`; `from` must not exceed `to`. No
  percent codec touches `r`: a timestamp cannot contain `..`, and a `Z`
  offset cannot contain the `+` the browser would turn into a space. The
  app normalizes to `Z` before it writes and accepts only `Z` when it
  reads. Opacity is what `f` earns because catalog names carry `,`, `=`
  and `&`; a timestamp pair from a closed grammar earns nothing, and a
  pasted link must show whether it spans fifteen minutes or a week.
- **The legacy filter reader is deleted.** Any `f` without the `v1.`
  prefix is malformed. Trawl owes no production back-compat, the old
  include links never worked, and a dialect nobody can produce is not a
  contract. Nothing rewrites a URL on the user's behalf; there is no
  canonicalization pass.
- **Malformed structured state refuses to run.** A bad `f`, a bad `r`, or
  a `page` beyond the server's result cap leaves the query unexecuted and
  shows a banner naming the parameter and echoing its raw value as text,
  truncated. The banner offers one repair (drop filters, use last 15m,
  page 0) that rewrites the URL only when clicked, with a replace
  navigation. The broken URL stays intact until then so it can be sent
  back to whoever shared it. An unparseable `page` is a missing value and
  reads as 0; an out-of-range one is a false claim and is malformed. A
  malformed versioned `f` is no longer "zero filters".
- **Bounds are validated and escaped before they enter the DSL.** The
  client checks RFC 3339 (or `now` on the right) before emitting
  `_time>=`/`_time<=`, and the bound still goes through the DSL string
  escaper `format_filter` already uses. Validation is a client courtesy;
  the server's opaque 500 on an unparseable bound is a `trawl-core` gap
  and stays outside this slice.
- **Decoding is bounded.** The raw `f` value is capped before base64 or
  JSON allocation, the decoded filter count and field/value lengths are
  capped, and the page offset is a checked multiplication. A URL is
  attacker-controlled input to the SPA.
- **The percent encoder is a pure, hand-rolled mirror of
  `encodeURIComponent`** (unreserved `A-Za-z0-9-_.!~*'()`, uppercase hex,
  UTF-8 bytes), used only for `q`. Two drift guards: an exhaustive native
  table test over every ASCII byte plus multibyte and malformed input, and
  one browser spec that submits the reserved set and compares
  `location.search` to the same literal. `percent-encoding` would need the
  same table written by hand and adds a direct dependency for nothing.
- **Back and Forward: the URL is the document.** After A → B → Back →
  Forward the URL is byte-identical to what the navigator built, exactly
  one query is posted per state change (the DSL sequence A, B, A, B), and
  the pills, range label, page, mode and the editor buffer all show the
  current entry. The editor buffer being replaced on Back is the contract,
  not a bug: a stale unsubmitted edit above results from another query is
  a lie about what ran, and the router exposes no Back-versus-push signal
  to do better with.

## Consequences

- `state/query.rs` splits into a pure, natively tested URL module and a
  thin wasm shell holding only navigation and the router memos.
- The docs site gains one paragraph on sharing a search that names `q` and
  `r` as stable and `f` as opaque and subject to change without notice.
  That sentence is what buys the freedom later.
- Slice E of #100 (date-range promotion to fleet-ui) reads `RangeSpec`
  through this contract; the `DateRange` component's props may change
  here, and E is named in the PR rather than frozen around.
