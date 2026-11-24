// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

test('collapse is a preference that survives a reload', async ({ page }) => {
  await page.goto('/search');
  const rail = page.locator('nav.rail');
  const collapse = page.locator(SEL.sidebarCollapse);
  await expect(rail).not.toHaveClass(/collapsed/);
  await expect(collapse).toHaveAccessibleName('Collapse sidebar');

  await collapse.click();
  await expect(rail).toHaveClass(/collapsed/);
  await expect(page.locator(SEL.sidebarCollapse)).toHaveAccessibleName('Expand sidebar');
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem('trawl.ui')!).sidebar))
    .toBe('collapsed');

  await page.reload();
  await expect(page.locator('nav.rail')).toHaveClass(/collapsed/);
  const search = page.locator('nav.rail .grp > a[title="Search"]');
  await expect(search).toHaveAccessibleName('Search');

  await page.locator(SEL.sidebarCollapse).click();
  await expect(page.locator('nav.rail')).not.toHaveClass(/collapsed/);
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem('trawl.ui')!).sidebar))
    .toBe('expanded');
});

for (const [route, label] of [
  ['/search', 'Search'],
  ['/search/history', 'History'],
  ['/search/schema', 'Schema'],
  ['/jobs/nets', 'Nets'],
  ['/jobs/runs', 'Runs'],
  ['/settings/health', 'Health'],
] as const) {
  test(`the current destination is marked and named in the crumb at ${route}`, async ({ page, request }) => {
    await resetScenario(request, route === '/settings/health' ? 'health-viewer' : 'corpus');
    await page.goto(route);
    const current = page.locator('nav.rail .grp > a[aria-current="page"]');
    await expect(current).toHaveCount(1);
    await expect(current).toHaveAttribute('title', label);
    await expect(current).toHaveClass(/active/);
    await expect(page.locator(SEL.topbarCrumb)).toHaveText(label);
  });
}

test('the narrow overlay owns the layer, so the palette chord stays inert until it closes', async ({ page }) => {
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto('/search');
  const toggle = page.locator(SEL.navToggle);
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');
  await toggle.click();
  await expect(toggle).toHaveAttribute('aria-expanded', 'true');
  await expect(page.locator('nav.rail.overlay')).toBeVisible();

  await page.keyboard.press('Control+k');
  await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);

  await page.keyboard.press('Escape');
  await expect(page.locator('nav.rail.overlay')).toHaveCount(0);
  await expect(toggle).toBeFocused();
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');

  await page.keyboard.press('Control+k');
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
});
