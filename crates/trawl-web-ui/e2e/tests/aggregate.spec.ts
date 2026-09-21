// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The aggregate presentation on the Events tab: the exact numbers, and
// the bars beside them (ADR-0032, functional finding F02).
//
// THE PROPERTY UNDER TEST IS WHAT THE TABLE REFUSES TO OFFER. An
// aggregate row has no underlying event, so it has no expansion control;
// `count` is a number this query generated, so it has no Include. Both
// are absences, and an absence only counts as evidence when the positive
// case is asserted beside it — so every test that says "no control on
// the metric" also says "a control on the group", from the same page.
//
// The chart is aria-hidden on purpose: the table beside it already
// states every number in it. The assertions read the chart by class and
// the numbers off the table, which is the same split a screen reader
// gets.

import { test, expect, resetScenario } from '../fixtures';
import { COPY, SEL } from '../selectors';

// The generated `aggregate` scenario, for the one case that needs more
// groups than a page holds. `corpus` is pinned at five.
const WIDE_GROUPS = 60;
const WIDE_URL = '/search?q=' + encodeURIComponent('service=nginx | stats count() by status');

// `| stats count() by status` — the harness dispatches the `corpus`
// scenario on that pipeline shape and answers `wire/query-stats-by.json`:
// five groups, one negative count and one null one.
const AGG_URL = '/search?q=service%3Dnginx%20%7C%20stats%20count()%20by%20status';
const GROUPS = 5;

test('an aggregate answers with an exact table and no expansion column', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const table = page.locator(SEL.exactTable);
  await expect(table).toHaveCount(1);
  await expect(table.locator('tbody tr')).toHaveCount(GROUPS);

  // No event underneath a group total, so nothing claims to reveal one.
  await expect(page.locator(SEL.resultsExpandControl)).toHaveCount(0);
  await expect(page.locator('.results-table th.exp-col')).toHaveCount(0);

  // The two columns the query named, still sortable.
  const headers = table.locator('thead th.sortable');
  await expect(headers).toHaveCount(2);
  await expect(headers.nth(0)).toContainText('status');
  await expect(headers.nth(1)).toContainText('count');

  // The filter rail stays out of an aggregate page: its values describe
  // events, and these rows are not events.
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});

test('only the grouped column offers a search, never the generated metric', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const table = page.locator(SEL.exactTable);
  // Exactly one control per row — the `status` cell — and none anywhere
  // in the `count` column. That is F02: a filter on a generated metric
  // would name a field the corpus has never held.
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

  // The URL carries the filter payload and the scope strip says so; the
  // pipeline itself is untouched (ADR-0027).
  await expect(page).toHaveURL(/[?&]f=v1\./);
  await expect(page).toHaveURL(/stats/);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page.locator(SEL.filterChip)).toContainText('status = 200');
});

test('the categorical chart draws one bar per group beside the numbers', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);

  const chart = page.locator(SEL.catChart);
  await expect(chart).toHaveCount(1);
  await expect(chart.locator('.cat-title')).toHaveText('count by status');

  const bars = chart.locator('.cat-bars li');
  await expect(bars).toHaveCount(GROUPS);
  // The table is the accessible representation, so the bars are hidden
  // from it rather than read out twice.
  await expect(chart.locator('.cat-bars')).toHaveAttribute('aria-hidden', 'true');

  // One negative count makes the whole track two-sided, and that row is
  // the one drawn to the left of the midpoint.
  await expect(chart.locator('.cat-bars.signed')).toHaveCount(1);
  await expect(chart.locator('.cat-bars li.neg')).toHaveCount(1);
  await expect(chart.locator('.cat-bars li.neg .cat-val')).toHaveText('-1');

  // A null count is an absent measurement, not a zero-length bar: no
  // fill is drawn at all, and the value reads as a dash.
  const nulls = chart.locator('.cat-bars li.null');
  await expect(nulls).toHaveCount(1);
  await expect(nulls.locator('.cat-track i')).toHaveCount(0);
  await expect(nulls.locator('.cat-val')).toHaveText('—');

  // The largest value fills its track; every other bar is measured
  // against it.
  await expect(bars.nth(0).locator('.cat-val')).toHaveText('940');
  await expect(bars.nth(0).locator('.cat-track i')).toHaveAttribute('style', 'width:50.00%');
});

test('a non-chartable aggregate renders the exact table alone', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // `top` is an aggregation the categorical renderer cannot draw: it
  // names no grouped field, so there is no axis to label.
  await page.goto('/search?q=service%3Dnginx%20%7C%20top%2010%20host');

  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  await expect(page.locator(SEL.catChart)).toHaveCount(0);
  await expect(page.locator('.agg-split.has-chart')).toHaveCount(0);
});

test('an aggregate drops the histogram', async ({ page, request }) => {
  await resetScenario(request, 'corpus');

  // The positive case first, from the same corpus: raw results keep the
  // strip, so the absence below is the aggregate
  // arm's doing and not a broken histogram.
  await page.goto('/search?q=service%3Dnginx&page=0');
  await expect(page.locator(SEL.histoStrip)).toHaveCount(1);

  await page.goto(AGG_URL);
  await expect(page.locator(SEL.exactTable)).toHaveCount(1);
  // One group per row and no events underneath them: the strip could
  // only paint "No usable timestamps in shown events." over 64px.
  await expect(page.locator(SEL.histoStrip)).toHaveCount(0);
});

test('the chart waits for the matching response while the group searches keep their own query', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);
  await expect(page.locator(SEL.catChart)).toHaveCount(1);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);

  // Hold the next query open. The resource keeps the response already on
  // screen, so without a gate the chart would be redrawn from those rows
  // under the NEW executed query. The group controls need no gate: each
  // carries the query its row was executed for, so a press while the
  // next response is pending still files the filter against that query.
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route('**/api/v1/query*', async (route) => {
    await held;
    await route.continue();
  });

  await page.locator(SEL.groupSearch).first().click();
  await expect(page).toHaveURL(/[?&]f=v1\./);

  await expect(page.locator(SEL.catChart)).toHaveCount(0);
  // The numbers and their controls stay on screen — this is a gate on
  // the chart, not a blank page.
  await expect(page.locator(`${SEL.exactTable} tbody tr`)).toHaveCount(GROUPS);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);

  release();
  await expect(page.locator(SEL.catChart)).toHaveCount(1);
  await expect(page.locator(SEL.groupSearch)).toHaveCount(GROUPS);
});

test('bar widths do not change with the page', async ({ page, request }) => {
  // 60 groups: more than one page, so the same result is read in two
  // slices and a bar can be compared across them.
  expect((await request.post('/__ctl/reset', { data: { scenario: 'aggregate', aggregate: { groups: WIDE_GROUPS } } })).ok()).toBe(true);
  await page.goto(WIDE_URL);

  const bars = page.locator(`${SEL.catChart} .cat-bars li`);
  await expect(bars).toHaveCount(50);

  // A bar's width is its count measured against the largest count in
  // the WHOLE result, so two groups with the same count draw the same
  // width wherever they are paged to. A scale taken from the page would
  // move under this assertion.
  const read = () => bars.evaluateAll((items) => items.map((li) => ({
    label: li.querySelector('.cat-lb')!.textContent ?? '',
    value: li.querySelector('.cat-val')!.textContent ?? '',
    width: li.querySelector('.cat-track i')?.getAttribute('style') ?? null,
  })));
  // The generator spikes row 0 above every other count, so the result's
  // maximum is on page 1 and page 1 alone. That is what makes the two
  // scales give different answers below: with the counts merely cycling,
  // page 1, page 2 and the whole result all peak at the same 13 and a
  // page-scoped scale would draw exactly the widths a whole-result scale
  // does.
  const onPageOne = await read();
  expect(onPageOne[0].width, 'the spiked row fills its track').toBe('width:100.00%');
  // The reference bar is therefore row 1, not the spike: an ordinary
  // count, one that recurs on page 2.
  const first = onPageOne[1];
  expect(first.width).not.toBeNull();

  await page.getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/[?&]page=1/);
  await expect(bars).toHaveCount(WIDE_GROUPS - 50);

  const onPageTwo = await read();
  const twin = onPageTwo.find((bar) => bar.value === first.value);
  expect(twin, `no group on page 2 shares the count ${first.value}`).toBeDefined();
  expect(twin!.width).toBe(first.width);
  // And page 2's own largest count fills nothing: the scale it is drawn
  // against left the page with row 0.
  expect(onPageTwo.map((bar) => bar.width)).not.toContain('width:100.00%');

  // Sorting re-orders the whole result, so it changes WHICH groups this
  // page holds — and the table and the bars have to agree on the answer.
  await page.locator(`${SEL.exactTable} thead th`).first().locator('button').click();
  const labels = await page.locator(`${SEL.exactTable} tbody tr td:first-child`).allInnerTexts();
  const barLabels = (await read()).map((bar) => bar.label);
  expect(barLabels).toEqual(labels);
});
