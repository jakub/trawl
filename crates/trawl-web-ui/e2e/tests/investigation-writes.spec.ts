// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Writes from the investigation board (ADR-0059, #311): Keep on a row of
// the evidence viewer, with a reason, and Close or Reopen beside the
// status. `investigations-writer` signs in a session that holds
// `investigation_write` beside both read grants; the stub records every
// write's body and applies it in trawld's order (harness/server.mjs):
// a closed investigation takes nothing but a reopen, then a PATCH whose
// `expected_seq` moved is `sequence_conflict`.
//
// `/__ctl/investigation/write` stands in for an agent writing while the
// page is open, so the board holds a sequence the server has moved past.
// `/__ctl/investigation/drop-write` lands the next write and cuts its
// answer short, so its outcome is unknown to the page.
//
// The grant matrix at the end snapshots the board header and a viewer
// row for every session that reads, and proves the page asks nothing at
// all of a session that cannot read, whatever else it holds.
//
// Each test freezes the page's clock before the page loads, so the board
// polls only when a test moves time on.

import { test, expect } from '../fixtures';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const ID = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const KEPT = 'fedcba9876543210fedcba9876543210';
const NOW = new Date('2026-10-24T09:45:00Z');
const AGENT = { prefix: 'AbCd_-12', name: 'claude-agent' };
const NOT_PERMITTED = 'You do not have permission to read investigations.';

async function freezeClock(page: Pg, at = NOW) {
  await page.clock.install({ time: at.getTime() - 10_000 });
  await page.clock.pauseAt(at);
}

async function as(request: Ctl, scenario: string) {
  const response = await request.post('/__ctl/reset', { data: { scenario } });
  expect(response.ok()).toBe(true);
}

async function investigationState(request: Ctl): Promise<{
  listReads: number;
  reads: Array<{ since: number }>;
  resultReads: Array<{ id: string; offset: number; limit: number | null }>;
  writes: Array<{ method: string; id: string; body: Record<string, unknown> }>;
}> {
  return (await (await request.get('/__ctl/state')).json()).investigation;
}

async function writes(request: Ctl) {
  return (await investigationState(request)).writes;
}

async function boardReads(request: Ctl): Promise<number[]> {
  return (await investigationState(request)).reads.map((r) => r.since);
}

async function agentWrites(request: Ctl, entries: object[]) {
  const response = await request.post('/__ctl/investigation/write', { data: { entries } });
  expect(response.ok()).toBe(true);
}

async function openBoard(page: Pg) {
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  return main;
}

/** Open the viewer on the board's kept result from its first cited row. */
async function openViewer(page: Pg) {
  const finding = page.locator('.inv-page').getByRole('list', { name: 'Findings' }).locator('> li').nth(0);
  await finding.getByRole('button', { name: 'Open Row 0 of result fedcba98 among its rows' }).click();
  const viewer = page.getByRole('dialog', { name: 'Result fedcba98' });
  await expect(viewer.getByRole('region', { name: 'Rows of result fedcba98' })).toBeVisible();
  return viewer;
}

const status = (main: import('@playwright/test').Locator) => main.locator('.page-hd .sub .bdg').first();

test('a stale close shows the conflict, reads the board again, and the person closes on the current sequence', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(activity).toHaveCount(6);
  expect(await boardReads(request)).toEqual([0]);

  // The agent writes while the board still holds sequence 9.
  await agentWrites(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'note', text: 'Pool back to 64 on gw-2.' } }]);
  await main.getByRole('button', { name: 'Close the investigation' }).click();
  const note = main.locator('.inv-write-note [role="status"]');
  await expect(note).toHaveText(
    'The investigation changed before your close reached it, so nothing changed. The board has read it again: check its status and act again if you still mean to.',
  );
  // The board read again at once, from where it was, without a poll.
  await expect(activity).toHaveCount(7);
  await expect(activity.nth(6)).toContainText('Pool back to 64 on gw-2.');
  expect(await boardReads(request)).toEqual([0, 9]);
  await expect(status(main)).toHaveText('Open');
  expect(await writes(request)).toEqual([
    { method: 'PATCH', id: ID, body: { expected_seq: 9, status: 'closed' } },
  ]);
  // Nothing retried it.
  await page.waitForTimeout(300);
  expect(await writes(request)).toHaveLength(1);

  // The person acts again, on the sequence the board now holds.
  await main.getByRole('button', { name: 'Close the investigation' }).click();
  await expect(note).toHaveText('You closed the investigation.');
  await expect(status(main)).toHaveText('Closed');
  await expect(main.getByRole('button', { name: 'Reopen the investigation' })).toBeVisible();
  await expect(activity).toHaveCount(8);
  await expect(activity.nth(7)).toContainText('Closed the investigation.');
  await expect(activity.nth(7)).toContainText('jakub');
  expect((await writes(request))[1]).toEqual({ method: 'PATCH', id: ID, body: { expected_seq: 10, status: 'closed' } });
  expect(await boardReads(request)).toEqual([0, 9, 10]);
});

test('a close that finds the investigation already closed shows the conflict and the board reads as closed', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  // Someone else closed it after the board read.
  await agentWrites(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'status', status: 'closed' } }]);
  await main.getByRole('button', { name: 'Close the investigation' }).click();
  await expect(main.locator('.inv-write-note [role="status"]')).toHaveText(
    'The investigation was already closed when your close reached it, so nothing changed. The board has read it again.',
  );
  await expect(status(main)).toHaveText('Closed');
  // The control is now the one that applies.
  await expect(main.getByRole('button', { name: 'Reopen the investigation' })).toBeVisible();
  await expect(main.getByRole('button', { name: 'Close the investigation' })).toHaveCount(0);
  expect(await writes(request)).toEqual([
    { method: 'PATCH', id: ID, body: { expected_seq: 9, status: 'closed' } },
  ]);
  expect(await boardReads(request)).toEqual([0, 9]);
});

test('a stale reopen shows the conflict and leaves the board open, as someone else reopened it', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await agentWrites(request, [{ actor: AGENT, at: '2026-10-24T09:43:00.000000Z', payload: { kind: 'status', status: 'closed' } }]);
  await freezeClock(page);
  const main = await openBoard(page);
  await expect(status(main)).toHaveText('Closed');
  await agentWrites(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'status', status: 'open' } }]);

  await main.getByRole('button', { name: 'Reopen the investigation' }).click();
  await expect(main.locator('.inv-write-note [role="status"]')).toHaveText(
    'The investigation changed before your reopen reached it, so nothing changed. The board has read it again: check its status and act again if you still mean to.',
  );
  await expect(status(main)).toHaveText('Open');
  await expect(main.getByRole('button', { name: 'Close the investigation' })).toBeVisible();
  expect(await writes(request)).toEqual([
    { method: 'PATCH', id: ID, body: { expected_seq: 10, status: 'open' } },
  ]);
});

test('a keep posts one row and its reason once, and the keep then reads on the board', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  const viewer = await openViewer(page);
  const rows = viewer.getByRole('region', { name: 'Rows of result fedcba98' });

  await rows.getByRole('button', { name: 'Keep row 2' }).click();
  const form = viewer.getByRole('form', { name: 'Keep row 2' });
  const reason = form.getByRole('textbox', { name: 'Why keep row 2?' });
  await expect(reason).toBeFocused();
  await expect(rows.getByRole('button', { name: 'Keep row 2' })).toHaveAttribute('aria-expanded', 'true');

  // Blank, then too long in bytes: the field says why, and nothing is sent.
  await reason.fill('   ');
  await page.keyboard.press('Enter');
  await expect(reason).toHaveAttribute('aria-invalid', 'true');
  await expect(reason).toHaveAccessibleDescription('Say why you keep this row.');
  await reason.fill('é'.repeat(513));
  await page.keyboard.press('Enter');
  await expect(reason).toHaveAccessibleDescription('The reason is 1026 bytes; it can be at most 1024.');
  expect(await writes(request)).toEqual([]);

  // Pressed twice before the first answer: sent once, trimmed, one row,
  // no `seen_seq`.
  await reason.fill('  the first 502 on gw-2, before the resize  ');
  await expect(reason).toHaveAttribute('aria-invalid', 'false');
  await form.evaluate((el) => {
    const f = el as HTMLFormElement;
    f.requestSubmit();
    f.requestSubmit();
  });
  await expect(viewer.locator('.inv-write-note [role="status"]')).toHaveText('Kept row 2. The board shows it in the activity.');
  expect(await writes(request)).toEqual([
    { method: 'POST', id: ID, body: { kind: 'keep', result_id: KEPT, rows: [2], reason: 'the first 502 on gw-2, before the resize' } },
  ]);
  await expect(form).toHaveCount(0);
  await expect(rows.getByRole('button', { name: 'Keep row 2' })).toBeFocused();

  // The keep reached the board through a read, not the write's answer.
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(activity).toHaveCount(7);
  await expect(activity.nth(6)).toContainText('Kept row 2 of result fedcba98: the first 502 on gw-2, before the resize');
  await expect(activity.nth(6)).toContainText('jakub');
  expect(await boardReads(request)).toEqual([0, 9]);
});

test('a keep whose answer is lost reads as unknown, keeps the reason, and is not sent again until the board is read', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  const viewer = await openViewer(page);
  await viewer.getByRole('button', { name: 'Keep row 0' }).click();
  const form = viewer.getByRole('form', { name: 'Keep row 0' });
  const reason = form.getByRole('textbox', { name: 'Why keep row 0?' });
  await reason.fill('pool exhausted at 64/64');

  expect((await request.post('/__ctl/investigation/drop-write')).ok()).toBe(true);
  await page.keyboard.press('Enter');
  const note = viewer.locator('.inv-write-note [role="status"]');
  await expect(note).toHaveText(
    'The answer to your keep of row 0 was lost, so it may or may not have been kept. Refresh the board and look for it in the activity before you keep it again.',
  );
  // The reason stays, and the form does not send again.
  await expect(reason).toHaveValue('pool exhausted at 64/64');
  const send = form.getByRole('button', { name: 'Keep row 0' });
  await expect(send).toBeDisabled();
  await reason.focus();
  await page.keyboard.press('Enter');
  await page.waitForTimeout(300);
  expect(await writes(request)).toHaveLength(1);
  // Nothing read the board by itself after a write it cannot vouch for.
  expect(await boardReads(request)).toEqual([0]);

  // The refresh the notice offers shows the keep did land, and the form
  // may send again only now.
  await viewer.getByRole('button', { name: 'Refresh the board' }).click();
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(activity).toHaveCount(7);
  await expect(activity.nth(6)).toContainText('Kept row 0 of result fedcba98: pool exhausted at 64/64');
  await expect(send).toBeEnabled();
  await expect(viewer.getByRole('button', { name: 'Refresh the board' })).toHaveCount(0);
  expect(await writes(request)).toHaveLength(1);
});

test('a close whose answer is lost reads as unknown and holds the control until the board is read', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  expect((await request.post('/__ctl/investigation/drop-write')).ok()).toBe(true);
  const close = main.getByRole('button', { name: 'Close the investigation' });
  await close.click();
  await expect(main.locator('.inv-write-note [role="status"]')).toHaveText(
    "The answer to your close was lost, so it may or may not have taken effect. Refresh to see the investigation's status before you act again.",
  );
  await expect(close).toBeDisabled();
  await page.waitForTimeout(300);
  expect(await writes(request)).toHaveLength(1);

  await main.getByRole('button', { name: 'Refresh the board' }).click();
  await expect(status(main)).toHaveText('Closed');
  await expect(main.getByRole('button', { name: 'Reopen the investigation' })).toBeEnabled();
  expect(await writes(request)).toHaveLength(1);
});

test('a keep that finds the investigation closed keeps nothing, and the closed board offers no Keep', async ({ page, request }) => {
  await as(request, 'investigations-writer');
  await freezeClock(page);
  const main = await openBoard(page);
  const viewer = await openViewer(page);
  await viewer.getByRole('button', { name: 'Keep row 2' }).click();
  await viewer.getByRole('textbox', { name: 'Why keep row 2?' }).fill('gw-2 again');
  await agentWrites(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'status', status: 'closed' } }]);
  await page.keyboard.press('Enter');
  await expect(viewer.locator('.inv-write-note [role="status"]')).toHaveText(
    'The investigation was closed before your keep of row 2 reached it, so nothing was kept.',
  );
  // The board read again: closed, and no row offers Keep any more.
  await expect(status(main)).toHaveText('Closed');
  await expect(viewer.getByRole('button', { name: /^Keep row/ })).toHaveCount(0);
  await expect(viewer.getByRole('columnheader', { name: 'Keep' })).toHaveCount(0);
  await expect(viewer.getByRole('form')).toHaveCount(0);
});

test.describe('the grant matrix', () => {
  test('a reader sees no write control on the board header or a viewer row', async ({ page, request }) => {
    await as(request, 'investigations');
    await freezeClock(page);
    const main = await openBoard(page);
    await expect(main.locator('.page-hd')).toMatchAriaSnapshot(`
      - paragraph:
        - link "Investigations":
          - /url: /investigations
      - heading "502s on checkout since 09:00" [level=1]
      - paragraph: Open Opened by claude-agent · updated 2026-10-24 09:41 UTC by claude-agent
      - paragraph: Live checked just now
      - status
      - button "Refresh"
    `);
    const viewer = await openViewer(page);
    await expect(viewer.locator('tbody tr').first()).toMatchAriaSnapshot(`
      - row "0 2026-10-24 09:03:40 gw-2 502 upstream connect timeout to payments:8443 after 3000ms; pool exhausted (64/64 in use)":
        - rowheader "0"
        - cell "2026-10-24 09:03:40"
        - cell "gw-2"
        - cell "502"
        - cell "upstream connect timeout to payments:8443 after 3000ms; pool exhausted (64/64 in use)"
    `);
    await expect(viewer.getByRole('button', { name: /^Keep/ })).toHaveCount(0);
    await expect(viewer.getByRole('columnheader', { name: 'Keep' })).toHaveCount(0);
    expect(await writes(request)).toEqual([]);
  });

  test('a writer has Close beside the status and Keep on a viewer row', async ({ page, request }) => {
    await as(request, 'investigations-writer');
    await freezeClock(page);
    const main = await openBoard(page);
    await expect(main.locator('.page-hd')).toMatchAriaSnapshot(`
      - paragraph:
        - link "Investigations":
          - /url: /investigations
      - heading "502s on checkout since 09:00" [level=1]
      - paragraph:
        - text: Open
        - button "Close the investigation"
        - text: Opened by claude-agent · updated 2026-10-24 09:41 UTC by claude-agent
      - paragraph: Live checked just now
      - status
      - button "Refresh"
    `);
    const viewer = await openViewer(page);
    await expect(viewer.locator('tbody tr').first()).toMatchAriaSnapshot(`
      - row "0 2026-10-24 09:03:40 gw-2 502 upstream connect timeout to payments:8443 after 3000ms; pool exhausted (64/64 in use) Keep row 0":
        - rowheader "0"
        - cell "2026-10-24 09:03:40"
        - cell "gw-2"
        - cell "502"
        - cell "upstream connect timeout to payments:8443 after 3000ms; pool exhausted (64/64 in use)"
        - cell "Keep row 0":
          - button "Keep row 0": Keep
    `);
    await expect(viewer.getByRole('columnheader', { name: 'Keep' })).toBeVisible();
    await expect(viewer.getByRole('button', { name: 'Keep row 0' })).toHaveAttribute('aria-expanded', 'false');
  });

  for (const [scenario, holds] of [
    ['default', 'query alone'],
    ['investigations-write-only', 'query and investigation_write without investigation_read'],
    ['investigations-manage-only', 'server_manage alone'],
  ] as const) {
    test(`a session holding ${holds} reads nothing and has no write control`, async ({ page, request }) => {
      await as(request, scenario);
      await freezeClock(page);
      await page.goto(`/investigations/${ID}`);
      const main = page.locator('.inv-page');
      await expect(main.getByRole('status').filter({ hasText: NOT_PERMITTED })).toBeVisible();
      await expect(page.locator('nav.rail')).toBeVisible();
      await expect(page.locator('nav.rail a[href="/investigations"]')).toHaveCount(0);
      await expect(main).toMatchAriaSnapshot(`
        - heading "Investigations" [level=1]
        - status: ${NOT_PERMITTED}
      `);
      await expect(main.getByRole('button')).toHaveCount(0);
      await expect(page.getByRole('dialog')).toHaveCount(0);
      await page.clock.fastForward(30_000);
      await page.waitForTimeout(300);
      expect(await investigationState(request)).toEqual({
        listReads: 0, reads: [], resultReads: [], writes: [], maxResultsInFlight: 0,
      });
    });
  }
});
