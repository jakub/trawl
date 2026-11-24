// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Query advice (ADR-0068).

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page } from '@playwright/test';

const ADVICE = '"10.244.0.83" -service=coredns';
const CORRECTED = '"10.244.0.83" service!=coredns';
/// An unknown escape after a one-unit CJK glyph and a two-unit emoji: the
/// pair sits at byte 9, UTF-16 offset 5, character column 5.
const ESCAPE = '"雪🌊 \\s"';

const negatedServiceLine = (column: number) =>
  `Line 1:${column} — -service=… is a text search, not a field filter: it drops events whose message or _raw contain that text — ` +
  'to filter the field, write service!=… (keeps events with no service) or NOT service=… (drops them too); ' +
  'to keep the text search and silence this, write NOT "service=…"';

async function typeDraft(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

function recordPaths(page: Page) {
  const paths: string[] = [];
  page.on('request', req => paths.push(new URL(req.url()).pathname));
  return paths;
}

const adviceMarker = (page: Page) => page.getByRole('img', { name: COPY.adviceMarkerName, exact: true });
const errorMarker = (page: Page) => page.getByRole('img', { name: COPY.lintMarkerName, exact: true });
const editorBlock = (page: Page) =>
  page.locator(SEL.dslEditor).locator('xpath=ancestor::div[@id="search-query"]');

test.describe('query advice', () => {
  test('marks a negated field shape as advice and clears when corrected', async ({ page, request }, testInfo) => {
    const paths = recordPaths(page);
    await page.goto('/search?r=15m');
    await typeDraft(page, ADVICE);

    const line = page.locator(SEL.draftDiagnosticFirst);
    await expect(page.locator(SEL.draftDiagnosticAdvice)).toBeVisible();
    await expect(line).toHaveText(
      /^Line 1:15 — -service=… is a text search, not a field filter: it drops events whose message or _raw contain that text — to filter the field, write service!=… /,
    );
    await expect(line).toContainText('or NOT service=… (drops them too); to keep the text search and silence this, write NOT "service=…"');
    await expect(page.locator(SEL.draftDiagnosticMore)).toHaveCount(0);
    await expect(page.locator(SEL.draftDiagnosticOmitted)).toHaveCount(0);

    await expect(adviceMarker(page)).toHaveCount(1);
    await expect(errorMarker(page)).toHaveCount(0);
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-warning`)).toHaveText('-service=coredns');
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-error`)).toHaveCount(0);
    await editorBlock(page).screenshot({ path: testInfo.outputPath('browser-advice.png') });

    await page.keyboard.press('Control+Enter');
    await expect.poll(async () => {
      const state = await (await request.get('/__ctl/state')).json();
      return state.queries.at(-1)?.query;
    }).toBe(`last=15m ${ADVICE}`);

    await typeDraft(page, CORRECTED);
    await expect(page.locator(SEL.draftDiagnostic)).toHaveCount(0);
    await expect(adviceMarker(page)).toHaveCount(0);
    await expect(errorMarker(page)).toHaveCount(0);
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-warning`)).toHaveCount(0);
    await editorBlock(page).screenshot({ path: testInfo.outputPath('browser-corrected.png') });

    // The observer saw the haul, so its silence on /validate counts.
    expect(paths).toContain('/api/v1/query');
    expect(paths.filter(p => p.includes('/validate'))).toEqual([]);
  });

  test('keeps further advice behind the disclosure and counts what the cap left out', async ({ page }) => {
    const paths = recordPaths(page);
    await page.goto('/search?r=15m');
    await typeDraft(page, Array.from({ length: 10 }, (_, i) => `-service=s${i}`).join(' '));

    await expect(page.locator(SEL.draftDiagnosticAdvice)).toBeVisible();
    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(negatedServiceLine(1));
    const more = page.locator(SEL.draftDiagnosticMore);
    await expect(more.locator('summary')).toHaveText('+7 more');
    await more.locator('summary').click();
    const entries = more.locator('li');
    await expect(entries).toHaveCount(7);
    for (let i = 0; i < 7; i++) {
      await expect(entries.nth(i)).toBeVisible();
      await expect(entries.nth(i)).toHaveText(negatedServiceLine(1 + 12 * (i + 1)));
    }
    await expect(page.locator(SEL.draftDiagnosticOmitted)).toHaveText('and 2 more advice notes not shown');
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-warning`)).toHaveCount(8);

    // The observer saw the page load, so its silence on /validate counts.
    expect(paths).toContain('/search');
    expect(paths.filter(p => p.includes('/validate'))).toEqual([]);
  });

  test('an error takes precedence over advice', async ({ page }) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, `-service=coredns | where m matches "^\\s+at "`);

    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(/^Line 1:38 — unknown escape \\s: /);
    await expect(page.locator(SEL.draftDiagnosticAdvice)).toHaveCount(0);
    await expect(errorMarker(page)).toHaveCount(1);
    await expect(adviceMarker(page)).toHaveCount(0);
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-warning`)).toHaveCount(0);
  });

  test('an unknown escape after multibyte text marks the backslash pair', async ({ page }, testInfo) => {
    await page.goto('/search?r=15m');
    await typeDraft(page, ESCAPE);

    await expect(page.locator(SEL.draftDiagnosticFirst)).toHaveText(
      /^Line 1:5 — unknown escape \\s: a quoted string decodes only /,
    );
    await expect(page.locator(SEL.draftDiagnosticAdvice)).toHaveCount(0);
    await expect(errorMarker(page)).toHaveCount(1);
    await expect(page.locator(`${SEL.dslEditor} .cm-lintRange-error`)).toHaveText('\\s');
    await editorBlock(page).screenshot({ path: testInfo.outputPath('browser-escape-utf16.png') });

    await page.keyboard.press('F8');
    await expect.poll(() => page.evaluate(() => window.getSelection()?.toString())).toBe('\\s');
  });
});
