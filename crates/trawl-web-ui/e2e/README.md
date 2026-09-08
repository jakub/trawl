# trawl-web-ui e2e suite

Real-browser Playwright coverage for the leptos SPA (issue #118). This
suite drives an actual chromium browser against the **built** SPA served
by a tiny zero-npm-dependency stub server (`harness/server.mjs`) that
stands in for `trawl-web`'s `/api/*` surface with canned, wire-shape
responses (`harness/fixtures.mjs`). It never talks to a real trawld,
postgres, or fleet-auth keystore — auth here is a stub `200`/`401`, not a
real cookie/AEAD/session round trip. **Real auth/session behavior
(cookie issuance, AEAD, the keystore) is covered by trawl-web's own test
job, not this suite.**

## Running it

```sh
cargo xtask e2e                       # trunk build (dev) + npm ci + install chromium + run
cargo xtask e2e --skip-build          # reuse an existing crates/trawl-web-ui/dist/
cargo xtask e2e --release             # trunk build --release first
cargo xtask e2e --headed              # visible browser window, for local debugging
cargo xtask e2e --grep "live tail"    # only run matching spec titles
```

Equivalent by hand, from `crates/trawl-web-ui/`:

```sh
trunk build                           # or --release
cd e2e
npm ci --no-audit --no-fund
npx playwright install chromium
npm run test                          # -- --headed / --grep <pattern>
```

`--skip-build` fails loudly at server startup (not silently against a
stale build) if `dist/index.html` is missing.

## CI

CI (the `web-ui-e2e` job in `.github/workflows/ci.yml`) does not rebuild
the SPA: it downloads the `trawl-web-ui-dist` artifact from the
`trunk-build` job, then runs `npm ci`, `npx playwright install
--with-deps chromium`, and `npm run test` as discrete steps — so the
browser job compiles no Rust. `cargo xtask e2e` is the local entry point
only (and installs chromium without `--with-deps`; on a dev machine the
shared libraries are your own problem). `playwright.config.ts` switches
its reporter to `['github', 'list']` under `CI=true` so failures annotate
the PR diff.

## What it (and doesn't) cover

- Routing (`/`, rail nav, 404), CodeMirror keyboard input → URL/query
  sync, results-table error rendering, and Live Tail SSE teardown
  (`EventSource` closes on unmount and does not reconnect).
- The search URL contract (ADR-0027): the date picker's absolute range
  round-trip, hand-written literal links (versioned filters, `..` ranges,
  legacy and malformed payloads, page-offset overflow), the percent
  encoder against the browser's own, and Back/Forward provenance. Every
  URL in that spec is a literal, never one the app's encoder built.
- The field case drawer's repin status poll dying with the drawer, by all
  four routes out of it (close, Escape, browser Back, and navigating
  straight to another field's case file). A leaked `gloo_timers` Interval
  is network-silent, so that spec reads three separate observables: the
  browser's timer table, the stub's status-read count, and a status read
  the stub parks open across the teardown and answers afterwards.
- The native controls and the shared menu contract (ADR-0028): the
  topbar account menu's keyboard lifecycle (open, arrow walk with wrap,
  Home/End, Escape, Tab, activation, outside mousedown, and Escape being
  topmost-only with a modal above it), the row `ActionsMenu`'s single tab
  stop and its restore-before-callback ordering, both tab strips as named
  tablists with manual activation, and the modal close / toast dismiss /
  bare copy button as keyboard-operable named buttons. These are the
  first specs that assert on `document.activeElement` (through
  `toBeFocused`) and on a tabindex vector across a widget's items — one
  item at `0` is the claim, so asserting the focused item alone would
  pass with every item tabbable.
- No visual regression / screenshot diffing.
- No real backend — every response is a fixture in `harness/fixtures.mjs`.
  Re-verify those shapes against `crates/trawl-api/src/lib.rs` /
  `crates/trawl-web-ui/src/api/mod.rs` when the wire types change; the
  suite decodes the SAME structs the SPA does, so a drifted fixture
  either 500s inside `serde_json` or silently renders the empty state.
  The bodies under `harness/wire/` are guarded for you:
  `crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs` decodes each
  into its trawl-api struct on every `cargo nextest` run, so prefer a
  `wire/` file over a new inline payload.

## The per-test contract

Every spec imports `test` from `fixtures.ts`, and that object carries an
**auto fixture** that resets the stub to the `default` scenario, installs
the homelab-independence network guard, and afterwards fails the test on
any `pageerror` or unstubbed `/api/*` call. It is an auto fixture rather
than a `test.beforeEach` on purpose: this module is loaded once per
worker, so a hook written here is registered against whichever spec file
imported it first and silently never runs for the others. That is what
was happening — only `api-failure.spec.ts` was getting the reset and the
guards, which is why a spec asserting an exact captured-query count
failed when it ran after another file and passed when run alone.

## Scenarios

`default` is what the auto fixture resets to. The others are named in
`harness/server.mjs` and selected with `resetScenario(request, name)` at
the top of a test body: `unauth` (a 401 from `/api/auth/me`),
`query-500`, `stream-burst`, `populated`, and `corpus`.

`corpus` is `populated` plus data: it answers everything `populated`
does, with the same service (`nginx`) and the same net (id `1`), and adds
the fixtures the row, sort, facet and detail specs need. `populated`
itself is byte-identical to what it was before, so specs written against
the empty state keep reading it.

What `corpus` serves:

- `POST /api/v1/query` dispatches by DSL SHAPE, because the service
  drawer's reads carry a field name the stub cannot predict. Three
  pipeline shapes are sniffed as substrings, pinned in
  `harness/fixtures.mjs`'s `QUERY_SHAPES` against `src/drawer_query.rs`:
  `| stats dc(` answers `wire/query-cardinality.json`, `| top 10 `
  answers `wire/query-top-values.json`, and
  `| timechart span=1h count()` answers `wire/query-timechart.json`. A
  query with no pipe at all is a plain search and gets
  `wire/query-rows.json`: 8 events over `_time, host, status, message`,
  with 6 distinct hosts so the facet rail hides one behind `+ 1 more`,
  and no two sort orders agreeing.
- Any OTHER pipeline shape is a 500 carrying its own DSL, recorded in the
  stub's `unhandledQueries`. Falling through to the rows fixture would
  hand a spec a body that says nothing about the query it asked, which is
  the exact failure this dispatch exists to prevent. The drawer's
  `stats count() as hits` collision form is the one that lands here.
- `GET /api/v1/schema/services` gets its own body,
  `wire/service-schema-corpus.json`, rather than `populated`'s. The one
  thing it adds is a degraded column: the service names `duration` in
  `degraded_fields`, which is what renders the field row's degraded
  badge, and `wire/catalog-field.json` is about that same field, so the
  case file the badge opens is about the field the badge sits on.
- `GET /api/v1/history` answers `wire/history.json`: two entries, newest
  first. The first is an ordinary rerunnable query. The second is
  `host=` followed by 32764 `a`s, 32769 bytes in all, one byte over
  `MAX_SEARCH_BYTES`, so the navigator refuses to rerun it instead of
  writing a URL nothing can read back (ADR-0027).
- The report-run routes, which have no fixture outside `corpus` at all:
  `/api/v1/saved/{id}/runs`, `/api/v1/saved/{id}/runs/{run_id}`,
  `/api/v1/runs` and `/api/v1/runs/stats`.

Every one of those bodies decodes into its `trawl-api` struct in
`crates/trawl-web-ui/tests/e2e_wire_fixture_contract.rs`, and the content
the specs navigate by (row count, host values, the over-bound length, the
degraded field) is pinned in `fixtures.ts`'s `CORPUS`.

`populated` answers `/api/v1/saved` and `/api/v1/schema/services` with a
corpus that has one net and one service in it, which is what makes a row
`ActionsMenu` and the service drawer's tab strip reachable. It is an
addition rather than a change to `default`: specs written before it read
the empty state deliberately, and repopulating the default would have
rewritten their meaning silently. Both bodies live under `harness/wire/`
and are pinned by CONTENT as well as shape — the service is named
`nginx` and the net's id is `1`, mirrored in `fixtures.ts`'s `POPULATED`,
because the specs navigate to `?svc=nginx` and `?net=1` by hand.

## Flake policy

`retries: 0` in `playwright.config.ts`, deliberately. A flaky spec is a
bug in the spec or the harness (an unawaited async state change, a race
in `server.mjs`'s SSE bookkeeping, etc.) — fix it or quarantine it loudly
(skip with a comment linking the issue), never paper over it with
retries.

## Mutation-check: proving the suite actually catches regressions

`mutations/*.patch` are git-diff-format patches, each breaking exactly
one thing the suite is supposed to catch:

| patch | breaks | caught by |
|---|---|---|
| `01-route.patch` | misspells the `/search/history` route path | `routing.spec.ts` |
| `02-editor-onchange.patch` | `DslEditor`'s onChange stops writing into `query` | `editor-input.spec.ts` |
| `03-sse-teardown.patch` | leaks the live-tail `EventSource` (`mem::forget` instead of drop) at BOTH close paths: `on_cleanup` and the mode/query effect's own handle-clear | `teardown-sse.spec.ts` |
| `04-error-fallback.patch` | `<ResultsTable>` overrides `Loaded`'s error arm to render nothing | `api-failure.spec.ts` |
| `05-search-url-codec.patch` | `encode_range` writes the retired `abs:<from>:<to>` form again | `search-url.spec.ts` |
| `06-repin-poll-leak.patch` | `on_cleanup` leaks the field case drawer's repin poll `Interval` | `repin-poll-teardown.spec.ts` |
| `07-repin-alive-latch.patch` | the drawer's `is_alive` latch always answers true, so a status read landing after teardown acts on a dead surface | `repin-poll-teardown.spec.ts` |
| `08-menu-walk.patch` | `roving::next_index` answers `current` for every navigation, so arrows, Home and End all stand still | `topbar-menu.spec.ts` |
| `09-menu-roving-tabindex.patch` | every menu item renders `tabindex="0"`, so the menu has as many tab stops as it has items | `actions-menu.spec.ts` |
| `10-menu-topmost-escape.patch` | the menu's Escape listener drops its `is_topmost` guard and answers Escape from under a modal | `topbar-menu.spec.ts` |
| `11-menu-restore-before-callback.patch` | activating an item closes the menu and runs the callback without restoring the trigger | `actions-menu.spec.ts` |
| `12-toast-dismiss-span.patch` | the toast dismiss goes back to a `<span class="x">` with the same click and no name | `native-controls.spec.ts` |
| `13-sort-th-div.patch` | `sort_th` renders the header cell as a bare `<div on:click>` again, so no header on the div tables is focusable or named | `sort-headers.spec.ts` |
| `14-results-row-handler.patch` | the pre-ADR-0029 whole-row `on:click` returns to the results `<tr>`, beside the caret button whose click bubbles into it: one press expands and collapses | `row-controls.spec.ts` |
| `15-schema-anchor-push.patch` | the schema row anchor loses `prop:replace`, so opening the drawer pushes a second history entry | `row-controls.spec.ts` |
| `16-nets-anchor-prevent-default.patch` | the nets row anchor cancels its own default action and navigates by hand, so the router swallows a Ctrl-click the browser owns | `row-controls.spec.ts` |
| `17-range-dialog-no-layer.patch` | the range dialog drops its `use_overlay_layer_with` registration: no opener capture, no initial focus, no Tab trap, no restore | `range-dialog.spec.ts` |
| `18-facet-actions-display-none.patch` | `.facets .v .act` goes back to `display: none` until hover, which takes include and exclude out of the tab order | `facets.spec.ts` |
| `19-results-th-no-aria-sort.patch` | `aria-sort` comes off the results `<th>`, so the sorted column and its direction are announced nowhere | `sort-headers.spec.ts` |

Run the mechanism:

```sh
crates/trawl-web-ui/e2e/scripts/mutation-check.sh                     # all nineteen
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 02-editor-onchange.patch  # just one
```

For each patch: `git apply` it, `trunk build`, run the ONE spec file
named above, require it to EXECUTE and FAIL, then run a control spec the
mutation does not touch and require that to pass, then `git apply -R` to
revert. The control is what tells a kill from a broken environment: a
missing browser fails the target spec too, and playwright records a
launch failure as executed-and-failed tests rather than as no tests. It
refuses to run against a dirty working tree, since a patch that can't be
cleanly reverted would strand a mutation in your tree. This is evidence
tooling for reviewing the suite's own effectiveness, not a CI job.

08 through 11 are focus-order sensitive: the thing they break is
where `document.activeElement` ends up after a keypress, and a browser
can lose a focus race that a network assertion would never notice. So
each of the four was run five consecutive times, as five separate
invocations, and killed all five (transcripts under
`visual-evidence/issue-159/`). 12 is a DOM-shape mutation with no timing
in it and was run once.

13 through 19 follow the same rule, with transcripts under
`visual-evidence/issue-161/`. 14 and 17 are the timing-sensitive pair and
were each run five consecutive times: 14 turns on whether a click on the
caret button bubbles into the row handler before the detail row settles,
and 17 is entirely about where focus lands after Escape. Both killed all
five. The other five are DOM-shape or CSS mutations with no race in them
and were run once.

Three of these are worth reading before you trust the table.

`14` keeps the caret button and adds the row handler back beside it,
rather than replacing one with the other. Deleting the button would break
the spec at the first assertion, which proves only that the selector
still resolves. Both handlers live means the DOM the spec walks is
unchanged and the only thing that moved is the number of times one press
acts, which is what the spec counts.

`16` does two things where the acceptance criterion names one. A bare
`e.prevent_default()` on the anchor is not enough: with nothing else in
the handler, a Ctrl-click does nothing at all, and the spec's assertion
that THIS page stayed put still holds. Restoring the navigation beside it
is the real regression, the router interception the anchor replaced, and
that is what makes the Ctrl-click open the drawer on the page the spec is
watching.

`17` cannot simply delete the registration: the scrim and Escape handlers
both ask the layer whether they are topmost. It swaps in a stand-in that
always answers yes, so the panel still renders and Escape still closes
it. What is gone is the hook, and with it the opener capture, the initial
focus move, the Tab trap and the restore. `scrim mousedown closes` and
`From and To are labelled` survive it on purpose: they are about the
panel, not about the stack.

Two of these mutations are worth reading before you trust the table.

`03` mutates both places the live tail drops its stream handle, not just
`on_cleanup`. Navigating away flips the URL-derived mode and query
signals, so the mode/query effect can clear the handle before disposal
ever reaches `on_cleanup`. Mutate `on_cleanup` alone and the slot may
already be empty by the time it runs, which leaves the mutant alive for
a reason that has nothing to do with the suite. Mutating both sites
removes that ordering dependence.

`06` and `07` both target the field case drawer, and they are not two
spellings of one thing. `06` leaks the poll timer, which is network-
silent: its body runs a disposed leptos callback that no-ops, so the leak
issues no HTTP read and throws no `pageerror`, and only the spec's
`setInterval` wrapper can see it. `07` leaves the timer alone and
disables the `alive` latch instead, so the status read the stub parked
open across the teardown comes back to a dead surface and announces a
finished repin nobody is looking at. Different mechanism, different
observable, same spec file.

### Health page acceptance and gate mutation

`health-page.spec.ts` uses isolated `health-*` identities. JSON fixtures live
under `harness/wire/health-*.json` and decode in the native wire contract.
The dashboard has separate lifetime counters for opens, current connections,
maximum simultaneous connections, and closes. These counters survive scenario
resets so a late socket close cannot conceal a leak. Explicit controls hold,
release, and drop stream data, and release a delayed bootstrap response.

| Patch | Required failure | Unrelated control |
| --- | --- | --- |
| `20-health-admin-gate.patch` | `non-admin request silence:` must fail its stats or dashboard request counter assertion | `routing.spec.ts`, the unknown-route 404 test outside `AuthShell` |

Run `e2e/scripts/mutation-check.sh 20-health-admin-gate.patch` from a clean
checkout. The script reads the named test's JSON result and requires the
request-counter assertion itself to fail. A rendering failure elsewhere in
the spec is not a kill. The `web-ui-health-mutation` CI job depends on the
same commit's passing `web-ui-e2e` job and builds both the mutant and restored
SPA. Browser traces remain available on job failure.

Health cases cover health 200 and structured 503, permission-gated network
silence and DOM, independent query permission, one shared stream across
navigation, bootstrap waiting and recovery, dropped-stream staleness, late
bootstrap precedence, active and recent ownership, confirmation and exact
DELETE count, false and unknown cancellation outcomes, and shell teardown.
The existing shell does not expose an in-place identity refresh to the UI;
identity generation races belong to the native dashboard state tests rather
than a production-only browser testing hook.
