// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Both tab families as named WAI-ARIA tablists (issue #159, ADR-0028).
//
// Two properties, and the two strips prove them against different
// evidence on purpose.
//
// MANUAL ACTIVATION. Arrows move focus and nothing else; Enter or Space
// selects. The workspace strip's selection is local component state, so
// the observable there is the selected tab itself — `aria-selected` and
// the tabindex vector, which are one predicate over the active tab. The
// drawer strip writes `?stab=` into the URL, so the observable there is
// the URL: unchanged after arrowing, changed after Enter. A strip that
// activated on arrow would rewrite the URL under a user who was only
// looking around.
//
// WHAT THE TABLIST CONTAINS. The `role="tablist"` node wraps the tab
// buttons and nothing else. The workspace strip's Save/Export actions
// and the drawer strip's metadata live in the outer container, so a
// screen reader counts two tabs and three tabs, not four and five.

import { test, expect, resetScenario, POPULATED } from '../fixtures';
import { SEL } from '../selectors';

type Loc = import('@playwright/test').Locator;

/** The tabindex of every tab in the strip, in render order. */
function tabindexes(tabs: Loc): Promise<(string | null)[]> {
  return tabs.evaluateAll((els) => els.map((el) => el.getAttribute('tabindex')));
}

/** Whether each tab reports itself selected, in render order. */
function selectedFlags(tabs: Loc): Promise<(string | null)[]> {
  return tabs.evaluateAll((els) => els.map((el) => el.getAttribute('aria-selected')));
}

/** Whether this element sits inside a tablist. The negative is the
 * assertion: an action announced as a tab is the defect. */
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

  // The selected tab is the strip's only tab stop.
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  await tabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  // Focus moved; selection did not, so neither did the tab stop.
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  // Wrap, then the endpoints — all focus-only.
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(0)).toBeFocused();
  await page.keyboard.press('End');
  await expect(tabs.nth(1)).toBeFocused();
  await page.keyboard.press('Home');
  await expect(tabs.nth(0)).toBeFocused();
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);

  // Enter is what selects, and it moves the tab stop with the selection.
  await page.keyboard.press('End');
  await page.keyboard.press('Enter');
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  expect(await selectedFlags(tabs)).toEqual(['false', 'true']);
  expect(await tabindexes(tabs)).toEqual(['-1', '0']);

  // The trailing actions are in `.tabs`, outside the tablist.
  await expect(page.locator(SEL.saveAction)).toBeVisible();
  expect(await insideTablist(page.locator(SEL.saveAction))).toBe(false);
  expect(await insideTablist(page.locator(SEL.exportAction))).toBe(false);
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

  // Arrowing across the whole strip leaves `?stab=` exactly as it was.
  await tabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(2)).toBeFocused();
  await page.keyboard.press('ArrowLeft');
  await expect(tabs.nth(1)).toBeFocused();
  await expect(page).toHaveURL(/[?&]stab=overview(&|$)/);
  expect(await selectedFlags(tabs)).toEqual(['true', 'false', 'false']);

  // Enter is what writes the URL.
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/[?&]stab=fields(&|$)/);
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  expect(await tabindexes(tabs)).toEqual(['-1', '0', '-1']);

  // The drawer's own furniture is outside the tablist: its title, its
  // close button and the strip's trailing metadata.
  expect(await insideTablist(page.locator(SEL.drawerTitle))).toBe(false);
  expect(await insideTablist(page.locator(SEL.drawerClose))).toBe(false);
  const meta = page.locator('.sd-tabs .meta');
  await expect(meta).toHaveCount(1);
  expect(await insideTablist(meta)).toBe(false);
});

test('an unknown drawer tab id still yields one tab stop', async ({ page, request }) => {
  // `?ntab=` is client text. A strip that answered "none of these"
  // rendered every tab at tabindex="-1" — no keyboard entry point at
  // all — while the pane below it showed the first tab's content. An
  // unmatched id selects the first tab, so markup and pane agree.
  await resetScenario(request, 'populated');
  await page.goto(`/jobs/nets?net=${POPULATED.netId}&ntab=bogus`);

  await expect(page.locator(SEL.drawerPanel)).toBeVisible();
  const strip = page.getByRole('tablist', { name: 'Saved query details' });
  await expect(strip).toHaveCount(1);

  const tabs = page.locator(SEL.drawerTab);
  await expect(tabs).toHaveCount(2);
  expect(await selectedFlags(tabs)).toEqual(['true', 'false']);
  expect(await tabindexes(tabs)).toEqual(['0', '-1']);

  // The strip is reachable by Tab: the drawer's close button is the
  // last control before it in DOM order.
  await page.locator(SEL.drawerClose).focus();
  await page.keyboard.press('Tab');
  await expect(tabs.nth(0)).toBeFocused();

  // And once inside, the arrow walk works from that entry point.
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await expect(tabs.nth(1)).toHaveAccessibleName('Runs');
});
