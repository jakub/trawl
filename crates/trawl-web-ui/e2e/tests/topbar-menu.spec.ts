// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The topbar account menu on the shared menu contract (issue #159,
// ADR-0028).
//
// Everything here is keyboard-first, because that is what the rebuild
// bought: before it, the trigger was a <div> with no role and no
// tabindex, the panel never registered with the overlay stack, and
// Escape did nothing at all.
//
// Three observables, and they are not interchangeable:
//
//   1. `toBeFocused` — where the browser's own focus went. The only
//      thing that can tell "the menu closed" from "the menu closed and
//      left the user's place intact".
//   2. The tabindex VECTOR across every item. One item at "0" is the
//      whole roving-tabindex claim; asserting only the focused item's
//      value would pass with every item at "0", which is exactly the
//      regression a modal's Tab cycle would walk into.
//   3. Which layers are still mounted after a keypress. Escape is
//      topmost-only, so a modal opened above the menu has to eat the
//      first one.

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;

/** The tabindex of every menu item, in render order. */
async function itemTabindexes(page: Pg): Promise<(string | null)[]> {
  return page.locator(SEL.userMenuItem).evaluateAll((els) =>
    els.map((el) => el.getAttribute('tabindex')),
  );
}

/** Open the account menu the way a keyboard user does: focus the
 * trigger, press Enter. Never a mouse click — a pointer open would
 * leave focus on the trigger for a reason that has nothing to do with
 * the menu's own initial-focus effect. */
async function openByKeyboard(page: Pg): Promise<void> {
  await expect(page.locator(SEL.topbarUser)).toBeEnabled();
  await page.locator(SEL.topbarUser).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.userMenu)).toBeVisible();
}

test('keyboard open focuses the first item and arrows walk with wrap', async ({ page }) => {
  await page.goto('/search');
  await openByKeyboard(page);

  const items = page.locator(SEL.userMenuItem);
  await expect(items).toHaveCount(2);
  const first = items.nth(0);
  const last = items.nth(1);

  await expect(first).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['0', '-1']);

  await page.keyboard.press('ArrowDown');
  await expect(last).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['-1', '0']);

  // Past the end wraps to the first rather than escaping the menu.
  await page.keyboard.press('ArrowDown');
  await expect(first).toBeFocused();

  // And before the start wraps to the last.
  await page.keyboard.press('ArrowUp');
  await expect(last).toBeFocused();

  await page.keyboard.press('Home');
  await expect(first).toBeFocused();
  await page.keyboard.press('End');
  await expect(last).toBeFocused();
  expect(await itemTabindexes(page)).toEqual(['-1', '0']);

  // The identity header sits before and OUTSIDE role="menu", so the
  // walk cannot land on it: the panel carries a header, the menu node
  // holds only the two items.
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

  // Tab: the menu closes and focus lands back on the trigger, rather
  // than walking into the panel's items (which is what the FOCUSABLE
  // fix and the roving tabindex together rule out).
  await openByKeyboard(page);
  await page.keyboard.press('Tab');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();

  // Shift+Tab is the same cause and the same answer.
  await openByKeyboard(page);
  await page.keyboard.press('Shift+Tab');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
});

test('theme item restores the trigger and flips the theme', async ({ page }) => {
  await page.goto('/search');
  const themeAttr = () => page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  const before = await themeAttr();

  await openByKeyboard(page);
  // The first item IS the theme item, and it already has focus.
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  // Activation returns focus to the trigger. This says nothing about
  // whether restore runs before or after the item's callback: the theme
  // callback never moves focus, so the ordering is unobservable here.
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
  await expect.poll(themeAttr).not.toBe(before);
});

test('outside mousedown leaves focus on the target', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
  await openByKeyboard(page);

  // A real pointer press on the editor: dismissal runs off mousedown,
  // and focus belongs to what the user pressed, not to the trigger.
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
  // mounted, which is a different test. This leaves BOTH layers up,
  // which is the only arrangement that can prove Escape is
  // topmost-only.
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

  // ADR-0025's dispositions, as rendered: the notifications bell (never
  // wired), the two disabled account rows, and the theme item's ⌘⇧L
  // hint chip (no chord is bound, so the hint would be a lie).
  await expect(page.locator('.topbar .iconbtn')).toHaveCount(0);
  await openByKeyboard(page);
  await expect(page.locator('.user-menu').getByText('Profile', { exact: true })).toHaveCount(0);
  await expect(page.locator('.user-menu').getByText('API tokens', { exact: true })).toHaveCount(0);
  await expect(page.locator('.user-menu').getByText('⌘⇧L')).toHaveCount(0);
  await expect(page.locator('.user-menu .kbd')).toHaveCount(0);
  // The account panel's accessible name is still there to be found.
  await expect(page.locator(SEL.userMenu)).toHaveAccessibleName(COPY.accountMenuName);
});
