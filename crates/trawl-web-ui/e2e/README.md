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

The same `cargo xtask e2e --release` invocation is the CI entry point —
no separate CI-only script. `playwright.config.ts` switches its reporter
to `['github', 'list']` under `CI=true` so failures annotate the PR diff.

## What it (and doesn't) cover

- Routing (`/`, rail nav, 404), CodeMirror keyboard input → URL/query
  sync, results-table error rendering, and Live Tail SSE teardown
  (`EventSource` closes on unmount and does not reconnect).
- No visual regression / screenshot diffing.
- No real backend — every response is a fixture in `harness/fixtures.mjs`.
  Re-verify those shapes against `crates/trawl-api/src/lib.rs` /
  `crates/trawl-web-ui/src/api/mod.rs` when the wire types change; the
  suite decodes the SAME structs the SPA does, so a drifted fixture
  either 500s inside `serde_json` or silently renders the empty state.

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
| `03-sse-teardown.patch` | `on_cleanup` leaks the live-tail `EventSource` (`mem::forget` instead of drop) | `teardown-sse.spec.ts` |
| `04-error-fallback.patch` | `<ResultsTable>` overrides `Loaded`'s error arm to render nothing | `api-failure.spec.ts` |

Run the mechanism:

```sh
crates/trawl-web-ui/e2e/scripts/mutation-check.sh                     # all four
crates/trawl-web-ui/e2e/scripts/mutation-check.sh 02-editor-onchange.patch  # just one
```

For each patch: `git apply` it, `trunk build`, run the ONE spec named
above with `--grep`, expect a **nonzero** exit (the mutation must break
something observable), then `git apply -R` to revert. It refuses to run
against a dirty working tree — a patch that can't be cleanly reverted
would otherwise strand a mutation in your tree. This is evidence tooling
for reviewing the suite's own effectiveness, not a CI job.
