// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The filter rail at wide widths (ADR-0044).

import fs from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';
import { expectRail, expectTextShown, holdNextQuery, watchToggles } from '../filter-rail';
import { COPY, SEL } from '../selectors';
import type { Page } from '@playwright/test';

const WIDE = { width: 1440, height: 900 };

/** A plain search: `corpus` answers it with 8 countable rows. */
const COUNTABLE = 'service=nginx';
const COUNTABLE_URL = '/search?q=service%3Dnginx';
const AGGREGATE = 'service=nginx | stats count() by service';

const presentation = JSON.parse(
  fs.readFileSync('harness/wire/query-field-presentation.json', 'utf8'),
);
const healthQueries = JSON.parse(fs.readFileSync('harness/wire/health-queries.json', 'utf8'));
const UNCOUNTABLE_COLUMNS = ['_time', 'timestamp', '_raw', '_ingested'];
const uncountable = (() => {
  const keep = presentation.columns
    .map((c: { name: string }, i: number) => (UNCOUNTABLE_COLUMNS.includes(c.name) ? i : -1))
    .filter((i: number) => i >= 0);
  return {
    ...presentation,
    columns: keep.map((i: number) => presentation.columns[i]),
    rows: presentation.rows.map((row: unknown[]) => keep.map((i: number) => row[i])),
  };
})();

async function haul(page: Page, query: string) {
  await page.locator(SEL.dslEditor).getByRole('textbox').fill(query);
  await page.locator(SEL.runButton).click();
}

/** A settled snapshot answer: the scope strip shows execution facts
 * for an accepted response only, never while a request is pending. */
async function expectAccepted(page: Page) {
  await expect(page.locator(SEL.scopeExecution)).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  await page.setViewportSize(WIDE);
});

test('filter rail: idle /search is a closed strip', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await expect(page.locator('.search-quick-start')).toBeVisible();
  await expectRail(page, false);
  await expectTextShown(page.locator(SEL.filterRailSummary), 'Filters');
  await expect(page.locator(SEL.facetFilterInput)).toBeHidden();
});

// Each non-countable answer arrives AFTER a countable page opened the
// rail, so a pass means the answer closed it, not that it never opened.
const NOT_COUNTABLE: Array<{
  name: string;
  arrange: (page: Page, request: import('@playwright/test').APIRequestContext) => Promise<void>;
  query: string;
  settled: (page: Page) => Promise<void>;
}> = [
  {
    name: 'a zero-row query',
    arrange: async (_page, request) => resetScenario(request, 'default'),
    query: 'service=nowhere',
    settled: expectAccepted,
  },
  {
    name: 'a page of only reserved and one-off time columns',
    arrange: async (page) => {
      await page.route('**/api/v1/query', (route) => route.fulfill({ json: uncountable }));
    },
    query: 'service=api',
    settled: async (page) => {
      await expect(page.locator(SEL.resultsRow)).toHaveCount(uncountable.rows.length);
      await expectAccepted(page);
    },
  },
  {
    name: 'a failed query',
    arrange: async (_page, request) => resetScenario(request, 'query-500'),
    query: 'service=broken',
    settled: async (page) => {
      await expect(page.getByText(COPY.loadHintErrorPrefix)).toBeVisible();
    },
  },
  {
    name: 'an aggregation',
    arrange: async () => {},
    query: AGGREGATE,
    settled: expectAccepted,
  },
];

for (const { name, arrange, query, settled } of NOT_COUNTABLE) {
  test(`filter rail: ${name} closes the rail to the strip`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.goto(COUNTABLE_URL);
    await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
    await expectRail(page, true);
    await arrange(page, request);
    await haul(page, query);
    await settled(page);
    await expectRail(page, false);
  });
}

test('filter rail: a countable page opens it, a following aggregation closes it', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(COUNTABLE_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
  await haul(page, AGGREGATE);
  await expectAccepted(page);
  await expectRail(page, false);
});

// `toggle` fires for
// the automatic open as well as for a press.
test('filter rail: an automatic open is not a hand choice', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await expectRail(page, false);
  const toggles = await watchToggles(page);
  await haul(page, COUNTABLE);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  expect(await toggles()).toBe(1);
  await haul(page, AGGREGATE);
  await expectAccepted(page);
  await expectRail(page, false);
  expect(await toggles()).toBe(2);
});

test('filter rail: a pending answer does not move it', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(COUNTABLE_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  const toggles = await watchToggles(page);

  let held = await holdNextQuery(page);
  await haul(page, AGGREGATE);
  await held.arrived;
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expectRail(page, true);
  expect(await toggles()).toBe(0);
  held.release();
  await expectAccepted(page);
  await expectRail(page, false);
  expect(await toggles()).toBe(1);

  held = await holdNextQuery(page);
  await haul(page, COUNTABLE);
  await held.arrived;
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expectRail(page, false);
  expect(await toggles()).toBe(1);
  held.release();
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  expect(await toggles()).toBe(2);

  held = await holdNextQuery(page);
  await page.locator(SEL.runButton).click();
  await held.arrived;
  await expectRail(page, true);
  held.release();
  await expectAccepted(page);
  await expectRail(page, true);

  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByRole('tab', { name: 'Visualization' })).toHaveAttribute('aria-selected', 'true');
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.getByRole('tab', { name: /^Events/ })).toHaveAttribute('aria-selected', 'true');
  await expectRail(page, true);
  expect(await toggles()).toBe(2);
});

test('filter rail: a page turn does not move it', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto(COUNTABLE_URL);
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator(SEL.resultsSummary)).toHaveText('Page 1 · showing 50 rows');
  await expectRail(page, true);
  const toggles = await watchToggles(page);
  const held = await holdNextQuery(page);
  await footer.getByRole('button', { name: 'Next' }).click();
  await held.arrived;
  await expectRail(page, true);
  held.release();
  await expect(footer.locator(SEL.resultsSummary)).toHaveText('Page 2 · showing 3 rows');
  await expectRail(page, true);
  expect(await toggles()).toBe(0);
});

test('filter rail: Enter and Space on the summary toggle it once each', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(COUNTABLE_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  const toggles = await watchToggles(page);
  const summary = page.locator(SEL.filterRailSummary);
  await summary.focus();
  await page.keyboard.press('Enter');
  await expectRail(page, false);
  expect(await toggles()).toBe(1);
  await page.keyboard.press(' ');
  await expectRail(page, true);
  expect(await toggles()).toBe(2);
  await expect(summary).toBeFocused();
});

test('filter rail: a hand close holds through a new Haul and a visit to Health, until a reload', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(COUNTABLE_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);

  const url = page.url();
  const prefs = await page.evaluate(() => localStorage.getItem('trawl.ui'));
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, false);
  expect(page.url()).toBe(url);
  expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBe(prefs);

  const toggles = await watchToggles(page);
  await haul(page, 'service=nginx host=web-01');
  await expect(page).toHaveURL(/host%3Dweb-01/);
  await expectAccepted(page);
  await expectRail(page, false);
  expect(await toggles()).toBe(0);

  // `corpus` does not serve Health's running-queries read, so it gets
  // the body the health scenarios serve.
  await page.route('**/api/v1/queries', (route) => route.fulfill({ json: healthQueries }));
  await page.locator(SEL.railHealthLink).click();
  await expect(page.locator(SEL.healthPage)).toBeVisible();
  await page.goBack();
  await expect(page).toHaveURL(/host%3Dweb-01/);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectAccepted(page);
  await expectRail(page, false);

  await page.reload();
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
});

test('filter rail: a hand open on idle holds through an aggregation and says why it is empty', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await expect(page.locator('.search-quick-start')).toBeVisible();
  await expectRail(page, false);
  const url = page.url();
  const prefs = await page.evaluate(() => localStorage.getItem('trawl.ui'));
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, true);
  expect(page.url()).toBe(url);
  expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBe(prefs);
  await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintNothingToCount);
  await expectTextShown(page.locator(SEL.facetHint), COPY.railHintNothingToCount);

  await haul(page, AGGREGATE);
  await expectAccepted(page);
  await expectRail(page, true);
  await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintAggregate);
  await expectTextShown(page.locator(SEL.facetHint), COPY.railHintAggregate);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});

test('filter rail: a hand-opened rail on a zero-row page says there is nothing to count', async ({ page }) => {
  // `default` answers every query with no rows.
  await page.goto(COUNTABLE_URL);
  await expectAccepted(page);
  await expectRail(page, false);
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, true);
  await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintNothingToCount);
  await expectTextShown(page.locator(SEL.facetHint), COPY.railHintNothingToCount);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});

test('filter rail: a malformed link leaves the strip, and an open rail shows its header only', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&page=0&f=v1.!');
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await expectRail(page, false);
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, true);
  await expect(page.locator(SEL.filterRailSummary)).toHaveText('Filters');
  await expectTextShown(page.locator(SEL.filterRailSummary), 'Filters');
  await expect(page.locator(SEL.facetCount)).toHaveText('');
  await expect(page.locator(SEL.facetHint)).toHaveCount(0);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.facetClear)).toHaveCount(0);
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});

// Twelve filters give the
// count two digits, as wide as it gets under the link's cap of 32.
test('filter rail: Clear all stands apart from a two-digit active count, inside the rail', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const filters = Array.from({ length: 12 }, (_, i) => ({
    op: '+',
    field: 'host',
    value: `web-${String(i + 1).padStart(2, '0')}`,
  }));
  const f = 'v1.' + Buffer.from(JSON.stringify(filters)).toString('base64url');
  await page.goto(`${COUNTABLE_URL}&f=${f}`);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  await expectTextShown(page.locator(SEL.facetCount), '12 active');
  await expect(page.locator(SEL.facetClear)).toBeVisible();

  const box = async (selector: string) => (await page.locator(selector).boundingBox())!;
  type Box = { x: number; y: number; width: number; height: number };
  const inside = (inner: Box, outer: Box) =>
    inner.x >= outer.x &&
    inner.y >= outer.y &&
    inner.x + inner.width <= outer.x + outer.width &&
    inner.y + inner.height <= outer.y + outer.height;
  const overlap = (a: Box, b: Box) =>
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
  await expect
    .poll(async () => {
      const [rail, count, clear] = await Promise.all([
        box(SEL.filterRail),
        box(SEL.facetCount),
        box(SEL.facetClear),
      ]);
      return {
        countInside: inside(count, rail),
        clearInside: inside(clear, rail),
        overlap: overlap(count, clear),
      };
    })
    .toEqual({ countInside: true, clearInside: true, overlap: false });
});

test('filter rail: a closed wide rail is inert, an open one and the narrow disclosure are not', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await expectRail(page, false);
  const content = page.locator(SEL.filterRailContent);
  await expect(content).toHaveAttribute('inert', '');
  await haul(page, COUNTABLE);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  await expect(content).not.toHaveAttribute('inert');
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, false);
  await expect(content).toHaveAttribute('inert', '');
  await page.setViewportSize({ width: 720, height: 900 });
  await expect(content).not.toHaveAttribute('inert');
  await expect(page.locator(SEL.filterRail)).not.toHaveAttribute('open');
});

// The browser opens a closed <details> by itself to show a match inside
// it: find-in-page does, and so does a link's text fragment, which is
// the one a test can drive.
test('filter rail: a link cannot open a closed wide rail, and the narrow disclosure follows one it opens', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(COUNTABLE_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  const rail = page.locator(SEL.filterRail);
  const summary = page.locator(SEL.filterRailSummary);
  await summary.click();
  await expectRail(page, false);

  // "Show 1 more listed value" is the host group's control; nothing
  // outside the rail says it. Chromium drops the directive from the URL
  // once it has read it, so the same link can be followed twice.
  const followLink = () => page.goto(page.url() + '#:~:text=' + encodeURIComponent('Show 1 more listed value'));
  const toggles = await watchToggles(page);
  await followLink();
  expect(await toggles()).toBe(0);
  await expectRail(page, false);
  await summary.click();
  await expectRail(page, true);

  await page.setViewportSize({ width: 720, height: 900 });
  await expect(rail).not.toHaveAttribute('open');
  const closed = await toggles();
  await followLink();
  await expect.poll(toggles).toBe(closed + 1);
  await expect(rail).toHaveAttribute('open', '');
  await summary.click();
  await expect(rail).not.toHaveAttribute('open');
});
