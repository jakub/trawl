// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Query errors (ADR-0039): what the page says when a query is wrong.
//
// Two owners, never crossed. The server's verdict on a SENT query is the
// query error notice in the results region; it quotes the effective text
// that request carried. The local parser's verdict on the DRAFT is the
// draft diagnostic under the editor. Every case here asserts on one of
// the two, and the identity cases assert the notice never quotes a text
// other than the one the failing request sent.

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page } from '@playwright/test';

/// The issue's sample, as typed into the editor. The local parser's first
/// error is at byte 34, the `h` of `host`.
const SAMPLE = 'service=kubelet | stats count( by host';

async function typeDraft(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

test.describe('draft diagnostic', () => {
  test('names the first error in full and clears when the paren closes', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, SAMPLE);

    const line = page.locator(SEL.draftDiagnosticFirst);
    await expect(line).toBeVisible();
    await expect(line).toHaveText(/^Line 1:35 — found 'h', expected /);
    // Full text: the rendered line is not clipped by an ellipsis.
    const clipped = await line.evaluate(el => el.scrollWidth > el.clientWidth);
    expect(clipped).toBe(false);
    await expect(line).not.toHaveAttribute('aria-live');
    await expect(page.locator(SEL.draftDiagnostic).locator('[aria-live]')).toHaveCount(0);
    // One error: no disclosure.
    await expect(page.locator(SEL.draftDiagnosticMore)).toHaveCount(0);

    await typeDraft(page, 'service=kubelet | stats count() by host');
    await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
  });

  test('keeps the second error behind a closed +1 more that closes again on edit', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, 'f=#a,#b');

    // Two local errors, one per unquoted `#`.
    const more = page.locator(SEL.draftDiagnosticMore);
    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(/^Line 1:3 — '#' inside an unquoted value/);
    await expect(more).toBeVisible();
    await expect(more.locator('summary')).toHaveText('+1 more');
    await expect(more).not.toHaveAttribute('open');
    const second = more.locator('li');
    await expect(second).toHaveCount(1);
    await expect(second).toBeHidden();

    await more.locator('summary').click();
    await expect(more).toHaveAttribute('open', '');
    await expect(second).toBeVisible();
    await expect(second).toHaveText(/^Line 1:6 — '#' inside an unquoted value/);

    // An edit that keeps the errors closes the disclosure.
    await page.locator(SEL.cmContent).click();
    await page.keyboard.press('End');
    await page.keyboard.type(' ');
    await expect(more).toBeVisible();
    await expect(more).not.toHaveAttribute('open');
  });

  test('F8 moves to the first diagnostic and the gutter marker is named', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, SAMPLE);
    await expect(page.locator(SEL.draftDiagnosticFirst)).toBeVisible();

    // Named from the accessibility tree, which honours aria-hidden: a
    // marker under a hidden ancestor would not be found by role. The
    // editor's linter runs after a pause, so this also waits for it.
    const marker = page.getByRole('img', { name: COPY.lintMarkerName, exact: true });
    await expect(marker).toHaveCount(1);
    await expect(marker).toHaveAccessibleName(COPY.lintMarkerName);
    await expect(page.locator(SEL.dslEditor)).toMatchAriaSnapshot(`
      - img "${COPY.lintMarkerName}"
    `);
    // Line numbers stay out of the tree.
    const lineNumbersHidden = await page
      .locator(`${SEL.dslEditor} .cm-lineNumbers`)
      .evaluate(el => el.closest('[aria-hidden="true"]') !== null);
    expect(lineNumbersHidden).toBe(true);

    // The cursor sits at the end after typing; F8 wraps to the first
    // diagnostic and selects its span, the `h` of `host`.
    await expect.poll(() => page.evaluate(() => window.getSelection()?.toString())).toBe('');
    await page.keyboard.press('F8');
    await expect.poll(() => page.evaluate(() => window.getSelection()?.toString())).toBe('h');
  });
});
