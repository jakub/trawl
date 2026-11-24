// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;

/** Count clicks that reach the tool row, i.e. that were allowed to
 * bubble out of the button that was pressed. */
async function countBubbledToolClicks(page: Pg): Promise<void> {
  await page.evaluate(() => {
    const row = document.querySelector('.editor-tools');
    if (!row) throw new Error('.editor-tools is not mounted');
    (window as any).__e2eToolClicks = 0;
    row.addEventListener('click', () => {
      (window as any).__e2eToolClicks += 1;
    });
  });
}

function bubbledToolClicks(page: Pg): Promise<number> {
  return page.evaluate(() => (window as any).__e2eToolClicks as number);
}

test('modal close is a named button that closes on Enter', async ({ page }) => {
  await page.goto('/search');
  await page.locator(SEL.exportAction).click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();

  const close = page.locator(SEL.modalClose);
  await expect(close).toHaveJSProperty('tagName', 'BUTTON');
  await expect(close).toHaveAttribute('type', 'button');
  await expect(close).toHaveRole('button');
  await expect(close).toHaveAccessibleName(COPY.modalCloseName);

  await close.focus();
  await expect(close).toBeFocused();
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
});

test('toast dismiss is a named button that removes one toast', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/search');

  const share = page.locator(SEL.editorTool).filter({ hasText: COPY.copyUrlTool });
  await share.click();
  await expect(page.locator(SEL.toastAny)).toHaveCount(1);
  await share.click();
  await expect(page.locator(SEL.toastAny)).toHaveCount(2);

  const dismiss = page.locator(SEL.toastDismiss).first();
  await expect(dismiss).toHaveJSProperty('tagName', 'BUTTON');
  await expect(dismiss).toHaveAttribute('type', 'button');
  await expect(dismiss).toHaveRole('button');
  await expect(dismiss).toHaveAccessibleName(COPY.toastDismissName);

  await dismiss.focus();
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.toastAny)).toHaveCount(1);
});

test('bare copy button copies on Space and stops propagation', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  const share = page.locator(SEL.editorTool).filter({ hasText: COPY.copyUrlTool });
  const save = page.locator(SEL.editorTool).filter({ hasText: COPY.saveAsNetTool });
  await expect(share).toHaveCount(1);
  await expect(share).toHaveJSProperty('tagName', 'BUTTON');
  await expect(share).toHaveAttribute('type', 'button');
  await expect(share).toHaveAccessibleName(COPY.copyUrlTool);

  await countBubbledToolClicks(page);

  // Control first: the Save tool next to it does NOT stop propagation,
  // so its press reaches the row and the counter is known to work.
  await save.click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  expect(await bubbledToolClicks(page)).toBe(1);

  await share.focus();
  await expect(share).toBeFocused();
  await page.keyboard.press(' ');

  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toContain('/search');
  await expect(page.locator(SEL.toastAny)).toHaveCount(1);
  expect(await bubbledToolClicks(page)).toBe(1);
});
