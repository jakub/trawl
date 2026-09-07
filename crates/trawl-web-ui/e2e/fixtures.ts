// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Shared Playwright fixture: every spec imports `test`/`expect` from
// here instead of `@playwright/test` directly, so the reset / homelab
// -independence / pageerror / unstubbed-call contract applies uniformly
// without being re-typed per file.

import { test as base, expect } from '@playwright/test';

// Same variable playwright.config.ts and harness/server.mjs read, so the
// network guard below can never disagree with where the stub actually is.
const E2E_ORIGIN = `http://127.0.0.1:${Number(process.env.E2E_PORT ?? 8123)}`;

type PageErrors = { errors: Error[] };

// The reset / network-guard / pageerror / unstubbed contract rides an
// AUTO FIXTURE, not `test.beforeEach`. This module is loaded once per
// worker (ESM module cache), so a `test.beforeEach` written here is
// registered against whichever spec file imported it FIRST and silently
// does not run for any other file — which is exactly what happened:
// every spec but the first was running with no scenario reset, no
// homelab-independence guard and no pageerror check. An auto fixture is
// attached to the `test` object itself, so it runs for every test that
// imports it, whatever the file.
export const test = base.extend<{ pageErrors: PageErrors; contract: void }>({
  pageErrors: async ({ page }, use) => {
    const bucket: PageErrors = { errors: [] };
    page.on('pageerror', (err) => bucket.errors.push(err));
    await use(bucket);
  },

  contract: [
    async ({ page, request, pageErrors }, use) => {
      // Default scenario; a spec that needs a different one calls
      // `resetScenario(request, name)` itself at the top of the test
      // body — simpler than threading it through a hook, since
      // Playwright runs fixtures before the test body even sees its own
      // annotations.
      await request.post('/__ctl/reset', { data: { scenario: 'default' } });

      // Homelab-independence contract: abort any request whose origin
      // isn't our own stub server. `data:`/`blob:` are allowed (inline
      // fonts, blob-backed workers).
      await page.context().route('**/*', (route) => {
        const url = new URL(route.request().url());
        const ok =
          url.origin === E2E_ORIGIN ||
          url.protocol === 'data:' ||
          url.protocol === 'blob:';
        if (ok) {
          route.continue();
        } else {
          route.abort();
        }
      });

      await use();

      // console.error is deliberately NOT checked — headless-GL / wasm
      // warmup noise is expected and not a test failure. A JS exception
      // (pageerror) is a real bug and does fail the test.
      if (pageErrors.errors.length > 0) {
        throw pageErrors.errors[0];
      }
      const state = await (await request.get('/__ctl/state')).json();
      expect(state.unstubbed, `unstubbed /api/* calls: ${JSON.stringify(state.unstubbed)}`).toEqual(
        [],
      );
    },
    { auto: true },
  ],
});

export { expect };

/** Re-point the stub server at a non-default scenario for this test. Call
 * at the top of the test body — `beforeEach` above already reset to
 * 'default' by the time the body runs. */
export async function resetScenario(request: import('@playwright/test').APIRequestContext, name: string) {
  await request.post('/__ctl/reset', { data: { scenario: name } });
}

type Ctl = import('@playwright/test').APIRequestContext;

/** How many `POST /api/v1/query` bodies the stub has captured since the
 * last reset. A spec that asserts a URL does NOT run reads this before
 * and after a bounded quiet wait. */
export async function capturedQueryCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.length;
}

/** The last captured query body, once exactly `expectedCount` have
 * arrived. Polls rather than sleeping, and throws with the bodies it did
 * see — a spec must never read a stale `at(-1)` as this navigation's
 * query. */
export async function lastCapturedQuery(request: Ctl, expectedCount: number, timeoutMs = 5_000) {
  const deadline = Date.now() + timeoutMs;
  let queries: any[] = [];
  for (;;) {
    const state = await (await request.get('/__ctl/state')).json();
    queries = state.queries;
    if (queries.length === expectedCount) return queries.at(-1);
    if (Date.now() > deadline) {
      throw new Error(
        `expected ${expectedCount} captured queries, saw ${queries.length}: ${JSON.stringify(queries)}`,
      );
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

/** How many `POST /api/v1/export` bodies the stub has captured since the
 * last reset. An export is the request that mattered most in ADR-0027's
 * review: the server reads an empty query as every row, so a link the
 * page refused must not be exportable. */
export async function capturedExportCount(request: Ctl): Promise<number> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.exports.length;
}

/** Every captured query's DSL, oldest first. */
export async function capturedQueries(request: Ctl): Promise<string[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.map((q: { query: string }) => q.query);
}
