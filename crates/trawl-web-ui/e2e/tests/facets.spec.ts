// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The facet rail's controls (ADR-0029).

import fs from 'node:fs';
import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { expectRail, holdNextQuery, watchToggles } from '../filter-rail';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

const presentation = JSON.parse(
  fs.readFileSync('harness/wire/query-field-presentation.json', 'utf8'),
);
const PRESENTATION_URL = '/search?q=service%3Dapi&page=0';

// `| stats count() by status`: `corpus` answers it with
// `wire/query-stats-by.json`, five groups.
const AGG_URL = '/search?q=' + encodeURIComponent(`service=${CORPUS.service} | stats count() by status`);
const AGG_GROUPS = 5;
/** `host="web-01"`, one filter in the link's versioned payload. */
const FILTER = 'v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0';

type Loc = import('@playwright/test').Locator;
type Pg = import('@playwright/test').Page;

const FIELD = CORPUS.columns[1];
const VALUE = CORPUS.hosts[0];

function hostGroup(page: Pg): Loc {
  return page.locator(SEL.facetGroup).filter({
    has: page.getByRole('button', {
      name: nameFrom(COPY.facetGroupName, FIELD, String(CORPUS.hosts.length)),
    }),
  });
}

async function openCorpusSearch(page: Pg, request: import('@playwright/test').APIRequestContext) {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
}

test('include is reachable by Tab and adds one filter', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const group = hostGroup(page);
  await expect(group).toHaveCount(1);
  const row = group.locator(SEL.facetValue).first();
  const include = row.getByRole('button', {
    name: nameFrom(COPY.facetIncludeName, FIELD, VALUE),
  });
  await expect(include).toHaveJSProperty('tagName', 'BUTTON');
  await expect(include).toHaveAttribute('type', 'button');
  // Hidden at rest, and hidden by opacity: `display: none` would keep
  // the Tab below from ever reaching it. Read through a retrying
  // assertion, never a single `evaluate`: opacity is what the reveal
  // animates, so a one-shot read can catch it mid-flight.
  await expect(row.locator(SEL.facetActions)).toHaveCSS('opacity', '0');

  // The group's own header is the focusable before the first value.
  await group.locator(SEL.facetGroupHeader).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(include);
  await expect
    .poll(() => row.locator(SEL.facetActions).evaluate((el) => getComputedStyle(el).opacity))
    .toBe('1');

  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page).toHaveURL(/[?&]f=/);
  await expect(hostGroup(page).locator(SEL.facetValue).first()).toHaveClass(/selected/);
});

test('exclude on Space adds one filter', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const group = hostGroup(page);
  const row = group.locator(SEL.facetValue).first();
  const exclude = row.getByRole('button', {
    name: nameFrom(COPY.facetExcludeName, FIELD, VALUE),
  });
  await expect(exclude).toHaveJSProperty('tagName', 'BUTTON');

  await group.locator(SEL.facetGroupHeader).focus();
  await page.keyboard.press('Tab');
  await page.keyboard.press('Tab');
  await expectFocusRing(exclude);

  await page.keyboard.press(' ');

  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(hostGroup(page).locator(SEL.facetValue).first()).toHaveClass(/excluded/);
});

test('group header toggles aria-expanded', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  // That is `host`: the corpus's first column,
  // `_time`, is an event instant and never earns a group.
  const group = page.locator(SEL.facetGroup).first();
  const header = group.locator(SEL.facetGroupHeader);
  await expect(header).toHaveJSProperty('tagName', 'BUTTON');
  await expect(header).toHaveAttribute('type', 'button');
  await expect(header).toHaveAccessibleName(
    nameFrom(COPY.facetGroupName, FIELD, String(CORPUS.hosts.length)),
  );
  await expect(header).toHaveAttribute('aria-expanded', 'true');

  await page.locator(SEL.facetFilterInput).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(header);

  await page.keyboard.press('Enter');
  await expect(header).toHaveAttribute('aria-expanded', 'false');
  await expect(group.locator(SEL.facetValue).first()).toBeHidden();

  await page.keyboard.press(' ');
  await expect(header).toHaveAttribute('aria-expanded', 'true');
  await expect(group.locator(SEL.facetValue).first()).toBeVisible();
});

test('show more reveals the hidden values', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const group = hostGroup(page);
  const shown = CORPUS.hosts.length - CORPUS.hostsHidden;
  await expect(group.locator(SEL.facetValue)).toHaveCount(shown);
  const more = group.locator(SEL.facetMore);
  await expect(more).toHaveJSProperty('tagName', 'BUTTON');
  await expect(more).toHaveAccessibleName(
    nameFrom(COPY.facetMoreName, String(CORPUS.hostsHidden), CORPUS.hostsHidden === 1 ? 'value' : 'values', FIELD),
  );

  await group.locator(SEL.facetOp).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(more);

  await page.keyboard.press('Enter');

  await expect(group.locator(SEL.facetValue)).toHaveCount(CORPUS.hosts.length);
  await expect(group.locator(SEL.facetValueName).last()).toHaveText(
    CORPUS.hosts[CORPUS.hosts.length - 1],
  );
  await expect(more).toHaveCount(0);
});

// The rail lists up to 10 values per field (`FACET_TOP_N`).
const LIST_CAP_LINE = 'Lists show up to 10 values per field';

test('the scope names the list cap only while a field holds more values than it lists', async ({ page, request }) => {
  // No corpus field reaches 10 distinct values, so the line stays away.
  await openCorpusSearch(page, request);
  await expect(page.locator('.facets-scope').first()).toBeVisible();
  await expect(page.getByText(LIST_CAP_LINE, { exact: true })).toHaveCount(0);

  const hostAt = presentation.columns.findIndex((c: { name: string }) => c.name === 'host');
  const rows = Array.from({ length: 12 }, (_, i) => {
    const row = [...presentation.rows[i % presentation.rows.length]];
    row[hostAt] = `host-${String(i + 1).padStart(2, '0')}`;
    return row;
  });
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: { ...presentation, rows } }));
  await page.goto(PRESENTATION_URL);
  const line = page.getByText(LIST_CAP_LINE, { exact: true });
  await expect(line).toHaveCount(1);
  await expect(line).toHaveClass('facets-scope');

  const group = page.locator(SEL.facetGroup).filter({
    has: page.getByRole('button', { name: nameFrom(COPY.facetGroupName, 'host', '12') }),
  });
  await group.locator(SEL.facetMore).click();
  await expect(group.locator(SEL.facetValue)).toHaveCount(10);
});

test('long value keeps clear of the actions', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const group = hostGroup(page);
  const row = group.locator(SEL.facetValue).first();
  // Written into the DOM rather than the fixture: a
  // 200-character hostname in the corpus would change every other
  // spec's rows.
  await row.locator(SEL.facetValueName).evaluate((el) => {
    el.textContent = 'x'.repeat(200);
  });

  await group.locator(SEL.facetGroupHeader).focus();
  await page.keyboard.press('Tab');
  await expect(row.locator(SEL.facetOp).first()).toBeFocused();

  // Every read here retries. The reveal is a style change the browser
  // may animate, and the row reflows around the 200 characters just
  // written into it, so a single snapshot taken the instant focus lands
  // can catch either one part way.
  const act = row.locator(SEL.facetActions);
  await expect(act).toHaveCSS('opacity', '1');
  await expect(act).not.toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');

  const gap = () =>
    row.evaluate((el) => {
      const name = el.querySelector('.n')!.getBoundingClientRect();
      const actions = el.querySelector('.act')!.getBoundingClientRect();
      return actions.left - name.right;
    });
  await expect
    .poll(gap, {
      message: 'a long value must stop before the action area, not run under it',
    })
    .toBeGreaterThan(0);
});

test('clear all removes every filter', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const addInclude = async (value: string) => {
    await hostGroup(page)
      .getByRole('button', { name: nameFrom(COPY.facetIncludeName, FIELD, value) })
      .click();
  };
  await addInclude(CORPUS.hosts[0]);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await addInclude(CORPUS.hosts[1]);
  await expect(page.locator(SEL.filterChip)).toHaveCount(2);

  const clear = page.locator(SEL.facetClear);
  await expect(clear).toHaveJSProperty('tagName', 'BUTTON');
  await expect(clear).toHaveAttribute('type', 'button');
  // Reached BACKWARDS from the rail's filter box. The rail's tab order
  // is its <summary>, then this control, then the filter box, so one
  // step back from the box is Clear all and one more is the summary,
  // whose predecessor is page chrome.
  await page.locator(SEL.facetFilterInput).focus();
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(clear);
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(page.locator(SEL.filterRailSummary));
  await page.keyboard.press('Tab');
  await expectFocusRing(clear);

  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
  await expect(clear).toHaveCount(0);

  await addInclude(CORPUS.hosts[0]);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await page.locator(SEL.facetFilterInput).focus();
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(page.locator(SEL.facetClear));

  await page.keyboard.press(' ');
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});

test('Enter and Space on the rail summary open and close the rail', async ({ page, request }) => {
  await openCorpusSearch(page, request);
  await expectRail(page, true);
  const toggles = await watchToggles(page);
  const summary = page.locator(SEL.filterRailSummary);
  await summary.focus();

  // Each press waits for its own toggle before the next key. The browser
  // fires `toggle` from a queued task, and a second change made before
  // that task runs folds both into one event, so two quick presses can
  // count as one.
  let pressed = 0;
  for (const key of ['Enter', ' ']) {
    for (const open of [false, true]) {
      await page.keyboard.press(key);
      await expectRail(page, open);
      pressed += 1;
      await expect.poll(toggles).toBe(pressed);
    }
  }
  expect(await toggles()).toBe(4);
  await expectFocusRing(summary);
});

test('an aggregation that closes the rail moves focus from a group header to the summary', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // The aggregation first, so Back can run it again from the countable
  // search without anything taking focus from the rail.
  await page.goto(AGG_URL);
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await page.locator(SEL.cmContent).fill(`service=${CORPUS.service}`);
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
  await expectRail(page, true);

  const header = page.locator(SEL.facetGroup).first().locator(SEL.facetGroupHeader);
  await header.focus();
  await page.goBack();
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await expect(page.locator(SEL.filterRailSummary)).toBeFocused();
});

test('an aggregation that closes the rail moves focus from Clear all to the summary', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`${AGG_URL}&f=${FILTER}`);
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await page.locator(SEL.cmContent).fill(`service=${CORPUS.service}`);
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
  await expectRail(page, true);

  await page.locator(SEL.facetClear).focus();
  await page.goBack();
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await expect(page.locator(SEL.filterRailContent)).toHaveAttribute('inert', '');
  await expect(page.locator(SEL.filterRailSummary)).toBeFocused();
});

// The top bar's page title takes no focus and sits
// outside `<main>`, so the press leaves `body` active.
test('an aggregation that closes the rail leaves focus alone after a press outside the rail', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(AGG_URL);
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await page.locator(SEL.cmContent).fill(`service=${CORPUS.service}`);
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.facetGroup).first()).toBeVisible();
  await expectRail(page, true);

  const bodyActive = () => page.evaluate(() => document.activeElement === document.body);
  await page.locator(SEL.facetGroup).first().locator(SEL.facetGroupHeader).focus();
  const held = await holdNextQuery(page);
  await page.goBack();
  await held.arrived;
  await expect(page.locator(SEL.facetGroup)).toHaveCount(0);
  expect(await bodyActive()).toBe(true);
  await page.locator(SEL.topbarCrumb).click();
  expect(await bodyActive()).toBe(true);

  held.release();
  await expect(page.locator(SEL.exactTable).locator('tbody tr')).toHaveCount(AGG_GROUPS);
  await expectRail(page, false);
  await expect(page.locator(SEL.filterRailSummary)).not.toBeFocused();
  expect(await bodyActive()).toBe(true);
});

for (const [name, url] of [
  ['idle', '/search'],
  ['an aggregation carrying a filter', `${AGG_URL}&f=${FILTER}`],
] as const) {
  test(`a collapsed rail is one tab stop between the skip links and the console: ${name}`, async ({ page, request }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await resetScenario(request, 'corpus');
    await page.goto(url);
    await expect(page.locator(SEL.cmContent)).toBeVisible();
    await expectRail(page, false);
    await page.keyboard.press('Tab');
    await expect(page.getByRole('link', { name: 'Skip to main content', exact: true })).toBeFocused();
    await page.keyboard.press('Enter');
    await expect(page.locator('#fleet-main-content')).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('link', { name: 'Skip to query editor', exact: true })).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(page.getByRole('link', { name: 'Skip to results', exact: true })).toBeFocused();
    await page.keyboard.press('Tab');
    await expectFocusRing(page.locator(SEL.filterRailSummary));
    await page.keyboard.press('Tab');
    await expect(page.locator(SEL.cmContent)).toBeFocused();
  });
}

async function railFields(page: Pg): Promise<string[]> {
  const names = await page
    .locator(`${SEL.facetGroup} ${SEL.facetGroupHeader}`)
    .evaluateAll((els) => els.map((el) => el.getAttribute('aria-label') ?? ''));
  return names.map((name) => name.replace(/, \d+ values?$/, ''));
}

test('the rail counts dimensions, not instants, the raw event or one-off times', async ({ page }) => {
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: presentation }));
  await page.goto(PRESENTATION_URL);

  await expect(
    page.getByRole('button', { name: nameFrom(COPY.facetGroupName, 'level', '3') }),
  ).toBeVisible();
  const fields = await railFields(page);
  expect(fields).toContain('level');
  expect(fields).toContain('host');
  for (const field of ['_time', '_ingested', '_raw', 'timestamp']) {
    expect(fields, `${field} must not earn a group`).not.toContain(field);
  }
});

test('filters on a field the rail no longer counts stay visible and removable', async ({ page }) => {
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: presentation }));
  await page.goto(PRESENTATION_URL);
  await expect(page.locator(SEL.resultsRow)).toHaveCount(presentation.rows.length);
  const ts = presentation.columns.findIndex((c: { name: string }) => c.name === 'timestamp');
  const first = presentation.rows[0][ts] as string;
  const second = presentation.rows[1][ts] as string;

  // The exclude from the inspector, which is the only
  // view that offers one.
  await page.locator(SEL.resultsExpandControl).nth(0).click();
  await page
    .locator(SEL.resultsDetailCell)
    .getByRole('button', { name: `Include timestamp = ${first}`, exact: true })
    .click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);

  await page.locator(SEL.viewControl).click();
  await page.locator(SEL.viewPanel).getByRole('button', { name: 'Inspector', exact: true }).click();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.viewPanel)).toHaveCount(0);
  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await page
    .locator(SEL.inspector)
    .getByRole('button', { name: `Exclude timestamp = ${second}`, exact: true })
    .click();

  const chips = page.locator(SEL.filterChip);
  await expect(chips).toHaveCount(2);
  await expect(chips.filter({ hasText: `timestamp = ${first}` })).not.toHaveClass(/excl/);
  await expect(chips.filter({ hasText: `timestamp ≠ ${second}` })).toHaveClass(/excl/);
  await expect(
    page.getByRole('button', { name: nameFrom(COPY.facetGroupName, 'level', '3') }),
  ).toBeVisible();
  expect(await railFields(page)).not.toContain('timestamp');

  await page.getByRole('button', { name: `Remove filter timestamp = ${second}`, exact: true }).click();
  await expect(chips).toHaveCount(1);
  await expect(chips).toContainText(`timestamp = ${first}`);
  await page.getByRole('button', { name: `Remove filter timestamp = ${first}`, exact: true }).click();
  await expect(chips).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});
