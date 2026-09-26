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
//
// The rail's own tests here read it at the default wide viewport, where
// it opens for a countable page and is otherwise a closed strip
// (ADR-0044).

import { test, expect, resetScenario, capturedQueryCount, lastCapturedQuery } from '../fixtures';
import { expectRail, watchToggles } from '../filter-rail';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

/** `host="web-01"`, the same versioned payload live-raw-recovery uses.
 * Stop live has to carry it through untouched. */
const FILTERS = 'v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0';

/** `host="web-01"` and `status="200"`: two filters, so a count or a
 * Clear all that handled only one of them shows. */
const TWO_FILTERS =
  'v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifSx7Im9wIjoiKyIsImZpZWxkIjoic3RhdHVzIiwidmFsdWUiOiIyMDAifV0';

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

// The two tests below are about the REQUEST the page builds, not the
// events that come back. The stub never evaluates the DSL: `stream-burst`
// writes its 6,000 events whatever the query asks for, so a stream still
// carrying `last=15m` would look identical on screen here. What they read
// is the `query` parameter on the wire and the DSL the snapshot runs on
// either side of it. That the server then DELIVERS under a range-free
// query is issue #232's real-stack check, which no stub can answer.

test('the live stream carries the query with no range, and Stop live returns to the ranged snapshot', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  // Collected in the browser rather than at the stub, because the claim
  // is about what the page ASKED for — including a second ask the stub
  // would happily serve.
  const streamUrls: string[] = [];
  page.on('request', r => {
    if (new URL(r.url()).pathname === '/api/v1/stream') streamUrls.push(r.url());
  });
  const opened = page.waitForRequest(r => new URL(r.url()).pathname === '/api/v1/stream');

  await page.goto('/search?q=service%3Dnginx&mode=live');
  // Equality, not merely "no last=": a stream that folded the range in
  // under some other spelling would still pass an absence check.
  expect(new URL((await opened).url()).searchParams.get('query')).toBe('service=nginx');

  await burstArrived(page);
  expect(streamUrls).toHaveLength(1);
  expect(await capturedQueryCount(request)).toBe(0);

  // The range never went anywhere: it is what the snapshot on the way
  // out runs, which is the whole reason `r` stays in the URL while live.
  await page.locator(SEL.stopLive).click();
  expect((await lastCapturedQuery(request, 1)).query).toBe('last=15m service=nginx');
  await expect(page).not.toHaveURL(/mode=live/);
  expect(new URL(page.url()).searchParams.get('mode')).toBeNull();
});

test('entering live from the range dialog keeps r=1h out of the stream and back in the snapshot', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto(`/search?q=service%3Dnginx&r=${RANGE}`);
  expect((await lastCapturedQuery(request, 1)).query).toBe(`last=${RANGE} service=nginx`);

  const opened = page.waitForRequest(r => new URL(r.url()).pathname === '/api/v1/stream');
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();

  // The router writes the URL through an effect, so wait for it rather
  // than reading whichever tick `page.url()` is answering from.
  await page.waitForURL(url => url.searchParams.get('mode') === 'live');
  expect(new URL(page.url()).searchParams.get('r')).toBe(RANGE);
  expect(new URL((await opened).url()).searchParams.get('query')).toBe('service=nginx');

  await burstArrived(page);
  await page.locator(SEL.stopLive).click();
  expect((await lastCapturedQuery(request, 2)).query).toBe(`last=${RANGE} service=nginx`);
  await expect(page).not.toHaveURL(/mode=live/);
  const url = new URL(page.url());
  expect(url.searchParams.get('r')).toBe(RANGE);
  expect(url.searchParams.get('mode')).toBeNull();
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

// Active filters alone do not open the rail: an aggregation counts no
// field values, whatever the link filters on. The strip still shows the
// count, and a press opens the rail to say why it is empty and to offer
// Clear all, which #180 kept usable on an aggregation.
test('an aggregation-shaped result computes no groups while Clear all still removes URL filters', async ({ page }) => {
  const aggregation = {
    columns: [{ name: 'status' }, { name: 'count' }],
    rows: [['200', 2], ['404', 2]],
    pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
  };
  await page.route('**/api/v1/query', route => route.fulfill({ json: aggregation }));

  // Control: the same rows under a plain search DO get a rail, and it
  // opens for them.
  await page.goto(`/search?q=service%3Dnginx&f=${TWO_FILTERS}`);
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
  await expectRail(page, true);

  await page.goto(
    `/search?q=${encodeURIComponent('service=nginx | stats count() by status')}&f=${TWO_FILTERS}`,
  );
  // Settled: the aggregate's own table, not the pending page.
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(2);
  await expect(page.locator(SEL.filterChip)).toHaveCount(2);
  await expectRail(page, false);
  const summary = page.locator(SEL.filterRailSummary);
  await expect(summary).toContainText('Filters');
  await expect(page.locator(SEL.facetCount)).toHaveText('2 active');

  await summary.click();
  await expectRail(page, true);
  await expect(summary).toContainText('Filters');
  await expect(page.locator(SEL.facetCount)).toHaveText('2 active');
  await expect(page.locator(SEL.facetClear)).toBeVisible();
  await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintAggregate);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);

  await page.locator(SEL.facetClear).click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});

// A live stream starts with the rail closed and opens it on the first
// countable frame. From idle the rail is a strip already, so the whole
// stream moves it once; from a countable snapshot it would close on the
// empty ring first. The stream is held in the browser until the rail
// has been read on an empty ring.
test('Live Tail from idle opens the filter rail once, on the first countable frame', async ({ page, request }) => {
  // `corpus` streams a `tick N` event every 150ms and ends each stream
  // after about a second, and the browser reconnects into the same ring.
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await expect(page.locator('.search-quick-start')).toBeVisible();
  await expectRail(page, false);
  const toggles = await watchToggles(page);

  let release!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  let arrive!: () => void;
  const arrived = new Promise<void>(resolve => { arrive = resolve; });
  await page.route('**/api/v1/stream?*', async route => {
    arrive();
    await gate;
    await route.continue();
  }, { times: 1 });

  // Written, never run: the page is still idle when Live Tail starts.
  await page.locator(SEL.cmContent).fill('service=nginx');
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.liveTailButton).click();
  await expect(page).toHaveURL(/mode=live/);
  await arrived;
  await expectRail(page, false);
  expect(await toggles()).toBe(0);

  const opens = (await sseState(request)).opens;
  release();
  await expect(page.locator(SEL.facetValue).filter({ hasText: 'tick 1' }).first()).toBeVisible();
  await expectRail(page, true);
  expect(await toggles()).toBe(1);

  // Past a reconnect, with frames still arriving: the rail stays open.
  const received = async () =>
    Number((await page.locator(SEL.footerCount).textContent())?.match(/Received (\d+)/)?.[1] ?? 0);
  await expect.poll(async () => (await sseState(request)).opens).toBeGreaterThan(opens + 1);
  const before = await received();
  await expect.poll(received).toBeGreaterThan(before);
  await expectRail(page, true);
  expect(await toggles()).toBe(1);
  expect(await capturedQueryCount(request)).toBe(0);
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
