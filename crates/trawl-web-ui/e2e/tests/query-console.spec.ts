// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The query console: its draft state, its executed-scope strip, and the
// two keyboard bypasses that open the page (ADR-0027, ADR-0032).
//
// ONE PROPERTY RUNS THROUGH ALL OF IT. The console header describes the
// EDITOR BUFFER; the strip under it describes the QUERY THE LINK RAN.
// Typing moves the first and must never move the second — which is the
// distinction ADR-0027 draws, now said in words on screen. So the
// evidence is a pair: after an edit the header must flip AND the strip's
// window must be the one the histogram caption still names. Asserting
// only the header would pass a strip that silently followed the buffer.

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { APIRequestContext, Page } from '@playwright/test';

// Literal link, independent of the app's own encoder: `q` is the
// executed query, `r` the 15-minute window, and the base64url `f`
// payload is one include filter on `host`.
const FILTERED_URL = '/search?q=service%3Dnginx&page=0' +
  '&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0';

async function editBuffer(page: Page, text: string) {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(text);
}

test('the header states the draft while the strip stays with the executed query', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&r=15m');

  const draft = page.locator(SEL.draftState);
  await expect(draft).toHaveText(COPY.draftClean);

  // The strip's window and the histogram caption are the same sentence
  // about the same execution, so they are asserted together.
  await expect(page.locator(SEL.scopeWindow)).toHaveText('last 15m');
  await expect(page.locator(SEL.histoCaption)).toHaveText(`${COPY.histoCaptionPrefix}last 15m`);
  await expect(page.locator(SEL.scopeStrip)).toContainText('Executed scope');
  await expect(page.locator(`${SEL.scopeStrip} .mode`)).toHaveText('Snapshot');
  await expect(page.locator('.scope-count')).toHaveText(`${CORPUS.rowCount} rows`);

  // A buffer whose own time clause would change the window if the strip
  // read the buffer. It does not: nothing has run yet.
  await editBuffer(page, 'last=7d');
  await expect(draft).toHaveText(COPY.draftDirty);
  await expect(page.locator(SEL.scopeWindow)).toHaveText('last 15m');
  await expect(page).toHaveURL(/q=service%3Dnginx/);

  await page.locator(SEL.runButton).click();
  await expect(page).toHaveURL(/q=last%3D7d/);
  await expect(draft).toHaveText(COPY.draftClean);
  await expect(page.locator(SEL.scopeWindow)).toHaveText('last 7d');
});

test('the strip carries the link\'s filter chips', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(FILTERED_URL);

  const chip = page.locator(`${SEL.scopeStrip} ${SEL.filterChip}`);
  await expect(chip).toHaveCount(1);
  await expect(chip).toContainText(`host = ${CORPUS.firstHost}`);
  await expect(page.locator(SEL.chipRemove)).toHaveCount(1);
});

test('a link that cannot be read leaves the strip saying only that', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&f=v1.!');

  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  const strip = page.locator(SEL.scopeStrip);
  await expect(strip.locator(SEL.filtersBadChip)).toHaveText(COPY.filtersUnreadableChip);
  // Nothing ran, so the strip describes nothing: no label, no window, no
  // mode badge, no count, and no removable chip (ADR-0027).
  await expect(strip).toHaveText(COPY.filtersUnreadableChip);
  await expect(page.locator(SEL.scopeWindow)).toHaveCount(0);
  await expect(page.locator('.scope-count')).toHaveCount(0);
  await expect(page.locator(SEL.chipRemove)).toHaveCount(0);
});

test('a malformed range refuses the strip as completely as a malformed filter', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // `f` reads perfectly here; `r` is the parameter the link got wrong.
  // The chips used to survive that, so a refused link still listed the
  // filters of a query that had not run (ADR-0027).
  await page.goto(`${FILTERED_URL}&r=garbage`);

  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  const strip = page.locator(SEL.scopeStrip);
  await expect(strip.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page.locator(SEL.chipRemove)).toHaveCount(0);
  await expect(page.locator(SEL.scopeWindow)).toHaveCount(0);
  await expect(page.locator('.scope-count')).toHaveCount(0);
});

test('the strip counts the response it is describing, never the one before it', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&page=0');
  await expect(page.locator('.scope-count')).toHaveText(`${CORPUS.rowCount} rows`);

  // Hold the next query open. The resource keeps the rows already on
  // screen, so an ungated count would state them under the new scope and
  // on the Events tab beside it.
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route('**/api/v1/query*', async (route) => {
    await held;
    await route.continue();
  });

  await editBuffer(page, 'service=nginx last=7d');
  await page.locator(SEL.runButton).click();

  await expect(page.locator(SEL.scopeWindow)).toHaveText('last 7d');
  await expect(page.locator('.scope-count')).toHaveText('…');
  await expect(page.locator(`${SEL.workspaceTab} .c`)).toHaveCount(0);

  release();
  await expect(page.locator('.scope-count')).toHaveText(`${CORPUS.rowCount} rows`);
});

test('the result header badges a truncated answer', async ({ page, request }) => {
  await truncatedScenario(request);
  await page.goto('/search?q=service%3Dnginx');

  // `.tabs .bdg` is the trailing slot's one badge; the actions beside it
  // are buttons.
  await expect(page.locator('.tabs .bdg')).toHaveText(COPY.truncatedBadge);
  await expect(page.locator(SEL.saveAction)).toBeVisible();
  await expect(page.locator(SEL.exportAction)).toBeVisible();
});

test('the editor tools name what they act on', async ({ page }) => {
  await page.goto('/search');
  const tools = page.locator(SEL.editorTool);
  await expect(tools).toHaveCount(3);
  await expect(tools.nth(0)).toHaveAccessibleName(COPY.saveAsNetTool);
  await expect(tools.nth(1)).toHaveAccessibleName(COPY.copyUrlTool);
  await expect(tools.nth(2)).toHaveAccessibleName('Format');
});

test('the skip links reach the editor and the results', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsPane)).toBeVisible();

  const toQuery = page.locator(SEL.skipToQuery);
  await toQuery.focus();
  await expect(toQuery).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.cmContent)).toBeFocused();

  const toResults = page.getByRole('link', { name: 'Skip to results' });
  await toResults.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.resultsPane)).toBeFocused();
});

// The bypass used to be inert wherever the results region was not the
// snapshot table: the handler prevented the anchor's default and then
// focused nothing at all, so the key press moved neither focus nor the
// document (A03). Both remaining panes are asserted here.
test('skip to results reaches live mode and the Visualization tab', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await expect(page.locator(SEL.resultsPane)).toBeVisible();

  const toResults = page.getByRole('link', { name: 'Skip to results' });
  await toResults.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.resultsPane)).toBeFocused();

  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&page=0');
  await page.locator(SEL.workspaceTab).filter({ hasText: 'Visualization' }).click();
  await expect(page.locator(SEL.resultsPane)).toBeVisible();

  await toResults.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.resultsPane)).toBeFocused();
});

/** The stub's truncated answer: the `pagination` scenario with its
 * `truncated` flag set, the same control `pagination.spec.ts` uses. */
async function truncatedScenario(request: APIRequestContext) {
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'pagination', pagination: { truncated: true } },
  });
  expect(response.ok()).toBeTruthy();
}
