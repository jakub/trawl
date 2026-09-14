// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The labelled sidebar and the command bar that replaced the icon rail
// and the topbar mode tabs (ADR-0032). The overlay's geometry at narrow
// widths is responsive-layout's; what is here is the state the sidebar
// owns: the persisted collapse, the current-page marking, the crumb the
// command bar derives from it, and what the overlay does to ⌘K.

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
  // Collapsed is icon-only, so the label the control offers flips too.
  await expect(page.locator(SEL.sidebarCollapse)).toHaveAccessibleName('Expand sidebar');
  // The state is written where the theme lives, not kept in memory.
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem('trawl.ui')!).sidebar))
    .toBe('collapsed');

  await page.reload();
  await expect(page.locator('nav.rail')).toHaveClass(/collapsed/);
  // A collapsed link is still a named destination, not a bare glyph.
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
    // The command bar no longer carries mode tabs: the crumb is the
    // active item's label, derived from the same groups.
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

  // The overlay is the topmost layer, so the palette refuses the chord
  // rather than stacking a second dialog over the navigation.
  await page.keyboard.press('Control+k');
  await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);

  await page.keyboard.press('Escape');
  await expect(page.locator('nav.rail.overlay')).toHaveCount(0);
  await expect(toggle).toBeFocused();
  await expect(toggle).toHaveAttribute('aria-expanded', 'false');

  // And it works again once the overlay is gone.
  await page.keyboard.press('Control+k');
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
});
