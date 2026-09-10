// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';

for (const width of [320, 390, 768, 1440]) {
  test(`field service names, timestamps and counts remain readable at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 });
    await page.goto('/search/schema?field=duration');
    const rows = page.locator('.fc-svc');
    await expect(rows).toHaveCount(2);
    await expect(rows.first().locator('.nm')).toHaveText('nginx');
    await expect(rows.last().locator('.nm')).toHaveText(
      'edgegatewaywithalongunbrokenservicenameforthefielddrawer',
    );
    await expect(rows.last().locator('.ct')).toHaveText('1000000000.0B rows');
    await expect(rows.first().locator('.dt')).toHaveText(
      '2026-07-24T00:00:00.824072Z → 2026-08-01T09:00:00.987035Z',
    );
    await page.evaluate(() => document.fonts.ready);
    for (const row of await rows.all()) {
      for (const selector of ['.nm', '.dt', '.ct']) {
        await expect(row.locator(selector)).toBeVisible();
      }
    }

    // Text can paint beyond a zero-width flex item. Measure each actual
    // text fragment, including wrapped lines, rather than only its box.
    const violations = await rows.evaluateAll((elements) => {
      const failures: string[] = [];
      for (const [index, row] of elements.entries()) {
        const bounds = row.getBoundingClientRect();
        const groups = ['.nm', '.dt', '.ct'].map((selector) => {
          const element = row.querySelector(selector)!;
          const range = document.createRange();
          range.selectNodeContents(element);
          const fragments = Array.from(range.getClientRects());
          if (fragments.length === 0) {
            failures.push(`row ${index}: ${selector} has no rendered text`);
          }
          for (const rect of fragments) {
            if (rect.left < bounds.left - 1 || rect.right > bounds.right + 1
              || rect.top < bounds.top - 1 || rect.bottom > bounds.bottom + 1) {
              failures.push(`row ${index}: ${selector} text escapes its row`);
            }
          }
          return { selector, fragments };
        });
        for (let a = 0; a < groups.length; a += 1) {
          for (let b = a + 1; b < groups.length; b += 1) {
            for (const left of groups[a].fragments) {
              for (const right of groups[b].fragments) {
                if (left.left < right.right && left.right > right.left
                  && left.top < right.bottom && left.bottom > right.top) {
                  failures.push(`row ${index}: ${groups[a].selector} overlaps ${groups[b].selector}`);
                }
              }
            }
          }
        }
      }
      return failures;
    });
    expect(violations, 'service text must fit without overlapping other values').toEqual([]);
  });
}
