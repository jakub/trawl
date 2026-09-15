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
