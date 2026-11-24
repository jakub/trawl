// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The fixture contract fails the test on any `pageerror`, which is how a
// stack overflow in the wasm module (a RangeError) surfaces.

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL } from '../selectors';
import type { Page } from '@playwright/test';

/// The parser's length limit, `MAX_QUERY_LEN` in trawl-core.
const MAX_QUERY_LEN = 64 * 1024;

/// The most levels the search pre-scan admits, `MAX_SEARCH_PRESCAN` in
/// trawl-core.
const MAX_SEARCH_PRESCAN = 512;

function bracketsToTheLimit(): string {
  const n = Math.floor((MAX_QUERY_LEN - 'a=1'.length) / 2);
  return `${'('.repeat(n)}a=1${')'.repeat(n)}`;
}

function notsToTheLimit(): string {
  const n = Math.floor((MAX_QUERY_LEN - 'a=1'.length) / 'NOT '.length);
  return `${'NOT '.repeat(n)}a=1`;
}

const DEEP_OR = `* | where ${Array.from({ length: 65 }, (_, i) => `a == ${i}`).join(' or ')}`;

/// Typing 64 KiB key by
/// key would reparse the draft once per keystroke and test nothing more.
async function replaceDraft(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

/// The editor's whole document, read from CodeMirror's state rather than
/// the DOM, which renders only what is in view. The view is reached the
/// way the vendored build's `EditorView.findFromDOM` does: the content
/// element's tile.
async function editorText(page: Page): Promise<string> {
  return page.locator(SEL.cmContent).evaluate(el => {
    const tile = (el as unknown as { cmTile?: { view?: unknown; root?: { view?: unknown } } }).cmTile;
    const view = (tile?.view ?? tile?.root?.view) as { state: { doc: { toString(): string } } } | undefined;
    if (!view) throw new Error('no CodeMirror view on the content element');
    return view.state.doc.toString();
  });
}

async function expectResponsive(page: Page) {
  const answered = await Promise.race([
    page.evaluate(() => new Promise<boolean>(resolve => requestAnimationFrame(() => resolve(true)))),
    new Promise<boolean>(resolve => setTimeout(() => resolve(false), 2_000)),
  ]);
  expect(answered, 'the page did not answer within 2 s').toBe(true);
}

test('the editor survives a query deeper than 64 levels and runs a shallow one afterwards', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?r=15m');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  const brackets = bracketsToTheLimit();
  const nots = notsToTheLimit();
  for (const text of [brackets, nots]) {
    expect(text.length).toBeLessThanOrEqual(MAX_QUERY_LEN);
    expect(text.length).toBeGreaterThan(MAX_QUERY_LEN - 'NOT '.length);
  }
  // The pre-scan's refusal starts at the 513th bracket or `NOT`.
  const bracketColumn = MAX_SEARCH_PRESCAN + 1;
  const notColumn = MAX_SEARCH_PRESCAN * 'NOT '.length + 1;
  const cases = [
    { text: brackets, diagnostic: new RegExp(`^Line 1:${bracketColumn} — search is deeper than 64 levels`) },
    { text: nots, diagnostic: new RegExp(`^Line 1:${notColumn} — search is deeper than 64 levels`) },
    { text: DEEP_OR, diagnostic: /^Line 1:11 — expression is deeper than 64 levels/ },
  ];
  for (const { text, diagnostic } of cases) {
    await replaceDraft(page, text);
    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(diagnostic);
    expect(await editorText(page)).toBe(text);
    await expectResponsive(page);
  }

  const shallow = `service=${CORPUS.service}`;
  await replaceDraft(page, shallow);
  await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
  await page.keyboard.press('Control+Enter');
  await expect(page).toHaveURL(new RegExp(`[?&]q=${encodeURIComponent(shallow)}(&|$)`));
  await expect(page.locator(SEL.resultsRow)).toHaveCount(CORPUS.rowCount);
});
