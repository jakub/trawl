// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Shared Playwright fixture: every spec imports `test`/`expect` from
// here instead of `@playwright/test` directly, so the reset / homelab
// -independence / pageerror / unstubbed-call contract applies uniformly
// without being re-typed per file.

import { test as base, expect } from '@playwright/test';

type PageErrors = { errors: Error[] };

export const test = base.extend<{ pageErrors: PageErrors }>({
  pageErrors: async ({ page }, use) => {
    const bucket: PageErrors = { errors: [] };
    page.on('pageerror', (err) => bucket.errors.push(err));
    await use(bucket);
  },
});

test.beforeEach(async ({ page, request }) => {
  // Default scenario; a spec that needs a different one calls
  // `resetScenario(request, name)` itself at the top of the test body —
  // simpler than threading it through beforeEach, since Playwright runs
  // beforeEach before the test body even sees its own annotations.
  await request.post('/__ctl/reset', { data: { scenario: 'default' } });

  // Homelab-independence contract: abort any request whose origin isn't
  // our own stub server. `data:`/`blob:` are allowed (inline fonts,
  // blob-backed workers).
  await page.context().route('**/*', (route) => {
    const url = new URL(route.request().url());
    const ok =
      url.origin === 'http://127.0.0.1:8123' ||
      url.protocol === 'data:' ||
      url.protocol === 'blob:';
    if (ok) {
      route.continue();
    } else {
      route.abort();
    }
  });
});

test.afterEach(async ({ request, pageErrors }) => {
  // console.error is deliberately NOT checked — headless-GL / wasm
  // warmup noise is expected and not a test failure. A JS exception
  // (pageerror) is a real bug and does fail the test.
  if (pageErrors.errors.length > 0) {
    throw pageErrors.errors[0];
  }
  const state = await (await request.get('/__ctl/state')).json();
  expect(state.unstubbed, `unstubbed /api/* calls: ${JSON.stringify(state.unstubbed)}`).toEqual([]);
});

export { expect };

/** Re-point the stub server at a non-default scenario for this test. Call
 * at the top of the test body — `beforeEach` above already reset to
 * 'default' by the time the body runs. */
export async function resetScenario(request: import('@playwright/test').APIRequestContext, name: string) {
  await request.post('/__ctl/reset', { data: { scenario: name } });
}
