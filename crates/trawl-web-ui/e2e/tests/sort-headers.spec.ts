// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Sort headers as controls (issue #161, ADR-0029).
//
// Three tables, one rule: the header CELL is not the control, the button
// inside it is. Where the table is a real `<table>` the direction is
// announced by `aria-sort` on the sorted `<th>` and the glyph is
// decoration; where it is a div table there is no `<th>` to carry it, no
// ARIA table roles are added, and the direction lives in the button's
// accessible name instead ("Sort by Events, descending").
//
// The service drawer's four field headers moved onto the same `sort_th`
// helper the schema and nets tables use, and the helper stores DESCENDING
// where the drawer stored ASCENDING. Flipping that polarity is exactly
// the kind of migration that silently reverses a default, so the third
// test reads the drawer's starting state and the Field column's first
// press rather than trusting the enum.

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';

type Loc = import('@playwright/test').Locator;

async function expectFocusRing(control: Loc): Promise<void> {
  await expect(control).toBeFocused();
  expect(await control.evaluate((el) => el.matches(':focus-visible'))).toBe(true);
  expect(await control.evaluate((el) => getComputedStyle(el).boxShadow)).not.toBe('none');
}

/** `aria-sort` across every sortable header, in render order. The claim
 * is about the whole vector: the attribute belongs on the ONE sorted
 * column, and reading only that column would pass with it on all four. */
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
  // Nothing is sorted until something is pressed.
  expect(await ariaSorts(page)).toEqual([null, null, null, null]);

  // `host` is the second column; its predecessor in the tab order is
  // the first column's own control.
  const host = controls.nth(1);
  await controls.nth(0).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(host);
  await expect(host).toHaveJSProperty('tagName', 'BUTTON');
  await expect(host).toHaveAttribute('type', 'button');
  await expect(host).toHaveAccessibleName(CORPUS.columns[1]);

  await page.keyboard.press('Enter');
  expect(await ariaSorts(page)).toEqual([null, 'descending', null, null]);
  await expect(page.locator(SEL.resultsRow).first().locator('td').nth(2)).toHaveText(
    CORPUS.hostLastAlphabetically,
  );

  // Space reverses it once, and the attribute stays on this column
  // alone rather than accumulating.
  await page.keyboard.press(' ');
  expect(await ariaSorts(page)).toEqual([null, 'ascending', null, null]);

  // A different column takes the sort with it.
  await page.keyboard.press('Tab');
  await page.keyboard.press('Enter');
  expect(await ariaSorts(page)).toEqual([null, null, 'descending', null]);
});

test('schema header: Enter sorts, Space reverses', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const controls = page.locator(SEL.tableSortControl);
  // Service is the seeded sort, so it is the one header that starts
  // with a direction in its name.
  await expect(controls.first()).toHaveAccessibleName(
    nameFrom(COPY.sortNameAscending, COPY.schemaServiceHeader),
  );

  // Events, reached by Tab from the header before it.
  const events = controls.nth(3);
  await controls.nth(2).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(events);
  await expect(events).toHaveJSProperty('tagName', 'BUTTON');
  await expect(events).toHaveAttribute('type', 'button');
  // Unsorted, so the name is the column and no direction.
  await expect(events).toHaveAccessibleName(nameFrom(COPY.sortName, COPY.schemaEventsHeader));

  await page.keyboard.press('Enter');
  await expect(events).toHaveAccessibleName(
    nameFrom(COPY.sortNameDescending, COPY.schemaEventsHeader),
  );
  // The sort MOVED rather than spread: Service is back to no direction.
  await expect(controls.first()).toHaveAccessibleName(
    nameFrom(COPY.sortName, COPY.schemaServiceHeader),
  );

  // One press, one reversal. Two would land back on descending and read
  // as if nothing happened.
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
  // The pane still opens on cardinality, descending — the polarity flip
  // onto `sort_th` did not change what the operator sees first.
  await expect(controls.nth(2)).toHaveAccessibleName(
    nameFrom(COPY.sortNameDescending, COPY.serviceCardinalityHeader),
  );

  const field = controls.first();
  await expect(field).toHaveAccessibleName(nameFrom(COPY.sortName, COPY.serviceFieldHeader));
  // Reached from the drawer's tab strip, the focusable before it.
  await page.locator(SEL.drawerTab).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(field);

  await page.keyboard.press('Enter');
  // A fresh key starts at its natural direction, and for a name that is
  // ascending: A first, not Z.
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
