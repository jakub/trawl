// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The investigation board's evidence viewer (ADR-0057, ADR-0059, #311):
// a drawer over one captured result's stored rows, paged by the server's
// page size, opened from a step that captured it or from a row a finding
// cites, with Open in Search below the rows.
//
// The stub stores three results (harness/server.mjs `STORED_RESULTS`):
// the board's three-row kept result; `result-page-paged.json`, seven kept
// rows from January that the stub serves three to a page, as trawld
// serves `[capture] page_max_rows` to a read without `limit`; and
// `result-page-truncated.json`, a capture that hit the cap and kept the 4
// most recent of 31402 rows. The paged result's bounds lie long before
// anything the stub's corpus answers, so the viewer showing them proves
// it reads stored rows, never the query: the stub's `/api/v1/query`
// counter stays at zero. e2e_wire_fixture_contract.rs pins the first two
// fixtures and the Search path below, and decodes every one.
//
// Each test freezes the page's clock before the page loads, so the
// board polls only when a test moves time on.

import { test, expect, lastCapturedQuery } from '../fixtures';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const ID = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const PAGED = '7e57ab1e0c0ffee0d15ea5e0ba5eba11';
const PRUNED = '0a1b2c3d4e5f60718293a4b5c6d7e8f9';
const TRUNCATED = '0ddba11c0ffee0d15ea5e0ba5eba1100';
const NOW = new Date('2026-10-24T09:45:00Z');
const AGENT = { prefix: 'AbCd_-12', name: 'claude-agent' };
// The shared codec's Search path for the paged result's executed text and
// bounds: PAGED_SEARCH_PATH in e2e_wire_fixture_contract.rs.
const PAGED_SEARCH_PATH = '/search?q=service%3Dbilling%20status%3E%3D500%20earliest%3D%222026-01-05T06%3A00%3A00.250000Z%22%20latest%3D%222026-01-05T07%3A00%3A00.000000Z%22%20%7C%20sort%20-_time%2C%20-_ingested&page=0&r=2026-01-05T06:00:00Z..2026-01-05T07:00:00Z';
const PAGED_EXECUTED = 'service=billing status>=500 earliest="2026-01-05T06:00:00.250000Z" latest="2026-01-05T07:00:00.000000Z" | sort -_time, -_ingested';
// The paged result's rows, by captured row number: host and message.
const PAGED_ROWS = [
  ['bill-1', 'payment provider answered 503 after 2900ms'],
  ['bill-2', 'upstream reset before headers'],
  ['bill-1', 'invoice render failed: template missing'],
  ['bill-3', 'upstream connect timeout to ledger:9443'],
  ['bill-2', 'payment provider answered 503 after 3100ms'],
  ['bill-1', 'ledger write refused: lock wait'],
  ['bill-3', 'upstream reset before headers'],
];

async function freezeClock(page: Pg, at = NOW) {
  await page.clock.install({ time: at.getTime() - 10_000 });
  await page.clock.pauseAt(at);
}

async function asReader(request: Ctl, knobs: object = {}) {
  const response = await request.post('/__ctl/reset', { data: { scenario: 'investigations', ...knobs } });
  expect(response.ok()).toBe(true);
}

async function state(request: Ctl) {
  return (await request.get('/__ctl/state')).json();
}

/** The stub's reads of one result: `[offset, limit]`, `limit` null when
 * the page sent none. */
async function resultReads(request: Ctl, id: string): Promise<Array<[number, number | null]>> {
  return (await state(request)).investigation.resultReads
    .filter((r: { id: string }) => r.id === id)
    .map((r: { offset: number; limit: number | null }) => [r.offset, r.limit]);
}

async function write(request: Ctl, entries: object[]) {
  const response = await request.post('/__ctl/investigation/write', { data: { entries } });
  expect(response.ok()).toBe(true);
}

/** The agent's step that captured the paged result, and a finding that
 * cites its row 4: seq 10 and 11 on the board. */
async function writePagedEvidence(request: Ctl) {
  await write(request, [
    {
      actor: AGENT,
      at: '2026-10-24T09:42:00.000000Z',
      payload: {
        kind: 'step',
        executed: PAGED_EXECUTED,
        bounds: { earliest: '2026-01-05T06:00:00.250000Z', latest: '2026-01-05T07:00:00.000000Z' },
        request_id: 'req-jan',
        query_id: 90,
        purpose: 'Look at the January billing outage for the same pattern',
        outcome: { outcome: 'success', result_id: PAGED, rows: 7, duration_ms: 75 },
      },
    },
    {
      actor: AGENT,
      at: '2026-10-24T09:43:00.000000Z',
      payload: {
        kind: 'finding',
        statement: 'January had the same lock wait in front of the 502s.',
        status: 'hypothesis',
        references: [{ result_id: PAGED, row: 4 }],
        counterevidence: [],
      },
    },
  ]);
}

/** Open the step at `index` from the keyboard (the frozen clock holds
 * animation frames) and press its View rows. */
async function viewStepRows(page: Pg, index: number) {
  const step = page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li').nth(index);
  await step.locator('summary').focus();
  await page.keyboard.press('Enter');
  await expect(step.locator('details')).toHaveAttribute('open', '');
  await step.getByRole('button', { name: /^View the rows of result / }).click();
}

/** Each shown row as `[row number, host, message]`. */
async function shownRows(viewer: import('@playwright/test').Locator) {
  return viewer.locator('tbody tr').evaluateAll((rows) => rows.map((row) => {
    const cells = [...row.children].map((cell) => cell.textContent ?? '');
    return [cells[0], cells[2], cells[5]];
  }));
}

function expected(from: number, to: number) {
  return PAGED_ROWS.slice(from, to).map(([host, message], i) => [String(from + i), host, message]);
}

test('the viewer pages a captured result by the server page size, from stored rows only', async ({ page, request }) => {
  await asReader(request);
  await writePagedEvidence(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  await expect(page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li')).toHaveCount(4);

  await viewStepRows(page, 3);
  const viewer = page.getByRole('dialog', { name: 'Result 7e57ab1e' });
  await expect(viewer).toBeVisible();
  const rows = viewer.getByRole('region', { name: 'Rows of result 7e57ab1e' });
  await expect(rows.locator('thead th')).toHaveText(['Row', '_time', 'host', 'status', 'bytes', 'message']);
  expect(await shownRows(rows)).toEqual(expected(0, 3));
  // Typed cells: the timestamp's fraction, the status as an integer.
  await expect(rows.locator('tbody tr').nth(0).locator('td').nth(0)).toHaveText('2026-01-05 06:59:58.12');
  await expect(viewer.locator('.results-summary')).toContainText('1–3 of 7');
  // No row is marked when a step opened the viewer.
  await expect(rows.locator('tr[aria-current]')).toHaveCount(0);

  await viewer.getByRole('button', { name: 'Next →' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('4–6 of 7');
  expect(await shownRows(rows)).toEqual(expected(3, 6));
  // A u64 past 2^53 in full, a null as NULL.
  await expect(rows.locator('tbody tr').nth(0).locator('td').nth(3)).toHaveText('18446744073709551615');
  await expect(rows.locator('tbody tr').nth(2).locator('td').nth(3)).toHaveText('NULL');

  await viewer.getByRole('button', { name: 'Next →' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('7–7 of 7');
  expect(await shownRows(rows)).toEqual(expected(6, 7));
  await expect(viewer.getByRole('button', { name: 'Next →' })).toBeDisabled();

  await viewer.getByRole('button', { name: '← Prev' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('4–6 of 7');
  expect(await shownRows(rows)).toEqual(expected(3, 6));

  // Every page came from the stored result, without a limit, at the
  // offsets the server's page size makes; the board's cited row 4 was
  // read once with its one-row page. Nothing ran the query: the rows,
  // from January, are older than anything the corpus would answer.
  expect(await resultReads(request, PAGED)).toEqual([[4, 1], [0, null], [3, null], [6, null], [3, null]]);
  expect((await state(request)).queries).toEqual([]);
});

test('a capture the cap cut says how many rows it kept, and a whole one says nothing', async ({ page, request }) => {
  await asReader(request);
  await writePagedEvidence(request);
  await write(request, [{
    actor: AGENT,
    at: '2026-10-24T09:44:00.000000Z',
    payload: {
      kind: 'step',
      executed: 'service=billing earliest="2026-01-05T00:00:00.000000Z" latest="2026-01-05T07:00:00.000000Z" | sort -_time, -_ingested',
      bounds: { earliest: '2026-01-05T00:00:00.000000Z', latest: '2026-01-05T07:00:00.000000Z' },
      request_id: 'req-jan-all',
      query_id: 91,
      purpose: 'Every billing event that morning',
      outcome: { outcome: 'success', result_id: TRUNCATED, rows: 4, duration_ms: 410 },
    },
  }]);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  await expect(page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li')).toHaveCount(5);
  const cut = /^This result kept /;

  // A whole result: no notice.
  await viewStepRows(page, 3);
  let viewer = page.getByRole('dialog', { name: 'Result 7e57ab1e' });
  await expect(viewer.locator('.results-summary')).toContainText('1–3 of 7');
  await expect(viewer.getByText(cut)).toHaveCount(0);
  await page.keyboard.press('Escape');
  await expect(viewer).toHaveCount(0);

  // The cut result: one plain line above the rows, on every page.
  await viewStepRows(page, 4);
  viewer = page.getByRole('dialog', { name: 'Result 0ddba11c' });
  const notice = viewer.getByText(cut);
  await expect(notice).toHaveText('This result kept the 4 most recent of 31402 rows.');
  await expect(notice).not.toHaveAttribute('role');
  await expect(viewer.getByRole('alert')).toHaveCount(0);
  const order = await viewer.locator('.inv-viewer > *').evaluateAll((els) => els.map((el) => el.textContent ?? ''));
  expect(order.findIndex((t) => t.startsWith('This result kept'))).toBe(0);
  await expect(viewer.locator('.results-summary')).toContainText('1–3 of 4');
  await viewer.getByRole('button', { name: 'Next →' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('4–4 of 4');
  await expect(notice).toHaveText('This result kept the 4 most recent of 31402 rows.');
  expect((await state(request)).queries).toEqual([]);
});

test('a cited row opens the viewer at its page with the row marked, and the page size is learned once', async ({ page, request }) => {
  await asReader(request);
  await writePagedEvidence(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const finding = page.locator('.inv-page').getByRole('list', { name: 'Findings' }).locator('> li').nth(2);
  await expect(finding.getByRole('region', { name: 'Row 4 of result 7e57ab1e' })).toContainText('payment provider answered 503 after 3100ms');

  const open = finding.getByRole('button', { name: 'Open Row 4 of result 7e57ab1e among its rows' });
  await open.click();
  const viewer = page.getByRole('dialog', { name: 'Result 7e57ab1e' });
  const rows = viewer.getByRole('region', { name: 'Rows of result 7e57ab1e' });
  await expect(viewer.locator('.results-summary')).toContainText('4–6 of 7');
  expect(await shownRows(rows)).toEqual(expected(3, 6));
  const marked = rows.locator('tr[aria-current="true"]');
  await expect(marked).toHaveCount(1);
  await expect(marked.locator('th')).toHaveText('4');
  // The page size was not known: one read of the first page learned it.
  expect(await resultReads(request, PAGED)).toEqual([[4, 1], [0, null], [3, null]]);

  // Escape closes the viewer and gives focus back to the control.
  await page.keyboard.press('Escape');
  await expect(viewer).toHaveCount(0);
  await expect(open).toBeFocused();

  // Now the page knows the size and goes straight to the row's page.
  await open.click();
  await expect(marked.locator('th')).toHaveText('4');
  expect(await resultReads(request, PAGED)).toEqual([[4, 1], [0, null], [3, null], [3, null]]);
  expect((await state(request)).queries).toEqual([]);
});

test('Open in Search is the shared codec path, below the rows, and Search runs the exact bounds', async ({ page, request }) => {
  await asReader(request);
  await writePagedEvidence(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  await viewStepRows(page, 3);
  const viewer = page.getByRole('dialog', { name: 'Result 7e57ab1e' });
  const link = viewer.getByRole('link', { name: 'Open in Search' });
  await expect(link).toHaveAttribute('href', PAGED_SEARCH_PATH);
  // The rows come first, then the pager, then the link.
  const order = await viewer.locator('.inv-viewer > *').evaluateAll((els) => els.map((el) => el.className));
  expect(order.findIndex((c) => c.includes('inv-viewer-rows'))).toBeLessThan(order.findIndex((c) => c.includes('inv-open-search')));
  expect((await state(request)).queries).toEqual([]);

  await link.click();
  await expect(page).toHaveURL(PAGED_SEARCH_PATH);
  const sent = await lastCapturedQuery(request, 1);
  // Search merged the hull ahead of the executed text, which it sent
  // unchanged: the exact bounds stand, and nothing widened them.
  expect(sent.query.replace(/\s+/g, ' ').trim())
    .toBe(`_time>="2026-01-05T06:00:00Z" _time<="2026-01-05T07:00:00Z" ${PAGED_EXECUTED}`);
});

test('a pruned result reads Pruned in the viewer and the finding keeps its statement and citation', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  // The fixture's third step captured the result whose evidence was pruned.
  await viewStepRows(page, 2);
  const viewer = page.getByRole('dialog', { name: 'Result 0a1b2c3d' });
  await expect(viewer.locator('.inv-pruned')).toHaveText(
    "Pruned the investigation's evidence retention ended, so this result's file was removed; the findings that cite it remain",
  );
  await expect(viewer.getByRole('region')).toHaveCount(0);
  await expect(viewer.getByRole('link', { name: 'Open in Search' })).toHaveCount(0);
  await expect(viewer.locator('.results-footer')).toHaveCount(0);

  const finding = main.getByRole('list', { name: 'Findings' }).locator('> li').nth(1);
  await expect(finding).toContainText('The resize halved the pool on gw-2 only; gw-1 kept its old size.');
  await expect(finding.locator('.inv-ref')).toHaveText('Row 1 of result 0a1b2c3d');
  await expect(finding.locator('.inv-pruned')).toContainText('Pruned');
  // A pruned cited row offers no viewer of its own.
  await expect(finding.getByRole('button', { name: /among its rows/ })).toHaveCount(0);
  expect(await resultReads(request, PRUNED)).toEqual([[1, 1], [0, null]]);
});

test('a missing file reads unavailable, a corrupt one keeps a Retry, and a 429 waits; none reads as empty', async ({ page, request }) => {
  await asReader(request, { investigationResultFail: { id: PAGED, status: 409, count: 1 } });
  await writePagedEvidence(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const finding = page.locator('.inv-page').getByRole('list', { name: 'Findings' }).locator('> li').nth(2);
  // The cited row's own read took the 409; Refresh reads it again.
  await expect(finding).toContainText("Could not load this row. the result's file is missing");
  await page.locator('.inv-page').getByRole('button', { name: 'Refresh' }).click();
  await expect(finding.getByRole('region', { name: 'Row 4 of result 7e57ab1e' })).toBeVisible();

  await asReader(request, { investigationResultFail: { id: PAGED, status: 409, count: 1 } });
  await writePagedEvidence(request);
  await viewStepRows(page, 3);
  const viewer = page.getByRole('dialog', { name: 'Result 7e57ab1e' });
  await expect(viewer.getByRole('status')).toHaveText("These rows are unavailable. the result's file is missing");
  await expect(viewer.getByRole('region')).toHaveCount(0);
  await expect(viewer.getByText('This result captured no rows.')).toHaveCount(0);
  await viewer.getByRole('button', { name: 'Check again' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('1–3 of 7');

  // A corrupt file: an error with a Retry, then the page it asked for.
  await asReader(request, { investigationResultFail: { id: PAGED, status: 500, count: 1 } });
  await writePagedEvidence(request);
  await viewer.getByRole('button', { name: 'Next →' }).click();
  await expect(viewer.getByRole('status')).toHaveText("Could not read the rows. the result's file does not match its digest");
  await viewer.getByRole('button', { name: 'Retry' }).click();
  await expect(viewer.locator('.results-summary')).toContainText('4–6 of 7');

  // A 429: the viewer says it waits, keeps the page it shows, and reads
  // the page it was asked for once the Retry-After has passed.
  await asReader(request, { investigationThrottle: 1, investigationRetryAfter: '2' });
  await writePagedEvidence(request);
  await viewer.getByRole('button', { name: 'Next →' }).click();
  await expect(viewer.getByRole('status')).toHaveText('The server is limiting reads for your API key. Reading again shortly.');
  await expect(viewer.locator('.results-summary')).toContainText('4–6 of 7');
  expect(await resultReads(request, PAGED)).toEqual([[6, null]]);
  await page.clock.fastForward(2_000);
  await expect(viewer.locator('.results-summary')).toContainText('7–7 of 7');
  await expect(viewer.getByRole('status')).toHaveCount(0);
  expect(await resultReads(request, PAGED)).toEqual([[6, null], [6, null]]);
  expect((await state(request)).queries).toEqual([]);
});
