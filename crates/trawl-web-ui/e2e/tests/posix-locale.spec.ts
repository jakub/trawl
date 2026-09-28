// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A browser can report a language that is not a BCP 47 tag. Chromium on
// the ubuntu-24.04-arm runner reported `en-US@posix`, the POSIX locale
// spelling, and `new Intl.NumberFormat('en-US@posix')` throws a
// RangeError. uPlot built its number formatter from `navigator.language`
// while its module loaded, so the error stopped the SPA before the sign-in
// form rendered. The vendor build now routes that language through
// `vendor/src/uplot-navigator.ts`.
//
// Playwright's `locale` sets `navigator.language` to exactly that string.
// Each test first checks that the page sees the invalid tag, so the test
// cannot pass because the browser corrected the tag. The contract fixture
// fails the test on any page error.

import { test, expect } from '../fixtures';

const POSIX_TAG = 'en-US@posix';

test.use({ locale: POSIX_TAG });

async function expectInvalidTag(page: import('@playwright/test').Page) {
  expect(await page.evaluate(() => navigator.language)).toBe(POSIX_TAG);
  const error = await page.evaluate(tag => {
    try {
      new Intl.NumberFormat(tag);
      return null;
    } catch (err) {
      return String(err);
    }
  }, POSIX_TAG);
  expect(error).toBe(`RangeError: Invalid language tag: ${POSIX_TAG}`);
}

test('sign-in renders under a POSIX language tag', async ({ page }) => {
  await page.goto('/login');
  await expectInvalidTag(page);
  await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
});

test('a chart formats its values under a POSIX language tag', async ({ page }) => {
  const value = 1234567;
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, { name: 'count' }],
    rows: [['2026-09-01T00:00:00Z', 1], ['2026-09-01T00:01:00Z', value], ['2026-09-01T00:02:00Z', 1]],
    pagination: { limit: 50, offset: 0, returned: 3, total: 3 },
  } }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1m count()'));
  await expectInvalidTag(page);
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  // The legend value is uPlot's own number formatter. Without a valid
  // tag, that formatter uses the browser's default locale, the same one
  // the bar chart tooltip's `toLocaleString()` uses.
  const expected = await page.evaluate(v => new Intl.NumberFormat().format(v), value);
  const plot = await page.locator('.chart .u-over').boundingBox();
  await page.mouse.move(plot!.x + plot!.width / 2, plot!.y + plot!.height / 2);
  const row = page.locator('.chart .u-series').filter({ has: page.locator('.u-label', { hasText: 'count' }) });
  await expect(row.locator('.u-value')).toHaveText(expected);
});
