// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The chart draws the whole result or nothing (ADR-0037).
import { test, expect, resetScenario } from '../fixtures';
import { LIMITS, SEL } from '../selectors';

const BUCKETS = 59;

// The range rides the URL as `r=4h` and merges into the search stage, so
// the posted DSL is longer than `q`; the harness dispatches on the
// `| timechart` pipeline.
const AGG_URL = `/search?q=${encodeURIComponent('level=error | timechart span=2m count()')}&r=4h`;

type Ctl = import('@playwright/test').APIRequestContext;

async function configure(request: Ctl, aggregate: Record<string, unknown>) {
  const response = await request.post('/__ctl/reset', { data: { scenario: 'aggregate', aggregate } });
  expect(response.ok(), `reset aggregate: HTTP ${response.status()}`).toBe(true);
}

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

  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
  expect(windows.map((w: { offset: number }) => w.offset)).toEqual([0]);
});

test('an out-of-range local page recovers', async ({ page, request }) => {
  await configure(request, { buckets: BUCKETS });
  await page.goto(`${AGG_URL}&page=9`);

  const summary = page.locator(SEL.resultsSummary);
  await expect(summary).toHaveText(`0–0 of ${BUCKETS}`);
  await expect(page.locator(SEL.exactTable)).toContainText('No rows on this page.');

  await page.getByRole('button', { name: 'Prev' }).click();
  await expect(page).toHaveURL(/[?&]page=1/);
  await expect(summary).toHaveText(`51–${BUCKETS} of ${BUCKETS}`);

  const windows = await capturedWindows(request);
  expect(windows).toHaveLength(1);
});

const STATS_URL = `/search?q=${encodeURIComponent('level=error | stats count() by status')}`;

const GROUPED_URL = `/search?q=${encodeURIComponent('level=error | timechart span=2m count() by host')}&r=4h`;

test('above the ceiling the chart refuses with the measured count', async ({ page, request }) => {
  await configure(request, { buckets: LIMITS.aggregateFetchRows / 2, hosts: 2, total: 43210 });
  await page.goto(GROUPED_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  const refusal = page.locator('.visualization .results-empty');
  await expect(refusal).toContainText('43,210');
  await expect(refusal).toContainText('20,000');
  await expect(refusal).toContainText('whole result only');

  await expect(page.locator(SEL.chartHost)).not.toHaveAttribute('data-points', /.*/);
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(0);

  await page.getByRole('tab', { name: 'Events' }).click();
  const cap = page.locator(SEL.resultsCap);
  await expect(cap).toContainText('43,210');
  await expect(cap).toContainText('20,000');
});

test('a complete result at the ceiling draws', async ({ page, request }) => {
  await configure(request, { buckets: LIMITS.aggregateFetchRows, total: LIMITS.aggregateFetchRows });
  await page.goto(AGG_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', String(LIMITS.aggregateFetchRows));

  await page.getByRole('tab', { name: 'Events' }).click();
  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  await expect(page.locator(SEL.resultsCap)).toHaveCount(0);
});

test('a cut result whose shape cannot be drawn says so first', async ({ page, request }) => {
  await configure(request, { groups: 60, total: 90 });
  await page.goto(`/search?q=${encodeURIComponent('level=error | pivot count() on status by host')}`);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();

  const refusal = page.locator('.visualization .results-empty');
  await expect(refusal).toHaveText('Pivot results are not drawn as lines. Open Events for the table.');
  await expect(refusal).not.toContainText('were fetched');
  await expect(page.locator(SEL.chartHost)).not.toHaveAttribute('data-points', /.*/);
});

test('a stats by result draws columns, and the picker disables the type that does not fit', async ({ page, request }) => {
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
    .toHaveText('20 of 200 groups drawn; the 180 smallest are not, among them 261, 274 and 287.');
  await expect(page.locator('.visualization .chart-note'))
    .toHaveText('Column: count by status, 20 groups. Hover a bar for its value. Open Events for the exact table.');
});

test('a chart type is session state, kept across tabs and queries and out of the URL', async ({ page, request }) => {
  await configure(request, { groups: 6 });
  await page.goto(STATS_URL);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await page.locator('.chart-types label', { hasText: 'Bar' }).click();
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-chart-type', 'bar');

  await page.getByRole('tab', { name: 'Events' }).click();
  await expect(page.locator(SEL.chartHost)).toHaveCount(0);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(page.locator('.chart-types input[value="bar"]')).toBeChecked();

  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText('level=warn | stats count() by status');
  await page.keyboard.press('Control+Enter');
  await expect(page).toHaveURL(/level%3Dwarn/);
  await expect(page.locator('.chart-types input[value="bar"]')).toBeChecked();
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-chart-type', 'bar');

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

  await expect(page.locator('.tabs .bdg')).toHaveCount(0);
  await expect(page.locator('.results')).not.toContainText('Truncated');
  await expect(page.locator('.results')).not.toContainText('(truncated)');
  await expect(page.locator(SEL.exportAction)).toBeVisible();
});
