// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The query console: its draft state, its execution facts, and the
// two keyboard bypasses that open the page (ADR-0027, ADR-0032).
//
// ONE PROPERTY RUNS THROUGH ALL OF IT. The console header describes the
// EDITOR BUFFER; the strip under it describes the QUERY THE LINK RAN.
// Typing moves the first and must never move the second — which is the
// distinction ADR-0027 draws, now said in words on screen. So the
// evidence is a pair: after an edit the header must flip AND the strip's
// execution facts must still describe the accepted response. Asserting
// only the header would pass a strip that silently followed the buffer.

import { test, expect, resetScenario, CORPUS, capturedQueryCount, lastCapturedQuery } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page } from '@playwright/test';

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

async function expectFacts(page: Page, duration = '0.125s', started = '2026-09-15 12:34:56 UTC') {
  await expect(page.locator(SEL.scopeExecution)).toHaveText(`Execution in ${duration}`);
  await expect(page.locator(SEL.scopeStarted)).toHaveText(`Started ${started}`);
}

async function noTiming(page: Page) {
  await expect(page.locator(SEL.scopeExecution)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
}

async function responseRendered(page: Page) {
  // Let the Wasm fetch continuation and reactive DOM updates run after the
  // response body finishes, before asserting that an obsolete reply did nothing.
  await page.evaluate(() => new Promise<void>(resolve => {
    requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
  }));
}

// Router navigation preserves the mounted Search resource for ownership races.
async function navigateSearch(page: Page, query: string) {
  await page.evaluate((search) => {
    const link = document.createElement('a');
    link.href = `/search${search}`;
    document.body.append(link);
    link.click();
    link.remove();
  }, query);
}

function queryResponse(started: string, duration: number, rows: unknown[][] = [['accepted']]) {
  return {
    columns: [{ name: 'message' }], rows,
    pagination: { limit: 50, offset: 0, returned: rows.length, total: rows.length },
    execution: { started_at: started, duration_ms: duration },
  };
}

test('the header states the draft while the strip stays with the executed query', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.route('**/api/v1/query', async route => {
    const response = await route.fetch();
    const body = await response.json();
    if (route.request().postDataJSON().query.includes('last=7d')) {
      body.execution = { started_at: '2026-09-15T12:35:00Z', duration_ms: 250 };
    }
    await route.fulfill({ response, json: body });
  });
  await page.goto('/search?q=service%3Dnginx&r=15m');

  const draft = page.locator(SEL.draftState);

  // The strip describes the executed query, independent of later edits.
  await expectFacts(page);
  await expect(page.locator(SEL.scopeStrip)).not.toContainText('Executed scope');
  await expect(page.locator(`${SEL.scopeStrip} .mode`)).toHaveCount(0);
  await expect(page.locator(SEL.scopeCount)).toHaveText(`${CORPUS.rowCount} rows returned`);

  await expect(draft).toHaveCount(0);

  // The edited query has not run, so it cannot change the execution facts.
  await editBuffer(page, 'last=7d');
  await expect(draft).toHaveText(COPY.draftDirty);
  await expectFacts(page);
  await expect(page).toHaveURL(/q=service%3Dnginx/);

  await page.locator(SEL.runButton).click();
  await expect(page).toHaveURL(/q=last%3D7d/);
  await expect(draft).toHaveCount(0);
  await expectFacts(page, '0.250s', '2026-09-15 12:35:00 UTC');
});

test('an unrun search gives both tabs the same guidance and runs the example in Events', async ({ page, request }) => {
  await page.goto('/search?r=15m');
  const guidance = page.locator('.search-quick-start');
  await expect(guidance.getByRole('heading', { name: 'Quick start', exact: true })).toBeVisible();
  await expect(guidance.locator('.qs-example')).toHaveCount(4);
  expect(await capturedQueryCount(request)).toBe(0);
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(guidance).toBeVisible();
  await expect(guidance.locator('.qs-example')).toHaveCount(4);
  await expect(page.getByText('No events on this page.', { exact: true })).toHaveCount(0);
  await expect(page.locator('.uplot')).toHaveCount(0);
  expect(await capturedQueryCount(request)).toBe(0);
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(guidance).toBeVisible();
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(guidance).toBeVisible();
  expect(await capturedQueryCount(request)).toBe(0);
  await guidance.getByRole('button', { name: 'Run Explore events', exact: true }).click();
  expect((await lastCapturedQuery(request, 1)).query).toBe('last=15m * | head 20');
  await expect(page.getByRole('tab', { name: /^Events/ })).toHaveAttribute('aria-selected', 'true');
  await expect(guidance).toHaveCount(0);
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
  // Nothing ran, so there are no facts, mode badge or removable chips.
  await expect(strip).toHaveText(COPY.filtersUnreadableChip);
  await noTiming(page);
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
  await noTiming(page);
  await expect(page.locator('.scope-count')).toHaveCount(0);
});

test('the strip counts the response it is describing, never the one before it', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&page=0');
  await expect(page.locator(SEL.scopeCount)).toHaveText(`${CORPUS.rowCount} rows returned`);
  await expectFacts(page);

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

  await noTiming(page);
  await expect(page.locator('.scope-count')).toHaveText('…');
  await expect(page.locator(`${SEL.workspaceTab} .c`)).toHaveCount(0);

  release();
  await expect(page.locator(SEL.scopeCount)).toHaveText(`${CORPUS.rowCount} rows returned`);
  await expectFacts(page);
});

test('never-run has no execution, while zero rows and zero duration are valid', async ({ page }) => {
  let requests = 0;
  await page.route('**/api/v1/query', async route => {
    requests += 1;
    await route.fulfill({ json: queryResponse('2026-09-15T12:34:56Z', 0, []) });
  });
  await page.goto('/search');
  await expect(page.locator(SEL.scopeCount)).toHaveText('');
  await noTiming(page);
  expect(requests).toBe(0);
  await editBuffer(page, 'service=empty');
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.scopeCount)).toHaveText('0 rows returned');
  await expectFacts(page, '0ms');
  expect(requests).toBe(1);
});

test('aggregate groups count as returned rows and malformed timestamps have no UTC fallback', async ({ page }) => {
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    ...queryResponse('not-a-timestamp', 9, [['web', 42], ['db', 7]]),
    columns: [{ name: 'host' }, { name: 'count' }],
  } }));
  await page.goto(`/search?q=${encodeURIComponent('* | stats count() by host')}`);
  await expect(page.locator(SEL.scopeCount)).toHaveText('2 rows returned');
  await expect(page.locator(SEL.scopeExecution)).toHaveText('Execution in 9ms');
  await expect(page.locator(SEL.scopeStarted)).toHaveCount(0);
  await expect(page.locator(SEL.scopeStrip)).not.toContainText('not-a-timestamp');
});

test('page changes report the execution for that response page', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/search?q=service%3Dnginx');
  await expectFacts(page);
  await expect(page.locator(SEL.scopeCount)).toHaveText('50 rows returned');
  await page.locator('.results-footer').getByRole('button', { name: /^Next/ }).click();
  await expect(page).toHaveURL(/page=1/);
  await expectFacts(page, '0.175s', '2026-09-15 12:35:46 UTC');
  // The pagination fixture has 53 rows, so the second page returns three.
  await expect(page.locator(SEL.scopeCount)).toHaveText('3 rows returned');
});

test('same-query retry, failed response and malformed link hide accepted execution facts', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx');
  await expectFacts(page);
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  await page.route('**/api/v1/query', async route => {
    await held;
    await route.fulfill({ status: 500, json: { error: 'failed' } });
  });
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.scopeCount)).toHaveText('…');
  await noTiming(page);
  release();
  await expect(page.locator(SEL.resultsPane)).toContainText("Couldn't load results: server returned 500");
  await expect(page.locator(SEL.resultsPane).getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
  await expect(page.locator(SEL.scopeCount)).toHaveText('');
  await noTiming(page);
  await page.unroute('**/api/v1/query');
  await page.locator(SEL.runButton).click();
  await expectFacts(page);
  await navigateSearch(page, '?q=service%3Dnginx&f=v1.!');
  await expect(page.locator(SEL.urlNotice)).toBeVisible();
  await noTiming(page);
  await expect(page.locator(SEL.scopeFacts)).toHaveCount(0);
});

for (const intermediate of [false, true]) {
  test(`serialized queries reject the older response${intermediate ? ' and execute only the latest queued intent' : ''}`, async ({ page }) => {
    const held = new Map<string, import('@playwright/test').Route>();
    await page.route('**/api/v1/query', route => { held.set(route.request().postDataJSON().query, route); });
    await page.goto('/search?q=service%3Dolder');
    await expect.poll(() => held.size).toBe(1);
    if (intermediate) {
      await navigateSearch(page, '?q=service%3Dmiddle');
      await expect(page).toHaveURL(/q=service%3Dmiddle/);
      await responseRendered(page);
    }
    await navigateSearch(page, '?q=service%3Dnewer');
    await expect(page.locator(SEL.scopeCount)).toHaveText('…');
    await noTiming(page);
    const older = [...held].find(([query]) => query.includes('older'))![1];
    // LocalResource serializes requests. Completing A must start the latest
    // requested query, without making A's facts current in the meantime.
    const received = page.waitForResponse(response => response.request().postData()?.includes('older') ?? false);
    await older.fulfill({ json: queryResponse('2026-09-15T10:00:00Z', 100, [['older']]) });
    await (await received).finished();
    await expect.poll(() => held.size).toBe(2);
    await responseRendered(page);
    await expect(page.locator(SEL.scopeCount)).toHaveText('…');
    await noTiming(page);
    expect([...held.keys()].some(query => query.includes('middle'))).toBe(false);
    const newer = [...held].find(([query]) => query.includes('newer'))![1];
    await newer.fulfill({ json: queryResponse('2026-09-15T11:00:00Z', 200, [['newer']]) });
    await expectFacts(page, '0.200s', '2026-09-15 11:00:00 UTC');
    await expect(page.locator(SEL.scopeCount)).toHaveText('1 row returned');
    await expect(page.locator(SEL.resultsPane)).toContainText('newer');
    await expect(page.locator(SEL.resultsPane)).not.toContainText('older');
  });
}

test('an empty query invalidates pending ownership before the same query runs again', async ({ page }) => {
  const held: import('@playwright/test').Route[] = [];
  await page.route('**/api/v1/query', route => { held.push(route); });
  await page.goto('/search?q=service%3Dnginx');
  await expect.poll(() => held.length).toBe(1);
  await navigateSearch(page, '');
  await expect(page.locator(SEL.scopeCount)).toHaveText('');
  await noTiming(page);
  await navigateSearch(page, '?q=service%3Dnginx');
  await expect(page.locator(SEL.scopeCount)).toHaveText('…');
  await noTiming(page);
  const oldResponse = page.waitForResponse('**/api/v1/query');
  await held[0].fulfill({ json: queryResponse('2026-09-15T10:00:00Z', 100) });
  await (await oldResponse).finished();
  await expect.poll(() => held.length).toBe(2);
  await responseRendered(page);
  await expect(page.locator(SEL.scopeCount)).toHaveText('…');
  await noTiming(page);
  await held[1].fulfill({ json: queryResponse('2026-09-15T11:00:00Z', 200) });
  await expectFacts(page, '0.200s', '2026-09-15 11:00:00 UTC');
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
  await expect(page.getByRole('tab', { name: /^Events/ })).toBeFocused();
});

// The bypass used to be inert wherever the results region was not the
// snapshot table: the handler prevented the anchor's default and then
// focused nothing at all, so the key press moved neither focus nor the
// document (A03). The link lands on the active results tab, which every
// mode renders; both remaining panes are asserted here.
test('skip to results reaches live mode and the Visualization tab', async ({ page, request }) => {
  await resetScenario(request, 'stream-burst');
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await expect(page.locator(SEL.resultsPane)).toBeVisible();

  const toResults = page.getByRole('link', { name: 'Skip to results' });
  await toResults.focus();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('tab', { name: /^Events/ })).toBeFocused();

  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx&page=0');
  await page.locator(SEL.workspaceTab).filter({ hasText: 'Visualization' }).click();
  await expect(page.locator(SEL.resultsPane)).toBeVisible();

  await toResults.focus();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('tab', { name: 'Visualization' })).toBeFocused();
});

