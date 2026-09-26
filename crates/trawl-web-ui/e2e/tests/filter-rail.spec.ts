// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The filter rail at wide widths (issue #231, ADR-0044).
//
// At 900px and wider the rail opens for a countable page and closes to a
// 32px strip for every other settled answer, until the reader presses
// its <summary>; that hand choice holds for the browser session. Each
// test here reads the rail's `open` attribute and its measured width
// together: one without the other would pass a rail that is open but
// drawn as a strip, or the reverse.
//
// "Only a settled answer moves the rail" is checked two ways. Held
// responses (page.route, never a harness control) show the rail keeping
// its place while a request is in flight, and a `toggle` listener on the
// <details> counts every open or close, automatic or by hand, so a move
// that no single screenshot would catch still shows up as a count.

import fs from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';
import { COPY, SEL } from '../selectors';
import type { Page } from '@playwright/test';

const WIDE = { width: 1440, height: 900 };
/** fleet-ui's `--facets-w`, the open rail. */
const OPEN_WIDTH = 224;
/** trawl-web-ui's `--facet-strip-w`, the closed rail. */
const STRIP_WIDTH = 32;

/** A plain search: `corpus` answers it with 8 countable rows. */
const COUNTABLE = 'service=nginx';
const COUNTABLE_URL = '/search?q=service%3Dnginx';
/** An aggregation `corpus` answers with its `stats count() by` fixture. */
const AGGREGATE = 'service=nginx | stats count() by service';

// Issue 238's page cut down to the columns the rail may never count:
// the event instant, the ingest instant, the raw event and a sender
// timestamp that differs on every row. Derived from the wire fixture the
// native contract decodes, so the body stays a real response shape.
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

/** The rail's state, read the way a reader sees it: open or not, and
 * how wide it is drawn. Width within a pixel, per the acceptance test. */
async function expectRail(page: Page, open: boolean) {
  const rail = page.locator(SEL.filterRail);
  if (open) {
    await expect(rail).toHaveAttribute('open', '');
  } else {
    await expect(rail).not.toHaveAttribute('open');
  }
  const want = open ? OPEN_WIDTH : STRIP_WIDTH;
  await expect
    .poll(async () => Math.abs((await rail.boundingBox())!.width - want), {
      message: `the rail should be ${want}px wide`,
    })
    .toBeLessThanOrEqual(1);
}

/** Count every `toggle` the rail's <details> fires from here on, and
 * return a reader. The reader waits two frames and a task first: the
 * event is queued after the attribute changes, so a count read in the
 * same task as the change would miss it. */
async function watchToggles(page: Page): Promise<() => Promise<number>> {
  await page.locator(SEL.filterRail).evaluate((el) => {
    const w = window as unknown as { __railToggles: number };
    w.__railToggles = 0;
    el.addEventListener('toggle', () => {
      w.__railToggles += 1;
    });
  });
  return () =>
    page.evaluate(
      () =>
        new Promise<number>((resolve) =>
          requestAnimationFrame(() =>
            requestAnimationFrame(() =>
              setTimeout(() => resolve((window as unknown as { __railToggles: number }).__railToggles)),
            ),
          ),
        ),
    );
}

/** Run `query` from the console, as a reader would. */
async function haul(page: Page, query: string) {
  await page.locator(SEL.dslEditor).getByRole('textbox').fill(query);
  await page.locator(SEL.runButton).click();
}

/** Hold the next `/api/v1/query` until `release` is called. `arrived`
 * settles once the request is parked, so the page is provably pending. */
async function holdNextQuery(page: Page) {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let arrive!: () => void;
  const arrived = new Promise<void>((resolve) => {
    arrive = resolve;
  });
  await page.route(
    '**/api/v1/query',
    async (route) => {
      arrive();
      await gate;
      await route.continue();
    },
    { times: 1 },
  );
  return { arrived, release };
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
  // The strip is the summary, set vertically, and nothing else shows.
  await expect(page.locator(SEL.filterRailSummary)).toBeVisible();
  await expect(page.locator(SEL.filterRailSummary)).toContainText('Filters');
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

// The test the record-from-toggle mutation must fail. `toggle` fires for
// the automatic open as well as for a press, so a choice recorded from it
// turns this first automatic open into a hand choice that holds the rail
// open through the aggregation below.
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

  // Held on its way to an aggregation, which will close the rail once it
  // lands: until then the rail stays as the last answer left it.
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

  // And the other way: a countable answer, held, leaves the strip shut.
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

  // A same-query Haul re-sends the request on screen. Held, and after.
  held = await holdNextQuery(page);
  await page.locator(SEL.runButton).click();
  await held.arrived;
  await expectRail(page, true);
  held.release();
  await expectAccepted(page);
  await expectRail(page, true);

  // A switch between the Events and Visualization tabs sends nothing.
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByRole('tab', { name: 'Visualization' })).toHaveAttribute('aria-selected', 'true');
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.getByRole('tab', { name: /^Events/ })).toHaveAttribute('aria-selected', 'true');
  await expectRail(page, true);
  expect(await toggles()).toBe(2);
});

test('filter rail: a page turn does not move it', async ({ page, request }) => {
  // `pagination` pages a plain search 50 rows at a time over 53 rows,
  // every one of them countable.
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
  // The press cancels the native toggle and sets the choice, so a key
  // that the browser turns into a click still moves the rail once.
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

  // The press is presentation: it writes neither the link nor the
  // stored reading preferences.
  const url = page.url();
  const prefs = await page.evaluate(() => localStorage.getItem('trawl.ui'));
  await page.locator(SEL.filterRailSummary).click();
  await expectRail(page, false);
  expect(page.url()).toBe(url);
  expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBe(prefs);

  // A new countable answer would open it in automatic mode.
  const toggles = await watchToggles(page);
  await haul(page, 'service=nginx host=web-01');
  await expect(page).toHaveURL(/host%3Dweb-01/);
  await expectAccepted(page);
  await expectRail(page, false);
  expect(await toggles()).toBe(0);

  // Away to Health by the navigation, and Back to the same search.
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

  // A reload returns the rail to automatic behaviour.
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

  await haul(page, AGGREGATE);
  await expectAccepted(page);
  await expectRail(page, true);
  await expect(page.locator(SEL.facetHint)).toHaveText(COPY.railHintAggregate);
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
  // Suppression outranks the hint: nothing but the header row.
  await expect(page.locator(SEL.filterRailSummary)).toHaveText('Filters');
  await expect(page.locator(SEL.facetCount)).toHaveText('');
  await expect(page.locator(SEL.facetHint)).toHaveCount(0);
  await expect(page.locator(SEL.facetFilterInput)).toHaveCount(0);
  await expect(page.locator(SEL.facetClear)).toHaveCount(0);
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
});
