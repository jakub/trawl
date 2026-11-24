// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Both tab families as named WAI-ARIA tablists (ADR-0028).

import { test, expect, resetScenario, POPULATED, CORPUS } from '../fixtures';
import { SEL } from '../selectors';

type Loc = import('@playwright/test').Locator;

function tabindexes(tabs: Loc): Promise<(string | null)[]> {
  return tabs.evaluateAll((els) => els.map((el) => el.getAttribute('tabindex')));
}

function selectedFlags(tabs: Loc): Promise<(string | null)[]> {
  return tabs.evaluateAll((els) => els.map((el) => el.getAttribute('aria-selected')));
}

function insideTablist(loc: Loc): Promise<boolean> {
  return loc.evaluate((el) => el.closest('[role="tablist"]') !== null);
}

test('results strip is a named tablist with manual activation', async ({ page }) => {
  await page.goto('/search');

  const strip = page.getByRole('tablist', { name: 'Results' });
  await expect(strip).toHaveCount(1);

  const tabs = page.locator(SEL.workspaceTab);
  await expect(tabs).toHaveCount(2);
  await expect(tabs.nth(0)).toHaveRole('tab');
  await expect(tabs.nth(0)).toHaveJSProperty('tagName', 'BUTTON');
  await expect(tabs.nth(0)).toHaveAttribute('type', 'button');
  await expect(tabs.nth(1)).toHaveAccessibleName('Visualization');

  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  await tabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(0)).toBeFocused();
  await page.keyboard.press('End');
  await expect(tabs.nth(1)).toBeFocused();
  await page.keyboard.press('Home');
  await expect(tabs.nth(0)).toBeFocused();
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);

  await page.keyboard.press('End');
  await page.keyboard.press('Enter');
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  expect(await selectedFlags(tabs)).toEqual(['false', 'true']);
  expect(await tabindexes(tabs)).toEqual(['-1', '0']);

  await expect(page.locator(SEL.viewControl)).toBeVisible();
  await expect(page.locator(SEL.exportAction)).toBeVisible();
  expect(await insideTablist(page.locator(SEL.viewControl))).toBe(false);
  expect(await insideTablist(page.locator(SEL.exportAction))).toBe(false);
  await expect(page.locator('.tabs').getByRole('button', { name: /save/i })).toHaveCount(0);
});

test('drawer strip is a named tablist', async ({ page, request }) => {
  await resetScenario(request, 'populated');
  await page.goto(`/search/schema?svc=${POPULATED.service}&stab=overview`);

  await expect(page.locator(SEL.drawerPanel)).toBeVisible();
  const strip = page.getByRole('tablist', { name: 'Service details' });
  await expect(strip).toHaveCount(1);

  const tabs = page.locator(SEL.drawerTab);
  await expect(tabs).toHaveCount(3);
  await expect(tabs.nth(0)).toHaveAccessibleName('Overview');
  await expect(tabs.nth(1)).toHaveAccessibleName('Fields');
  await expect(tabs.nth(2)).toHaveAccessibleName('Live Tail');
  for (const i of [0, 1, 2]) {
    await expect(tabs.nth(i)).toHaveRole('tab');
    await expect(tabs.nth(i)).toHaveJSProperty('tagName', 'BUTTON');
    await expect(tabs.nth(i)).toHaveAttribute('type', 'button');
  }
  expect(await selectedFlags(tabs)).toEqual(['true', 'false', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1', '-1']);

  await tabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(2)).toBeFocused();
  await page.keyboard.press('ArrowLeft');
  await expect(tabs.nth(1)).toBeFocused();
  await expect(page).toHaveURL(/[?&]stab=overview(&|$)/);
  expect(await selectedFlags(tabs)).toEqual(['true', 'false', 'false']);

  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/[?&]stab=fields(&|$)/);
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  expect(await tabindexes(tabs)).toEqual(['-1', '0', '-1']);

  expect(await insideTablist(page.locator(SEL.drawerTitle))).toBe(false);
  expect(await insideTablist(page.locator(SEL.drawerClose))).toBe(false);
});

test('an unknown drawer tab id still yields one tab stop', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}&ntab=bogus`);

  await expect(page.locator(SEL.drawerPanel)).toBeVisible();
  const strip = page.getByRole('tablist', { name: 'Saved query details' });
  await expect(strip).toHaveCount(1);

  const tabs = page.locator(SEL.drawerTab);
  await expect(tabs).toHaveCount(2);
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  await page.locator(SEL.drawerClose).focus();
  await page.keyboard.press('Tab');
  await expect(tabs.nth(0)).toBeFocused();

  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await expect(tabs.nth(1)).toHaveAccessibleName('Runs');
});
