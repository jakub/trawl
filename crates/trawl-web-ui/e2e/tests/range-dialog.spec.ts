// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The date-range picker as a modal dialog (issue #161, ADR-0029).
//
// It had the shape of a dialog and none of its behaviour: a `<div>`
// trigger no keyboard could reach, a full-screen scrim that blocked the
// pointer, no overlay layer, no Escape, no focus move and no name. It is
// now a `Trap` layer on fleet-ui's overlay stack — Trap rather than
// Capture precisely because its own scrim blocks the pointer, so letting
// Tab walk out into a page the mouse cannot reach is what would strand a
// keyboard user.
//
// Focus restore is the assertion worth reading twice. The hook captures
// `document.activeElement` at mount as the opener and hands focus back to
// it on unmount, and only a focusable element is the active one when the
// press lands. That works because the trigger is a button now; as a div
// the captured opener would have been the editor or the body.

import { test, expect, capturedQueryCount, lastCapturedQuery } from '../fixtures';
import { SEL, COPY } from '../selectors';
import { expectFocusRing } from '../a11y';

type Pg = import('@playwright/test').Page;

/** Focus the trigger the way the tab order reaches it: Shift+Tab from
 * the Run button, which is the focusable immediately after it. */
async function tabToTrigger(page: Pg): Promise<void> {
  await page.locator(SEL.runButton).focus();
  await page.keyboard.press('Shift+Tab');
  await expectFocusRing(page.locator(SEL.dateRangeTrigger));
}

test('opens from the keyboard and focuses the first control', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  const trigger = page.locator(SEL.dateRangeTrigger);
  await expect(trigger).toHaveJSProperty('tagName', 'BUTTON');
  await expect(trigger).toHaveAttribute('type', 'button');
  await expect(trigger).toHaveAttribute('aria-haspopup', 'dialog');
  await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  // Its neighbour, for the same reason: a bare <button> inside a form
  // submits it.
  await expect(page.locator(SEL.runButton)).toHaveAttribute('type', 'button');

  await tabToTrigger(page);
  await page.keyboard.press('Enter');

  const dialog = page.locator(SEL.rangeDialog);
  await expect(dialog).toBeVisible();
  await expect(dialog).toHaveAttribute('aria-modal', 'true');
  await expect(dialog).toHaveAccessibleName(COPY.rangeDialogName);
  await expect(trigger).toHaveAttribute('aria-expanded', 'true');
  // The hook moves focus into the panel: the tab strip's first option.
  await expect(page.locator(SEL.segmentedOption).first()).toBeFocused();

  // Space opens it too — it is a button, so both keys are a click.
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(trigger).toBeFocused();
  await page.keyboard.press(' ');
  await expect(dialog).toBeVisible();
  await expect(page.locator(SEL.segmentedOption).first()).toBeFocused();
});

test('Tab does not leave the dialog', async ({ page }) => {
  await page.goto('/search');
  await tabToTrigger(page);
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.rangeDialog)).toBeVisible();

  // One more press than the panel has controls, so the cycle has to
  // wrap at least once. Anything outside the panel — the Run button
  // above all, which the pointer cannot reach through the scrim — is a
  // failure at the press that lands there.
  const controls = await page.locator(`${SEL.rangeDialog} button:not([disabled])`).count();
  expect(controls).toBeGreaterThan(1);
  for (let i = 0; i <= controls; i += 1) {
    await page.keyboard.press('Tab');
    const inside = await page.evaluate(() => {
      const active = document.activeElement;
      return {
        inPanel: active?.closest('.dr-pop') !== null && active?.closest('.dr-pop') !== undefined,
        isRun: active?.classList.contains('run') ?? false,
      };
    });
    expect(inside.isRun, `Tab ${i + 1} escaped to the Run button`).toBe(false);
    expect(inside.inPanel, `Tab ${i + 1} left the dialog`).toBe(true);
  }
});

test('Escape restores the trigger', async ({ page }) => {
  await page.goto('/search');
  await tabToTrigger(page);
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.rangeDialog)).toBeVisible();

  // Walk off the first control first: restore has to be the opener the
  // hook captured, not "wherever focus already was".
  await page.keyboard.press('Tab');
  await page.keyboard.press('Escape');

  await expect(page.locator(SEL.rangeDialog)).toHaveCount(0);
  await expect(page.locator(SEL.dateRangeTrigger)).toBeFocused();
  await expect(page.locator(SEL.dateRangeTrigger)).toHaveAttribute('aria-expanded', 'false');
});

test('scrim mousedown closes', async ({ page }) => {
  await page.goto('/search');
  await tabToTrigger(page);
  await page.keyboard.press('Enter');
  const dialog = page.locator(SEL.rangeDialog);
  await expect(dialog).toBeVisible();
  await expect(page.locator(SEL.rangeScrim)).toHaveCount(1);

  // A point the scrim covers and the panel does not. Dismissal runs off
  // mousedown, so the press alone is the whole event under test.
  const outside = await dialog.evaluate((el) => {
    const box = el.getBoundingClientRect();
    return { x: Math.max(4, box.left / 2), y: window.innerHeight - 8 };
  });
  await page.mouse.move(outside.x, outside.y);
  await page.mouse.down();

  await expect(dialog).toHaveCount(0);
  // The handler suppresses mousedown's own focus fix-up, so focus lands
  // where Escape leaves it rather than on <body>.
  await expect(page.locator(SEL.dateRangeTrigger)).toBeFocused();
  await page.mouse.up();
});

test('From and To are labelled', async ({ page }) => {
  await page.goto('/search');
  await tabToTrigger(page);
  await page.keyboard.press('Enter');
  await page.locator(SEL.absoluteTab).click();

  const from = page.locator(SEL.dateRangeFrom);
  const to = page.locator(SEL.dateRangeTo);
  await expect(from).toBeVisible();
  // `for` has to resolve to THIS input, which is what an accessible
  // name by label means: a label sitting next to an input names nothing.
  await expect(page.locator(SEL.dateRangeFromLabel)).toHaveCount(1);
  await expect(page.locator(SEL.dateRangeToLabel)).toHaveCount(1);
  const pairs = await page.evaluate(
    ([fromSel, toSel]) =>
      [fromSel, toSel].map((sel) => {
        const label = document.querySelector<HTMLLabelElement>(sel);
        return {
          text: label?.textContent ?? null,
          controlClass: label?.control?.className ?? null,
        };
      }),
    [SEL.dateRangeFromLabel, SEL.dateRangeToLabel],
  );
  expect(pairs[0].controlClass).toBe('dr-from');
  expect(pairs[1].controlClass).toBe('dr-to');
  await expect(from).toHaveAccessibleName(pairs[0].text ?? '');
  await expect(to).toHaveAccessibleName(pairs[1].text ?? '');
});

test('navigator refusal keeps the valid range draft and error in the dialog', async ({ page, request }) => {
  test.setTimeout(35_000);
  // Legal current link, close enough to MAX_SEARCH_BYTES that absolute
  // bounds make the next link too long. ASCII avoids encoding ambiguity.
  const query = 'a'.repeat(32 * 1024 - 40);
  await page.goto(`/search?q=${query}&page=0`);
  await lastCapturedQuery(request, 1);
  await expect(page.locator(SEL.dateRangeTrigger)).toBeEnabled();
  const originalUrl = page.url();
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  const from = '2026-01-01T03:00:00+03:00';
  const to = '2026-01-02T00:00:00Z';
  await page.locator(SEL.dateRangeFrom).fill(from);
  await page.locator(SEL.dateRangeTo).fill(to);
  await page.locator(SEL.dateRangeApply).click();

  await expect(page.locator('.dr-err')).toHaveText("Can't open this search: link too long");
  await expect(page.locator(SEL.toastError)).toHaveCount(0);
  await expect(page.locator(SEL.rangeDialog)).toBeVisible();
  await expect(page.locator(SEL.dateRangeFrom)).toHaveValue(from);
  await expect(page.locator(SEL.dateRangeTo)).toHaveValue(to);
  expect(page.url()).toBe(originalUrl);
  expect(await capturedQueryCount(request)).toBe(1);
});

test('absolute drafts survive tab switches but are discarded on close', async ({ page }) => {
  await page.goto('/search');
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  await page.locator(SEL.dateRangeFrom).fill('raw draft');
  await page.locator(SEL.dateRangeTo).fill('other draft');
  await page.locator(SEL.realtimeTab).click();
  await page.locator(SEL.absoluteTab).click();
  await expect(page.locator(SEL.dateRangeFrom)).toHaveValue('raw draft');
  await expect(page.locator(SEL.dateRangeTo)).toHaveValue('other draft');
  await page.keyboard.press('Escape');
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  await expect(page.locator(SEL.dateRangeFrom)).toHaveValue('');
  await expect(page.locator(SEL.dateRangeTo)).toHaveValue('now');
});

test('both blank bounds close without committing', async ({ page, request }) => {
  await page.goto('/search?q=service%3Dnginx');
  await lastCapturedQuery(request, 1);
  const url = page.url();
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  await page.locator(SEL.dateRangeFrom).fill('  ');
  await page.locator(SEL.dateRangeTo).fill('  ');
  await page.locator(SEL.dateRangeApply).click();
  await expect(page.locator(SEL.rangeDialog)).toHaveCount(0);
  expect(page.url()).toBe(url);
  expect(await capturedQueryCount(request)).toBe(1);
});

test('a same-range external URL change closes and discards the draft', async ({ page, request }) => {
  await page.goto('/search?q=service%3Dnginx');
  await lastCapturedQuery(request, 1);
  await page.locator(SEL.dateRangeTrigger).click();
  const label = await page.locator(SEL.dateRangeTrigger).innerText();
  await page.locator(SEL.absoluteTab).click();
  await page.locator(SEL.dateRangeFrom).fill('discard me');
  // A router-intercepted anchor changes the existing SPA's URL identity.
  await page.evaluate(() => {
    const anchor = document.createElement('a');
    anchor.href = '/search?q=service%3Dapache';
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
  });
  await lastCapturedQuery(request, 2);
  await expect(page.locator(SEL.rangeDialog)).toHaveCount(0);
  await expect(page.locator(SEL.dateRangeTrigger)).toHaveText(label);
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.absoluteTab).click();
  await expect(page.locator(SEL.dateRangeFrom)).toHaveValue('');
});

test('Live refusal uses the editor buffer and remains visible across tabs', async ({ page, request }) => {
  await page.goto('/search?q=service%3Dnginx');
  await lastCapturedQuery(request, 1);
  const url = page.url();
  await page.locator(SEL.cmContent).fill('a'.repeat(32 * 1024));
  await page.locator(SEL.dateRangeTrigger).click();
  await page.locator(SEL.realtimeTab).click();
  await page.getByRole('button', { name: COPY.liveTailButtonText, exact: true }).click();
  await expect(page.locator('.dr-err')).toHaveText("Can't open this search: link too long");
  await page.locator(SEL.absoluteTab).click();
  await expect(page.locator('.dr-err')).toBeVisible();
  await expect(page.locator(SEL.toastError)).toHaveCount(0);
  expect(page.url()).toBe(url);
  expect(await capturedQueryCount(request)).toBe(1);
});
