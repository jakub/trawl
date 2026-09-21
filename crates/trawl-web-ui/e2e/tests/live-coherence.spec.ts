// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Live-mode coherence on the Search page (issue #180, ADR-0027 as
// amended 2026-09-12).
//
// `mode=live` is a claim the page keeps true: there is a way out of it,
// the footer and the tab count describe the source actually on screen,
// no snapshot query runs behind the stream, and the filter rail reads
// the ring. Each of those is one test below.
//
// Two of them are also the subjects of mutations 27 and 28: Stop live
// pushes (so Back returns to the stream), and the snapshot resource is
// gated on the mode (so live posts no query at all).

import { test, expect, resetScenario, capturedQueryCount } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

/** `host="web-01"`, the same versioned payload live-raw-recovery uses.
 * Stop live has to carry it through untouched. */
const FILTERS = 'v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0';

/** A NON-default range: the URL producer elides `15m`, so a spec that
 * used the default could not tell preservation from omission. */
const RANGE = '1h';

async function sseState(request: Ctl): Promise<{ open: number; opens: number }> {
  return (await (await request.get('/__ctl/state')).json()).sse;
}

/** The burst scenario writes 6,000 events synchronously; the ring keeps
 * the last 5,000, so the final one is the arrival to wait on. */
async function burstArrived(page: Pg): Promise<void> {
  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
}

/** Replace the editor's text and run it, the way a person does. */
async function haul(page: Pg, query: string): Promise<void> {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(query);
  await page.keyboard.press('Control+Enter');
}

test('Stop live runs one snapshot, keeps q/f/r, and Back returns to the stream', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto(`/search?q=service%3Dnginx&f=${FILTERS}&r=${RANGE}&mode=live`);
  await burstArrived(page);
  expect(await capturedQueryCount(request)).toBe(0);

  await page.locator(SEL.stopLive).click();
  await expect(page.locator(SEL.stopLive)).toHaveCount(0);

  const url = new URL(page.url());
  expect(url.searchParams.get('q')).toBe('service=nginx');
  expect(url.searchParams.get('f')).toBe(FILTERS);
  expect(url.searchParams.get('r')).toBe(RANGE);
  expect(url.searchParams.get('mode')).toBeNull();

  await expect.poll(async () => (await sseState(request)).open).toBe(0);
  await expect.poll(async () => capturedQueryCount(request)).toBe(1);

  // Push, not replace: the stream is where Back goes.
  await page.goBack();
  await expect(page).toHaveURL(/mode=live/);
  await burstArrived(page);
  await expect.poll(async () => (await sseState(request)).open).toBe(1);
  expect(await capturedQueryCount(request)).toBe(1);
});

test('any committed range selection leaves live, including the preset already picked', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto(`/search?q=service%3Dnginx&r=${RANGE}&mode=live`);
  await burstArrived(page);

  // Re-picking the SELECTED preset: in live the mode is the change.
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.quickRangeOption).filter({ hasText: `Last ${RANGE}` }).click();
  await expect(page).not.toHaveURL(/mode=live/);
  expect(new URL(page.url()).searchParams.get('r')).toBe(RANGE);
  await expect.poll(async () => (await sseState(request)).open).toBe(0);

  // …and an absolute pick does the same from a fresh stream.
  await page.goto(`/search?q=service%3Dnginx&r=${RANGE}&mode=live`);
  await burstArrived(page);
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  await page.locator(SEL.dateRangeFrom).fill('2026-09-05T06:00:00Z');
  await page.locator(SEL.dateRangeTo).fill('2026-09-05T06:30:00Z');
  await page.locator(SEL.dateRangeApply).click();
  await expect(page).not.toHaveURL(/mode=live/);
  expect(new URL(page.url()).searchParams.get('r')).toBe(
    '2026-09-05T06:00:00Z..2026-09-05T06:30:00Z',
  );
  await expect.poll(async () => (await sseState(request)).open).toBe(0);
});

test('Haul in live opens a new stream for a changed buffer and does nothing for an unchanged one', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await burstArrived(page);
  await expect.poll(async () => (await sseState(request)).open).toBe(1);
  // `opens` is cumulative across the worker's tests by design, so this
  // reads the DELTA around each action rather than an absolute count.
  const opened = (await sseState(request)).opens;

  await haul(page, 'service=postgres');
  await expect(page).toHaveURL(/q=service%3Dpostgres/);
  await expect(page).toHaveURL(/mode=live/);
  await expect.poll(async () => (await sseState(request)).opens).toBe(opened + 1);
  await expect.poll(async () => (await sseState(request)).open).toBe(1);

  // Unchanged buffer: no navigation, no stream, no query.
  await haul(page, 'service=postgres');
  await page.waitForTimeout(500);
  expect((await sseState(request)).opens).toBe(opened + 1);
  await expect(page).toHaveURL(/mode=live/);
  expect(await capturedQueryCount(request)).toBe(0);
});

test('live posts no snapshot query, from page load through the whole burst', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await burstArrived(page);
  await expect(page.locator(SEL.statusLabel)).toHaveText('Live');
  // Settle: a request the page was about to make has had its chance by
  // the time 6,000 events have rendered and the footer has caught up.
  await expect(page.locator(SEL.footerCount)).toContainText('Received 6000');
  await expect(page.locator(SEL.scopeCount)).toHaveText('5000 buffered rows');
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
  expect(await capturedQueryCount(request)).toBe(0);
});

test('the burst footer counts what the ring accepted, survives a drop, and restarts on retry', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  let blocked = false;
  await page.route('**/api/v1/stream?*', route => blocked
    ? route.fulfill({ status: 400, json: { error: 'unavailable' } })
    : route.continue());
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await burstArrived(page);

  // Received counts events the ring ACCEPTED, not SSE messages it kept:
  // 6,000 arrived, 5,000 are on screen.
  await expect(page.locator(SEL.footerCount)).toContainText('Received 6000');
  await expect(page.locator(SEL.workspaceTab).first()).toContainText('5000');
  await expect(page.locator(SEL.statusLabel)).toHaveText('Live');

  // Block the automatic reconnect, or the harness replays the burst and
  // the failure never reaches the footer.
  blocked = true;
  expect((await request.post('/__ctl/stream/drop')).ok()).toBe(true);
  await expect(page.locator(SEL.statusLabel)).toHaveText('Error');
  // Wait for the REFUSED reconnect, not just the drop: an HTTP error
  // closes the EventSource for good, so from here only the Retry button
  // can open a stream, and the retry assertion below cannot be answered
  // by a reconnect that happened to win the race.
  await expect(page.getByRole('alert')).toContainText('Live stream unavailable.');
  await expect(page.locator(SEL.liveResultsTable).locator('tbody tr')).toHaveCount(5000);

  blocked = false;
  await page.getByRole('button', { name: 'Retry live stream' }).click();
  await expect(page.locator(SEL.statusLabel)).toHaveText('Live');
  // A fresh session counts from zero: 6000, never 12000.
  await expect(page.locator(SEL.footerCount)).toContainText('Received 6000');
});

test('live aggregation counts frames and reads a malformed one as an error', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  const frame = async (data: unknown) =>
    (await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify(data) } })).ok();
  const rows = (n: number) => ({
    columns: ['_time', 'count'],
    rows: Array.from({ length: n }, (_, i) => ({ _time: `2026-09-01T00:0${i}:00Z`, count: i + 1 })),
  });

  await page.goto(`/search?q=${encodeURIComponent('service=nginx | timechart count()')}&mode=live`);
  await expect.poll(async () => (await sseState(request)).open).toBe(1);

  expect(await frame(rows(2))).toBe(true);
  expect(await frame(rows(3))).toBe(true);
  await expect(page.locator(SEL.footerCount)).toContainText('Updates 2');
  await expect(page.locator(SEL.workspaceTab).first()).toContainText('3');

  expect((await request.post('/__ctl/stream/frame', { data: { data: 'not a frame' } })).ok()).toBe(true);
  await expect(page.locator(SEL.statusLabel)).toHaveText('Error');
  expect(await frame(rows(1))).toBe(true);
  await expect(page.locator(SEL.statusLabel)).toHaveText('Live');
});

test('the filter rail summarizes the ring in live, not the snapshot it replaced', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx');
  // A value only the snapshot has.
  await expect(page.locator(SEL.facetValue).filter({ hasText: 'web-01' }).first()).toBeVisible();

  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();
  await expect(page).toHaveURL(/mode=live/);

  // A value only the stream has.
  await expect(page.locator(SEL.facetValue).filter({ hasText: 'tick 1' }).first()).toBeVisible();
  await expect(page.locator(SEL.facetValue).filter({ hasText: 'web-01' })).toHaveCount(0);
});

test('an aggregation-shaped result computes no groups while Clear all still removes URL filters', async ({ page }) => {
  const aggregation = {
    columns: [{ name: 'status' }, { name: 'count' }],
    rows: [['200', 2], ['404', 2]],
    pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
  };
  await page.route('**/api/v1/query', route => route.fulfill({ json: aggregation }));

  // Control: the same rows under a plain search DO get a rail.
  await page.goto(`/search?q=service%3Dnginx&f=${FILTERS}`);
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();

  await page.goto(
    `/search?q=${encodeURIComponent('service=nginx | stats count() by status')}&f=${FILTERS}`,
  );
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await page.locator(SEL.facetClear).click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
});

test('the histogram and execution timing are absent in live and return with a snapshot', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await burstArrived(page);
  await expect(page.locator(SEL.histoStrip)).toHaveCount(0);
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
  await page.locator(SEL.stopLive).click();
  await expect(page.locator(SEL.histoStrip)).toHaveCount(1);
  await expect(page.locator(SEL.scopeExecution)).toHaveText('Execution in 0.125s');
  await expect(page.locator(SEL.scopeStarted)).toHaveText('Started 2026-09-15 12:34:56 UTC');

  // An explicit DSL window still reports response timing in the strip.
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=last%3D24h&r=15m');
  await expect(page.locator(SEL.scopeExecution)).toHaveText('Execution in 0.125s');
  await expect(page.locator(SEL.scopeStarted)).toHaveText('Started 2026-09-15 12:34:56 UTC');
});

test('a snapshot finishing after Live cannot restore timing or complete a later snapshot', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  const held: import('@playwright/test').Route[] = [];
  await page.route('**/api/v1/query', route => { held.push(route); });
  await page.goto('/search?q=service%3Dnginx');
  await expect.poll(() => held.length).toBe(1);
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();
  await burstArrived(page);
  await expect(page.locator(SEL.scopeCount)).toHaveText('5000 buffered rows');
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);

  await page.locator(SEL.stopLive).click();
  await expect(page.locator(SEL.scopeCount)).toHaveText('…');
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
  const responseBody = (started: string, duration: number) => ({
    columns: [{ name: 'message' }], rows: [['snapshot']],
    pagination: { limit: 50, offset: 0, returned: 1, total: 1 },
    execution: { started_at: started, duration_ms: duration },
  });
  const oldResponse = page.waitForResponse('**/api/v1/query');
  await held[0].fulfill({ json: responseBody('2026-09-15T10:00:00Z', 100) });
  await (await oldResponse).finished();
  await expect.poll(() => held.length).toBe(2);
  await page.evaluate(() => new Promise<void>(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  }));
  await expect(page.locator(SEL.scopeCount)).toHaveText('…');
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
  await held[1].fulfill({ json: responseBody('2026-09-15T11:00:00Z', 200) });
  await expect(page.locator(SEL.scopeExecution)).toHaveText('Execution in 0.200s');
  await expect(page.locator(SEL.scopeStarted)).toHaveText('Started 2026-09-15 11:00:00 UTC');
  await expect(page.locator(SEL.scopeCount)).toHaveText('1 row returned');
});
