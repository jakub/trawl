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
  a `page` whose offset the request cannot carry leaves the query
  unexecuted and shows a banner naming the parameter and echoing its raw
  value as text, truncated. A page is malformed exactly when
  `page * PAGE_SIZE` does not fit `MAX_OFFSET`, which is `u32::MAX`, the
  browser's own `usize`. The server's `max_result_rows` is
  per-deployment configuration that appears on no API response, so the
  client does not mirror a number it cannot read: a page past the real
  cap gets the server's 400 through the error banner the results pane
  already has (run ruling 2026-09-06, closing a prep gap). The banner
  offers one repair (drop filters, use last 15m, page 0) that rewrites
  the URL only when clicked, with a replace navigation. A repair whose
  own candidate link cannot be admitted degrades to "Start over" — the
  banner keeps its sentence and swaps the button, because carrying the
  other parameters through re-encodes them and can push the repaired
  link past the length bound, and a button that refuses itself on a page
  where every other control is disabled is a dead end (run ruling
  2026-09-06). The verdict is computed with the href, not discovered at
  the click. The broken URL
  stays intact until then so it can be sent back to whoever shared it. An
  unparseable `page` is a missing value and reads as 0; an out-of-range
  one is a false claim and is malformed. A malformed versioned `f` is no
  longer "zero filters".
- **While a parameter is malformed, the repair is the only control that
  navigates or runs.** Blanking the effective query is not the gate: the
  export modal read that empty string and posted it, and the server's
  emitter turns an empty query into `SELECT *` with no WHERE, so a link
  the page had refused exported the whole corpus (run ruling 2026-09-06).
  Every callback that navigates or submits returns early while the link
  is unreadable, and every control that reaches one renders `disabled` —
  Haul (and the editor's own Ctrl+Enter), the range presets, Apply, Live
  Tail, Save, Export, pagination, facet include/exclude and clear, chip
  removal, and the row actions that navigate. A modal already open when
  the URL turns unreadable is closed. Nothing is re-routed through the
  fallback memos: blocking is the fix. Underneath it, an empty query
  never reaches an execution endpoint at all — the export modal refuses
  it in its own body and `api::export` refuses it again.
- **Bounds are validated and escaped before they enter the DSL.** The
  client checks RFC 3339 (or `now` on the right) before emitting
  `_time>=`/`_time<=`, and the bound still goes through the DSL string
  escaper `format_filter` already uses. Validation is a client courtesy;
  the server's opaque 500 on an unparseable bound is a `trawl-core` gap
  and stays outside this slice.
- **Decoding is bounded, starting with the whole link.** A raw query
  string over `MAX_SEARCH_BYTES` (32 KiB) is one malformed verdict about
  the link itself, refused by length before it is split into anything and
  repaired by starting over at `/search`; a per-parameter cap bounds
  nothing while the number of parameters does not, and `?a&` a million
  times used to be a million owned pairs (run ruling 2026-09-06). Inside
  that bound only the five keys this app reads are retained, first
  occurrence each, over at most 64 non-empty pairs, so an unknown
  parameter costs a name comparison rather than a decoded copy and does
  not survive a repair. Both whole-link bounds fail CLOSED: reaching
  either one refuses the link rather than answering from the part that
  fit, because the cap that merely stopped reading let 64 empty pairs
  push an unreadable `f` out of sight and run the link with no filters
  and no banner (run ruling 2026-09-06). The producer asks the same
  reader before it navigates (`admit_search` on both navigators, beside
  `admit_filters`): a link this app cannot read back is never written,
  so a query too long to share is an error toast with the address bar
  and the editor buffer untouched, not a banner over an editor the page
  just emptied. Then per parameter: the raw `f` value is capped before
  base64 or JSON allocation, the decoded filter count and field/value
  lengths are capped, and the page offset is a checked multiplication. A
  URL is attacker-controlled input to the SPA.
- **The SPA reads the raw query string and decodes it once itself.**
  `leptos_router`'s `ParamsMap` percent-decodes a value `UrlSearchParams`
  has already decoded, which turns `?q=message%3D%2F100%2541%2F` into the
  query `message=/100A/`, so the reader takes `use_location().search` and
  applies the `application/x-www-form-urlencoded` rules in the same pure
  module as the encoder (run ruling 2026-09-06). `f` is base64url and `r`
  is a closed timestamp grammar, so only `q` could carry the `%` that
  exposed it.
- **The percent encoder writes the string the browser will keep**
  (unreserved `A-Za-z0-9-_.!~*()`, uppercase hex, UTF-8 bytes), used only
  for `q`. That is `encodeURIComponent`'s set minus the apostrophe, and
  the app encodes the apostrophe because the browser does: `'` is in the
  URL standard's special-query percent-encode set, so a literal one is
  stored as `%27` and an encoder that wrote it literally measured a third
  of the bytes admission is about, letting ~10 900 apostrophes through
  the door and back as the "too long" banner (run ruling 2026-09-06).
  Two drift guards: an exhaustive native table test over every ASCII byte
  plus multibyte and malformed input, and one browser spec that submits
  the reserved set and compares `location.search` to the same literal.
  `percent-encoding` would need the same table written by hand and adds a
  direct dependency for nothing.
- **Back and Forward: the URL is the document.** After A → B → Back →
  Forward the URL is byte-identical to what the navigator built, exactly
  one query is posted per state change (the DSL sequence A, B, A, B), and
  the pills, range label, page, mode and the editor buffer all show the
  current entry. The editor buffer being replaced on Back is the contract,
  not a bug: a stale unsubmitted edit above results from another query is
  a lie about what ran, and the router exposes no Back-versus-push signal
  to do better with.

  *Amended 2026-09-12 (UI-audit remainder prep, Search live-mode
  coherence): `mode=live` is a claim the page keeps true. Leaving live is
  a push navigation to the same `q`, `f` and `r` with `mode` elided, from
  Stop live or from any committed range selection; Haul and the filter
  controls keep the current mode. No local pause exists: a frozen table
  under a live URL is a false claim, the class this ADR refuses to run.
  While live, `/api/v1/query` is not called, and status, counts, the
  filter rail and the histogram read only the active result source. A
  same-`q` navigation does not replace the editor buffer: the sync is
  keyed on the executed query, so an unsubmitted edit survives Stop live
  the way it survives a page change.*

## Consequences

- `state/query.rs` splits into a pure, natively tested URL module and a
  thin wasm shell holding only navigation and the router memos.
- The docs site gains one paragraph on sharing a search that names `q` and
  `r` as stable and `f` as opaque and subject to change without notice.
  That sentence is what buys the freedom later.
- Slice E of #100 (date-range promotion to fleet-ui) reads `RangeSpec`
  through this contract; the `DateRange` component's props may change
  here, and E is named in the PR rather than frozen around.
- Accepted gap: a link to page 2000 is inside the offset ceiling, so it
  runs, and a default install answers it with the API error rather than
  the malformed banner. One broken link, two different explanations.
  Closing that means publishing the cap on a response, `/api/v1/health`
  being the obvious place, which is its own decision and not a client
  one.

## Amendment: execution facts in the query console

Accepted 2026-09-14 during the UI follow-up prep. The mode badge was
simplified on 2026-09-15.

The executed-scope strip keeps its hanging well and removable filter chips.
A Live badge appears only while streaming; snapshot searches have no mode
badge. The old scope label and window caption are
replaced by rows returned, execution duration and a timestamp labelled
"Started". Snapshot rows mean rows in this response, including aggregate
groups; they do not claim a corpus total. Live counts name buffered rows.

Successful query responses carry one optional execution record containing
an absolute UTC start time and duration in milliseconds. The server
captures the UTC instant beside its existing monotonic timer and uses
that timer's existing duration boundary. This includes admission and
query execution but excludes later history processing, serialization and
browser transit. An empty placeholder represents no execution and has
no record. A real execution may measure zero milliseconds.

Execution facts belong to the accepted response and the executed query
captured with it. Draft edits do not change them. A new pending request,
failed request or malformed link cannot present facts from a retained
previous response. A successful zero-row response still has execution
facts. Live mode presents no snapshot start time or duration. Page
navigation runs a new query and therefore supplies new execution facts.

The timestamp includes the date, seconds and UTC suffix. The display
does not substitute the browser's request latency or current clock.
Existing malformed-link refusal and repair behavior, unreadable-filter
notice and guarded chip removal remain in force. No query ID is added
by this decision.
