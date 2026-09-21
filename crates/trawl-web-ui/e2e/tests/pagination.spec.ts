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

test('history reads hpage once, replaces on Next, and keeps draft edits unapplied', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search/history?hpage=%31');
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('51–100 of 103');
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(50);
  await expect(page.locator('.tbl-body .row-stretch').first()).toHaveText('history-row-51');
  const length = await page.evaluate(() => history.length);
  await page.getByPlaceholder('Filter history…').fill('history-row-51');
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(50);
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
  await expect(page).toHaveURL(/\/search\/history$/);
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  // A distinct raw URL refetches even when its single-decoded page is zero.
  const decodedPage = page.waitForResponse(r => new URL(r.url()).pathname === '/api/v1/history'
    && new URL(r.url()).searchParams.get('offset') === '0');
  await navigateHistory(page, '%2531');
  await (await decodedPage).finished();
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  expect((await paginationState(request)).history.map((r: { offset: number }) => r.offset)).toEqual([50, 100, 50, 0, 0]);
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

test('Probe results use response offsets, full and partial pages', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dnginx');
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect(page).toHaveURL(/[?&]page=1(?:&|$)/);
  await expect(footer.locator('.results-summary')).toHaveText('Page 2 · showing 3 rows');
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

test('history hides stale rows and completes a newer page before the older request', async ({ page, request }) => {
  await configure(request, { holdHistoryOffset: 50 });
  await page.goto('/search/history');
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('1–50 of 103');
  // Keep an actual element identity across navigation, not just a URL check.
  const mountedInput = await page.getByPlaceholder('Filter history…').elementHandle();
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect.poll(async () => (await paginationState(request)).held).toBe(true);
  await expect(footer).toHaveCount(0);
  await expect(page.locator('.tbl-body .row-stretch')).toHaveCount(0);

  // Page controls are unavailable, but browser navigation can change the page.
  // The newer page starts and completes while the older response remains held.
  await navigateHistory(page, '2');
  await expect(page).toHaveURL(/hpage=2$/);
  expect(await mountedInput!.evaluate(el => el.isConnected)).toBe(true);
  await expect(footer.locator('.results-summary')).toHaveText('101–103 of 103');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeEnabled();
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  expect((await paginationState(request)).completed).toEqual([0, 100]);

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
  expect((await paginationState(request)).completed).toEqual([0, 100, 50]);
});

// The largest page the search URL admits: its offset is one page short of
// `MAX_OFFSET` (u32::MAX), the ceiling a wasm32 `usize` can address.
//
// This case used to reach the overflow branch by asking the harness for
// `queryTotal: 4_294_967_300` — 50 rows at that offset, and
// `offset + returned` past `usize::MAX`. It cannot any more, and not
// because the range is bounded: the raw table passes `PageTotal::Probe`,
// under which `PageWindow::new` clamps `returned` to the page size and
// the total bounds nothing. It is because the harness now stamps that
// number on the wire as `pagination.total`, and 4,294,967,300 does not
// decode into a wasm32 `usize` — the response fails to parse before any
// window is computed.
//
// So `PageWindowOverflow` → "This result page extends past the supported
// row range.", in both tables, is no longer exercised from a browser. The
// arithmetic it guards is covered natively by fleet-ui's
// `page_window_checked_arithmetic_at_usize_limits`, which drives both
// `checked_add` sites in `PageWindow::new`. What this case proves is the
// surviving half: the extreme page renders its empty state instead of
// panicking the module.
test('Probe last addressable page renders without a wasm panic', async ({ page, request }) => {
  await configure(request);
  await page.goto('/search?q=service%3Dnginx&page=85899345');
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('Page 85899346 · showing 0 rows');
  await expect(page.locator('.results-table tbody')).toContainText('No events on this page.');
});


test('same-page query reload disables pagination while retained rows remain visible', async ({ page, request }) => {
  await configure(request, { holdQueryNumber: 2 });
  await page.goto('/search?q=service%3Dnginx');
  const footer = page.locator('.results .results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeEnabled();
  const mountedEditor = await page.locator(SEL.dslEditor).elementHandle();
  await page.evaluate(() => {
    const link = document.createElement('a');
    link.href = '/search?q=service%3Dapache';
    document.body.append(link);
    link.click();
    link.remove();
  });
  await expect.poll(async () => (await paginationState(request)).queryHeld).toBe(true);
  expect(await mountedEditor!.evaluate(el => el.isConnected)).toBe(true);
  await expect(page).toHaveURL('/search?q=service%3Dapache');
  await expect(footer.locator('.results-summary')).toHaveText('Page 1 · showing 50 rows');
  await expect(page.locator('.results-table tbody')).toContainText('service=nginx');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();

  expect((await request.post('/__ctl/pagination/query-release')).ok()).toBeTruthy();
  await expect(page.locator('.results-table tbody')).toContainText('service=apache');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeEnabled();
  const state = await (await request.get('/__ctl/state')).json();
  expect(state.queries.map((q: { offset: number }) => q.offset)).toEqual([0, 0]);
});

for (const surface of ['global', 'drawer'] as const) {
  test(`${surface} empty runs keep onboarding guidance and a disabled pager`, async ({ page, request }) => {
    await configure(request, { runsTotal: 0 });
    await page.goto(surface === 'global' ? '/jobs/runs' : '/jobs/nets?net=1&ntab=runs');
    const scope = surface === 'global' ? page.locator('main') : page.locator(SEL.drawerPanel);
    const guidance = surface === 'global'
      ? 'No runs yet — attach a schedule to a net to get started'
      : 'No runs yet — attach a schedule to start.';
    await expect(scope.getByText(guidance, { exact: true })).toBeVisible();
    await expect(scope.locator('.results-footer')).toBeVisible();
    await expect(scope.getByRole('button', { name: 'Prev' })).toBeDisabled();
    await expect(scope.getByRole('button', { name: 'Next' })).toBeDisabled();
  });
}

test('global Runs orders across page boundaries for every key and direction', async ({ page, request }) => {
  test.setTimeout(45_000);
  await configure(request, { runsTotal: 43 });
  // Pause before navigation so a slow initial load cannot start the Jobs poll
  // during this exact request-order assertion. This route needs no load timer.
  await page.clock.install({ time: new Date('2026-09-15T12:05:00Z') });
  await page.clock.pauseAt(new Date('2026-09-15T12:05:01Z'));
  await page.goto('/jobs/runs');
  const frame = page.getByRole('region', { name: 'Recent runs table', exact: true });
  const footer = frame.locator('.results-footer');
  const rows = frame.locator('.row-stretch');
  const idOf = async (row: import('@playwright/test').Locator) =>
    Number(new URL((await row.getAttribute('href'))!, page.url()).searchParams.get('run'));
  await expect(rows).toHaveCount(20);
  // These IDs are independently pinned to the deliberately disordered
  // 43-run wire fixture. Page-two and last-page checks reject sorting only
  // the twenty rows the browser already holds.
  const cases = [
    ['Net', 'net', 'asc', 502, 521, 543], ['Net', 'net', 'desc', 501, 514, 539],
    ['Status', 'status', 'asc', 503, 540, 542], ['Status', 'status', 'desc', 502, 537, 543],
    ['When', 'started', 'desc', 502, 522, 543], ['When', 'started', 'asc', 543, 524, 502],
    ['Duration', 'duration', 'desc', 503, 514, 539], ['Duration', 'duration', 'asc', 506, 508, 539],
    ['Rows', 'rows', 'desc', 504, 508, 541], ['Rows', 'rows', 'asc', 507, 509, 541],
  ] as const;
  for (const [label, key, dir, first, secondPage, last] of cases) {
    await frame.locator('thead').getByRole('button', { name: new RegExp(`^Sort by ${label}(,|$)`) }).click();
    await expect(footer.locator('.results-summary')).toHaveText('1–20 of 43');
    await expect.poll(() => idOf(rows.first())).toBe(first);
    const state = await paginationState(request);
    expect(state.runs.at(-1)).toMatchObject({ sort: key, dir, offset: 0 });
    await footer.getByRole('button', { name: 'Next' }).click();
    await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
    expect(await idOf(rows.first())).toBe(secondPage);
    await footer.getByRole('button', { name: 'Next' }).click();
    await expect(footer.locator('.results-summary')).toHaveText('41–43 of 43');
    expect(await idOf(rows.last())).toBe(last);
  }
  const calls = (await paginationState(request)).runs as Array<{ sort: string; dir: string; offset: number }>;
  // Each new ordering starts at zero, including returning to initial When
  // after visiting other keys. No intermediate old-page/new-sort request.
  expect(calls.map(({ sort, dir, offset }) => [sort, dir, offset])).toEqual([
    ['started', 'desc', 0],
    ...cases.flatMap(([, key, dir]) => [[key, dir, 0], [key, dir, 20], [key, dir, 40]]),
  ]);
  await page.getByPlaceholder('Filter by net…').fill('no such net on this page');
  await expect(rows).toHaveCount(0);
  await expect(footer.locator('.results-summary')).toContainText('41–43 of 43');
  await expect(footer).toContainText('0 matches on this page');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
});
