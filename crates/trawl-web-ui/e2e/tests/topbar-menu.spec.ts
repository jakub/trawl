// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The topbar account menu on the shared menu contract (ADR-0028).

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;

// The tabindex VECTOR across every item: asserting only the focused item's
// value would pass with every item at "0".
async function itemTabindexes(page: Pg): Promise<(string | null)[]> {
  return page.locator(SEL.userMenuItem).evaluateAll((els) =>
    els.map((el) => el.getAttribute('tabindex')),
  );
}

/** Never a mouse click — a pointer open would
 * leave focus on the trigger for a reason that has nothing to do with
 * the menu's own initial-focus effect. */
async function openByKeyboard(page: Pg): Promise<void> {
  await expect(page.locator(SEL.topbarUser)).toBeEnabled();
  await page.locator(SEL.topbarUser).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.userMenu)).toBeVisible();
}

test('keyboard open focuses System and arrows walk all choices and commands without selecting', async ({ page }) => {
  await page.goto('/search');
  await openByKeyboard(page);

  const items = page.locator(SEL.userMenuItem);
  await expect(items).toHaveCount(4);
  const first = page.getByRole('menuitemradio', { name: 'Light', exact: true });
  const last = page.getByRole('menuitem', { name: 'Sign Out', exact: true });
  const system = page.getByRole('menuitemradio', { name: 'System', exact: true });

  await expect(system).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['-1', '-1', '0', '-1']);

  await page.keyboard.press('ArrowDown');
  await expect(last).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['-1', '-1', '-1', '0']);

  await page.keyboard.press('ArrowDown');
  await expect(first).toBeFocused();

  await page.keyboard.press('ArrowUp');
  await expect(last).toBeFocused();

  await page.keyboard.press('Home');
  await expect(first).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(page.getByRole('menuitemradio', { name: 'Dark', exact: true })).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(system).toBeFocused();
  await page.keyboard.press('End');
  await expect(last).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['-1', '-1', '-1', '0']);
  await expect(system).toHaveAttribute('aria-checked', 'true');
  await expect(first).toHaveAttribute('aria-checked', 'false');
  expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBeNull();

  await expect(page.locator('.user-menu .hdr')).toBeVisible();
  expect(
    await page.locator('.user-menu .hdr').evaluate((el) => el.closest('[role="menu"]') !== null),
  ).toBe(false);
});

test('Escape restores the trigger', async ({ page }) => {
  await page.goto('/search');
  await openByKeyboard(page);

  await page.keyboard.press('Escape');

  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
});

test('Tab closes and restores the trigger', async ({ page }) => {
  await page.goto('/search');

  await openByKeyboard(page);
  await page.keyboard.press('Tab');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();

  await openByKeyboard(page);
  await page.keyboard.press('Shift+Tab');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
});

test('theme choice restores the trigger and selects the explicit preference', async ({ page }) => {
  await page.goto('/search');
  const themeAttr = () => page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  const before = await themeAttr();

  await openByKeyboard(page);
  await page.keyboard.press('Home');
  await page.keyboard.press('ArrowDown');
  await expect(page.getByRole('menuitemradio', { name: 'Dark', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
  await expect.poll(themeAttr).not.toBe(before);
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem('trawl.ui')!).theme)).toBe('dark');
});

test('command activation restores the trigger while sign out is pending and after failure', async ({ page }) => {
  let release!: () => void;
  let requested = false;
  const pending = new Promise<void>(resolve => { release = resolve; });
  await page.route('**/api/auth/logout', async route => {
    requested = true;
    await pending;
    await route.fulfill({ status: 503, contentType: 'application/json', body: '{"error":"temporarily unavailable"}' });
  });
  try {
    await page.goto('/search');
    await openByKeyboard(page);
    await page.keyboard.press('End');
    await expect(page.getByRole('menuitem', { name: 'Sign Out', exact: true })).toBeFocused();
    await page.keyboard.press('Enter');
    await expect.poll(() => requested).toBe(true);
    await expect(page.locator(SEL.userMenu)).toHaveCount(0);
    await expect(page.locator(SEL.topbarUser)).toBeFocused();
    release();
    await expect(page.getByRole('alert').filter({ hasText: 'Sign out was not confirmed.' })).toBeVisible();
    await expect(page.locator(SEL.topbarUser)).toBeFocused();
  } finally { release(); }
});

test('outside mousedown leaves focus on the target', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await openByKeyboard(page);

  await page.locator(SEL.cmContent).click();

  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.cmContent)).toBeFocused();
  await expect(page.locator(SEL.topbarUser)).not.toBeFocused();
});

test('first Escape closes only the modal above the menu', async ({ page }) => {
  await page.goto('/search');
  await openByKeyboard(page);

  // dispatchEvent, not click(): a real press would fire mousedown
  // outside the menu's wrapper and dismiss it before the modal ever
  // mounted.
  await page.locator(SEL.exportAction).dispatchEvent('click');
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  await expect(page.locator(SEL.userMenu)).toBeVisible();

  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  await expect(page.locator(SEL.userMenu)).toBeVisible();

  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
});

test('retired affordances are absent', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator('.topbar')).toBeVisible();

  await expect(page.locator('.topbar .iconbtn')).toHaveCount(0);
  await openByKeyboard(page);
  await expect(page.locator('.user-menu').getByText('Profile', { exact: true })).toHaveCount(0);
  await expect(page.locator('.user-menu').getByText('API tokens', { exact: true })).toHaveCount(0);
  await expect(page.locator('.user-menu').getByText('⌘⇧L')).toHaveCount(0);
  await expect(page.locator('.user-menu .kbd')).toHaveCount(0);
  await expect(page.locator(SEL.userMenu)).toHaveAccessibleName(COPY.accountMenuName);
});
