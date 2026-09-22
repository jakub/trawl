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

import { test, expect, resetScenario } from '../fixtures';
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
  // And the table says which absence this is. The result has 59 rows;
  // this page has none of them, which is not the same thing as an empty
  // net.
  await expect(page.locator(SEL.exactTable)).toContainText('No rows on this page.');

  await page.getByRole('button', { name: 'Prev' }).click();
  await expect(page).toHaveURL(/[?&]page=1/);
  await expect(summary).toHaveText(`51–${BUCKETS} of ${BUCKETS}`);

  // Still one fetch: an out-of-range page is a slice of the result
  // already in hand, the same as any other.
  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
});

// `* | stats count() by status` over the same generated scenario: no
// time axis, so Column is the type that fits it and Line is the one the
// picker has to disable.
const STATS_URL = `/search?q=${encodeURIComponent('level=error | stats count() by status')}`;

// The same timechart with a `by`: a grouped shape, which now DRAWS, so
// the coverage rung is the only thing standing between a cut window and
// a line that ends early with nothing to say it did.
const GROUPED_URL = `/search?q=${encodeURIComponent('level=error | timechart span=2m count() by host')}&r=4h`;

test('above the ceiling the chart refuses with the measured count', async ({ page, request }) => {
  // The execution produced 43,210 rows and one fetch carried 20,000 of
  // them. What is on screen is a window, and a chart of a window reads
  // as a chart of the result. Grouped, and so a shape the ladder is
  // otherwise happy to draw: the refusal here is the coverage rung and
  // nothing else.
  await configure(request, { buckets: LIMITS.aggregateFetchRows / 2, hosts: 2, total: 43210 });
  await page.goto(GROUPED_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  const refusal = page.locator('.visualization .results-empty');
  await expect(refusal).toContainText('43,210');
  await expect(refusal).toContainText('20,000');
  await expect(refusal).toContainText('whole result only');

  // Nothing was drawn, and the host says so rather than leaving the
  // claim to an opaque canvas.
  await expect(page.locator(SEL.chartHost)).not.toHaveAttribute('data-points', /.*/);
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(0);

  // The refusal sends the reader to Events, so Events has to say the
  // same thing about the same numbers: the table pages 20,000 of the
  // 43,210 rows the execution produced, and the cap line under it is
  // the only place that gap is written down.
  await page.getByRole('tab', { name: 'Events' }).click();
  const cap = page.locator(SEL.resultsCap);
  await expect(cap).toContainText('43,210');
  await expect(cap).toContainText('20,000');
});

test('a complete result at the ceiling draws', async ({ page, request }) => {
  // Every row the execution produced, and exactly as many as one fetch
  // can carry: whole, so there is nothing to refuse.
  await configure(request, { buckets: LIMITS.aggregateFetchRows, total: LIMITS.aggregateFetchRows });
  await page.goto(AGG_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', String(LIMITS.aggregateFetchRows));

  // Nothing was left behind, so Events has nothing to cap. The line is
  // absent, not empty: a cap line that always rendered would make the
  // assertion above it meaningless.
  await page.getByRole('tab', { name: 'Events' }).click();
  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  await expect(page.locator(SEL.resultsCap)).toHaveCount(0);
});

test('a cut result whose shape cannot be drawn says so first', async ({ page, request }) => {
  // Cut AND unchartable. The shape refusal names something the operator
  // can act on in the query, so it stays ahead of the coverage sentence
  // (ADR-0038).
  await configure(request, { groups: 60, total: 90 });
  await page.goto(`/search?q=${encodeURIComponent('level=error | pivot count() on status by host')}`);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  const refusal = page.locator('.visualization .results-empty');
  await expect(refusal).toHaveText('Pivot results are not drawn as lines. Open Events for the table.');
  await expect(refusal).not.toContainText('were fetched');
  await expect(page.locator(SEL.chartHost)).not.toHaveAttribute('data-points', /.*/);
});

test('a stats by result draws columns, and the picker disables the type that does not fit', async ({ page, request }) => {
  // No `_time` column at all, so the default follows the shape: Column,
  // one series over the groups, with Line disabled and saying why.
  await configure(request, { groups: 6 });
  await page.goto(STATS_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  const host = page.locator(SEL.chartHost);
  await expect(host).toHaveAttribute('data-chart-type', 'column');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(host).toHaveAttribute('data-points', '6');
  await expect(host).toHaveAttribute('data-series', '1');

  const radio = (type: string) => page.locator(`.chart-types input[value="${type}"]`);
  await expect(radio('column')).toBeChecked();
  await expect(radio('line')).toBeDisabled();
  await expect(radio('line')).toHaveAttribute('title', 'Line needs a timechart result.');
  await expect(radio('bar')).toBeEnabled();

  // The label is the part a pointer can hit: the input itself is clipped.
  await page.locator('.chart-types label', { hasText: 'Bar' }).click();
  await expect(host).toHaveAttribute('data-chart-type', 'bar');
  await expect(radio('bar')).toBeChecked();
});

test('a wide stats by result draws the twenty largest groups and says what it left out', async ({ page, request }) => {
  await configure(request, { groups: 200 });
  await page.goto(STATS_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '20');
  await expect(page.locator('.visualization .chart-caption'))
    .toHaveText('20 of 200 groups drawn; the 180 smallest are not.');
});

test('a chart type is session state, kept across tabs and queries and out of the URL', async ({ page, request }) => {
  // The reader's choice is not part of the search URL (ADR-0027), so it
  // has to survive both the tab it lives on and the next result.
  await configure(request, { groups: 6 });
  await page.goto(STATS_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await page.locator('.chart-types label', { hasText: 'Bar' }).click();
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-chart-type', 'bar');

  await page.getByRole('tab', { name: 'Events' }).click();
  await expect(page.locator(SEL.chartHost)).toHaveCount(0);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(page.locator('.chart-types input[value="bar"]')).toBeChecked();

  // A new result of the same shape: still Bar, because that is what the
  // reader asked for.
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText('level=warn | stats count() by status');
  await page.keyboard.press('Control+Enter');
  await expect(page).toHaveURL(/level%3Dwarn/);
  await expect(page.locator('.chart-types input[value="bar"]')).toBeChecked();
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-chart-type', 'bar');

  // And the URL carries no chart-type parameter — only the search
  // parameters the page has always owned.
  const params = [...new URL(page.url()).searchParams.keys()];
  expect(params.every(key => ['q', 'page', 'mode', 'f', 'r'].includes(key)), params.join(',')).toBe(true);
});

test('no truncation affordance remains', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/search?q=service%3Dnginx');
  // The rendered page of rows is the anchor: the negative assertions
  // under it only mean something once a successful answer is on screen.
  await expect(page.locator('.results .results-footer .results-summary'))
    .toHaveText('Page 1 · showing 50 rows');

  // The window a page asks for is not a verdict on the answer, so
  // nothing in the results region calls a result cut short.
  await expect(page.locator('.tabs .bdg')).toHaveCount(0);
  await expect(page.locator('.results')).not.toContainText('Truncated');
  await expect(page.locator('.results')).not.toContainText('(truncated)');
  await expect(page.locator(SEL.saveAction)).toBeVisible();
  await expect(page.locator(SEL.exportAction)).toBeVisible();
});
