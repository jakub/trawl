// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

test('global Runs zero-match filter preserves the unfiltered page window', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/jobs/runs');
  await expect(page.locator('.tbl-body .tbl-row')).toHaveCount(3);
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('1–3 of 3');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();

  await page.getByPlaceholder('Filter by net…').fill('no-such-net');
  await expect(page.locator('.tbl-body .tbl-row')).toHaveCount(0);
  await expect.soft(footer.locator('.results-summary')).toContainText('1–3 of 3');
  await expect.soft(footer).toContainText('0 matches on this page');
  await expect.soft(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
});

test('net drawer preserves its unfiltered three-row page window', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/jobs/nets?net=1&ntab=runs');
  const drawer = page.locator(SEL.drawerPanel);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await expect(drawer.locator('.results-summary')).toHaveText('1–3 of 3');
  await expect(drawer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(drawer.getByRole('button', { name: 'Next' })).toBeDisabled();
});

// Same-document URL changes exercise the mounted resource, including requests
// that remain in flight across URL changes. A full reload cannot prove it.
async function navigateHistory(page: import('@playwright/test').Page, hpage: string) {
  await page.evaluate((value) => {
    const link = document.createElement('a');
    link.href = `/search/history?hpage=${value}`;
    document.body.append(link);
    link.click();
    link.remove();
  }, hpage);
}

async function configure(request: import('@playwright/test').APIRequestContext, pagination: Record<string, unknown> = {}) {
  const response = await request.post('/__ctl/reset', { data: { scenario: 'pagination', pagination } });
  expect(response.ok()).toBeTruthy();
}

async function paginationState(request: import('@playwright/test').APIRequestContext) {
  return (await (await request.get('/__ctl/state')).json()).pagination;
}

test('history reads hpage once, replaces on Next, and filters only rendered rows', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search/history?hpage=%31');
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('51–100 of 103');
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(50);
  await expect(page.locator('.tbl-body .row-stretch').first()).toHaveText('history-row-51');
  const length = await page.evaluate(() => history.length);
  await page.getByPlaceholder('Filter history…').fill('history-row-51');
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(1);
  await expect(footer.locator('.results-summary')).toHaveText('51–100 of 103');
  await page.getByPlaceholder('Filter history…').clear();
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/hpage=2$/);
  await expect(footer.locator('.results-summary')).toHaveText('101–103 of 103');
  expect(await page.evaluate(() => history.length)).toBe(length);
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  await footer.getByRole('button', { name: 'Prev' }).click();
  await expect(footer.locator('.results-summary')).toHaveText('51–100 of 103');
  await footer.getByRole('button', { name: 'Prev' }).click();
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await navigateHistory(page, '%2531');
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  expect((await paginationState(request)).history.map((r: { offset: number }) => r.offset)).toEqual([50, 100, 50, 0]);
});

test('history refuses an overflowing page before sending a request', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search/history?hpage=184467440737095516160');
  await expect(page.getByText('This history page is too large to request.', { exact: false })).toBeVisible();
  expect((await paginationState(request)).history).toEqual([]);
  await expect(page.locator('.results-footer')).toHaveCount(0);
});

test('history empty out-of-range page offers the last real page without inventing rows', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search/history?hpage=9');
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('0–0 of 103');
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(0);
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  await footer.getByRole('button', { name: 'Prev' }).click();
  await expect(page).toHaveURL(/hpage=2$/);
  await expect(footer.locator('.results-summary')).toHaveText('101–103 of 103');
});

test('Probe results use response offsets, full and partial pages, and truncated suffix', async ({ page, request }) => {
  await configure(request, { truncated: true });
  await page.goto('/search?q=service%3Dnginx');
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows (truncated)');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/[?&]page=1(?:&|$)/);
  await expect(footer.locator('.results-summary')).toHaveText('Page 2 · showing 3 rows (truncated)');
  await expect(page.locator('.results-table tbody')).toContainText('query-row-51');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  const state = await (await request.get('/__ctl/state')).json();
  expect(state.queries.map((q: { offset: number }) => q.offset)).toEqual([0, 50]);
});

test('Probe empty final page keeps its page and available Prev', async ({ page, request }) => {
  await configure(request, { queryTotal: 50 });
  await page.goto('/search?q=service%3Dnginx');
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows');
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect(footer.locator('.results-summary')).toHaveText('Page 2 · showing 0 rows');
  await expect(page).toHaveURL(/[?&]page=1(?:&|$)/);
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeEnabled();
  await footer.getByRole('button', { name: 'Prev' }).click();
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows');
});

for (const surface of ['global', 'drawer']) {
  test(`${surface} runs pages use the shared twenty-row offset`, async ({ page, request }) => {
    await configure(request, { runsTotal: 43 });
    await page.goto(surface === 'global' ? '/jobs/runs' : '/jobs/nets?net=1&ntab=runs');
    const root = surface === 'global' ? page.locator('.page') : page.locator(SEL.drawerPanel);
    const footer = root.locator('.results-footer');
    await expect(footer.locator('.results-summary')).toHaveText('1–20 of 43');
    await footer.getByRole('button', { name: 'Next' }).click();
    await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
    await footer.getByRole('button', { name: 'Next' }).click();
    await expect(footer.locator('.results-summary')).toHaveText('41–43 of 43');
    await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
    await footer.getByRole('button', { name: 'Prev' }).click();
    await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
    expect((await paginationState(request)).runs.map((r: { offset: number }) => r.offset)).toEqual([0, 20, 40, 20]);
  });
}

test('history keeps fetched rows while busy and completes the latest queued page', async ({ page, request }) => {
  await configure(request, { holdHistoryOffset: 50 });
  await page.goto('/search/history');
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  // Keep an actual element identity across navigation, not just a URL check.
  const mountedInput = await page.getByPlaceholder('Filter history…').elementHandle();
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect.poll(async () => (await paginationState(request)).held).toBe(true);
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  await expect(page.locator('.tbl-body .row-stretch').first()).toHaveText('history-row-1');

  // The pager is disabled, but browser URL navigation still changes the page.
  // LocalResource serializes fetches: page 2 starts only after page 1 resolves.
  await navigateHistory(page, '2');
  await expect(page).toHaveURL(/hpage=2$/);
  expect(await mountedInput!.evaluate(el => el.isConnected)).toBe(true);
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  expect((await paginationState(request)).completed).toEqual([0]);

  const oldResponse = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/history'
    && new URL(r.url()).searchParams.get('offset') === '50');
  const released = await request.post('/__ctl/pagination/release');
  expect(released.ok()).toBeTruthy();
  await (await oldResponse).finished();
  try {
    await expect(footer.locator('.results-summary')).toHaveText('101–103 of 103');
  } finally {
    await test.info().attach('delayed-pagination-state', {
      contentType: 'application/json',
      body: JSON.stringify({
        server: await paginationState(request), url: page.url(),
        sameComponent: await mountedInput!.evaluate(el => el.isConnected),
        firstRow: await page.locator('.tbl-body .row-stretch').first().textContent(),
        summary: await footer.locator('.results-summary').textContent(),
        prevDisabled: await footer.getByRole('button', { name: 'Prev' }).isDisabled(),
        nextDisabled: await footer.getByRole('button', { name: 'Next' }).isDisabled(),
      }, null, 2),
    });
  }
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(3);
  await expect(page.locator('.tbl-body .row-stretch').first()).toHaveText('history-row-101');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeEnabled();
  await expect(page).toHaveURL(/hpage=2$/);
  expect(await mountedInput!.evaluate(el => el.isConnected)).toBe(true);
  expect((await paginationState(request)).history.map((r: { offset: number }) => r.offset)).toEqual([0, 50, 100]);
  expect((await paginationState(request)).completed).toEqual([0, 50, 100]);
});

test('Probe returned range overflow is visible without a wasm panic', async ({ page, request }) => {
  await configure(request, { queryTotal: 4_294_967_300 });
  await page.goto('/search?q=service%3Dnginx&page=85899345');
  await expect(page.getByText('This result page extends past the supported row range.', { exact: false })).toBeVisible();
});
