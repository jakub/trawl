// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, lastCapturedQuery } from '../fixtures';
import { SEL } from '../selectors';

test('login explains personal keys and links operators to provisioning', async ({ page }) => {
  await page.goto('/login');
  await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
  await expect(page.getByText('Ask your Trawl operator for a personal API key.')).toBeVisible();
  const help = page.getByRole('link', { name: 'Create your first API key' });
  await expect(help).toHaveAttribute('href', 'https://trawl.sh/operate/access/#create-roles-and-keys');
  await expect(help).toHaveAttribute('rel', 'noopener noreferrer');
  await page.setViewportSize({ width: 375, height: 667 });
  await expect(help).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Sign In', exact: true })).toBeInViewport();
});

for (const [platform, ua, shortcut] of [
  ['Linux', 'Mozilla/5.0 (X11; Linux x86_64)', 'Ctrl + Enter'],
  ['Mac', 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', '⌘ + Enter'],
]) {
  test(`first search on ${platform} offers a bounded executable example and platform shortcut`, async ({ page, request }) => {
    await page.addInitScript((userAgent) => {
      Object.defineProperty(navigator, 'userAgent', { get: () => userAgent });
    }, ua);
    await page.goto('/search');
    await expect(page.locator('.results-empty')).toContainText(`Or enter a query and press ${shortcut}.`);
    await expect(page.locator('.run')).toContainText(shortcut);
    await expect(page.getByRole('link', { name: 'Query guide', exact: true })).toHaveAttribute('href', 'https://trawl.sh/use/query-tutorial/');
    await page.getByRole('button', { name: 'Run example', exact: true }).click();
    expect((await lastCapturedQuery(request, 1)).query).toBe('last=1h | head 20');
    await expect(page.locator(SEL.cmContent)).toHaveText('last=1h | head 20');
    await expect(page.getByText('No events match this query. Check the time range and filters.')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Run example', exact: true })).toHaveCount(0);
  });
}

test('a direct zero-match search gives range and filter guidance', async ({ page }) => {
  await page.goto('/search?q=service%3Dmissing');
  await expect(page.getByText('No events match this query. Check the time range and filters.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Run example', exact: true })).toHaveCount(0);
});

test('an empty later page does not claim that the query has no matches', async ({ page }) => {
  await page.route('**/api/v1/query', async (route) => {
    await route.fulfill({ json: {
      columns: [{ name: '_raw' }], rows: [], truncated: false,
      pagination: { limit: 50, offset: 50, returned: 0 },
      degraded_fields: [], severity_columns: [],
    } });
  });
  await page.goto('/search?q=service%3Dmissing&page=1');
  await expect(page.getByText('No events on this page. Try the previous page or check the time range and filters.')).toBeVisible();
  await expect(page.getByText('No events match this query. Check the time range and filters.')).toHaveCount(0);
});
