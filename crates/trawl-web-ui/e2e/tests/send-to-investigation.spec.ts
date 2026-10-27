// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Send to investigation from Search (ADR-0057, ADR-0059, #311). The
// `investigations-writer` session holds `investigation_write` beside
// both read grants, and `investigationSearch` makes every search answer
// the `query-send-rows` events inside the `_time` bounds of its text, in
// stored order; a capture keeps the same events inside its window, newest
// first as trawld sorts raw events (harness/server.mjs). So the rows a
// capture keeps equal the rows Search showed, as a set, only when the
// request carries the window Search ran. The stub records each
// `POST /api/v1/results` body in `captures` and, for the served board,
// appends the human's step and stores the result so the evidence viewer
// pages it.
//
// The dialog sends the search Search ran: the response's own query state,
// read as the range-free text and a half-open window by the shared codec,
// so a fixed `[from, to]` is sent as `[from, to + 1 µs)`. A moving range
// is pinned to `execution.started_at` (`2026-09-15T12:34:56Z` in every
// stubbed answer), Search runs again on the pinned range, and that search
// is sent.

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;
type Loc = import('@playwright/test').Locator;

const BOARD = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const BOARD_TITLE = '502s on checkout since 09:00';
const OTHER = 'c0ffee00c0ffee00c0ffee00c0ffee00';
const OTHER_TITLE = 'Nightly backup job slower than usual';
const ABSOLUTE = '/search?q=status%3E%3D500&r=2026-09-15T12:00:00Z..2026-09-15T12:30:00Z';
const MOVING = '/search?q=status%3E%3D500&r=15m';
// `query-send-rows` holds ten events from 11:50 to 12:40: seven inside
// ABSOLUTE (an event exactly at 12:30 included), all ten for a text with
// no `_time` bound, four inside the range MOVING pins to.
const ABSOLUTE_ROWS = 7;
// The first result the stub keeps for a capture.
const FIRST_CAPTURE = '5e2d05e2d05e2d05e2d05e2d05e2d001';

async function as(request: Ctl, scenario: string, search = true, knobs: Record<string, unknown> = {}) {
  const response = await request.post('/__ctl/reset', { data: { scenario, investigationSearch: search, ...knobs } });
  expect(response.ok()).toBe(true);
}

async function captures(request: Ctl): Promise<Array<Record<string, unknown>>> {
  return (await (await request.get('/__ctl/state')).json()).captures;
}

async function queries(request: Ctl): Promise<string[]> {
  return (await (await request.get('/__ctl/state')).json()).queries.map((q: { query: string }) => q.query);
}

function trailing(page: Pg): Loc {
  return page.locator('.tabs-actions');
}

/** The rows a results table shows, cell by cell, without its controls. */
async function tableRows(rows: Loc): Promise<string[][]> {
  return rows.evaluateAll((trs) => trs.map((tr) =>
    Array.from(tr.querySelectorAll('td:not(.exp-col):not(.inv-keep-col)'), (td) => (td.textContent ?? '').trim())));
}

function searchRows(page: Pg): Loc {
  return page.locator('#search-results table.results-table > tbody > tr:not(:has(td.detail))');
}

async function openSearch(page: Pg, path: string, rows = ABSOLUTE_ROWS) {
  await page.goto(path);
  await expect(searchRows(page)).toHaveCount(rows);
}

/** Every row the evidence viewer pages, in the order it pages them. */
async function viewerRows(viewer: Loc, rows: Loc): Promise<string[][]> {
  const kept: string[][] = [];
  const summary = viewer.locator('.results-summary');
  const next = viewer.getByRole('button', { name: 'Next →' });
  for (;;) {
    await expect(viewer).not.toHaveAttribute('aria-busy', 'true');
    kept.push(...await tableRows(rows.locator('tbody tr')));
    if (await next.count() === 0 || await next.isDisabled()) return kept;
    const before = await summary.textContent();
    await next.click();
    await expect(summary).not.toHaveText(before ?? '');
  }
}

/** Rows as a set: the same events, whatever order they came in. */
function asSet(rows: string[][]): string[] {
  return rows.map((row) => JSON.stringify(row)).sort();
}

async function openDialog(page: Pg) {
  await trailing(page).getByRole('button', { name: 'Send to investigation' }).click();
  const dialog = page.getByRole('dialog', { name: 'Send to investigation' });
  await expect(dialog.getByRole('radio')).toHaveCount(2);
  return dialog;
}

test('a fixed range sends the executed text and its half-open window, and the board pages the same rows', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const shown = await tableRows(searchRows(page));
  expect(shown).toHaveLength(ABSOLUTE_ROWS);

  const dialog = await openDialog(page);
  // No default target: nothing is sent until a person picks one.
  await expect(dialog.getByRole('radio', { checked: true })).toHaveCount(0);
  const send = dialog.getByRole('button', { name: 'Send', exact: true });
  await expect(send).toBeDisabled();
  await expect(dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` })).toBeVisible();
  await expect(dialog.getByRole('radio', { name: `${OTHER_TITLE} c0ffee00` })).toBeVisible();
  // The exact text and window the capture carries.
  await expect(dialog.locator('.send-query')).toHaveText('status>=500');
  await expect(dialog.locator('.send-window')).toHaveText(
    'From 2026-09-15T12:00:00.000000Z up to, not including, 2026-09-15T12:30:00.000001Z');
  await expect(dialog.locator('#sendPurposeHint')).toHaveText('0 of 1024 bytes.');

  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('  Which hosts answered 500 in the half hour  ');
  await expect(dialog.locator('#sendPurposeHint')).toHaveText('41 of 1024 bytes.');
  await send.click();

  const toast = page.locator('.toast').filter({ hasText: 'Sent to investigation' });
  await expect(toast).toContainText(`Step 10 in ${BOARD_TITLE}`);
  await expect(dialog).toHaveCount(0);
  expect(await captures(request)).toEqual([{
    query: 'status>=500',
    window: { earliest: '2026-09-15T12:00:00.000000Z', latest: '2026-09-15T12:30:00.000001Z' },
    investigation: { id: BOARD, purpose: 'Which hosts answered 500 in the half hour' },
  }]);

  await toast.getByRole('link', { name: 'Open the board' }).click();
  await expect(page).toHaveURL(`/investigations/${BOARD}`);
  const steps = page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li');
  const step = steps.filter({ hasText: 'Which hosts answered 500 in the half hour' });
  await expect(step).toHaveCount(1);
  await step.locator('summary').click();
  await expect(step.locator('.inv-executed')).toHaveText(
    'status>=500 earliest="2026-09-15T12:00:00.000000Z" latest="2026-09-15T12:30:00.000001Z" | sort -_time, -_ingested');
  await expect(step.locator('.inv-meta').last()).toContainText('jakub');
  await step.getByRole('button', { name: `View the rows of result ${FIRST_CAPTURE.slice(0, 8)}` }).click();

  const viewer = page.getByRole('dialog', { name: `Result ${FIRST_CAPTURE.slice(0, 8)}` });
  const rows = viewer.getByRole('region', { name: `Rows of result ${FIRST_CAPTURE.slice(0, 8)}` });
  await expect(viewer.locator('.results-summary')).toContainText(`1–3 of ${ABSOLUTE_ROWS}`);
  // The same events Search showed. A capture sorts raw events newest
  // first while Search showed them in stored order, so they are compared
  // as sets.
  const kept = await viewerRows(viewer, rows);
  expect(asSet(kept)).toEqual(asSet(shown));
  expect(await queries(request)).toHaveLength(1);
});

test('a window that holds none of the events keeps none, whatever the stub holds', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await page.goto('/search?q=status%3E%3D500&r=2020-01-01T00:00:00Z..2020-01-01T00:01:00Z');
  await expect(trailing(page).getByRole('button', { name: 'Send to investigation' })).toBeVisible();
  await expect(searchRows(page)).toHaveText(['No events match this query. Check the time range and filters.']);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('A minute in 2020');
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();
  await expect(page.locator('.toast').filter({ hasText: 'Sent to investigation' })).toContainText(`Step 10 in ${BOARD_TITLE}`);
  expect(await captures(request)).toEqual([{
    query: 'status>=500',
    window: { earliest: '2020-01-01T00:00:00.000000Z', latest: '2020-01-01T00:01:00.000001Z' },
    investigation: { id: BOARD, purpose: 'A minute in 2020' },
  }]);

  await page.goto(`/investigations/${BOARD}`);
  const step = page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li')
    .filter({ hasText: 'A minute in 2020' });
  await step.locator('summary').click();
  await step.getByRole('button', { name: `View the rows of result ${FIRST_CAPTURE.slice(0, 8)}` }).click();
  const viewer = page.getByRole('dialog', { name: `Result ${FIRST_CAPTURE.slice(0, 8)}` });
  await expect(viewer.getByText('This result captured no rows.')).toBeVisible();
});

test('a moving range is pinned to when the search ran, run again, and that search is sent', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, MOVING, 10);
  const dialog = await openDialog(page);
  await expect(dialog.locator('.send-moving')).toHaveText(
    "This search's range moves: it ends when the search runs. A capture is fixed, so the range is pinned to when "
    + 'this search ran, 2026-09-15T12:19:56Z to 2026-09-15T12:34:56Z. Search runs again on that range, and then that search is sent.');
  await expect(dialog.locator('.send-query')).toHaveCount(0);
  const pin = dialog.getByRole('button', { name: 'Pin and re-run' });
  await expect(pin).toBeDisabled();
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('The last quarter hour, pinned');
  await pin.click();

  await expect(page.locator('.toast').filter({ hasText: 'Sent to investigation' })).toContainText(`Step 10 in ${BOARD_TITLE}`);
  await expect(page).toHaveURL(/r=2026-09-15T12:19:56Z\.\.2026-09-15T12:34:56Z/);
  // Search shows the pinned range's four events, and the capture keeps
  // those four.
  await expect(searchRows(page)).toHaveCount(4);
  await page.goto(`/investigations/${BOARD}`);
  const step = page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li')
    .filter({ hasText: 'The last quarter hour, pinned' });
  await step.locator('summary').click();
  await step.getByRole('button', { name: `View the rows of result ${FIRST_CAPTURE.slice(0, 8)}` }).click();
  const viewer = page.getByRole('dialog', { name: `Result ${FIRST_CAPTURE.slice(0, 8)}` });
  await expect(viewer.locator('.results-summary')).toContainText('1–3 of 4');
  // Search ran twice: the quick range, then the pinned absolute one.
  expect(await queries(request)).toEqual([
    'last=15m status>=500',
    '_time>="2026-09-15T12:19:56Z" _time<="2026-09-15T12:34:56Z" status>=500',
  ]);
  expect(await captures(request)).toEqual([{
    query: 'status>=500',
    window: { earliest: '2026-09-15T12:19:56.000000Z', latest: '2026-09-15T12:34:56.000001Z' },
    investigation: { id: BOARD, purpose: 'The last quarter hour, pinned' },
  }]);
});

test('Ctrl+Enter in a ready dialog sends, once', async ({ page, request }) => {
  // The control for the refused dialogs below: the keystroke reaches the
  // dialog's submit, so their "nothing is sent" is the guard's doing.
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('Sent from the keyboard');
  await dialog.getByLabel('Purpose (optional)').press('Control+Enter');
  await expect(page.locator('.toast').filter({ hasText: 'Sent to investigation' })).toContainText(`Step 10 in ${BOARD_TITLE}`);
  expect(await captures(request)).toEqual([{
    query: 'status>=500',
    window: { earliest: '2026-09-15T12:00:00.000000Z', latest: '2026-09-15T12:30:00.000001Z' },
    investigation: { id: BOARD, purpose: 'Sent from the keyboard' },
  }]);
});

test('a query whose own time clause moves is not sent, and nothing offers to send it', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, '/search?q=last%3D1h%20status%3E%3D500&r=15m', 10);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await expect(dialog.getByRole('alert')).toHaveText(
    'This query sets its own time range, and that range moves: it ends when the capture runs, not when this '
    + 'search ran. A capture is fixed, so this search cannot be sent. Remove last=, earliest= and latest= from '
    + 'the query, pick the range in the range control, run the search, then send it.');
  await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
  await expect(dialog.getByRole('button', { name: 'Pin and re-run' })).toHaveCount(0);
  await expect(dialog.locator('.send-query')).toHaveCount(0);
  // Ctrl+Enter submits the dialog: still nothing is sent.
  await dialog.getByLabel('Purpose (optional)').fill('A moving hour');
  await dialog.getByLabel('Purpose (optional)').press('Control+Enter');
  await page.waitForTimeout(500);
  expect(await captures(request)).toEqual([]);
  expect(await queries(request)).toHaveLength(1);
});

test('a range longer than one capture may cover is neither pinned nor sent', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  // The 7-day preset pins to seven days and a microsecond, past trawld's
  // default `[capture] max_span_secs` of seven days.
  await openSearch(page, '/search?q=status%3E%3D500&r=7d', 10);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await expect(dialog.getByRole('alert')).toHaveText(
    'The range is longer than one capture may cover, 7 days on this server. Pick a shorter range, run the search, then send it.');
  await expect(dialog.getByRole('button', { name: 'Pin and re-run' })).toHaveCount(0);
  await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
  await dialog.getByLabel('Purpose (optional)').press('Control+Enter');
  await page.waitForTimeout(500);
  expect(await captures(request)).toEqual([]);
  expect(await queries(request)).toHaveLength(1);
  await expect(page).toHaveURL(/r=7d/);

  // A fixed range is held to the limit the server advertises too.
  await as(request, 'investigations-writer', true, { captureMaxSpanSecs: 600 });
  await openSearch(page, ABSOLUTE);
  const fixed = await openDialog(page);
  await expect(fixed.getByRole('alert')).toHaveText(
    'The range is longer than one capture may cover, 10 minutes on this server. Pick a shorter range, run the search, then send it.');
  await expect(fixed.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
  expect(await captures(request)).toEqual([]);
});

test('capabilities that cannot be read refuse nothing: the server decides', async ({ page, request }) => {
  await as(request, 'investigations-writer', true, { captureMaxSpanSecs: 600, capabilitiesFail: true });
  await openSearch(page, ABSOLUTE);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();
  await expect(page.locator('.toast').filter({ hasText: 'Sent to investigation' })).toContainText(`Step 10 in ${BOARD_TITLE}`);
  expect(await captures(request)).toHaveLength(1);
  expect((await (await request.get('/__ctl/state')).json()).capabilityReads).toBe(1);
});

test('the dialog sends the accepted search, not the draft in the editor', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const editor = page.locator('.dsl-editor [contenteditable=true]');
  await editor.click();
  await page.keyboard.press('End');
  await page.keyboard.type(' host=web-01');
  await expect(page.locator(SEL.draftState)).toHaveText(COPY.draftDirty);

  const dialog = await openDialog(page);
  await expect(dialog.locator('.send-query')).toHaveText('status>=500');
  await dialog.getByRole('radio', { name: `${OTHER_TITLE} c0ffee00` }).check();
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();
  await expect(page.locator('.toast').filter({ hasText: 'Sent to investigation' })).toContainText(`Step 4 in ${OTHER_TITLE}`);
  expect(await captures(request)).toEqual([{
    query: 'status>=500',
    window: { earliest: '2026-09-15T12:00:00.000000Z', latest: '2026-09-15T12:30:00.000001Z' },
    investigation: { id: OTHER },
  }]);
  // The draft is still the draft.
  await expect(editor).toHaveText('status>=500 host=web-01');
});

test('a target closed meanwhile keeps the input and lists the open investigations again', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('Before it closed');
  // The agent closes the investigation while the dialog is open.
  const closed = await request.post('/__ctl/investigation/write', {
    data: { entries: [{ payload: { kind: 'status', status: 'closed' } }] },
  });
  expect(closed.ok()).toBe(true);
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();

  await expect(dialog.getByRole('alert')).toHaveText(
    `“${BOARD_TITLE}” was closed before the search reached it. Nothing was sent. Choose an open investigation.`);
  await expect(dialog.getByRole('radio')).toHaveCount(1);
  await expect(dialog.getByRole('radio', { name: `${OTHER_TITLE} c0ffee00` })).not.toBeChecked();
  await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toBeDisabled();
  await expect(dialog.getByLabel('Purpose (optional)')).toHaveValue('Before it closed');
  expect(await captures(request)).toHaveLength(1);
  expect((await (await request.get('/__ctl/state')).json()).investigation.listReads).toBe(2);
});

test('an answer lost on the way back is an unknown outcome, and nothing is sent again', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('Lost on the way back');
  expect((await request.post('/__ctl/investigation/drop-write')).ok()).toBe(true);
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();

  await expect(dialog.getByRole('alert')).toHaveText(
    'The answer was lost on the way back, so it is not known whether the search reached the investigation. '
    + 'Open its board to check before you send it again.');
  await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
  await expect(dialog.getByLabel('Purpose (optional)')).toHaveValue('Lost on the way back');
  await expect(dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` })).toBeChecked();
  // Nothing sends it again by itself.
  await page.waitForTimeout(1_500);
  expect(await captures(request)).toHaveLength(1);

  // The board says whether it landed: here it did.
  await dialog.getByRole('link', { name: 'Open the board' }).click();
  await expect(page).toHaveURL(`/investigations/${BOARD}`);
  await expect(page.locator('.inv-page').getByRole('list', { name: 'Steps' }).locator('> li')
    .filter({ hasText: 'Lost on the way back' })).toHaveCount(1);
  expect(await captures(request)).toHaveLength(1);
});

test('a capture the server ran and then answered with an internal error is an unknown outcome', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await openSearch(page, ABSOLUTE);
  const dialog = await openDialog(page);
  await dialog.getByRole('radio', { name: `${BOARD_TITLE} 5f0c1a2b` }).check();
  await dialog.getByLabel('Purpose (optional)').fill('Answered 500 after it ran');
  expect((await request.post('/__ctl/investigation/drop-write', { data: { status: 500, code: 'internal_error' } })).ok()).toBe(true);
  await dialog.getByRole('button', { name: 'Send', exact: true }).click();
  await expect(dialog.getByRole('alert')).toHaveText(
    'The answer was lost on the way back, so it is not known whether the search reached the investigation. '
    + 'Open its board to check before you send it again.');
  await expect(dialog.getByRole('button', { name: 'Send', exact: true })).toHaveCount(0);
  await expect(dialog.getByRole('link', { name: 'Open the board' })).toBeVisible();
  expect(await captures(request)).toHaveLength(1);
});

test.describe('the control is offered only where a send can work', () => {
  test('a writer sees it beside Export on an accepted snapshot; a reader does not', async ({ page, request }) => {
    await as(request, 'investigations-writer');
    await openSearch(page, ABSOLUTE);
    await expect(trailing(page)).toMatchAriaSnapshot(`
      - button "View" [expanded=false]
      - button "Export"
      - button "Send to investigation"
    `);

    await as(request, 'investigations');
    await openSearch(page, ABSOLUTE);
    await expect(trailing(page)).toMatchAriaSnapshot(`
      - /children: equal
      - button "View" [expanded=false]
      - button "Export"
    `);
    await as(request, 'investigations-write-only');
    await openSearch(page, ABSOLUTE);
    await expect(trailing(page).getByRole('button', { name: 'Send to investigation' })).toHaveCount(0);
    await as(request, 'investigations-manage-only');
    await page.goto(ABSOLUTE);
    await expect(trailing(page).getByRole('button', { name: 'Export' })).toBeVisible();
    await expect(trailing(page).getByRole('button', { name: 'Send to investigation' })).toHaveCount(0);
    expect(await captures(request)).toEqual([]);
  });

  test('a live search is not offered', async ({ page, request }) => {
    await as(request, 'investigations-writer');
    await page.goto(`${ABSOLUTE}&mode=live`);
    await expect(trailing(page).getByRole('button', { name: 'Stop live' })).toBeVisible();
    await expect(trailing(page)).toMatchAriaSnapshot(`
      - /children: equal
      - button "Stop live"
      - button "View" [expanded=false]
      - button "Export"
    `);
  });

  test('a from saved search is not offered', async ({ page, request }) => {
    await as(request, 'investigations-writer');
    await page.goto('/search?q=%7C%20from%20saved%20nightly&r=2026-09-15T12:00:00Z..2026-09-15T12:30:00Z');
    await expect(searchRows(page)).toHaveCount(ABSOLUTE_ROWS);
    await expect(trailing(page)).toMatchAriaSnapshot(`
      - /children: equal
      - button "View" [expanded=false]
      - button "Export"
    `);
  });
});
