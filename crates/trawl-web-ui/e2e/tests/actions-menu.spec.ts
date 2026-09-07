// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The row overflow menu on the shared contract (issue #159, ADR-0028).
//
// `ActionsMenu` had the shape of this pattern before the rebuild and
// none of its guarantees: every item was tabbable, the arrow walk hopped
// element siblings, and focus returned to the trigger on Escape only.
// It now mounts the same `menu::MenuPanel` the account menu does, so
// this spec is the row-side half of one contract rather than a second
// menu's tests.
//
// The ordering proof is the interesting one. Activation closes the menu
// and restores the trigger BEFORE running the callback, so a callback
// that opens a dialog captures the trigger as that dialog's opener. Run
// the callback first and the dialog's opener is whatever the disposal
// left behind — <body> — and dismissing it drops the user at the top of
// the document. Escaping the confirm dialog and finding the SAME
// trigger focused is what distinguishes the two orders from outside.

import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

test('actions menu has one tab stop and restores its trigger', async ({ page, request }) => {
  await resetScenario(request, 'populated');
  await page.goto('/jobs/nets');

  const trigger = page.locator(SEL.actionsMenuTrigger);
  await expect(trigger).toHaveCount(1);
  await expect(trigger).toHaveJSProperty('tagName', 'BUTTON');
  await expect(trigger).toHaveAttribute('type', 'button');
  await expect(trigger).toHaveAccessibleName(COPY.actionsName);

  await trigger.focus();
  await page.keyboard.press('Enter');

  const menu = page.locator(`.actions-menu [role="menu"]`);
  await expect(menu).toHaveAccessibleName(COPY.actionsName);

  const items = page.locator(SEL.actionsMenuItem);
  await expect(items).toHaveCount(3);
  await expect(items.first()).toBeFocused();
  // One tab stop, across every item — the claim is about the whole
  // vector, not about the item that happens to have focus.
  expect(
    await items.evaluateAll((els) => els.map((el) => el.getAttribute('tabindex'))),
  ).toEqual(['0', '-1', '-1']);

  // Walk to the destructive item. The walk indexes role="menuitem",
  // so the separator-free list here is the same list the arrows see.
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowDown');
  const destructive = items.nth(2);
  await expect(destructive).toBeFocused();
  await expect(destructive).toHaveRole('menuitem');
  expect(
    await items.evaluateAll((els) => els.map((el) => el.getAttribute('tabindex'))),
  ).toEqual(['-1', '-1', '0']);

  // Enter activates: the menu closes and the callback opens the delete
  // confirmation.
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.actionsMenuItem)).toHaveCount(0);
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  // The item's click never reached the row underneath it, so no drawer
  // opened behind the dialog.
  await expect(page).toHaveURL(/\/jobs\/nets$/);

  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  await expect(trigger).toBeFocused();
});
