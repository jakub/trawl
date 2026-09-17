// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page, APIRequestContext } from '@playwright/test';

const input = (page: Page) => page.locator(SEL.historyFilter);
const rows = (page: Page) => page.locator(SEL.historyPage).locator(SEL.rowStretch);
const summary = (page: Page) => page.locator(SEL.historyPage).locator(SEL.resultsSummary);
const frame = (page: Page) => page.getByRole('region', { name: COPY.historyH1, exact: true });
const exportButton = (page: Page) => page.getByRole('button', { name: COPY.historyExport });
async function state(request: APIRequestContext) { return (await (await request.get('/__ctl/state')).json()).history; }
async function configure(request: APIRequestContext, data: Record<string, unknown>) {
  expect((await request.post('/__ctl/history/configure', { data })).ok()).toBeTruthy();
}
async function release(request: APIRequestContext, id: number, status = 200) {
  expect((await request.post('/__ctl/history/load', { data: { id, status } })).ok()).toBeTruthy();
}
async function navigate(page: Page, query: string) {
  await page.evaluate(query => {
    const link = document.createElement('a');
    link.href = `/search/history${query}`;
    document.body.append(link); link.click(); link.remove();
  }, query);
}
async function settle(page: Page) {
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}
async function search(page: Page, text: string) {
  await input(page).fill(text);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
}

test.beforeEach(async ({ request }) => { await resetScenario(request, 'history-search'); });

test('draft submission, paging, reload, and browser history retain the applied view', async ({ page, request }) => {
  await page.goto('/search/history');
  await expect(summary(page)).toHaveText('1–50 of 180');
  const originalLength = await page.evaluate(() => history.length);
  await input(page).fill('match');
  await settle(page);
  expect((await state(request)).requests).toHaveLength(1);
  await expect(rows(page)).toHaveCount(50);
  await expect(page.getByText('Edited. Press Enter or Search to apply.', { exact: true })).toBeVisible();
  await input(page).press('Enter');
  await expect(page).toHaveURL(/\/search\/history\?hq=match$/);
  await expect(summary(page)).toHaveText('1–50 of 120');
  expect(await page.evaluate(() => history.length)).toBe(originalLength + 1);
  await input(page).fill('unapplied');
  await page.getByRole('button', { name: COPY.historyNext, exact: true }).click();
  await expect(summary(page)).toHaveText('51–100 of 120');
  await expect(input(page)).toHaveValue('match');
  await page.getByRole('button', { name: COPY.historyNext, exact: true }).click();
  await expect(summary(page)).toHaveText('101–120 of 120');
  expect(await page.evaluate(() => history.length)).toBe(originalLength + 1);
  await page.reload();
  await expect(summary(page)).toHaveText('101–120 of 120');
  await expect(input(page)).toHaveValue('match');
  await page.goBack();
  await expect(summary(page)).toHaveText('1–50 of 180');
  await expect(input(page)).toHaveValue('');
  await page.goForward();
  await expect(summary(page)).toHaveText('101–120 of 120');
  const count = (await state(request)).requests.length;
  const length = await page.evaluate(() => history.length);
  await input(page).press('Enter');
  await expect.poll(async () => (await state(request)).requests.length).toBe(count + 1);
  await expect(summary(page)).toHaveText('101–120 of 120');
  expect(await page.evaluate(() => history.length)).toBe(length);
  await search(page, 'other');
  await expect(summary(page)).toHaveText('1–50 of 60');
  await expect(page).toHaveURL(/\?hq=other$/);
  await page.getByRole('button', { name: 'Clear filter', exact: true }).click();
  await expect(page).toHaveURL(/\/search\/history$/);
  await expect(summary(page)).toHaveText('1–50 of 180');
  const clearCount = (await state(request)).requests.length;
  const clearLength = await page.evaluate(() => history.length);
  await page.getByRole('button', { name: 'Clear filter', exact: true }).click();
  await expect.poll(async () => (await state(request)).requests.length).toBe(clearCount + 1);
  expect(await page.evaluate(() => history.length)).toBe(clearLength);
  expect((await state(request)).requests.some((r: { filter: string; offset: number }) => r.filter === 'match' && r.offset === 100)).toBe(true);
});

test('IME Enter does not submit and keyboard Search applies once', async ({ page, request }) => {
  await page.goto('/search/history');
  await expect(rows(page)).toHaveCount(50);
  await expect(input(page)).toHaveAccessibleName('Search query text');
  await input(page).fill('match');
  await input(page).dispatchEvent('compositionstart');
  await input(page).dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true });
  await page.locator('form.history-search').dispatchEvent('submit');
  await settle(page);
  expect((await state(request)).requests).toHaveLength(1);
  await input(page).dispatchEvent('compositionend');
  await page.getByRole('button', { name: 'Search', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(summary(page)).toHaveText('1–50 of 120');
  expect((await state(request)).requests).toHaveLength(2);
});

test('explicit Search submits the completed composition by pointer', async ({ page, request }) => {
  await page.goto('/search/history');
  await expect(rows(page)).toHaveCount(50);
  await input(page).dispatchEvent('compositionstart');
  await input(page).fill('match');
  // Model the completed composition before the button's pointer activation.
  // The composition flag must not suppress this explicit Search submission.
  await input(page).dispatchEvent('compositionend');
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(summary(page)).toHaveText('1–50 of 120');
  expect((await state(request)).requests).toHaveLength(2);
  expect((await state(request)).requests[1].filter).toBe('match');
});

test('invalid submission retains the draft, loaded view, and URL', async ({ page, request }) => {
  await page.goto('/search/history?hq=match');
  await expect(summary(page)).toHaveText('1–50 of 120');
  const text = 'x'.repeat(32768 - 'hq=&hpage=85899345'.length + 1);
  await search(page, text);
  await expect(input(page)).toHaveAttribute('aria-invalid', 'true');
  await expect(page.locator('#history-filter-error')).toContainText('too long');
  await expect(input(page)).toHaveValue(text);
  await expect(page).toHaveURL(/\?hq=match$/);
  await expect(summary(page)).toHaveText('1–50 of 120');
  await expect(exportButton(page)).toBeEnabled();
  await settle(page);
  expect((await state(request)).requests).toHaveLength(1);
});

for (const query of ['hq=%GG', 'hq=%FF', '%FF=x', 'hq=%00', 'hq=x&%68q=y', Array(65).fill('unknown=x').join('&'), `unknown=${'x'.repeat(32768)}`, `hq=${'x'.repeat(32768 - 'hq=&hpage=85899345'.length + 1)}`]) {
  test(`malformed History link is refused without a request (${query.slice(0, 24)})`, async ({ page, request }) => {
    await page.goto(`/search/history?${query}`);
    await expect(page.getByRole('button', { name: 'Reset history view' })).toBeVisible();
    await expect(rows(page)).toHaveCount(0);
    expect((await state(request)).requests).toHaveLength(0);
    const length = await page.evaluate(() => history.length);
    await page.getByRole('button', { name: 'Reset history view' }).click();
    await expect(page).toHaveURL(/\/search\/history$/);
    await expect(summary(page)).toHaveText('1–50 of 180');
    expect(await page.evaluate(() => history.length)).toBe(length);
  });
}

test('encoded boundary remains pageable and reload decodes it exactly once', async ({ page, request }) => {
  await page.goto('/search/history');
  await expect(rows(page)).toHaveCount(50);
  const text = 'x'.repeat(32768 - 'hq=&hpage=85899345'.length - 36) + "日本語+%'";
  const encoded = encodeURIComponent(text).replace(/'/g, '%27');
  await search(page, text);
  await expect(page.getByText('No matching queries', { exact: true })).toBeVisible();
  expect(new URL(page.url()).search).toBe(`?hq=${encoded}`);
  await navigate(page, `?hq=${encoded}&hpage=85899345`);
  await expect.poll(async () => (await state(request)).requests.at(-1)?.offset).toBe(4294967250);
  expect(new URL(page.url()).search.length - 1).toBe(32768);
  await page.reload();
  await expect(input(page)).toHaveValue(text);
  await expect(page.getByText('No matching queries', { exact: true })).toBeVisible();
  expect((await state(request)).requests.at(-1).filter).toBe(text);
});

test('newest filter response owns rows, totals, errors and export', async ({ page, request }) => {
  await page.goto('/search/history');
  await expect(rows(page)).toHaveCount(50);
  await configure(request, { hold: true });
  await search(page, 'match');
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(1);
  await expect(rows(page)).toHaveCount(0);
  await expect(frame(page)).toHaveAttribute('aria-busy', 'true');
  await expect(exportButton(page)).toBeDisabled();
  await input(page).fill('other');
  await input(page).press('Enter');
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(2);
  expect((await state(request)).pendingRequests.map((r: { id: number; filter: string }) => [r.id, r.filter])).toEqual([[2, 'match'], [3, 'other']]);
  await input(page).fill('draft during request');
  await release(request, 3);
  await expect(summary(page)).toHaveText('1–50 of 60');
  await expect(rows(page).first()).toHaveText('other-row-1');
  await expect(input(page)).toHaveValue('draft during request');
  await expect(exportButton(page)).toBeEnabled();
  const older = page.waitForResponse(response => {
    const url = new URL(response.url());
    return url.pathname === '/api/v1/history' && url.searchParams.get('filter') === 'match';
  });
  await release(request, 2, 503);
  await (await older).finished();
  await settle(page);
  await expect(summary(page)).toHaveText('1–50 of 60');
  await expect(frame(page)).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.load-hint.error')).toHaveCount(0);
});

test('same-view refresh retains rows but blocks actions through failure and retry', async ({ page, request }) => {
  await page.goto('/search/history?hq=match');
  await expect(summary(page)).toHaveText('1–50 of 120');
  await configure(request, { hold: true });
  await input(page).press('Enter');
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(1);
  await expect(rows(page)).toHaveCount(50);
  await expect(page.getByText('Refreshing history…', { exact: true })).toBeVisible();
  await expect(exportButton(page)).toBeDisabled();
  await expect(page.getByRole('button', { name: COPY.historyNext, exact: true })).toBeDisabled();
  await release(request, 2, 503);
  await expect(page.locator('.load-hint.error')).toContainText("Couldn't refresh history");
  await expect(rows(page)).toHaveCount(50);
  await expect(exportButton(page)).toBeDisabled();
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(1);
  await release(request, 3);
  await expect(exportButton(page)).toBeEnabled();
  await expect(page.locator('.load-hint.error')).toHaveCount(0);
});

test('newest filtered page wins when an earlier page finishes last', async ({ page, request }) => {
  await page.goto('/search/history?hq=match');
  await expect(summary(page)).toHaveText('1–50 of 120');
  await configure(request, { hold: true });
  let downloads = 0;
  page.on('download', () => { downloads += 1; });
  await page.getByRole('button', { name: COPY.historyNext, exact: true }).click();
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(1);
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  await exportButton(page).dispatchEvent('click');
  await navigate(page, '?hq=match&hpage=2');
  await expect.poll(async () => (await state(request)).pendingRequests.length).toBe(2);
  expect((await state(request)).pendingRequests.map((r: { id: number; filter: string; offset: number }) => [r.id, r.filter, r.offset]))
    .toEqual([[2, 'match', 50], [3, 'match', 100]]);
  await release(request, 3);
  await expect(summary(page)).toHaveText('101–120 of 120');
  const currentRows = await rows(page).allTextContents();
  const older = page.waitForResponse(response => {
    const url = new URL(response.url());
    return url.pathname === '/api/v1/history' && url.searchParams.get('offset') === '50';
  });
  await release(request, 2);
  await (await older).finished();
  await settle(page);
  expect(await rows(page).allTextContents()).toEqual(currentRows);
  await expect(summary(page)).toHaveText('101–120 of 120');
  await expect(exportButton(page)).toBeEnabled();
  expect(downloads).toBe(0);
});

test('initial failure cannot enable export and Retry loads the requested view', async ({ page, request }) => {
  await configure(request, { nextStatus: 503 });
  await page.goto('/search/history?hq=match');
  await expect(page.locator('.load-hint.error')).toContainText("Couldn't load history");
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(summary(page)).toHaveText('1–50 of 120');
  await expect(exportButton(page)).toBeEnabled();
});

test('failed transitions, zero matches, out-of-range pages and empty history remain distinct', async ({ page, request }) => {
  await page.goto('/search/history?hq=match');
  await expect(rows(page)).toHaveCount(50);
  await configure(request, { nextStatus: 503 });
  await search(page, 'other');
  await expect(page.locator('.load-hint.error')).toContainText("Couldn't load history");
  await expect(rows(page)).toHaveCount(0);
  await expect(exportButton(page)).toBeDisabled();
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(summary(page)).toHaveText('1–50 of 60');
  await navigate(page, '?hq=match&hpage=9');
  await expect(summary(page)).toHaveText('0–0 of 120');
  await expect(page.getByText('No matching queries', { exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: COPY.historyPrev, exact: true }).click();
  await expect(page).toHaveURL(/\?hq=match&hpage=2$/);
  await expect(summary(page)).toHaveText('101–120 of 120');
  await search(page, 'absent');
  await expect(page.getByText('No matching queries', { exact: true })).toBeVisible();
  await expect(exportButton(page)).toBeDisabled();
  await configure(request, { entries: [] });
  await page.getByRole('button', { name: 'Clear filter', exact: true }).click();
  await expect(page.getByText('No history', { exact: true })).toBeVisible();
});
