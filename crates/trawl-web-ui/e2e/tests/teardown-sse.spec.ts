// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import { SEL } from '../selectors';

async function pollState(request: import('@playwright/test').APIRequestContext) {
  return (await (await request.get('/__ctl/state')).json()) as {
    sse: { open: number; opens: number; closes: number };
  };
}

async function waitFor(fn: () => Promise<boolean>, timeoutMs: number) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (await fn()) return true;
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}

test('an unmounted live tail closes its EventSource and stops reconnecting', async ({ page, request }) => {
  await page.goto('/search');
  await page.locator(SEL.cmContent).click();
  await page.keyboard.type('service=nginx');

  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();

  // Live Tail navigates mode=live; the SSE EventSource should open.
  const opened = await waitFor(async () => (await pollState(request)).sse.open === 1, 5_000);
  expect(opened, 'expected /api/v1/stream to open exactly one EventSource after Live Tail').toBe(true);

  // Navigate away in-SPA (unmounts <Search>, which owns the stream).
  await page.locator(SEL.railHistoryLink).click();
  await expect(page.locator('h1')).toHaveText('Search history');

  const closed = await waitFor(async () => (await pollState(request)).sse.open === 0, 2_000);
  expect(closed, 'expected the EventSource to close within 2s of unmount').toBe(true);

  const afterClose = await pollState(request);
  const opensAtClose = afterClose.sse.opens;

  // A leaked EventSource would reconnect every ~200ms (our `retry:`
  // interval); wait well past that and assert `opens` didn't climb.
  await page.waitForTimeout(1_500);
  const later = await pollState(request);
  expect(later.sse.opens, 'opens count grew after unmount — the EventSource leaked and reconnected').toBe(opensAtClose);
});
