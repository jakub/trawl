// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';
import { expectFocusRing } from '../a11y';

test('Settings opens Health and offers only Health and the existing Schema page', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthPage)).toBeVisible();

  const rail = page.locator(SEL.paletteRailLink);
  await expect(rail).toHaveText(['Health', 'Schema']);
  await expect(rail.nth(0)).toHaveAttribute('href', '/settings/health');
  await expect(rail.nth(1)).toHaveAttribute('href', '/search/schema');
  await expect(rail.nth(0)).toHaveClass(/\bactive\b/);

  await rail.nth(1).click();
  await expect(page).toHaveURL(/\/search\/schema$/);
  await expect(page.getByRole('heading', { name: 'Schema', exact: true })).toBeVisible();
  const searchMode = page.locator(SEL.paletteModeLink).filter({ hasText: /^Search$/ });
  await expect(searchMode).toHaveClass(/\bactive\b/);
});

test('Settings replaces its intermediate history entry and preserves the shell stream', async ({ page, request }) => {
  const state = async () => (await request.get('/__ctl/state')).json();
  await expect.poll(async () => (await state()).dashboard.open).toBe(0);
  const baseline = (await state()).dashboard.opens;
  await resetScenario(request, 'health-admin');
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await expect(page.locator(SEL.healthFooterHot)).toContainText('731');
  await expect.poll(async () => (await state()).dashboard.open).toBe(1);
  await page.evaluate(() => { (window as any).__settingsNavigation = true; });

  const settingsMode = page.locator(SEL.paletteModeLink).filter({ hasText: /^Settings$/ });
  await expect(settingsMode).toHaveAttribute('href', '/settings');
  await settingsMode.click();
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  expect(await page.evaluate(() => (window as any).__settingsNavigation)).toBe(true);

  await page.goBack();
  await expect(page).toHaveURL(/\/search$/);
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await page.goForward();
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  expect(await page.evaluate(() => (window as any).__settingsNavigation)).toBe(true);

  const dashboard = (await state()).dashboard;
  expect(dashboard.open).toBe(1);
  expect(dashboard.opens - baseline).toBe(1);
  expect(dashboard.max).toBe(1);
});

for (const section of ['sources', 'retention', 'users']) {
  test(`obsolete Settings ${section} route renders the existing NotFound page`, async ({ page }) => {
    await page.goto(`/settings/${section}`);
    await expect(page).toHaveURL(new RegExp(`/settings/${section}$`));
    await expect(page.locator(SEL.notFoundHeading)).toHaveText(COPY.notFoundHeading);
    await expect(page.locator(SEL.notFoundSubtitle)).toHaveText(COPY.notFoundSubtitle);
  });
}

test('Help is a native keyboard link and stays outside the full Settings palette inventory', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page.locator(SEL.healthPage)).toBeVisible();
  const help = page.locator(SEL.helpLink);
  await expect(help).toHaveJSProperty('tagName', 'A');
  await expect(help).toHaveAccessibleName('Help');
  await expect(help).toHaveAttribute('href', 'https://trawl.sh');
  await expect(help).toHaveAttribute('target', '_blank');
  await expect(help).toHaveAttribute('rel', 'noopener noreferrer');

  await page.locator(SEL.paletteRailLink).last().focus();
  await page.keyboard.press('Tab');
  await expect(help).toBeFocused();
  await expectFocusRing(help);
  // Observe native Enter activation without contacting the external origin.
  await help.evaluate((link) => {
    link.addEventListener('click', (event) => {
      event.preventDefault();
      link.setAttribute('data-keyboard-activated', 'true');
    }, { once: true });
  });
  await page.keyboard.press('Enter');
  await expect(help).toHaveAttribute('data-keyboard-activated', 'true');
  await expect(page).toHaveURL(/\/settings\/health$/);

  await page.locator(SEL.paletteTrigger).click();
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
  await expect(page.locator(SEL.paletteLabel)).toHaveText(['Search', 'Jobs', 'Settings', 'Health', 'Schema']);
  expect(await page.locator(SEL.paletteOption).evaluateAll((options) =>
    options.map((option) => option.getAttribute('href')),
  )).toEqual(['/search', '/jobs/nets', '/settings', '/settings/health', '/search/schema']);
  await expect(page.locator(SEL.paletteLabel).filter({ hasText: /^Help$/ })).toHaveCount(0);
});
