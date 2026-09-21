// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The chart draws the whole result or nothing (ADR-0037): an aggregation
// is fetched once, whole, and paged in the browser.
//
// THE PROPERTY UNDER TEST IS THE REQUEST, not the picture. A canvas
// existing says nothing about how much of the result reached it, so the
// chart publishes its plotted series length on its host element and the
// harness records each request's `limit` and `offset`. One is what the
// browser drew, the other is what it asked for; a fetch that stopped at
// 50 rows fails both.

import { test, expect } from '../fixtures';
import { LIMITS, SEL } from '../selectors';

/** 59 buckets: more than one page, so "whole" and "the first page" are
 * different answers, and the 9 rows past the page are the ones a paged
 * fetch would have lost. */
const BUCKETS = 59;

// `level=error | timechart span=2m count()` over four hours — the shape
// and range of the defect this file is about. The range rides the URL as
// `r=4h` and merges into the search stage, so the posted DSL is longer
// than this; the harness dispatches on the `| timechart` pipeline.
const AGG_URL = `/search?q=${encodeURIComponent('level=error | timechart span=2m count()')}&r=4h`;

type Ctl = import('@playwright/test').APIRequestContext;

/** Select the `aggregate` scenario with its knobs. */
async function configure(request: Ctl, aggregate: Record<string, unknown>) {
  const response = await request.post('/__ctl/reset', { data: { scenario: 'aggregate', aggregate } });
  expect(response.ok(), `reset aggregate: HTTP ${response.status()}`).toBe(true);
}

/** Every recorded query's window, oldest first. The DSL rides along so a
 * failure names the request that was not supposed to happen. */
async function capturedWindows(request: Ctl) {
  const state = await (await request.get('/__ctl/state')).json();
  return state.queries.map((q: { query: string; limit: number; offset: number }) => ({
    query: q.query,
    limit: q.limit,
    offset: q.offset,
  }));
}

test('an aggregation is fetched whole and charted whole', async ({ page, request }) => {
  await configure(request, { buckets: BUCKETS });
  await page.goto(AGG_URL);

  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  // Every bucket plotted, not the 50 a page would have carried.
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', String(BUCKETS));

  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
  expect(windows[0]).toMatchObject({ limit: LIMITS.aggregateFetchRows, offset: 0 });
});

test('paging an aggregation posts nothing', async ({ page, request }) => {
  await configure(request, { buckets: BUCKETS });
  await page.goto(AGG_URL);

  const summary = page.locator(SEL.resultsSummary);
  await expect(summary).toHaveText(`1–50 of ${BUCKETS}`);

  await page.getByRole('button', { name: 'Next' }).click();
  await expect(summary).toHaveText(`51–${BUCKETS} of ${BUCKETS}`);
  await expect(page).toHaveURL(/[?&]page=1/);

  // The page turn is a slice of the result already in hand.
  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
  expect(windows.map((w: { offset: number }) => w.offset)).toEqual([0]);
});

test('an out-of-range local page recovers', async ({ page, request }) => {
  await configure(request, { buckets: BUCKETS });
  // A link naming a page past the fetched rows. The pager counts the
  // rows in hand, so it says what it has and offers the way back rather
  // than inventing a page (ADR-0030).
  await page.goto(`${AGG_URL}&page=9`);

  const summary = page.locator(SEL.resultsSummary);
  await expect(summary).toHaveText(`0–0 of ${BUCKETS}`);

  await page.getByRole('button', { name: 'Prev' }).click();
  await expect(page).toHaveURL(/[?&]page=1/);
  await expect(summary).toHaveText(`51–${BUCKETS} of ${BUCKETS}`);

  // Still one fetch: an out-of-range page is a slice of the result
  // already in hand, the same as any other.
  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
});
