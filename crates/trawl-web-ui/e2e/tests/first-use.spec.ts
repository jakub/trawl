// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, lastCapturedQuery, capturedQueryCount } from '../fixtures';
import { SEL } from '../selectors';
import { selectTheme } from '../theme';

test('login explains personal keys and links operators to provisioning', async ({ page }) => {
  await page.goto('/login');
  await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
  await expect(page.getByText('Ask your Trawl operator for a personal API key.')).toBeVisible();
  const help = page.getByRole('link', { name: 'Create your first API key' });
  await expect(help).toHaveAttribute('href', 'https://trawl.sh/operate/access/#create-roles-and-keys');
  await expect(help).toHaveAttribute('rel', 'noopener noreferrer');
  await page.setViewportSize({ width: 375, height: 667 });
  await expect(help).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeInViewport();
});

const EXAMPLES = [
  ['Explore events', '* | head 20'],
  ['Compare services', '* | stats count() by service'],
  ['Rank services by errors', '_severity>=error | stats count() as errors by service | sort -errors | head 10'],
  ['Chart web warnings and errors', 'service=web _severity>=warn | timechart span=5m count()'],
] as const;

for (const [title, query] of EXAMPLES) {
  test(`quick start runs ${title} within the selected relative range`, async ({ page, request }) => {
    await page.goto('/search?r=1h&page=2');
    await expect(page.getByRole('heading', { name: 'Quick start', exact: true })).toBeVisible();
    expect(await capturedQueryCount(request)).toBe(0);
    const run = page.getByRole('button', { name: `Run ${title}`, exact: true });
    await run.focus();
    await page.keyboard.press('Enter');
    expect((await lastCapturedQuery(request, 1)).query).toBe(`last=1h ${query}`);
    const params = new URL(page.url()).searchParams;
    expect(params.get('r')).toBe('1h');
    expect(params.get('q')).toBe(query);
    expect(params.get('page')).toBe('0');
    await expect(page.locator(SEL.cmContent)).toHaveText(query);
    const results = page.locator('#search-results:not(.search-quick-start)');
    await expect(results).toBeVisible();
    await expect(results).toBeFocused();
    if (title === 'Explore events') {
      await expect(page.getByText('No events match this query. Check the time range and filters.')).toBeVisible();
    }
    await expect(page.locator('.search-quick-start')).toHaveCount(0);
  });
}

test('quick start preserves an absolute range and active filter', async ({ page, request }) => {
  const range = '2026-01-01T00:00:00Z..2026-01-01T00:15:00Z';
  // Literal versioned include filter, independent of the application's encoder.
  const filter = 'v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0';
  await page.goto(`/search?r=${range}&f=${filter}`);
  await page.getByRole('button', { name: 'Run Explore events', exact: true }).click();
  expect((await lastCapturedQuery(request, 1)).query).toBe(
    'host="web-01" _time>="2026-01-01T00:00:00Z" _time<="2026-01-01T00:15:00Z" * | head 20',
  );
  const params = new URL(page.url()).searchParams;
  expect(params.get('r')).toBe(range);
  expect(params.get('f')).toBe(filter);
  await expect(page.locator(`${SEL.scopeStrip} ${SEL.filterChip}`)).toContainText('web-01');
});

for (const theme of ['light', 'dark'] as const) {
  for (const width of [390, 720, 1440]) {
    test(`quick start remains usable at ${width}px in ${theme}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.goto('/search');
      await selectTheme(page, theme === 'dark' ? 'Dark' : 'Light');
      await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
      const guide = page.locator('.search-quick-start');
      await expect(guide).toBeVisible();
      await expect(guide.locator('.qs-example')).toHaveCount(4);
      for (const [title, query] of EXAMPLES) {
        const button = guide.getByRole('button', { name: `Run ${title}`, exact: true });
        await button.scrollIntoViewIfNeeded();
        await expect(button).toBeInViewport();
        await expect(button).toBeEnabled();
        const row = guide.locator('.qs-example').filter({
          has: page.getByRole('button', { name: `Run ${title}`, exact: true }),
        });
        await expect(row.locator('code')).toHaveText(query);
        const geometry = await row.evaluate(element => {
          const code = element.querySelector('code')!.getBoundingClientRect();
          const button = element.querySelector('button')!.getBoundingClientRect();
          return {
            fits: element.scrollWidth <= element.clientWidth + 1,
            buttonInside: button.left >= 0 && button.right <= window.innerWidth,
            overlaps: code.left < button.right && code.right > button.left &&
              code.top < button.bottom && code.bottom > button.top,
          };
        });
        expect(geometry).toEqual({ fits: true, buttonInside: true, overlaps: false });
      }
      for (const [name, href] of [
        ['Full query reference', 'https://trawl.sh/reference/dsl/'],
        ['Event reference', 'https://trawl.sh/reference/events/'],
      ]) {
        const link = guide.getByRole('link', { name: new RegExp(name) });
        await expect(link).toHaveAttribute('href', href);
        await link.scrollIntoViewIfNeeded();
        await expect(link).toBeInViewport();
      }
      expect(await guide.evaluate(element => element.scrollWidth <= element.clientWidth + 1)).toBe(true);
    });
  }
}

test('a direct zero-match search gives range and filter guidance', async ({ page }) => {
  await page.goto('/search?q=service%3Dmissing');
  await expect(page.getByText('No events match this query. Check the time range and filters.')).toBeVisible();
  await expect(page.locator('.search-quick-start')).toHaveCount(0);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(page.locator('.search-quick-start')).toHaveCount(0);
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
