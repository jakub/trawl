// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The facet rail's controls (issue #161, ADR-0029).
//
// Include and exclude were spans inside a block the stylesheet hid with
// `display: none` until the row was hovered, which is the one way of
// hiding something that also takes it out of the tab order. They are
// buttons now, the action area is hidden with `opacity` instead, and the
// row reveals it on `:focus-within` as well as on hover. So the first
// two tests below only mean something because they arrive by Tab.
//
// The action area is also absolutely positioned at the row's right edge
// with an opaque background, which is what keeps a long value from
// reading through the two glyphs. The last test is that geometry, since
// nothing else in the suite would notice it going.

import fs from 'node:fs';
import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

// Issue 238's page: one-off `_time`, `_ingested` and sender `timestamp`
// columns, the raw event, a repeating `level` and a distinct `host`.
// Pinned in tests/e2e_wire_fixture_contract.rs and served to these specs
// alone, so the corpus every other spec counts stays as it is.
const presentation = JSON.parse(
  fs.readFileSync('harness/wire/query-field-presentation.json', 'utf8'),
);
const PRESENTATION_URL = '/search?q=service%3Dapi&page=0';

type Loc = import('@playwright/test').Locator;
type Pg = import('@playwright/test').Page;

const FIELD = CORPUS.columns[1];
const VALUE = CORPUS.hosts[0];

/** The `host` group, found by its header's own accessible name. */
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

  // One press, one filter. The chip is the count, and the URL carries
  // the filter it names.
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

  // Two stops past the header: include, then exclude.
  await group.locator(SEL.facetGroupHeader).focus();
  await page.keyboard.press('Tab');
  await page.keyboard.press('Tab');
  await expectFocusRing(exclude);

  await page.keyboard.press(' ');

  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  // Excluded rather than included: the two buttons sit next to each
  // other and a count alone could not tell them apart.
  await expect(hostGroup(page).locator(SEL.facetValue).first()).toHaveClass(/excluded/);
});

test('group header toggles aria-expanded', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  // The first group, which is the facet rail's first focusable after
  // its own filter box. That is `host`: the corpus's first column,
  // `_time`, is an event instant and never earns a group.
  const group = page.locator(SEL.facetGroup).first();
  const header = group.locator(SEL.facetGroupHeader);
  await expect(header).toHaveJSProperty('tagName', 'BUTTON');
  await expect(header).toHaveAttribute('type', 'button');
  // The count is a span inside the button, so the name has to say the
  // two in words or it reads as `host6`.
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
    nameFrom(COPY.facetMoreName, String(CORPUS.hostsHidden), FIELD),
  );

  // It follows the last value's controls in the tab order.
  await group.locator(SEL.facetOp).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(more);

  await page.keyboard.press('Enter');

  await expect(group.locator(SEL.facetValue)).toHaveCount(CORPUS.hosts.length);
  await expect(group.locator(SEL.facetValueName).last()).toHaveText(
    CORPUS.hosts[CORPUS.hosts.length - 1],
  );
  // Nothing left to reveal, so the control goes rather than sitting
  // there doing nothing.
  await expect(more).toHaveCount(0);
});

test('long value keeps clear of the actions', async ({ page, request }) => {
  await openCorpusSearch(page, request);

  const group = hostGroup(page);
  const row = group.locator(SEL.facetValue).first();
  // A value far longer than the row is wide. Written into the DOM
  // rather than the fixture: the geometry is what is under test, and a
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
  // The area is out of flow over the row, so it carries its own opaque
  // floor. Transparent would let the value read through the glyphs.
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

  // The two filters are made through the include control the tests
  // above prove, not by hand-writing the URL's base64 payload: a seed
  // written by hand would pass this test with the control broken.
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
  // Reached BACKWARDS from the rail's filter box. The control is the
  // first focusable in the rail, so its predecessor is page chrome; the
  // filter box is the neighbour that names something.
  await page.locator(SEL.facetFilterInput).focus();
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(clear);

  await page.keyboard.press('Enter');
  // Both chips, and the link they were carried in.
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
  // Nothing left to clear, so the control goes with the filters rather
  // than sitting there doing nothing.
  await expect(clear).toHaveCount(0);

  // Space on a fresh seed. A button answers to both keys; the div this
  // replaced answered to neither.
  await addInclude(CORPUS.hosts[0]);
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await page.locator(SEL.facetFilterInput).focus();
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(page.locator(SEL.facetClear));

  await page.keyboard.press(' ');
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});

/** The fields the rail has a group for, read from each header's name. */
async function railFields(page: Pg): Promise<string[]> {
  const names = await page
    .locator(`${SEL.facetGroup} ${SEL.facetGroupHeader}`)
    .evaluateAll((els) => els.map((el) => el.getAttribute('aria-label') ?? ''));
  return names.map((name) => name.replace(/, \d+ values?$/, ''));
}

test('the rail counts dimensions, not instants, the raw event or one-off times', async ({ page }) => {
  await page.route('**/api/v1/query', (route) => route.fulfill({ json: presentation }));
  await page.goto(PRESENTATION_URL);

  // A repeating field earns its group, and so does a distinct one that
  // is not a time: five hosts on five rows are still hosts.
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

  // Both filters come from the detail views' own controls, never from a
  // hand-written URL (see "clear all" above): the include from the
  // inline expansion, the exclude from the inspector, which is the only
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

  // Two chips, one of each kind, while the rail still has no group for
  // the field they filter.
  const chips = page.locator(SEL.filterChip);
  await expect(chips).toHaveCount(2);
  await expect(chips.filter({ hasText: `timestamp = ${first}` })).not.toHaveClass(/excl/);
  await expect(chips.filter({ hasText: `timestamp = ${second}` })).toHaveClass(/excl/);
  await expect(
    page.getByRole('button', { name: nameFrom(COPY.facetGroupName, 'level', '3') }),
  ).toBeVisible();
  expect(await railFields(page)).not.toContain('timestamp');

  // Each one comes off on its own.
  await page.getByRole('button', { name: `Remove filter timestamp = ${second}`, exact: true }).click();
  await expect(chips).toHaveCount(1);
  await expect(chips).toContainText(`timestamp = ${first}`);
  await page.getByRole('button', { name: `Remove filter timestamp = ${first}`, exact: true }).click();
  await expect(chips).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});
