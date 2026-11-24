// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The aggregate presentation on the Events tab: the exact numbers, and
// the bars beside them (ADR-0032).
//
// An absence only counts as evidence when the positive case is asserted
// beside it, so every test that says "no control on the metric" also
// says "a control on the group", from the same page.

import { test, expect, resetScenario } from '../fixtures';
import { COPY, SEL } from '../selectors';

const WIDE_GROUPS = 60;
const WIDE_URL = '/search?q=' + encodeURIComponent('service=nginx | stats count() by status');

// `| stats count() by status` — the harness dispatches the `corpus`
// scenario on that pipeline shape and answers `wire/query-stats-by.json`:
// five groups, one negative count and one null one.
const AGG_URL = '/search?q=service%3Dnginx%20%7C%20stats%20count()%20by%20status';
const GROUPS = 5;

// The quick-start "Rank services by errors" answer under `aggregate`:
// `RANKED_SERVICES` in harness/fixtures.mjs, five services, top first.
const RANKED = 5;
const TOP_SERVICE = 'api';

test('an aggregate answers with an exact table and no expansion column', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const table = page.locator(SEL.exactTable);
  await expect(table).toHaveCount(1);
  await expect(table.locator('tbody tr')).toHaveCount(GROUPS);

  await expect(page.locator(SEL.resultsExpandControl)).toHaveCount(0);
  await expect(page.locator('.results-table th.exp-col')).toHaveCount(0);

  const headers = table.locator('thead th.sortable');
  await expect(headers).toHaveCount(2);
  await expect(headers.nth(0)).toContainText('status');
  await expect(headers.nth(1)).toContainText('count');

  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});

test('only the grouped column offers a search, never the generated metric', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const table = page.locator(SEL.exactTable);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);
  await expect(table.locator('tbody tr td:nth-child(1) button')).toHaveCount(GROUPS);
  await expect(table.locator('tbody tr td:nth-child(2) button')).toHaveCount(0);

  const first = page.locator(SEL.groupSearch).first();
  await expect(first).toHaveAttribute('type', 'button');
  await expect(first).toHaveAccessibleName('Search status = 200');
});

test('a group search adds one include filter to the link', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await page.locator(SEL.groupSearch).first().click();

  await expect(page).toHaveURL(/[?&]f=v1\./);
  await expect(page).toHaveURL(/stats/);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page.locator(SEL.filterChip)).toContainText('status = 200');
});

test('the ranked quick-start result offers a group search on every service', async ({ page, request }) => {
  await resetScenario(request, 'aggregate');
  await page.goto('/search');
  await page.getByRole('button', { name: 'Run Rank services by errors', exact: true }).click();

  const table = page.locator(SEL.exactTable);
  const rows = table.locator('tbody tr');
  await expect(rows).toHaveCount(RANKED);
  await expect(table.locator('thead th.sortable').nth(0)).toContainText('service');

  await expect(page.locator(SEL.groupSearch)).toHaveCount(RANKED);
  await expect(table.locator('tbody tr td:nth-child(1) button')).toHaveCount(RANKED);
  await expect(table.locator('tbody tr td:nth-child(2) button')).toHaveCount(0);
  await expect(page.locator(SEL.groupSearch).first()).toHaveAccessibleName(`Search service = ${TOP_SERVICE}`);

  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await page.locator(SEL.groupSearch).first().click();

  await expect(page).toHaveURL(/[?&]f=v1\./);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page.locator(SEL.filterChip)).toContainText(`service = ${TOP_SERVICE}`);
  await expect(rows).toHaveCount(1);
  await expect(rows.first().locator('td').first()).toContainText(TOP_SERVICE);
});

test('the categorical chart draws one bar per group beside the numbers', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const chart = page.locator(SEL.catChart);
  await expect(chart).toHaveCount(1);
  await expect(chart.locator('.cat-title')).toHaveText('count by status');

  const bars = chart.locator('.cat-bars li');
  await expect(bars).toHaveCount(GROUPS);
  await expect(chart.locator('.cat-bars')).toHaveAttribute('aria-hidden', 'true');

  await expect(chart.locator('.cat-bars.signed')).toHaveCount(1);
  await expect(chart.locator('.cat-bars li.neg')).toHaveCount(1);
  await expect(chart.locator('.cat-bars li.neg .cat-val')).toHaveText('-1');

  const nulls = chart.locator('.cat-bars li.null');
  await expect(nulls).toHaveCount(1);
  await expect(nulls.locator('.cat-track i')).toHaveCount(0);
  await expect(nulls.locator('.cat-val')).toHaveText('—');

  await expect(bars.nth(0).locator('.cat-val')).toHaveText('940');
  await expect(bars.nth(0).locator('.cat-track i')).toHaveAttribute('style', 'width:50.00%');
});

test('a non-chartable aggregate renders the exact table alone', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx%20%7C%20top%2010%20host');

  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  await expect(page.locator(SEL.catChart)).toHaveCount(0);
  await expect(page.locator('.agg-split.has-chart')).toHaveCount(0);
});

test('an aggregate drops the histogram', async ({ page, request }) => {
  await resetScenario(request, 'corpus');

  await page.goto('/search?q=service%3Dnginx&page=0');
  await expect(page.locator(SEL.histoStrip)).toHaveCount(1);

  await page.goto(AGG_URL);
  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  await expect(page.locator(SEL.histoStrip)).toHaveCount(0);
});

test('the chart waits for the matching response while the group searches keep their own query', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);
  await expect(page.locator(SEL.catChart)).toHaveCount(1);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);

  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route('**/api/v1/query*', async (route) => {
    await held;
    await route.continue();
  });

  await page.locator(SEL.groupSearch).first().click();
  await expect(page).toHaveURL(/[?&]f=v1\./);

  await expect(page.locator(SEL.catChart)).toHaveCount(0);
  await expect(page.locator(`${SEL.exactTable} tbody tr`)).toHaveCount(GROUPS);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);

  release();
  await expect(page.locator(SEL.catChart)).toHaveCount(1);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);
});

test('a sorted table keeps its order until the next response lands', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const header = page.locator(`${SEL.exactTable} thead th`).nth(1);
  await header.locator('button').click();
  await expect(header).toHaveAttribute('aria-sort', 'descending');
  const labels = page.locator(`${SEL.exactTable} tbody tr td:first-child`);
  const sorted = await labels.allInnerTexts();

  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route('**/api/v1/query*', async (route) => { await held; await route.continue(); });

  await page.locator(SEL.groupSearch).first().click();
  await expect(page).toHaveURL(/[?&]f=v1\./);
  await expect(header).toHaveAttribute('aria-sort', 'descending');
  expect(await labels.allInnerTexts()).toEqual(sorted);

  release();
  await expect(header).not.toHaveAttribute('aria-sort', /.*/);
});

test('bar widths do not change with the page', async ({ page, request }) => {
  expect((await request.post('/__ctl/reset', { data: { scenario: 'aggregate', aggregate: { groups: WIDE_GROUPS } } })).ok()).toBe(true);
  await page.goto(WIDE_URL);

  const bars = page.locator(`${SEL.catChart} .cat-bars li`);
  await expect(bars).toHaveCount(50);

  const read = () => bars.evaluateAll((items) => items.map((li) => ({
    label: li.querySelector('.cat-lb')!.textContent ?? '',
    value: li.querySelector('.cat-val')!.textContent ?? '',
    width: li.querySelector('.cat-track i')?.getAttribute('style') ?? null,
  })));
  const onPageOne = await read();
  expect(onPageOne[0].width, 'the spiked row fills its track').toBe('width:100.00%');
  const first = onPageOne[1];
  expect(first.width).not.toBeNull();

  await page.getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/[?&]page=1/);
  await expect(bars).toHaveCount(WIDE_GROUPS - 50);

  const onPageTwo = await read();
  const twin = onPageTwo.find((bar) => bar.value === first.value);
  expect(twin, `no group on page 2 shares the count ${first.value}`).toBeDefined();
  expect(twin!.width).toBe(first.width);
  expect(onPageTwo.map((bar) => bar.width)).not.toContain('width:100.00%');

  await page.locator(`${SEL.exactTable} thead th`).first().locator('button').click();
  const labels = await page.locator(`${SEL.exactTable} tbody tr td:first-child`).allInnerTexts();
  const barLabels = (await read()).map((bar) => bar.label);
  expect(barLabels).toEqual(labels);
});
