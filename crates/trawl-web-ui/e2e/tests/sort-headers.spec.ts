// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Sort headers as controls (ADR-0029).

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

/** The whole vector: reading only the sorted column would pass with
 * `aria-sort` on all four. */
function ariaSorts(page: import('@playwright/test').Page): Promise<(string | null)[]> {
  return page
    .locator(SEL.resultsSortHeader)
    .evaluateAll((els) => els.map((el) => el.getAttribute('aria-sort')));
}

test('results th: aria-sort follows the sorted column', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);

  const controls = page.locator(SEL.resultsSortControl);
  await expect(controls).toHaveCount(CORPUS.columns.length);
  expect(await ariaSorts(page)).toEqual([null, null, null, null]);

  const host = controls.nth(1);
  await controls.nth(0).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(host);
  await expect(host).toHaveJSProperty('tagName', 'BUTTON');
  await expect(host).toHaveAttribute('type', 'button');
  await expect(host).toHaveAccessibleName(nameFrom(COPY.resultsSortName, CORPUS.columns[1]));

  await page.keyboard.press('Enter');
  expect(await ariaSorts(page)).toEqual([null, 'descending', null, null]);
  await expect(page.locator(SEL.resultsRow).first().locator('td').nth(2)).toHaveText(
    CORPUS.hostLastAlphabetically,
  );

  await page.keyboard.press(' ');
  expect(await ariaSorts(page)).toEqual([null, 'ascending', null, null]);

  await page.keyboard.press('Tab');
  await page.keyboard.press('Enter');
  expect(await ariaSorts(page)).toEqual([null, null, 'descending', null]);
});

test('schema header: Enter sorts, Space reverses', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const controls = page.locator(SEL.tableSortControl);
  await expect(controls.first()).toHaveAccessibleName(
    nameFrom(COPY.sortNameAscending, COPY.schemaServiceHeader),
  );

  const events = controls.nth(1);
  await controls.nth(0).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(events);
  await expect(events).toHaveJSProperty('tagName', 'BUTTON');
  await expect(events).toHaveAttribute('type', 'button');
  await expect(events).toHaveAccessibleName(nameFrom(COPY.sortName, COPY.schemaEventsHeader));

  await page.keyboard.press('Enter');
  await expect(events).toHaveAccessibleName(
    nameFrom(COPY.sortNameDescending, COPY.schemaEventsHeader),
  );
  await expect(controls.first()).toHaveAccessibleName(
    nameFrom(COPY.sortName, COPY.schemaServiceHeader),
  );

  await page.keyboard.press(' ');
  await expect(events).toHaveAccessibleName(
    nameFrom(COPY.sortNameAscending, COPY.schemaEventsHeader),
  );
});

test('service fields: name header keeps ascending default', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?svc=${CORPUS.service}&stab=fields`);

  const controls = page.locator(SEL.serviceFieldSortControl);
  await expect(controls).toHaveCount(4);
  await expect(controls.nth(2)).toHaveAccessibleName(
    nameFrom(COPY.sortNameDescending, COPY.serviceCardinalityHeader),
  );

  const field = controls.first();
  await expect(field).toHaveAccessibleName(nameFrom(COPY.sortName, COPY.serviceFieldHeader));
  await page.locator(SEL.drawerTab).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(field);

  await page.keyboard.press('Enter');
  await expect(field).toHaveAccessibleName(
    nameFrom(COPY.sortNameAscending, COPY.serviceFieldHeader),
  );
  await expect(page.locator(SEL.serviceFieldRow).first().locator('.c-name')).toHaveText(
    CORPUS.fieldFirstAlphabetically,
  );

  await page.keyboard.press(' ');
  await expect(field).toHaveAccessibleName(
    nameFrom(COPY.sortNameDescending, COPY.serviceFieldHeader),
  );
  await expect(page.locator(SEL.serviceFieldRow).first().locator('.c-name')).toHaveText(
    CORPUS.fieldLastAlphabetically,
  );
});

test('Runs headers expose one aria-sort and send each natural direction through keyboard controls', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  // Pause before navigation so a slow initial load cannot start the Jobs poll
  // during this exact request-order assertion. This route needs no load timer.
  await page.clock.install({ time: new Date('2026-09-15T12:05:00Z') });
  await page.clock.pauseAt(new Date('2026-09-15T12:05:01Z'));
  const requests: Array<{ sort: string | null; dir: string | null; offset: string | null }> = [];
  await page.route('**/api/v1/runs?*', async route => {
    const params = new URL(route.request().url()).searchParams;
    requests.push({ sort: params.get('sort'), dir: params.get('dir'), offset: params.get('offset') });
    await route.continue();
  });
  await page.goto('/jobs/runs');
  const table = page.locator('.runs-table');
  // Column headers as assistive technology reads them: the aria-hidden
  // slack column is presentation, not a column.
  const headers = table.getByRole('columnheader');
  const buttons = headers.getByRole('button');
  await expect(buttons).toHaveCount(5);
  await expect(table.locator('tbody tr')).toHaveCount(3);
  expect(await headers.evaluateAll(els => els.map(el => el.getAttribute('aria-sort'))))
    .toEqual([null, null, 'descending', null, null]);
  expect(requests).toEqual([{ sort: 'started', dir: 'desc', offset: '0' }]);
  // Visit When after another key, proving its inactive default too.
  for (const [index, key, first] of [[0, 'net', 'asc'], [1, 'status', 'asc'], [2, 'started', 'desc'], [3, 'duration', 'desc'], [4, 'rows', 'desc']] as const) {
    const control = buttons.nth(index);
    await control.focus();
    await expectFocusRing(control);
    await expect(control).toHaveAttribute('type', 'button');
    for (const [press, dir] of [['Enter', first], [' ', first === 'asc' ? 'desc' : 'asc']] as const) {
      const before = requests.length;
      await page.keyboard.press(press);
      await expect.poll(() => requests.length).toBe(before + 1);
      await expect(page.getByRole('region', { name: 'Recent runs table', exact: true })).toHaveAttribute('aria-busy', 'false');
      expect(requests.at(-1)).toEqual({ sort: key, dir, offset: '0' });
      const expected = Array<string | null>(5).fill(null);
      expected[index] = dir === 'asc' ? 'ascending' : 'descending';
      expect(await headers.evaluateAll(els => els.map(el => el.getAttribute('aria-sort')))).toEqual(expected);
    }
  }
});
