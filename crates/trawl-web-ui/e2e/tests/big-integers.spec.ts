// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Exact big integers in the browser (issue #214, ADR-0035 amended
// 2026-10-22).
//
// An `integer` column travels as decimal strings, so a value past 2^53
// reaches the SPA with every digit. These specs read that value back out
// of every surface that classifies a number: the grid's text, its sort,
// the filter rail's values and the filter they build, the exact table,
// and both charts. A JavaScript `Number` would spell 2^53 + 1 as
// 9007199254740992 and u64::MAX as 18446744073709552000, so each
// assertion below names the true digits.
//
// The bodies are `harness/wire/query-big-integers*.json`, written by the
// result wire's own encoder in tests/e2e_big_integer_fixtures.rs; this
// file reads them as text and never parses a cell into a number.

import fs from 'node:fs';
import { test, expect } from '../fixtures';
import { COPY, SEL, nameFrom } from '../selectors';

type Pg = import('@playwright/test').Page;

const body = (name: string) => fs.readFileSync(`harness/wire/${name}.json`, 'utf8');
const ROWS = body('query-big-integers');
const STATS_BY = body('query-big-integers-stats-by');
const TIMECHART = body('query-big-integers-timechart');

const ABOVE_2_53 = '9007199254740993';
const I64_MAX = '9223372036854775807';
const TWO_63 = '9223372036854775808';
const U64_MAX = '18446744073709551615';
const NEGATIVE = '-9007199254740993';

/** The `bytes` cells in ascending order, null first, as the grid's sort
 * puts them: numeric, so 2^53 + 1 sorts before 2^63 and u64::MAX last. A
 * text sort would put `18…` before `9…`. */
const ASCENDING = ['NULL', NEGATIVE, ABOVE_2_53, I64_MAX, TWO_63, U64_MAX];

/** Answer every query by shape, and keep the DSL each one asked. */
async function serve(page: Pg): Promise<string[]> {
  const asked: string[] = [];
  await page.route('**/api/v1/query', (route) => {
    const dsl: string = route.request().postDataJSON().query ?? '';
    asked.push(dsl);
    const text = dsl.includes('| timechart') ? TIMECHART : dsl.includes('| stats') ? STATS_BY : ROWS;
    return route.fulfill({ contentType: 'application/json', body: text });
  });
  return asked;
}

/** The `bytes` column's cells as the grid prints them, top to bottom. The
 * first cell of a row is the expander, so `bytes` is the fourth. */
const bytesCells = (page: Pg) => page.locator(`${SEL.resultsRow} > td:nth-child(4)`).allTextContents();

test('the grid prints every digit and sorts the column as numbers', async ({ page }, testInfo) => {
  await serve(page);
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(6);

  // Wire order, every digit.
  expect(await bytesCells(page)).toEqual([U64_MAX, ABOVE_2_53, 'NULL', TWO_63, NEGATIVE, I64_MAX]);
  // The `_time` column is `timestamp`-typed, so it prints its short form.
  await expect(page.locator(SEL.resultsRow).first()).toContainText('2026-09-01 10:00:00');

  const sort = page.getByRole('button', { name: nameFrom(COPY.resultsSortName, 'bytes'), exact: true });
  await sort.click();
  await expect.poll(() => bytesCells(page)).toEqual([...ASCENDING].reverse());
  await sort.click();
  await expect.poll(() => bytesCells(page)).toEqual(ASCENDING);

  // The capture the PR cites: the grid, sorted ascending.
  await page.locator('#search-results').screenshot({ path: testInfo.outputPath('big-integers-grid.png') });
});

test('the filter rail counts the column and filters on its exact digits', async ({ page }) => {
  const asked = await serve(page);
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(6);

  // Five values, each one a facet value spelled with all its digits:
  // the rail keys an integer by its text, never by a rounded number.
  const group = page.locator(SEL.facetGroup).filter({
    has: page.getByRole('button', { name: nameFrom(COPY.facetGroupName, 'bytes', '5'), exact: true }),
  });
  await expect(group).toHaveCount(1);
  for (const value of [NEGATIVE, ABOVE_2_53, I64_MAX, TWO_63, U64_MAX]) {
    await expect(group.getByRole('button', { name: nameFrom(COPY.facetIncludeName, 'bytes', value), exact: true }))
      .toHaveCount(1);
  }

  const before = asked.length;
  await group.getByRole('button', { name: nameFrom(COPY.facetIncludeName, 'bytes', ABOVE_2_53), exact: true }).click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page.locator(SEL.filterChip)).toContainText(ABOVE_2_53);
  await expect.poll(() => asked.length).toBeGreaterThan(before);
  expect(asked.at(-1)).toContain(ABOVE_2_53);
  expect(asked.at(-1)).not.toContain('9007199254740992');

  const afterInclude = asked.length;
  await group.getByRole('button', { name: nameFrom(COPY.facetExcludeName, 'bytes', U64_MAX), exact: true }).click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(2);
  await expect(page.locator(SEL.filterChip).nth(1)).toContainText(U64_MAX);
  await expect.poll(() => asked.length).toBeGreaterThan(afterInclude);
  expect(asked.at(-1)).toContain(U64_MAX);
  expect(asked.at(-1)).not.toContain('18446744073709552000');
});

test('a stats by result keeps exact counts in its table and draws them to scale', async ({ page }) => {
  await serve(page);
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | stats count() by host'));

  const table = page.locator(SEL.exactTable);
  await expect(table.locator('tbody tr')).toHaveCount(4);
  expect(await table.locator('tbody tr > td:nth-child(2)').allTextContents())
    .toEqual([U64_MAX, ABOVE_2_53, TWO_63, I64_MAX]);

  // The bars beside the table: each value printed whole, and each width
  // measured against u64::MAX, so 2^63 is exactly half of it.
  const bars = page.locator(`${SEL.catChart} .cat-bars li`);
  await expect(bars).toHaveCount(4);
  const drawn = await bars.evaluateAll((items) => Object.fromEntries(items.map((li) => [
    li.querySelector('.cat-lb')!.textContent,
    [li.querySelector('.cat-val')!.textContent, li.querySelector('.cat-track i')?.getAttribute('style')],
  ])));
  expect(drawn['web-01']).toEqual([U64_MAX, 'width:100.00%']);
  expect(drawn['web-03']).toEqual([TWO_63, 'width:50.00%']);
  expect(drawn['web-04']).toEqual([I64_MAX, 'width:50.00%']);
  expect(drawn['web-02']![0]).toBe(ABOVE_2_53);

  // The Visualization tab classifies the same column as a numeric metric.
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  const host = page.locator(SEL.chartHost);
  await expect(host).toHaveAttribute('data-chart-type', 'column');
  await expect(host).toHaveAttribute('data-points', '4');
  await expect(host).toHaveAttribute('data-series', '1');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
});

test('a timechart of big counts draws a line and keeps its exact table', async ({ page }) => {
  await serve(page);
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1h count()'));

  // The exact table: bucket instants in their short form, counts whole.
  const table = page.locator(SEL.exactTable);
  await expect(table.locator('tbody tr')).toHaveCount(4);
  expect(await table.locator('tbody tr > td:nth-child(1)').allTextContents()).toEqual([
    '2026-09-01 00:00:00', '2026-09-01 01:00:00', '2026-09-01 02:00:00', '2026-09-01 03:00:00',
  ]);
  expect(await table.locator('tbody tr > td:nth-child(2)').allTextContents())
    .toEqual([ABOVE_2_53, I64_MAX, TWO_63, U64_MAX]);

  // The series chart takes the column as its metric: one line over the
  // four buckets, not a refusal for a non-numeric metric.
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  const host = page.locator(SEL.chartHost);
  await expect(host).toHaveAttribute('data-chart-type', 'line');
  await expect(host).toHaveAttribute('data-points', '4');
  await expect(host).toHaveAttribute('data-series', '1');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator('.visualization .results-empty')).toHaveCount(0);
});
