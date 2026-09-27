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

### Workers and ports

There is no Playwright `webServer`. Each worker starts its own
`harness/server.mjs` from the worker-scoped `stubOrigin` fixture in
`fixtures.ts`, on port `E2E_PORT + parallelIndex` (`E2E_PORT` defaults
to 8123), and every test's `baseURL` points at its own worker's server.
A stub server keeps one mutable scenario, so it must never serve two
tests at once. A server per worker gives that at any worker count.

Local runs default to four workers, so `npm run test` runs four servers
on `E2E_PORT` through `E2E_PORT + 3` and the full suite takes about three
and a half minutes instead of nine. Under `CI=true` the default is one
worker. `--workers=N` overrides either. Suites running side by side
need port ranges that do not overlap. A port already in use fails the
worker with the server's own output. The suite never reuses a server
that is already listening, because that server may serve another
worktree's dist.

The harness serves a private copy of that `dist/`, taken at startup into
`e2e-artifacts/dist-snapshot-<port>/` (one per worker's port) and checked against index.html's
own asset list. A `trunk serve` running from the same checkout writes the
same directory, so without the copy a rebuild mid-run pulls the hashed
wasm out from under the browser, and trunk's injected autoreload client
(whose `{{__TRUNK_ADDRESS__}}` placeholder only trunk's server
substitutes) logs a WebSocket failure that console-error assertions read
as the SPA's. The snapshot drops that client; a `trunk build` index.html
carries none and is copied verbatim.

Everything a run writes — that snapshot, playwright traces, failure
screenshots, `.last-run.json` — goes to `e2e-artifacts/` at the
repository root, never under `crates/`. Trunk's watcher covers the whole
`crates/trawl-web-ui` tree and does not read `.gitignore`, so a run that
wrote beside the suite woke any live `trunk serve` for a full rebuild;
about a hundred seconds later that rebuild applied its distribution,
clearing `dist/.stage` under whatever `trunk build` was staging into it,
and the build died with `error writing JS loader file to stage dir: No
such file or directory`. CI uploads the traces from the new path.

## CI

CI (the `web-ui-e2e` job in `.github/workflows/ci.yml`) does not rebuild
the SPA: it downloads the `trawl-web-ui-dist` artifact from the
`trunk-build` job, then runs `npm ci`, `npx playwright install
--with-deps chromium`, and `npm run test -- --shard=N/4
--global-timeout=1500000` as discrete steps, so the browser job compiles
no Rust. The job is a four-shard matrix. Each shard is a separate job that runs
one worker (the `CI=true` default), so each shard runs one stub server.
Shard 1 also runs the BFCache suite. Each shard uploads its own
`e2e-traces-N` on failure. The shard that runs `theme-preference.spec.ts`
uploads the System screenshots as `theme-menu-captures-N`, and the
`theme-menu-captures` job fails when no shard uploaded them. `cargo xtask e2e` is the local entry point
only (and installs chromium without `--with-deps`; on a dev machine the
shared libraries are your own problem). `playwright.config.ts` switches
its reporter to `['github', 'list']` under `CI=true` so failures annotate
the PR diff.

The Chromium project selects `channel: 'chromium'`, which runs full
Chromium in headless mode. The default headless shell in Playwright
1.62.1 logged renderer SIGSEGV crashes during native Ctrl-click tests,
including a reproduction with only a plain HTML link. Full Chromium at
the same version passed 30 Ctrl-click repetitions and the 180-test suite
without those crashes. Both binaries come from the existing
`playwright install chromium` step and lockfile-keyed cache.

The main browser CI job enables `DEBUG=pw:browser` to retain process
exits and browser stderr in its log. Use the same environment variable
locally when investigating a popup timeout. A passing assertion does
not rule out a renderer crash: the headless-shell repetitions passed
despite their crash messages. The missing popup event in main CI run
34440325872 has not been reproduced locally, so its cause remains
unproven.

The full CI suite gets 25 minutes on the shared `k8s-small` runner,
inside a 40-minute job budget that also covers setup, the separate BFCache
suite and artifact upload. The BFCache CI step has a five-minute aggregate
budget; its individual test timeouts and zero-retry policy are unchanged.
The local and focused mutation-run default remains 12 minutes. To reproduce CI's aggregate
budget locally, run `npm run test -- --global-timeout=1500000` from this
directory. This override leaves per-test timeouts, assertions and
`retries: 0` unchanged.

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
  topmost-only with a modal above it), its single tab stop and focus
  restoration on activation, direct Nets actions and delete-dialog focus,
  both tab strips as named
  tablists with manual activation, and the modal close / toast dismiss /
  bare copy button as keyboard-operable named buttons. These are the
  first specs that assert on `document.activeElement` (through
  `toBeFocused`) and on a tabindex vector across a widget's items — one
  item at `0` is the claim, so asserting the focused item alone would
  pass with every item tabbable.
- The redesigned chrome (ADR-0032): `sidebar.spec.ts` for the collapse
  preference through a reload, one `aria-current="page"` per route and
  the command bar's crumb; `query-console.spec.ts` for the draft state,
  the executed-scope strip and the skip links; `reading-modes.spec.ts`
  for the inspector and message-first presentations, both off by
  default; `aggregate.spec.ts` for the exact table and the categorical
  chart; `range-wrap.spec.ts` for the range trigger and Haul not
  overlapping at 320 and 390px.
- The service and net panels DOCK beside their lists at 1100px and up,
  where there is neither a scrim nor a focus capture. A spec about
  either of those two things narrows the viewport first
  (`batch1-controls.spec.ts`, `chrome-controls.spec.ts`) — an
  assertion on `.sd-scrim` at the default width is asserting a panel
  that is not there.
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
any `pageerror` or unstubbed `/api/*` call. The guard allows exactly the
worker's own `stubOrigin`, the same origin `baseURL` carries. It is an
auto fixture rather than a `test.beforeEach` on purpose: this module is loaded once per
worker, so a hook written here is registered against whichever spec file
imported it first and silently never runs for the others. That is what
was happening — only `api-failure.spec.ts` was getting the reset and the
guards, which is why a spec asserting an exact captured-query count
failed when it ran after another file and passed when run alone.

## Scenarios

`default` is what the auto fixture resets to. The others are named in
`harness/server.mjs` and selected with `resetScenario(request, name)` at
the top of a test body: `unauth` (a 401 from `/api/auth/me`),
`query-500`, `stream-burst`, `populated`, `corpus`, and `aggregate`.

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

`aggregate` answers one aggregation at the size the test asks for:
`{ scenario: 'aggregate', aggregate: { buckets, groups, total } }` on the
reset. A `| timechart` pipeline gets `buckets` rows of `_time, count`,
two minutes apart from a fixed epoch; a `| stats count() by` pipeline
gets `groups` rows of `status, count`. Both honour the posted `limit`
and `offset` when slicing, and `pagination.total` is measured BEFORE the
slice (`total` overrides it, for a result larger than what came back).
Any other pipeline is a 500 recorded in `unhandledQueries`, the `corpus`
rule. The rows are generated rather than pinned under `wire/` because
what these specs read is how MANY rows arrived and which window was
asked for — `/__ctl/state`'s `queries` carries each request's `limit` and
`offset` alongside its DSL.

`populated` answers `/api/v1/saved` and `/api/v1/schema/services` with a
corpus that has one Net and one service in it, which makes the direct
Net actions and the service drawer's tab strip reachable. It is an
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

Motion is live by default: neither Playwright config sets
`reducedMotion`, so entrance animations and the login backdrop run in
every spec. A spec that needs reduced motion calls
`page.emulateMedia({ reducedMotion: 'reduce' })` itself, as
`batch1-controls.spec.ts` does. A spec that measures a box right after
the control that reveals it waits for that element's own animations to
finish, as `responsive-layout.spec.ts` does for the nav overlay.

`globalTimeout` defaults to 720s. The full CI job overrides it with 1500s;
focused mutation runs retain the default. A run that exceeds its budget
stops and reports the remainder as "did not run". Those cases did not pass,
and the aggregate timeout fails the run.

## Visual evidence

`scripts/visual-evidence.mjs` is evidence tooling, not a test, and
nothing in CI runs it. It drives the built SPA against this harness and
writes 18 scenes x light/dark x 1440/390 to
`visual-evidence/<stamp>/`, with a `manifest.json` naming the commit and
the SHA-256 of the `dist/` it photographed, and a `contact-sheet.html`
pairing each capture with its mockup.

```sh
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
env -u NO_COLOR node crates/trawl-web-ui/e2e/scripts/visual-evidence.mjs
```

Theme and the reading modes are seeded into `localStorage['trawl.ui']`
with `addInitScript`, because that is where fleet-ui reads them from;
fixed Light and Dark preferences override `colorScheme`. With System (the
default), `colorScheme` selects the resolved appearance. The shutter waits for every finite
animation to finish first — the Haul button transitions out of its
in-flight fill over 120ms, and a frame taken inside that window shows
near-white on near-white.

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
| `09-menu-roving-tabindex.patch` | every theme radio renders `tabindex="0"`, adding multiple menu tab stops | `topbar-menu.spec.ts` |
| `10-menu-topmost-escape.patch` | the menu's Escape listener drops its `is_topmost` guard and answers Escape from under a modal | `topbar-menu.spec.ts` |
| `11-menu-restore-before-callback.patch` | activating a theme radio closes the menu and runs the callback without restoring the trigger | `topbar-menu.spec.ts` |
| `31-menu-command-restore.patch` | activating the Sign Out command closes the menu without restoring the trigger while logout is pending or after failure | `topbar-menu.spec.ts` |
| `12-toast-dismiss-span.patch` | the toast dismiss goes back to a `<span class="x">` with the same click and no name | `native-controls.spec.ts` |
| `13-sort-th-div.patch` | `sort_th` renders the header cell as a bare `<div on:click>` again, so no header on the div tables is focusable or named | `sort-headers.spec.ts` |
| `14-results-row-handler.patch` | the pre-ADR-0029 whole-row `on:click` returns to the results `<tr>`, beside the caret button whose click bubbles into it: one press expands and collapses | `row-controls.spec.ts` |
| `15-schema-anchor-push.patch` | the schema row anchor loses `prop:replace`, so opening the drawer pushes a second history entry | `row-controls.spec.ts` |
| `16-nets-anchor-prevent-default.patch` | the nets row anchor cancels its own default action and navigates by hand, so the router swallows a Ctrl-click the browser owns | `row-controls.spec.ts` |
| `17-range-dialog-no-layer.patch` | the range dialog drops its `use_overlay_layer_with` registration: no opener capture, no initial focus, no Tab trap, no restore | `range-dialog.spec.ts` |
| `18-facet-actions-display-none.patch` | `.facets .v .act` goes back to `display: none` until hover, which takes include and exclude out of the tab order | `facets.spec.ts` |
| `19-results-th-no-aria-sort.patch` | `aria-sort` comes off the results `<th>`, so the sorted column and its direction are announced nowhere | `sort-headers.spec.ts` |
| `20-health-admin-gate.patch` | Health mounts admin stats/dashboard requests without permission | `health-page.spec.ts`; focused request counters and a 404 routing control |
| `21-atmosphere-speed-only.patch` | Delete only the terminal dead assignment, keeping speed zero and hidden canvas; reactive motion changes restart shader rAF | `atmosphere-fallback.spec.ts`, dedicated `scripts/atmosphere-mutation-check.sh` |
| `22-runs-filtered-window.patch` | Global Runs computes its page window from filtered rows instead of the server page | `pagination.spec.ts`, zero-match filter keeps `1–3 of 3` |
| `23-range-close-on-refusal.patch` | The shared range commit path closes after the app refuses a selection | `range-dialog.spec.ts`, retained absolute and quick drafts with inline errors |
| `24-palette-overlay-gate.patch` | removes both closed-state overlay admission checks, in Shell's open callback and global chord listener | `command-palette.spec.ts`, export-modal chord inertness |
| `25-palette-toggle.patch` | replaces the open palette's chord close callback with a no-op | `command-palette.spec.ts`, chord while open toggles closed |
| `26-save-editor-snapshot.patch` | captures the executed query with URL filters and range instead of the editor buffer | `settings-disposition.spec.ts`, exact preview and POST assertions for the console's Save as Net |
| `32-rail-choice-from-toggle.patch` | the filter rail's `toggle` handler records the wide hand choice, so the rail's first automatic open becomes a choice that holds it open | `filter-rail.spec.ts`, an automatic open is not a hand choice |

Run the mechanism:

```sh
crates/trawl-web-ui/e2e/scripts/mutation-check.sh                     # all standard mutations (21 has a dedicated runner)
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
tooling for reviewing the suite's effectiveness. Dedicated CI jobs run the
Health, pagination, range-dialog, command-palette, and Save mutations after the same commit's baseline E2E job passes.

08 through 11 and 31 are focus-order sensitive: the thing they break is
where `document.activeElement` ends up after a keypress, and a browser
can lose a focus race that a network assertion would never notice. So
the original issue-159 versions of 08 through 11 were run five consecutive times, as five separate
invocations, and killed all five (transcripts under
`visual-evidence/issue-159/`). 12 is a DOM-shape mutation with no timing
in it and was run once.

Issue #196 retargets 09 and 11 to theme radios and adds the independent command
mutation 31. The issue-159 transcripts do not validate these new targets.

On production-source candidate `ce9dc49e6ecd78d45e3ebd4805f0930da49ade8d`, the
root validator observed target failure and control success independently for
08, 09, 10, 11, and 31. After restoring source and rebuilding the pristine SPA,
the full ordinary Chromium suite passed **437 tests in 6.3 minutes**. These are
local executed results, not CI results. The
[durable evidence summary](../../../docs/evidence/issue-196/menu-mutation-validation.md)
retains the observed mutation outcomes and full-baseline result. This validates
the changed mutation targets without relying on the earlier focused theme/menu
passes. Documentation added afterward does not alter production behavior.

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

### Command palette mutations

The overlay mutation removes both admission checks. Removing only the listener's
`has_layers()` check leaves the open callback's guard intact and never exercises
the regression. The toggle mutation keeps the recognized chord and its default
prevention, but does not close the palette. Both use `routing.spec.ts` as the
unaffected control.

After a passing full browser baseline on the same commit, run from a clean tree:

```sh
env -u NO_COLOR E2E_PORT=8168 crates/trawl-web-ui/e2e/scripts/mutation-check.sh \
  24-palette-overlay-gate.patch 25-palette-toggle.patch
```

`NO_COLOR` is unset because the installed Trunk parses it as a boolean and rejects
an inherited value of `1`. The runner builds each mutant, requires its target spec
to fail and the routing control to pass, reverses the patch, and finally rebuilds
the pristine SPA. The `web-ui-palette-mutations` CI job runs these two patches alongside
`web-ui-e2e` and uploads browser traces on failure. A failed build or
control is not a killed mutation.

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
the spec is not a kill. The `web-ui-health-mutation` CI job runs alongside
`web-ui-e2e` on the same commit, and the workflow is green only when both pass.
It builds both the mutant and restored SPA. Browser traces remain available on job failure.

Health cases cover health 200 and structured 503, permission-gated network
silence and DOM, independent query permission, one shared stream across
navigation, bootstrap waiting and recovery, dropped-stream staleness, late
bootstrap precedence, active and recent ownership, confirmation and exact
DELETE count, false and unknown cancellation outcomes, and shell teardown.
The existing shell does not expose an in-place identity refresh to the UI;
identity generation races belong to the native dashboard state tests rather
than a production-only browser testing hook.

The Health regression cases also pin error ordering. A held stream refusal
is released only after the page renders the bootstrap's 403. A browser-side
EventSource observer then proves the terminal error callback ran before the
second Forbidden assertion. A reconnect before any snapshot must keep the
waiting label. Cancellation HTTP 500, 502, and 504 report an unknown outcome;
HTTP 403 reports a definite refusal.

Mutation 21 uses its own runner because it changes only the vendored JavaScript.
After committing a clean baseline and building the SPA, run
`E2E_PORT=8167 bash crates/trawl-web-ui/e2e/scripts/atmosphere-mutation-check.sh`.
`TRAWL_E2E_DIST` can name another baseline dist. The runner requires exactly one
shader snippet identical to the committed bundle, passes the baseline loss test,
removes only `state = "dead"`, rebuilds the vendor bundle, and copies that bundle
into the existing snippet and updates only its modulepreload integrity digest.
It accepts only the loss test's
`ATMOSPHERE_NO_RESTART` assertion as the failure, then requires the unaffected
compile-failure control to pass. Exit and signal traps restore and byte-check
the source, bundle, dist snippet and index. CI's `atmosphere-mutation` job downloads
the same `trunk-build` dist that `web-ui-e2e` tests and needs no Rust rebuild.

The atmosphere spec forces no-WebGL, compile, link and late constructor failures.
Its loss test starts with live motion, the default, and toggles reduced motion
with `page.emulateMedia`. It requires real rendered frames and a real
`WEBGL_lose_context` event, and counts only callbacks scheduled by the shader
snippet. It retains the original host through SPA unmount to inspect cleanup.
Expected shader diagnostics must remain silent, while an unrelated diagnostic
inside construction and another after construction must reach the console.

The login page has no theme control. Its loss test exercises the actual
reduced-motion effect; a separate browser test imports the unique emitted
shader module and calls its real public handle. That test first proves a live
color change reaches GL uniforms, then proves color and speed changes issue no
GL calls or shader callbacks after loss and after repeated disposal. No test
changes Rust signal access or framework callback lifetimes.


### Pagination and range acceptance

The `pagination` scenario supplies offset-aware History, query results and
report-run pages. Its three-run wire bodies and matching statistics are decoded
by the native wire contract; larger pages are generated from those same row
shapes. Query responses stamp the requested offset, the returned row count and
the pre-window total the execution produced. Configurable totals cover full,
short and empty pages. No real database or auth service is involved.

`pagination.spec.ts` checks History's single-decode reader, URL replacement,
filters, offset refusal, Known totals and Probe Next behavior; global Runs and
the unfiltered net drawer; and busy pagers with retained response summaries.
The delayed History case holds a response, changes the page through an anchor
intercepted by the existing SPA router, then releases the old response and
requires the latest queued offset, rows and summary to agree. The pinned
LocalResource executes serially: this proves final agreement after release,
not concurrent requests or a database snapshot. A separate held query proves
that a same-page query change disables the pager while old rows remain visible.

`range-dialog.spec.ts` keeps the keyboard, focus, scrim and label checks and
adds refused absolute/quick drafts, tab lifetime, close/reset behavior, and
Live refusal from the editor buffer. The Fleet workbench is a separate reuse
probe with different presets and no Live tab, not a second maintained suite.

Run the focused mutations from a clean checkout:

```sh
E2E_PORT=8166 crates/trawl-web-ui/e2e/scripts/mutation-check.sh \
  17-range-dialog-no-layer.patch \
  22-runs-filtered-window.patch \
  23-range-close-on-refusal.patch
```

Each uses `routing.spec.ts` as an independent passing control. The
`web-ui-pagination-range-mutations` CI job runs alongside the full `web-ui-e2e`
baseline, runs these three mutations and uploads failure traces. Apply or build
failure is not a kill; the target must execute and fail, the control must pass,
and the runner must restore both clean source and pristine app dist. Mutation
21 remains reserved for the dedicated Atmosphere runner.

### Save editor snapshot mutation

`26-save-editor-snapshot.patch` changes only the Save callback's input from
`query_text.get_untracked()` to `effective_q.get_untracked()`. The target test
opens the console's Save as Net with a buffer that differs from both the
executed and the effective query. It compares the preview's exact
`textContent` and the captured POST body with the editor buffer, including
whitespace.

The runner selects tests whose names start with `Save captures editor buffer:`.
Its JSON report checker requires the editor test to have an actual `failed`
result with the exact snapshot preview or POST assertion.
Timeouts, missing tests, malformed reports, and unrelated failures do not count.
The unaffected `routing.spec.ts` must pass. Synthetic report checks run with
`node --test crates/trawl-web-ui/e2e/scripts/check-save-mutation.test.mjs`.

Run the complete sequence from a clean repository root:

```sh
export E2E_PORT=8164 CARGO_BUILD_JOBS=4
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
(cd crates/trawl-web-ui/e2e && npx playwright test tests/settings-disposition.spec.ts)
env -u NO_COLOR crates/trawl-web-ui/e2e/scripts/mutation-check.sh 26-save-editor-snapshot.patch
(cd crates/trawl-web-ui/e2e && npx playwright test tests/settings-disposition.spec.ts)
```

The mutation runner reverses the patch and rebuilds the pristine SPA before it
returns. The final browser command verifies that restored build. The explicit
`web-ui-save-snapshot-mutation` CI job runs this sequence alongside `web-ui-e2e`,
with separate steps for the pristine and restored assertions. A failed build,
unattributed target failure, failed control, or dirty restored tree fails the job.
