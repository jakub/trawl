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

Run the mechanism:

```sh
crates/trawl-web-ui/e2e/scripts/mutation-check.sh                     # all seven
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
