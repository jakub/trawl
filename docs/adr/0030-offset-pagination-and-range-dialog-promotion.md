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
