// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The histogram strip draws the whole-result count trawld answers beside
// each page (issue #197). The stub counts nothing: each query text below
// names a canned response in `harness/wire/query-histogram-*.json`, which
// `tests/e2e_wire_fixture_contract.rs` holds to trawld's counting rule,
// and the stub returns its `histogram` only to a request that asks.

import { test, expect } from '../fixtures';
import { SEL } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

/** The strip's words, as `crates/trawl-web-ui/src/histogram.rs` builds them. */
const COPY = {
  paged: 'All 120 events in this result · first to last event · ≈1.2min per bucket · UTC',
  kept: 'All 10 events in this result · 2 without a usable time, not drawn · first to last event · ≈5.6s per bucket · UTC',
  plain: 'All 8 events in this result · first to last event · ≈8.8s per bucket · UTC',
  keptLine: 'Only the events your query kept are counted.',
  noSeverity: 'Events (no severity in this result)',
  sample: 'No histogram for a sampled result. Remove sample to count every event.',
  stored: "No histogram for rows read from a net's run.",
  noTime: 'No histogram: this result has no _time timestamp.',
  noUsable: 'No histogram: no event in this result has a usable time.',
};

/** Which canned response answers which query text. */
const FIXTURES = [
  ['service=nginx', 'query-histogram-paged'],
  ['service=kept', 'query-histogram-kept'],
  ['service=plain', 'query-rows'],
  ['service=empty', 'query-histogram-empty'],
  ['service=sampled', 'query-histogram-sample'],
  ['service=stored', 'query-histogram-stored-result'],
  ['service=notime', 'query-histogram-no-time-column'],
  ['service=undated', 'query-histogram-no-usable-time'],
];

const histo = (page: Pg) => page.locator(SEL.histoStrip);
const caption = (page: Pg) => page.locator('.histo .histo-scope');
const bars = (page: Pg) => page.locator('.histo .bar');
const hint = (page: Pg) => page.locator('.histo .histo-hint');
const summary = (page: Pg) => page.locator('.results .results-footer .results-summary');

async function configure(request: Ctl, pagination: Record<string, unknown> = {}) {
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'pagination', pagination: { histogramFixtures: FIXTURES, ...pagination } },
  });
  expect(response.ok()).toBeTruthy();
}

async function state(request: Ctl) {
  return (await (await request.get('/__ctl/state')).json());
}

async function release(request: Ctl, options: { status?: number } = {}) {
  expect((await request.post('/__ctl/pagination/query-release', { data: options })).ok()).toBeTruthy();
}

/** Every bar's tooltip text, in order. */
async function tips(page: Pg): Promise<string[]> {
  return page.locator('.histo .bar .tip').allTextContents();
}

/** Errors summed over every bar's tooltip. */
function errorsIn(texts: string[]): number {
  return texts
    .map(text => text.match(/ · ([\d,]+) errors$/)?.[1] ?? '0')
    .reduce((sum, n) => sum + Number(n.replaceAll(',', '')), 0);
}

/** Follow a same-document link, so the mounted page keeps its resource
 * and any request still in flight. */
async function follow(page: Pg, href: string) {
  await page.evaluate((target) => {
    const link = document.createElement('a');
    link.href = target;
    document.body.append(link);
    link.click();
    link.remove();
  }, href);
}

/** Run the editor's query again, as Ctrl+Enter does. */
async function resubmit(page: Pg) {
  await page.locator(SEL.cmContent).focus();
  await page.keyboard.press('Control+Enter');
}

/** Record every moment the strip shows a bar or a caption, from now on. */
async function watchStrip(page: Pg) {
  await page.evaluate(() => {
    const w = window as any;
    w.__stripSeen = [];
    const look = () => {
      const count = document.querySelectorAll('.histo .bar').length;
      const scope = document.querySelector('.histo .histo-scope')?.textContent ?? null;
      if (count > 0 || scope !== null) w.__stripSeen.push({ bars: count, caption: scope });
    };
    look();
    w.__stripObserver?.disconnect();
    w.__stripObserver = new MutationObserver(look);
    w.__stripObserver.observe(document.body, { childList: true, subtree: true, characterData: true });
  });
}

async function stripSeen(page: Pg): Promise<Array<{ bars: number; caption: string | null }>> {
  return page.evaluate(() => (window as any).__stripSeen);
}

/** Trace whether the strip is drawn at every DOM change, from now on.
 * Call it before a transition, while the settled strip still shows. */
async function traceStrip(page: Pg) {
  await page.evaluate(() => {
    const w = window as any;
    w.__stripTrace = [];
    const look = () => {
      const drawn = document.querySelectorAll('.histo .bar').length > 0
        || document.querySelector('.histo .histo-scope') !== null;
      if (w.__stripTrace.at(-1) !== drawn) w.__stripTrace.push(drawn);
    };
    look();
    w.__traceObserver?.disconnect();
    w.__traceObserver = new MutationObserver(look);
    w.__traceObserver.observe(document.body, { childList: true, subtree: true, characterData: true });
  });
}

/** Since `traceStrip`, the settled strip cleared once and nothing drew
 * again: no old bar or caption came back on the way to the wait. */
async function expectClearedOnce(page: Pg) {
  expect(await page.evaluate(() => (window as any).__stripTrace)).toEqual([true, false]);
}

/** The strip is waiting: the shared loading hint, and nothing else. */
async function expectWaiting(page: Pg) {
  await expect(histo(page)).toBeVisible();
  await expect(histo(page)).not.toHaveClass(/histo-empty/);
  await expect(page.locator('.histo .load-hint')).toBeVisible();
  await expect(bars(page)).toHaveCount(0);
  await expect(caption(page)).toHaveCount(0);
}

test('a raw query asks for the histogram and an aggregation does not', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dnginx');
  await expect(caption(page)).toHaveText(COPY.paged);
  await page.goto('/search?q=service%3Dagg%20%7C%20stats%20count()%20by%20host');
  await expect.poll(async () => (await state(request)).queries.length).toBe(2);
  const [raw, aggregation] = (await state(request)).queries;
  expect(raw.histogram).toBe(true);
  expect('histogram' in aggregation, JSON.stringify(aggregation)).toBe(false);
});

test('page 2 shows the whole-result count and the errors off its page', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dnginx');
  await expect(summary(page)).toHaveText('Page 1 · showing 50 rows');
  await expect(caption(page)).toHaveText(COPY.paged);
  await expect(bars(page)).toHaveCount(48);
  // Page 1 holds the 50 newest events, none of them an error, and the
  // strip still shows the 30 errors older than them.
  await expect(page.locator('.results-table tbody .lvl-error')).toHaveCount(0);
  const first = await tips(page);
  expect(errorsIn(first)).toBe(30);

  await page.locator('.results .results-footer').getByRole('button', { name: 'Next' }).click();
  await expect(summary(page)).toHaveText('Page 2 · showing 50 rows');
  await expect(caption(page)).toHaveText(COPY.paged);
  await expect(bars(page)).toHaveCount(48);
  expect(await tips(page)).toEqual(first);
  // Page 2 shows only 10 of the errors; the other 20 are on page 3.
  await expect(page.locator('.results-table tbody .lvl-error')).toHaveCount(10);

  // One execution per page turn, each asking for the histogram.
  const queries = (await state(request)).queries;
  expect(queries.map((q: { offset: number; histogram?: boolean }) => [q.offset, q.histogram])).toEqual([[0, true], [50, true]]);
});

test('a page past the end keeps the strip and an empty result collapses it', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dnginx&page=3');
  await expect(summary(page)).toHaveText('Page 4 · showing 0 rows');
  await expect(histo(page)).not.toHaveClass(/histo-empty/);
  await expect(caption(page)).toHaveText(COPY.paged);
  await expect(bars(page)).toHaveCount(48);

  await page.goto('/search?q=service%3Dempty');
  await expect(page.locator('.results-table tbody')).toContainText('No events');
  await expect(histo(page)).toHaveClass(/histo-empty/);
  await expect(histo(page)).toBeHidden();
});

test('each reason says why there is no histogram', async ({ page, request }) => {
  await configure(request);
  for (const [service, copy] of [
    ['sampled', COPY.sample],
    ['stored', COPY.stored],
    ['notime', COPY.noTime],
    ['undated', COPY.noUsable],
  ]) {
    await page.goto(`/search?q=service%3D${service}`);
    await expect(hint(page), service).toHaveText(copy);
    await expect(histo(page)).not.toHaveClass(/histo-empty/);
    await expect(bars(page)).toHaveCount(0);
    await expect(caption(page)).toHaveCount(0);
  }
});

test('a raw page without a histogram shows the quiet dash', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dbare');
  await expect(summary(page)).toHaveText('Page 1 · showing 50 rows');
  await expect(hint(page)).toHaveText('—');
  await expect(bars(page)).toHaveCount(0);
});

test('the key says when the query kept some events and when the result has no severity', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dkept');
  await expect(caption(page)).toHaveText(COPY.kept);
  await expect(page.locator('.histo .histo-note')).toHaveText(COPY.keptLine);
  await expect(histo(page)).toHaveClass(/histo-kept/);
  await expect(page.locator('.histo .histo-swatch')).toHaveText(['Other', 'Error or fatal']);

  await page.goto('/search?q=service%3Dplain');
  await expect(caption(page)).toHaveText(COPY.plain);
  await expect(page.locator('.histo .histo-note')).toHaveCount(0);
  await expect(histo(page)).not.toHaveClass(/histo-kept/);
  await expect(page.locator('.histo .histo-swatch')).toHaveText([COPY.noSeverity]);
  await expect(page.locator('.histo .histo-swatch .err')).toHaveCount(0);
});

test('histogram never shows bins from a pending or superseded query', async ({ page, request }) => {
  // Query 1 answers; 2 to 5 wait for the test to release them.
  await configure(request, { holdQueryNumber: [2, 3, 4, 5] });
  await page.goto('/search?q=service%3Dnginx');
  await expect(caption(page)).toHaveText(COPY.paged);
  await expect(bars(page)).toHaveCount(48);

  // A resubmit of the same query: its bins are the old ones, and the
  // strip still waits for them.
  await traceStrip(page);
  await resubmit(page);
  await expect.poll(async () => (await state(request)).pagination.heldQueryNumbers).toEqual([2]);
  await expectWaiting(page);
  await expectClearedOnce(page);
  await watchStrip(page);

  // A second resubmit supersedes the first. The first one's response
  // lands and the strip keeps waiting for the second.
  await resubmit(page);
  await release(request);
  await expect.poll(async () => (await state(request)).pagination.heldQueryNumbers).toEqual([3]);
  await expectWaiting(page);
  expect(await stripSeen(page)).toEqual([]);
  await release(request);
  await expect(caption(page)).toHaveText(COPY.paged);
  await expect(bars(page)).toHaveCount(48);

  // A new query that fails after another superseded it: neither the
  // failure nor the wait shows the old bins.
  await traceStrip(page);
  await follow(page, '/search?q=service%3Dkept');
  await expect.poll(async () => (await state(request)).pagination.heldQueryNumbers).toEqual([4]);
  await expectWaiting(page);
  await expectClearedOnce(page);
  await watchStrip(page);
  await follow(page, '/search?q=service%3Dplain');
  await release(request, { status: 500 });
  await expect.poll(async () => (await state(request)).pagination.heldQueryNumbers).toEqual([5]);
  await expect(bars(page)).toHaveCount(0);
  await expect(caption(page)).toHaveCount(0);
  // The current query fails too: the strip shows the quiet dash.
  await release(request, { status: 500 });
  await expect(hint(page)).toHaveText('—');
  expect(await stripSeen(page)).toEqual([]);

  // The next answer draws its own bins.
  await resubmit(page);
  await expect(caption(page)).toHaveText(COPY.plain);
  expect((await stripSeen(page)).every(seen => seen.caption === COPY.plain)).toBe(true);

  // From an empty result: the collapsed strip opens to wait for the next
  // query, and an old zero total never hides the loading hint.
  await configure(request, { holdQueryNumber: 2 });
  await page.goto('/search?q=service%3Dempty');
  await expect(histo(page)).toHaveClass(/histo-empty/);
  await follow(page, '/search?q=service%3Dnginx');
  await expect.poll(async () => (await state(request)).pagination.heldQueryNumbers).toEqual([2]);
  await expectWaiting(page);
  await watchStrip(page);
  await release(request);
  await expect(caption(page)).toHaveText(COPY.paged);
  expect((await stripSeen(page)).every(seen => seen.caption === COPY.paged)).toBe(true);
});
