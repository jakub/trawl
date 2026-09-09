// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, APIRequestContext } from '@playwright/test';

const loaded = JSON.parse(readFileSync(path.join(__dirname, '../harness/wire/history-export.json'), 'utf8'));
const filtered = loaded.entries.filter((row: { query: string }) => row.query.includes('prod'));
const exportButton = (page: Page) => page.getByRole('button', { name: COPY.historyExport, exact: true });
const clearButton = (page: Page) => page.getByRole('button', { name: COPY.historyClear, exact: true });
const rows = (page: Page) => page.locator(SEL.historyPage).locator(SEL.rowStretch);
const modal = (page: Page) => page.locator(SEL.healthConfirm);

async function state(request: APIRequestContext) {
  return (await (await request.get('/__ctl/state')).json()).history;
}
async function release(request: APIRequestContext, path = '/__ctl/history/release') {
  expect((await request.post(path)).status()).toBe(200);
}
async function openFiltered(page: Page, hpage = 0) {
  await page.goto(`/search/history?hpage=${hpage}`);
  await expect(rows(page)).toHaveCount(3);
  await page.locator(SEL.historyFilter).fill('prod');
  await expect(rows(page)).toHaveCount(2);
  await expect(exportButton(page)).toBeEnabled();
}
async function confirm(page: Page) {
  await clearButton(page).click();
  await expect(modal(page)).toBeVisible();
  await modal(page).getByRole('button', { name: COPY.historyClearConfirm, exact: true }).click();
}

test('exports exactly the filtered loaded page as CSV and unchanged JSON', async ({ page, request }, testInfo) => {
  await resetScenario(request, 'history-ready');
  await openFiltered(page);
  await page.screenshot({ path: testInfo.outputPath('history-export.png') });
  await expect(page.locator(SEL.historyFormat)).toHaveJSProperty('tagName', 'SELECT');
  const csvPromise = page.waitForEvent('download');
  await exportButton(page).click();
  const csv = await csvPromise;
  expect(csv.suggestedFilename()).toBe('trawl-history.csv');
  expect(readFileSync((await csv.path())!, 'utf8')).toBe(
    'id,executed_at,query,status,row_count,duration_ms\r\n' +
    "103,2026-09-08T12:00:00Z,'=cmd(prod),success,3,12\r\n" +
    '101,2026-09-08T10:00:00Z,"prod ""雪,one""\r\nnext",error,0,30\r\n',
  );
  await page.locator(SEL.historyFormat).selectOption('json');
  const jsonPromise = page.waitForEvent('download');
  await exportButton(page).click();
  const json = await jsonPromise;
  expect(json.suggestedFilename()).toBe('trawl-history.json');
  expect(readFileSync((await json.path())!, 'utf8')).toBe(JSON.stringify(filtered));
  expect((await state(request)).deletes).toBe(0);
  await expect(page.locator(SEL.toastAny)).toHaveCount(0);
});

test('export stays disabled during loading, then for an empty filtered set', async ({ page, request }) => {
  await resetScenario(request, 'history-loading');
  await page.goto('/search/history');
  await expect.poll(async () => (await state(request)).loadPending).toBe(true);
  await expect(exportButton(page)).toBeDisabled();
  await expect(page.locator(SEL.historyFormat)).toBeDisabled();
  await release(request, '/__ctl/history/load');
  await expect(rows(page)).toHaveCount(3);
  await expect(exportButton(page)).toBeEnabled();
  await page.locator(SEL.historyFilter).fill('no-such-query');
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  await expect(page.locator(SEL.historyFormat)).toBeDisabled();
});

test('export is disabled after a history load failure', async ({ page, request }) => {
  await resetScenario(request, 'history-failure');
  await page.goto('/search/history');
  await expect.poll(async () => (await state(request)).offsets).toEqual([0]);
  await expect(exportButton(page)).toBeDisabled();
  await expect(page.locator(SEL.historyFormat)).toBeDisabled();
  await expect(rows(page)).toHaveCount(0);
});

test('held history loads release independently in request order', async ({ request }) => {
  await resetScenario(request, 'history-loading');
  const completed: string[] = [];
  const reads: Promise<void>[] = [];
  const read = (name: string, offset: number) => request.get(`/api/v1/history?offset=${offset}`, { timeout: 5000 })
    .then(async (response) => {
      expect(response.status()).toBe(200);
      expect(await response.json()).toEqual(loaded);
      completed.push(name);
    });
  try {
    reads.push(read('first', 0));
    await expect.poll(async () => (await state(request)).offsets).toEqual([0]);
    reads.push(read('second', 50));
    await expect.poll(async () => (await state(request)).offsets).toEqual([0, 50]);
    await release(request, '/__ctl/history/load');
    await expect.poll(() => completed).toEqual(['first']);
    expect((await state(request)).loadPending).toBe(true);
    await release(request, '/__ctl/history/load');
    await Promise.all(reads);
    expect(completed).toEqual(['first', 'second']);
    expect((await state(request)).loadPending).toBe(false);
  } finally {
    await resetScenario(request, 'default');
    await Promise.allSettled(reads);
  }
});

test('retained rows cannot export while the next page is loading', async ({ page, request }) => {
  await resetScenario(request, 'history-page-loading');
  await openFiltered(page);
  let downloads = 0;
  page.on('download', () => { downloads += 1; });
  await page.getByRole('button', { name: COPY.historyNext, exact: true }).click();
  await expect.poll(async () => (await state(request)).offsets).toEqual([0, 50]);
  await expect(exportButton(page)).toBeDisabled();
  await expect(page.locator(SEL.historyFormat)).toBeDisabled();
  // Exercise the callback guard even if a click arrives while rows are retained.
  await exportButton(page).dispatchEvent('click');
  await release(request, '/__ctl/history/load');
  await expect(exportButton(page)).toBeEnabled();
  expect(downloads).toBe(0);
});

test('cancelling clear preserves rows, filter, and page without DELETE', async ({ page, request }) => {
  await resetScenario(request, 'history-ready');
  await openFiltered(page, 2);
  await clearButton(page).click();
  await expect(modal(page)).toContainText('current key, across every page');
  await modal(page).getByRole('button', { name: COPY.historyCancel, exact: true }).click();
  await expect(modal(page)).toHaveCount(0);
  await expect(page.locator(SEL.historyFilter)).toHaveValue('prod');
  await expect(rows(page)).toHaveCount(2);
  await expect(page).toHaveURL(/\/search\/history\?hpage=2$/);
  expect(await state(request)).toMatchObject({ deletes: 0, offsets: [100] });
});

for (const hpage of [0, 2]) {
  test(`clear on page ${hpage} confirms once and fetches canonical page zero`, async ({ page, request }) => {
    await resetScenario(request, 'history-ready');
    await openFiltered(page, hpage);
    await clearButton(page).click();
    const button = modal(page).getByRole('button', { name: COPY.historyClearConfirm, exact: true });
    // Two activations in one turn must consume the pending confirmation once.
    await button.evaluate((element: HTMLButtonElement) => { element.click(); element.click(); });
    await expect.poll(async () => (await state(request)).deletes).toBe(1);
    await expect(modal(page)).toHaveCount(0);
    await expect(clearButton(page)).toBeDisabled();
    await expect(exportButton(page)).toBeDisabled();
    await expect(page.locator(SEL.historyFormat)).toBeDisabled();
    await expect(rows(page)).toHaveCount(2);
    await expect(page.locator(SEL.historyFilter)).toHaveValue('prod');
    await release(request);
    await expect(page).toHaveURL(/\/search\/history$/);
    await expect(page.locator(SEL.historyFilter)).toHaveValue('');
    await expect(rows(page)).toHaveCount(0);
    await expect(clearButton(page)).toBeEnabled();
    await expect(page.locator(SEL.toastAny)).toContainText(COPY.historyClearDone);
    expect(await state(request)).toMatchObject({ deletes: 1, offsets: [hpage * 50, 0] });
    await expect(exportButton(page)).toBeDisabled();
  });
}

for (const scenario of ['history-clear-failure', 'history-clear-lost', 'history-clear-decode']) {
  test(`${scenario} preserves the current view and allows a safe retry`, async ({ page, request }) => {
    await resetScenario(request, scenario);
    await openFiltered(page, 2);
    await confirm(page);
    await expect.poll(async () => (await state(request)).pending).toBe(true);
    await release(request);
    await expect(page.locator(SEL.toastError)).toContainText(COPY.historyClearFailed);
    await expect(page.locator(SEL.historyFilter)).toHaveValue('prod');
    await expect(rows(page)).toHaveCount(2);
    await expect(page).toHaveURL(/\/search\/history\?hpage=2$/);
    await expect(clearButton(page)).toBeEnabled();
    await expect(exportButton(page)).toBeEnabled();
    expect(await state(request)).toMatchObject({ deletes: 1, offsets: [100] });
    await confirm(page);
    await expect.poll(async () => (await state(request)).deletes).toBe(2);
    await release(request);
    await expect(page).toHaveURL(/\/search\/history$/);
    await expect(page.locator(SEL.historyFilter)).toHaveValue('');
    await expect(rows(page)).toHaveCount(0);
    expect(await state(request)).toMatchObject({ deletes: 2, offsets: [100, 0] });
  });
}

for (const operation of ['GET', 'DELETE']) {
  test(`late history ${operation} completion leaves the new page alone`, async ({ page, request }) => {
    await resetScenario(request, operation === 'GET' ? 'history-loading' : 'history-ready');
    if (operation === 'GET') {
      await page.goto('/search/history');
      await expect.poll(async () => (await state(request)).loadPending).toBe(true);
    } else {
      await openFiltered(page);
      await confirm(page);
      await expect.poll(async () => (await state(request)).pending).toBe(true);
    }
    await page.locator(SEL.historyAwayLink).click();
    await expect(page).toHaveURL(/\/search\/schema$/);
    const response = page.waitForResponse((r) => new URL(r.url()).pathname === '/api/v1/history'
      && r.request().method() === operation);
    await release(request, operation === 'GET' ? '/__ctl/history/load' : '/__ctl/history/release');
    await (await response).finished();
    await page.evaluate(() => new Promise<void>((resolve) => {
      requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
    }));
    await expect(page).toHaveURL(/\/search\/schema$/);
    await expect(page.locator(SEL.historyPage)).toHaveCount(0);
    await expect(page.locator(SEL.toastAny)).toHaveCount(0);
  });
}
