// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import path from 'node:path';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, APIRequestContext } from '@playwright/test';

const fixture = JSON.parse(readFileSync(path.join(__dirname, '../harness/wire/history-export.json'), 'utf8'));
const filtered = fixture.entries.filter((row: { query: string }) => row.query.includes('prod'));
const exportButton = (page: Page) => page.getByRole('button', { name: COPY.historyExport, exact: true });
const clearButton = (page: Page) => page.getByRole('button', { name: COPY.historyClear, exact: true });
const rows = (page: Page) => page.locator(SEL.historyPage).locator(SEL.rowStretch);
const input = (page: Page) => page.locator(SEL.historyFilter);
const modal = (page: Page) => page.locator(SEL.healthConfirm);
async function state(request: APIRequestContext) { return (await (await request.get('/__ctl/state')).json()).history; }
async function configure(request: APIRequestContext, data: Record<string, unknown>) {
  expect((await request.post('/__ctl/history/configure', { data })).ok()).toBeTruthy();
}
async function release(request: APIRequestContext, id?: number, status = 200) {
  expect((await request.post(id == null ? '/__ctl/history/release' : '/__ctl/history/load', { data: { id, status } })).status()).toBe(200);
}
async function confirm(page: Page) {
  await clearButton(page).click();
  await expect(modal(page)).toContainText('including entries that do not match the current filter');
  await expect(modal(page)).toContainText('Saved queries will remain');
  await expect(modal(page)).toContainText('may add a new history entry');
  await modal(page).getByRole('button', { name: COPY.historyClearConfirm, exact: true }).click();
}
async function navigate(page: Page, url: string) {
  await page.evaluate(url => {
    const link = document.createElement('a'); link.href = url;
    document.body.append(link); link.click(); link.remove();
  }, url);
}
async function settle(page: Page) {
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('exports exactly the applied page with CSV protection and unchanged JSON despite draft edits', async ({ page, request }, testInfo) => {
  await resetScenario(request, 'history-ready');
  await page.goto('/search/history?hq=prod');
  await expect(rows(page)).toHaveCount(2);
  await input(page).fill('no-such-query');
  await expect(rows(page)).toHaveCount(2);
  await expect(exportButton(page)).toBeEnabled();
  await page.screenshot({ path: testInfo.outputPath('history-export.png') });
  const csvPromise = page.waitForEvent('download');
  await exportButton(page).click();
  const csv = await csvPromise;
  expect(csv.suggestedFilename()).toBe('trawl-history.csv');
  expect(readFileSync((await csv.path())!, 'utf8')).toBe(
    'id,executed_at,query,status,row_count,duration_ms\r\n' +
    "103,2026-09-08T12:00:00Z,'=cmd(prod),success,3,12\r\n" +
    '101,2026-09-08T10:00:00Z,"prod ""雪,one""\r\nnext",error,0,30\r\n');
  await page.locator(SEL.historyFormat).selectOption('json');
  const jsonPromise = page.waitForEvent('download');
  await exportButton(page).click();
  const json = await jsonPromise;
  expect(json.suggestedFilename()).toBe('trawl-history.json');
  expect(readFileSync((await json.path())!, 'utf8')).toBe(JSON.stringify(filtered));
  expect((await state(request)).requests).toHaveLength(1);
});

test('filtered later-page export contains only that successful page', async ({ page, request }) => {
  await resetScenario(request, 'history-search');
  await page.goto('/search/history?hq=match&hpage=2');
  await expect(rows(page)).toHaveCount(20);
  const visible = await rows(page).allTextContents();
  await input(page).fill('other');
  await page.locator(SEL.historyFormat).selectOption('json');
  const download = page.waitForEvent('download');
  await exportButton(page).click();
  const entries = JSON.parse(readFileSync((await (await download).path())!, 'utf8'));
  expect(entries.map((row: { query: string }) => row.query)).toEqual(visible);
  expect(entries).toHaveLength(20);
  expect((await state(request)).requests).toMatchObject([{ filter: 'match', offset: 100 }]);
});

test('cancelled Clear preserves filter, later page, and data', async ({ page, request }) => {
  await resetScenario(request, 'history-search');
  await page.goto('/search/history?hq=match&hpage=2');
  await expect(rows(page)).toHaveCount(20);
  await clearButton(page).click();
  await modal(page).getByRole('button', { name: COPY.historyCancel, exact: true }).click();
  await expect(rows(page)).toHaveCount(20);
  await expect(input(page)).toHaveValue('match');
  await expect(page).toHaveURL(/\?hq=match&hpage=2$/);
  expect((await state(request)).deletes).toBe(0);
});

for (const [suffix, offset, count] of [['', 0, 50], ['?hq=match&hpage=2', 100, 20]] as const) {
  test(`Clear blocks controls and refreshes canonical page zero from ${suffix || 'canonical URL'}`, async ({ page, request }) => {
    await resetScenario(request, 'history-search');
    await page.goto(`/search/history${suffix}`);
    await expect(rows(page)).toHaveCount(count);
    await clearButton(page).click();
    const button = modal(page).getByRole('button', { name: COPY.historyClearConfirm, exact: true });
    await button.evaluate((element: HTMLButtonElement) => { element.click(); element.click(); });
    await expect.poll(async () => (await state(request)).deletes).toBe(1);
    await expect(input(page)).toBeDisabled();
    await expect(page.getByRole('button', { name: 'Search', exact: true })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'Clear filter', exact: true })).toBeDisabled();
    await expect(clearButton(page)).toBeDisabled();
    await expect(exportButton(page)).toBeDisabled();
    await expect(page.locator(SEL.historyFormat)).toBeDisabled();
    await expect(page.getByRole('button', { name: COPY.historyPrev, exact: true })).toBeDisabled();
    await expect(page.getByRole('button', { name: COPY.historyNext, exact: true })).toBeDisabled();
    await expect(rows(page)).toHaveCount(count);
    await release(request);
    await expect(page).toHaveURL(/\/search\/history$/);
    await expect(input(page)).toHaveValue('');
    await expect(page.getByText('No history', { exact: true })).toBeVisible();
    await expect(clearButton(page)).toBeEnabled();
    await expect(exportButton(page)).toBeDisabled();
    expect((await state(request)).offsets).toEqual([offset, 0]);
    await expect(page.locator(SEL.toastAny)).toContainText(COPY.historyClearDone);
  });
}

test('pre-clear held read and failed post-clear refresh cannot resurrect data', async ({ page, request }) => {
  await resetScenario(request, 'history-ready');
  await page.goto('/search/history');
  await expect(rows(page)).toHaveCount(3);
  await configure(request, { hold: true });
  await input(page).press('Enter');
  await expect.poll(async () => (await state(request)).pendingRequests.map((r: { id: number }) => r.id)).toEqual([2]);
  await confirm(page);
  await expect.poll(async () => (await state(request)).pending).toBe(true);
  await release(request);
  await expect.poll(async () => (await state(request)).pendingRequests.map((r: { id: number }) => r.id)).toEqual([2, 3]);
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  const older = page.waitForResponse(response => new URL(response.url()).pathname === '/api/v1/history' && response.request().method() === 'GET');
  await release(request, 2);
  await (await older).finished();
  await settle(page);
  await expect(rows(page)).toHaveCount(0);
  await expect(page.getByRole('region', { name: COPY.historyH1, exact: true })).toHaveAttribute('aria-busy', 'true');
  await release(request, 3, 503);
  await expect(page.locator('.load-hint.error')).toContainText("Couldn't load history");
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  await configure(request, { hold: false });
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(page.getByText('No history', { exact: true })).toBeVisible();
});

for (const scenario of ['history-clear-failure', 'history-clear-lost', 'history-clear-decode']) {
  test(`${scenario} preserves the applied view and permits retry`, async ({ page, request }) => {
    await resetScenario(request, scenario);
    await page.goto('/search/history?hq=prod');
    await expect(rows(page)).toHaveCount(2);
    await confirm(page);
    await expect.poll(async () => (await state(request)).pending).toBe(true);
    await release(request);
    await expect(page.locator(SEL.toastError)).toContainText(COPY.historyClearFailed);
    await expect(rows(page)).toHaveCount(2);
    await expect(input(page)).toHaveValue('prod');
    await expect(page).toHaveURL(/\?hq=prod$/);
    await expect(exportButton(page)).toBeEnabled();
    await confirm(page);
    await expect.poll(async () => (await state(request)).deletes).toBe(2);
    await release(request);
    await expect(page).toHaveURL(/\/search\/history$/);
    await expect(page.getByText('No history', { exact: true })).toBeVisible();
  });
}

test('late Clear after same-component navigation cannot change the new view', async ({ page, request }) => {
  await resetScenario(request, 'history-search');
  await page.goto('/search/history?hq=match');
  await expect(rows(page)).toHaveCount(50);
  await confirm(page);
  await expect.poll(async () => (await state(request)).pending).toBe(true);
  await navigate(page, '/search/history?hq=other');
  await expect(rows(page).first()).toHaveText('other-row-1');
  const cleared = page.waitForResponse(response => new URL(response.url()).pathname === '/api/v1/history' && response.request().method() === 'DELETE');
  await release(request);
  await (await cleared).finished();
  await settle(page);
  await expect(page).toHaveURL(/\?hq=other$/);
  await expect(input(page)).toHaveValue('other');
  await expect(rows(page)).toHaveCount(50);
  await expect(page.locator(SEL.toastAny)).toHaveCount(0);
});

for (const operation of ['GET', 'DELETE']) {
  test(`late ${operation} after unmount leaves the new page alone`, async ({ page, request }) => {
    await resetScenario(request, operation === 'GET' ? 'history-loading' : 'history-ready');
    await page.goto('/search/history');
    if (operation === 'GET') await expect.poll(async () => (await state(request)).loadPending).toBe(true);
    else { await expect(rows(page)).toHaveCount(3); await confirm(page); await expect.poll(async () => (await state(request)).pending).toBe(true); }
    await page.locator(SEL.historyAwayLink).click();
    await expect(page).toHaveURL(/\/search\/schema$/);
    const response = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/history' && r.request().method() === operation);
    await release(request, operation === 'GET' ? 1 : undefined);
    await (await response).finished();
    await settle(page);
    await expect(page).toHaveURL(/\/search\/schema$/);
    await expect(rows(page)).toHaveCount(0);
    await expect(page.locator(SEL.toastAny)).toHaveCount(0);
  });
}
