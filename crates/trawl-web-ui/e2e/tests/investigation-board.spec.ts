// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The live investigation board (ADR-0059, #311): while its tab is
// visible the board reads the entries changed since its last read every
// five seconds, and while the tab is hidden it reads nothing.
//
// Each test freezes the page's clock before the page loads, so a poll
// happens only when the test moves time on, and counts the stub's reads
// of the investigation. `/__ctl/investigation/write` stands in for an
// agent writing through the API while the page is open, and
// `/__ctl/investigation/fail` fails the next reads. Hidden is
// `document.hidden` overridden and announced with `visibilitychange`, as
// jobs-live.spec.ts does.

import { test, expect } from '../fixtures';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const ID = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
// A few minutes after the fixture's last write.
const NOW = new Date('2026-10-24T09:45:00Z');

/** Freeze the page's clock at `at` before the page loads. */
async function freezeClock(page: Pg, at = NOW) {
  await page.clock.install({ time: at.getTime() - 10_000 });
  await page.clock.pauseAt(at);
}

async function asReader(request: Ctl) {
  const response = await request.post('/__ctl/reset', { data: { scenario: 'investigations' } });
  expect(response.ok()).toBe(true);
}

/** The `since` of every read of the investigation the stub answered or
 * failed, in order. */
async function boardReads(request: Ctl): Promise<number[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.investigation.reads.map((r: { since: number }) => r.since);
}

async function write(request: Ctl, entries: object[]) {
  const response = await request.post('/__ctl/investigation/write', { data: { entries } });
  expect(response.ok()).toBe(true);
}

async function setHidden(page: Pg, hidden: boolean) {
  await page.evaluate((value) => {
    Object.defineProperty(document, 'hidden', { configurable: true, value });
    document.dispatchEvent(new Event('visibilitychange'));
  }, hidden);
}

const AGENT = { prefix: 'AbCd_-12', name: 'claude-agent' };

test('the board reads every five seconds while visible, nothing while hidden, and at once when shown', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  await expect(live).toHaveText('Live checked just now');
  expect(await boardReads(request)).toEqual([0]);

  // Visible: a quiet read every five seconds, from where the board is.
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9]);
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9]);
  // A poll never says the page is reading.
  await expect(main).toHaveAttribute('aria-busy', 'false');

  // Hidden: nothing is read, however long it stays hidden.
  await setHidden(page, true);
  await expect(live).toContainText('Paused');
  await page.clock.fastForward(60_000);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(500);
  expect(await boardReads(request)).toEqual([0, 9, 9]);

  // Shown: one read at once, then every five seconds again.
  await setHidden(page, false);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9, 9]);
  await expect(live).toContainText('Live');
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9, 9, 9]);
  // Board, sections and log are as they were.
  await expect(main.getByRole('list', { name: 'Activity' }).locator('> li')).toHaveCount(6);
});

test('an entry written while the board is open arrives within one poll and is announced', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  const said = main.locator('.inv-hd [role="status"]');
  await expect(activity).toHaveCount(6);
  await expect(said).toHaveText('');

  await write(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'note', text: 'The gw-2 pool is back to 64.' } }]);
  await page.clock.fastForward(5_000);
  await expect(activity).toHaveCount(7);
  await expect(activity.nth(6)).toContainText('The gw-2 pool is back to 64.');
  await expect(said).toHaveText('1 new entry.');
  expect(await boardReads(request)).toEqual([0, 9]);

  // Two more, then a poll that brings nothing: the region says nothing
  // new, and the next read starts from where the board reached.
  await write(request, [
    { actor: AGENT, at: '2026-10-24T09:44:10.000000Z', payload: { kind: 'note', text: 'one' } },
    { actor: AGENT, at: '2026-10-24T09:44:20.000000Z', payload: { kind: 'note', text: 'two' } },
  ]);
  await page.clock.fastForward(5_000);
  await expect(activity).toHaveCount(9);
  await expect(said).toHaveText('2 new entries.');
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 10, 12]);
  await expect(said).toHaveText('2 new entries.');
});

test('a step that finishes updates in place, and a step the reader opened stays open', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const steps = main.getByRole('list', { name: 'Steps' }).locator('> li');
  await expect(steps).toHaveCount(3);

  const running = {
    kind: 'step',
    executed: 'service=payments-gateway host=gw-2 earliest="2026-10-24T09:00:00.000000Z" latest="2026-10-24T09:45:00.000000Z"',
    bounds: { earliest: '2026-10-24T09:00:00.000000Z', latest: '2026-10-24T09:45:00.000000Z' },
    request_id: 'req-0011',
    purpose: 'Check gw-2 after the pool fix',
  };
  await write(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: running }]);
  await page.clock.fastForward(5_000);
  await expect(steps).toHaveCount(4);
  const step = steps.nth(3);
  await expect(step).toContainText('Running');

  // The reader opens it and keeps focus on it while it runs.
  const summary = step.locator('summary');
  await summary.focus();
  await page.keyboard.press('Enter');
  await expect(step.locator('details')).toHaveAttribute('open', '');
  await step.locator('details').evaluate((el) => { (el as HTMLElement & { marked?: boolean }).marked = true; });

  await write(request, [{
    seq: 10,
    actor: AGENT,
    at: '2026-10-24T09:44:00.000000Z',
    payload: { ...running, query_id: 11, outcome: { outcome: 'success', result_id: 'fedcba9876543210fedcba9876543210', rows: 4, duration_ms: 310 } },
  }]);
  await page.clock.fastForward(5_000);
  await expect(step).toContainText('Succeeded');
  await expect(step).toContainText('4 rows');
  await expect(steps).toHaveCount(4);
  // The same disclosure, still open, still focused: nothing was rebuilt.
  await expect(step.locator('details')).toHaveAttribute('open', '');
  expect(await step.locator('details').evaluate((el) => (el as HTMLElement & { marked?: boolean }).marked)).toBe(true);
  await expect(summary).toBeFocused();
  // The activity updated its line instead of adding one.
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(activity).toHaveCount(7);
  await expect(activity.nth(6)).toContainText('Succeeded');
});

test('a lost poll keeps the board and its last check, backs off, and the board comes back live', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  const said = main.locator('.inv-hd [role="status"]');
  const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
  await expect(live).toContainText('Live');
  await expect(activity).toHaveCount(6);
  const checked = live.locator('.when');
  const firstCheck = await checked.getAttribute('title');
  expect(firstCheck).toBe('2026-10-24T09:45:00+00:00');

  const fail = await request.post('/__ctl/investigation/fail', { data: { count: 2, status: 503 } });
  expect(fail.ok()).toBe(true);
  await page.clock.fastForward(5_000);
  await expect(live).toContainText('Retrying');
  await expect(said).toHaveText('Checking for new entries failed. Retrying.');
  // The board stays as it was read, and so does the instant of the last
  // accepted read; no notice covers the board for a read nobody asked for.
  await expect(checked).toHaveAttribute('title', firstCheck!);
  await expect(activity).toHaveCount(6);
  await expect(main.getByRole('list', { name: 'Findings' }).locator('> li')).toHaveCount(2);
  await expect(main.locator('.inv-stale')).toHaveCount(0);
  await expect(main).toHaveAttribute('aria-busy', 'false');

  // The second try comes five seconds later and fails too; the third
  // waits ten.
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9]);
  await page.clock.fastForward(5_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 9, 9]);
  await expect(live).toContainText('Retrying');
  await expect(checked).toHaveAttribute('title', firstCheck!);

  // It answers again: live, checked now, and said so once.
  await write(request, [{ actor: AGENT, at: '2026-10-24T09:44:00.000000Z', payload: { kind: 'note', text: 'Still here.' } }]);
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9, 9]);
  await expect(live).toContainText('Live');
  await expect(checked).toHaveAttribute('title', '2026-10-24T09:45:20+00:00');
  await expect(said).toHaveText('The board is live again. 1 new entry.');
  await expect(activity).toHaveCount(7);
});

test('a 429 waits out its Retry-After before the board reads again', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  await expect(live).toContainText('Live');

  await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 429, retryAfter: 30 } });
  await page.clock.fastForward(5_000);
  await expect(live).toContainText('Retrying');
  await expect.poll(() => boardReads(request)).toEqual([0, 9]);
  // Not at the five-second backoff: the server asked for thirty.
  await page.clock.fastForward(25_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 9]);
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9, 9]);
  await expect(live).toContainText('Live');
});

test('a poll refused with 403 stops the board and keeps it; one answered 404 ends the visit', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  await expect(live).toContainText('Live');

  await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 403 } });
  await page.clock.fastForward(5_000);
  await expect(live).toContainText('Stopped');
  await expect(main.locator('.inv-stale')).toContainText('You do not have permission to read investigations.');
  await expect(main.getByRole('list', { name: 'Activity' }).locator('> li')).toHaveCount(6);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 9]);

  await page.reload();
  await expect(live).toContainText('Live');
  await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 404 } });
  await page.clock.fastForward(5_000);
  await expect(page.getByRole('heading', { level: 1, name: 'Investigation not found' })).toBeVisible();
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 9, 0, 9]);
});
