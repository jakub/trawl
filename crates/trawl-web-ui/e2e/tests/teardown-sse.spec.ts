// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, intervalCount, resetScenario, trackIntervals } from '../fixtures';
import { SEL } from '../selectors';

/** crates/trawl-web-ui/src/state/stream_session.rs — the live table's
 * render tick, the timer this spec proves is released on unmount. */
const RENDER_TICK_MS = 16;

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

test('a burst keeps the newest 5000 rows, updates columns, and releases its render timer', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await trackIntervals(page);
  await page.goto('/search');
  await page.locator(SEL.cmContent).click();
  await page.keyboard.type('service=nginx');
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();

  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
  await expect(page.locator(SEL.liveResultsTable).locator('tbody tr')).toHaveCount(5000);
  const rendered = await page.locator(SEL.liveResultsTable).evaluate(table => {
    const headers = Array.from(table.querySelectorAll('th'), th => th.textContent);
    const index = headers.indexOf('seq');
    return { headers, seqs: Array.from(table.querySelectorAll('tbody tr'), row => Number(row.children[index].textContent)) };
  });
  expect(rendered.seqs).toEqual(Array.from({ length: 5000 }, (_, i) => i + 1000));
  expect(rendered.headers).toContain('late_column');
  expect(rendered.headers).not.toContain('expired_only');
  expect(await intervalCount(page, RENDER_TICK_MS)).toBe(1);

  await page.locator(SEL.railHistoryLink).click();
  await expect(page.locator('h1')).toHaveText('Search history');
  expect(await intervalCount(page, RENDER_TICK_MS)).toBe(0);
  expect(await waitFor(async () => (await pollState(request)).sse.open === 0, 2_000)).toBe(true);
});
