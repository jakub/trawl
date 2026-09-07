// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The three remaining pseudo-buttons, now native (issue #159,
// ADR-0028): the modal's header close, a toast's dismiss and the bare
// mode of `CopyButton`.
//
// Each was a <div> or <span> with an `on:click` and nothing else — no
// role, no tabindex, no keydown — so a keyboard user could not reach
// them at all and a screen reader announced the close glyph as the text
// "×". The assertions are therefore about the ELEMENT as much as the
// behaviour: tag name, `type="button"` (a bare <button> inside a form
// would submit it), and an accessible name that is a sentence rather
// than a glyph.
//
// The copy test also proves the propagation stop survived the
// conversion. That is what keeps a copy trigger inside a clickable row
// from opening the row, and it is measured against a control press that
// does bubble — a counter nothing can increment proves nothing.

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
  // The glyph is an icon, so aria-label is the entire accessible name.
  await expect(close).toHaveAccessibleName(COPY.modalCloseName);

  await close.focus();
  await expect(close).toBeFocused();
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
});

test('toast dismiss is a named button that removes one toast', async ({ page }) => {
  await page.goto('/search');

  // Two toasts, so "removes one" is distinguishable from "clears the
  // host". The results strip's Save raises an informational toast and
  // nothing else, which makes it the cheapest producer on the page.
  await page.locator(SEL.saveAction).click();
  await page.locator(SEL.saveAction).click();
  await expect(page.locator(SEL.toastAny)).toHaveCount(2);

  const dismiss = page.locator(SEL.toastDismiss).first();
  await expect(dismiss).toHaveJSProperty('tagName', 'BUTTON');
  await expect(dismiss).toHaveAttribute('type', 'button');
  await expect(dismiss).toHaveRole('button');
  // "×" is aria-hidden, so the name is the label and not "times".
  await expect(dismiss).toHaveAccessibleName(COPY.toastDismissName);

  await dismiss.focus();
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.toastAny)).toHaveCount(1);
});

test('bare copy button copies on Space and stops propagation', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  const share = page.locator(SEL.editorTool).filter({ hasText: 'Share' });
  const save = page.locator(SEL.editorTool).filter({ hasText: 'Save' });
  await expect(share).toHaveCount(1);
  await expect(share).toHaveJSProperty('tagName', 'BUTTON');
  await expect(share).toHaveAttribute('type', 'button');
  await expect(share).toHaveAccessibleName('Share');

  await countBubbledToolClicks(page);

  // Control first: the Save tool next to it does NOT stop propagation,
  // so its press reaches the row and the counter is known to work.
  await save.click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  expect(await bubbledToolClicks(page)).toBe(1);

  // Space on a native button is a click. Nothing else on this page
  // could produce one: the trigger was a <span> before ADR-0028.
  await share.focus();
  await expect(share).toBeFocused();
  await page.keyboard.press(' ');

  await expect
    .poll(() => page.evaluate(() => navigator.clipboard.readText()))
    .toContain('/search');
  // The copy announced itself, and the press never reached the row.
  await expect(page.locator(SEL.toastAny)).toHaveCount(1);
  expect(await bubbledToolClicks(page)).toBe(1);
});
