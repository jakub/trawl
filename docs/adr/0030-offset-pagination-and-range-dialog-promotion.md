# Offset pagination is one page-window model; the range dialog promotes whole

status: accepted (2026-09-08) — prep ruling record for #100 (slice E)

Four surfaces render `fleet_ui::Pager` around four hand-rolled copies of
the same offset arithmetic: history (URL-backed `?hpage=`, its own shadow
`PAGE_SIZE`), the runs page and the net drawer (byte-identical local
signals, two `RUNS_PAGE_SIZE` consts), and the results table (page lifted
to a prop, no total — Next is probed by "a full page came back"). The
runs copy computes its summary from the client-FILTERED row count, so a
filter matching nothing on a three-row page reads "1–0 of 3" with Next
enabled (2026-09-04 audit). Separately, ADR-0027 and ADR-0029 both defer
one question to this slice: whether the date-range dialog — since #162 a
`Trap` overlay dialog in `editor_wrap.rs` — is promoted into fleet-ui.
Both design legs of the prep dialectic converged on the shapes below
after mutual critique.

## Decision

**One pure page-window model in fleet-ui, wrapped by one thin component;
everything else stays where it lives.** `PageWindow` (page, size, returned
rows, and a `Known(total) | Probe` protocol) owns first/last/summary/
can_prev/can_next with checked arithmetic, natively tested beside
`load_more::phase` in the crate's pure-fn-plus-wasm-component idiom. An
`OffsetPager` wrapper renders the existing `Pager` from a window signal
and an `on_page` callback. Page-state ownership is unchanged per site
(URL where it is URL, signal where it is a signal), fetch wiring is not
extracted, and page sizes stay app policy — fleet-ui learns no size and
no router. `LoadMore` (cursor) stays a separate protocol.

**The window is computed from the unfiltered server page.** The runs page
converges on history's semantics: its local filter narrows the rendered
rows and annotates the summary ("0 matches on this page"), never the
window or Next. The net drawer remains unfiltered. This is a deliberate,
visible behaviour change on the runs page riding an extraction slice;
it lands red-test-first.

**The range dialog promotes whole — trigger plus popover — because focus
restore is a property of the pair** (the overlay hook restores to the
opener; splitting them reopens the defect ADR-0029 closed). fleet-ui owns
a simple two-armed range value (quick preset id | absolute from/to) as
the dialog's vocabulary; presets are app-supplied `{id, label}` with no
default, because a fleet-ui preset list would double as trawl's URL
allowlist (`QUICK_RANGES` feeds the reader that admits `r=`), letting a
design-system edit widen another app's accepted URL protocol. Validation
and commit are one app-owned accept-or-refuse callback.

**The dialog closes only after the app ACCEPTS the full transition.**
Valid bounds are only one part of a valid search link: the navigator can
refuse the built URL after a range validates (reachable at HEAD when the
executed query is near `MAX_SEARCH_BYTES`), and today that refusal lands
as a toast over a lost draft. Refusal now renders in the dialog's error
line with the draft intact. Live Tail is a presence-gated third tab
(`on_live: Option<Callback>`; absent means two tabs) with app-supplied
copy. Second visible behaviour change, same red-test-first rule.

## Consequences

- No re-export shim: trawl call sites update their imports directly.
- The `.dr-*` class names and CSS travel byte-identical into fleet-ui's
  style delivery; the selector-contract pins move to
  `component_class_contract.rs` in the same commit, and mutation patch 17
  is rewritten against the moved path (a path-addressed patch that fails
  to apply is a silently degraded check).
- A fleet-ui workbench mount (different presets, a refusing validator, no
  third tab) is the reuse proof — compile-and-render only, not a second
  maintained e2e suite.
- Pagination accessibility (landmarks, live summaries) is deliberately
  NOT invented here; it is named future work with its own browser pins.
  One carve-out ships: prev/next disable synchronously while a request is
  in flight. The extraction makes no snapshot or exactly-once claim over
  a changing dataset, and the known stale-response gap in the fetch layer
  is recorded, not silently absorbed — if the one delayed-response test
  demands a fetch redesign, that boundary is reported instead of crossed.
- history's `hpage` read folds into the single-decode `query_params`
  reader while those lines are open (#156 residual); schema and nets stay
  untouched.

*Amended 2026-09-12 (UI-audit remainder prep): the stored-run preview
pages the fetched result locally with `PageWindow` and `OffsetPager`, the
total being the rows fetched, never the run's recorded row count. The run
endpoint stays whole-result; the server's row ceiling is disclosed as a
separate line when the recorded count exceeds the rows fetched, and it is
independent of paging.*

*Amended 2026-09-20 (Visualization pagination prep, ADR-0037): the
surface inventory above gains one. The exact table pages an
aggregation's fetched result locally with `PageTotal::Known`, on the
stored-run preview's rule and with its cap line; the raw results table
keeps `Probe`. The `" (truncated)"` suffix both tables appended is
removed with the flag behind it.*

## Amendment: global Runs ordering

Accepted 2026-09-14 during the UI follow-up prep.

The global Runs list orders the caller's authorized result set before
applying limit and offset. All five columns are sortable: Net, Status,
When, Duration and Rows. The per-net run-history endpoint keeps its
existing contract. The net-name filter on the global page remains local
to the fetched page and keeps its "matches on this page" explanation;
the pager continues to describe the unfiltered server page.

The global endpoint accepts typed sort keys and directions, mapped to
fixed SQL fragments. Invalid keys or directions return a parameter error.
Net compares lowercased names using PostgreSQL's lowercase behavior and
the C collation; Status compares canonical status tokens lexically with
the C collation. When uses the stored timestamp, Duration and Rows use
numeric values, and missing numeric values sort last in both directions.

Initial order remains When descending. An inactive Net or Status header
first selects ascending order; When, Duration and Rows first select
descending order. Clicking the active header reverses direction. For
non-time columns, ties use start time descending then run ID descending.
For When, both start time and ID use the requested direction. These
rules make each response deterministic; polling and offset pagination
do not promise a frozen dataset across page requests.

Changing sort resets the page to zero and preserves the selected run.
Response ownership includes page, sort key, direction and refresh
generation. During a transition to another page or order, the old body
is hidden, the frame is busy and the pager is disabled. The headers may
show the requested order because no old rows claim to match it. A failed
transition shows its error and retry state. A background refresh of the
same page and order retains the current rows, including on refresh error.
Late responses cannot replace a newer selection of page or order.

## Amendment: History search before pagination

Accepted 2026-09-16 during prep for [#192](https://github.com/jakub/trawl/issues/192).
This amendment replaces the local-filter rule for History. The global
Runs list and net drawer retain their existing filtering contracts.

History searches the current key's stored query text before applying
limit and offset. The returned total counts matching entries. Matching is
a literal substring after PostgreSQL lowercase conversion on both operands.
Spaces and wildcard-looking characters are literal. Database locale governs
case conversion; it need not match the browser's former Rust conversion.

The user chose explicit Enter or Search submission because a substring
search with an exact total can scan retained history. Typing edits a draft
and leaves the applied result view in place. History storage has no
configured retention cap; the in-memory query tracker's capacity is not
a bound on this search.

The user also chose to store the applied filter in `hq`, beside `hpage`.
Reload, Back/Forward, and bookmarks restore this requested view. The searched
text appears in the URL. A link does not freeze results or change which
key's history the server authorizes.

Applying a changed filter pushes one URL history entry and resets the page
to zero. Submitting unchanged text refreshes the current page. A separate
Clear filter control applies empty text at page zero. Paging preserves the
applied filter and replaces the current URL entry, as it already does.
Paging discards unapplied edits; URL navigation synchronizes the draft to
the applied filter. Generated URLs omit empty filters and page zero.

The new filter is decoded once from the raw query representation. Duplicate
filter keys, invalid percent encoding or UTF-8, and NUL are rejected.
The existing Search decoder and History page-number semantics stay intact.
History retains its 32 KiB raw URL-query and 64-pair limits. Admission also
reserves room for the largest supported page number in the canonical
encoded URL. A failed submission leaves the draft, URL, and current view
intact with a validation error. A malformed pasted link makes no history
request and offers a reset to unfiltered page zero. The GET API has its own
bounded filter admission, specified in the issue.

Count and rows share the key/text predicate and one explicit read-only,
repeatable-read transaction. They agree within one response even when
another connection records or clears history. Ordering stays execution
time descending, then id descending. Separate page requests do not promise
a frozen dataset, and an empty out-of-range page preserves the existing
page-window recovery behavior.

Response ownership includes applied filter, page, request generation, and
component lifetime. A transition to another filter or page hides old rows,
shows a busy frame, and disables paging and export. Refreshing the same view
may retain its rows, with refresh state visible and those actions disabled
while pending. Late responses cannot replace a newer view. Draft edits alone
leave the loaded view and its actions unchanged.

ADR0025's Export and Clear decisions still apply. Export serializes the
applied view's loaded page. Clear deletes this key's entire history, including
entries that do not match the filter; the confirmation says so. A successful
Clear invalidates older reads, resets draft/filter/page, replaces the URL
with canonical History, and refreshes even if that URL did not change.
Pre-clear rows cannot return during a pending or failed refresh. Errors
preserve the existing failure and component-lifetime rules. Concurrent
query execution can still add a later history entry.

This decision adds no date filter, all-pages export, retention policy,
shared Fleet interface, or search dependency. The implementation records
query plans and measured cost on disposable synthetic history. An index
or installation-wide latency guarantee is not assumed.
