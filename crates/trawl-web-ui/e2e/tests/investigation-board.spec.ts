// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The live investigation board (ADR-0059, #311): while its tab is
// visible the board reads the entries changed since its last read every
// five seconds, and while the tab is hidden it reads nothing, neither
// the investigation nor the rows its findings cite nor the viewer's.
//
// Each test freezes the page's clock before the page loads, so a poll
// happens only when the test moves time on, and counts the stub's reads
// of the investigation and of its captured results. A read the stub has
// served can be held from the page with `holdNextRead`, so the tab can
// hide while it is on its way back. `/__ctl/investigation/write` stands
// in for an agent writing through the API while the page is open, and
// `/__ctl/investigation/fail` fails the next reads. Hidden is
// `document.hidden` overridden and announced with `visibilitychange`, as
// jobs-live.spec.ts does.
//
// Every read of the board waits out a 429's pause, in a visible tab too,
// and the reads asked for meanwhile join into one; after a 403 the board
// reads nothing more on the visit and offers no control that would.
//
// On a phone, the board grows above and below a reader halfway down it
// without moving what they read. With TRAWL_E2E_EVIDENCE set, its before
// and after captures are written to docs/evidence/issue-311/ (the
// evidence checkpoint commits them at the final SHA); otherwise to the
// test's output dir.

import { join, resolve } from 'node:path';
import { devices } from '@playwright/test';
import { test, expect } from '../fixtures';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const ID = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const NOT_PERMITTED = 'You do not have permission to read investigations.';
const WAITING = 'The server asked the board to wait. It reads again when the wait ends.';
// A few minutes after the fixture's last write.
const NOW = new Date('2026-10-24T09:45:00Z');

/** Freeze the page's clock at `at` before the page loads. */
async function freezeClock(page: Pg, at = NOW) {
  await page.clock.install({ time: at.getTime() - 10_000 });
  await page.clock.pauseAt(at);
}

/** Sign in a reader; with `pageCap`, each read brings at most that many
 * entries, so a person's read follows itself. `knobs` are further reset
 * knobs (harness/server.mjs). */
async function asReader(request: Ctl, pageCap?: number, knobs: object = {}) {
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'investigations', investigationPageCap: pageCap ?? null, ...knobs },
  });
  expect(response.ok()).toBe(true);
}

/** The `since` of every read of the investigation the stub answered or
 * failed, in order. */
async function boardReads(request: Ctl): Promise<number[]> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.investigation.reads.map((r: { since: number }) => r.since);
}

/** Every read of a captured result the stub answered or failed, as
 * `[result id, offset, limit]`, `limit` null when the page sent none.
 * Sorted, since up to four run at once and land in any order. */
async function resultReads(request: Ctl): Promise<Array<[string, number, number | null]>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.investigation.resultReads
    .map((r: { id: string; offset: number; limit: number | null }) => [r.id, r.offset, r.limit])
    .sort((a: [string, number], b: [string, number]) => a[0].localeCompare(b[0]) || a[1] - b[1]);
}

/** Hold the page's next read of the investigation once the stub has
 * served it, until the returned function lets it through. A write to
 * the same path, and every later read, pass as they are. */
async function holdNextRead(page: Pg): Promise<() => void> {
  let letThrough!: () => void;
  const through = new Promise<void>((resolve) => { letThrough = resolve; });
  let held = false;
  await page.route((url) => url.pathname === `/api/v1/investigations/${ID}`, async (route) => {
    if (held || route.request().method() !== 'GET') {
      await route.fallback();
      return;
    }
    held = true;
    const response = await route.fetch();
    await through;
    await route.fulfill({ response }).catch(() => {});
  });
  return letThrough;
}

/** Time runs on in a hidden tab, and the stub's reads of the
 * investigation and of its results stay as they are. */
async function expectNothingReadWhileHidden(
  page: Pg,
  request: Ctl,
  reads: number[],
  results: Array<[string, number, number | null]>,
) {
  await page.waitForTimeout(300);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual(reads);
  expect(await resultReads(request)).toEqual(results);
}

/** Every write the stub recorded, with its method and body. */
async function writes(request: Ctl): Promise<Array<{ method: string; id: string; body: Record<string, unknown> }>> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.investigation.writes;
}

/** `n` notes an agent writes on gw-2, ten seconds apart from 09:44. */
function notes(n: number) {
  return Array.from({ length: n }, (_, i) => ({
    actor: AGENT,
    at: `2026-10-24T09:44:${String(i * 10).padStart(2, '0')}.000000Z`,
    payload: { kind: 'note', text: `Note ${i + 1} on gw-2.` },
  }));
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
const KEPT = 'fedcba9876543210fedcba9876543210';
const PRUNED = '0a1b2c3d4e5f60718293a4b5c6d7e8f9';
const PAGED = '7e57ab1e0c0ffee0d15ea5e0ba5eba11';
// The rows the fixture's findings cite, each read once with a one-row
// page: two of the kept result, one of the pruned one.
const CITED: Array<[string, number, number | null]> = [[PRUNED, 1, 1], [KEPT, 0, 1], [KEPT, 2, 1]];

const EVIDENCE_DIR = resolve(__dirname, '../../../../docs/evidence/issue-311');

function capturePath(testInfo: import('@playwright/test').TestInfo, name: string): string {
  return process.env.TRAWL_E2E_EVIDENCE ? join(EVIDENCE_DIR, name) : testInfo.outputPath(name);
}

/** A finished step the agent ran, `n` minutes after 09:30. */
function finishedStep(n: number) {
  const at = `2026-10-24T09:${String(30 + n).padStart(2, '0')}:00.000000Z`;
  return {
    actor: AGENT,
    at,
    payload: {
      kind: 'step',
      executed: `service=payments-gateway host=gw-${n} earliest="2026-10-24T08:30:00.000000Z" latest="2026-10-24T09:30:00.000000Z" | stats count by message`,
      bounds: { earliest: '2026-10-24T08:30:00.000000Z', latest: '2026-10-24T09:30:00.000000Z' },
      request_id: `req-02-${n}`,
      query_id: 100 + n,
      purpose: `Count what gateway gw-${n} logged in the hour`,
      outcome: { outcome: 'success', result_id: KEPT, rows: n, duration_ms: 90 + n },
    },
  };
}

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

test('a board opened in a hidden tab reads nothing and says Paused until the tab shows, then reads its whole log', async ({ page, request }) => {
  // Four entries per read: the mount read is followed by a second page.
  await asReader(request, 4);
  await page.addInitScript(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
  });
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  await page.waitForTimeout(300);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(500);
  expect(await boardReads(request)).toEqual([]);
  await expect(main.locator('.inv-live')).toHaveText('Paused');
  await expect(main.getByRole('status')).toHaveText('The investigation loads when this tab is shown.');
  await expect(main).not.toHaveAttribute('aria-busy', 'true');

  // Shown: the mount read and the page that follows it, at once.
  await setHidden(page, false);
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  expect(await boardReads(request)).toEqual([0, 7]);
  await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
  await expect(main.getByRole('list', { name: 'Activity' }).locator('> li')).toHaveCount(6);
  await expect(main).toHaveAttribute('aria-busy', 'false');
});

test('a page that falls due once the tab is hidden waits until the tab shows', async ({ page, request }) => {
  await asReader(request, 4);
  await freezeClock(page);
  // The tab hides while the mount read is on its way back, so the page
  // that would follow it falls due in a hidden tab.
  await page.route((url) => url.pathname === `/api/v1/investigations/${ID}` && url.searchParams.get('since') === '0', async (route) => {
    const response = await route.fetch();
    await setHidden(page, true);
    await route.fulfill({ response });
  }, { times: 1 });
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  await expect(live).toContainText('Paused');
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(500);
  expect(await boardReads(request)).toEqual([0]);
  // What it holds is named as of where it stopped, and nothing spins.
  await expect(main.locator('[data-anchor="latest"] h2')).toHaveText('Latest finding as of entry #7');
  await expect(main).toHaveAttribute('aria-busy', 'false');

  await setHidden(page, false);
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  expect(await boardReads(request)).toEqual([0, 7]);
  await expect(live).toHaveText('Live checked just now');
  await expect(main.getByRole('list', { name: 'Steps' }).locator('> li').nth(1)).toContainText('Interrupted');
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

test("a cut capture's step card says it was cut", async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const steps = main.getByRole('list', { name: 'Steps' }).locator('> li');
  await expect(steps).toHaveCount(3);

  await write(request, [{
    actor: AGENT,
    at: '2026-10-24T09:44:00.000000Z',
    payload: {
      kind: 'step',
      executed: 'service=payments-gateway earliest="2026-10-24T09:00:00.000000Z" latest="2026-10-24T09:45:00.000000Z"',
      bounds: { earliest: '2026-10-24T09:00:00.000000Z', latest: '2026-10-24T09:45:00.000000Z' },
      request_id: 'req-0012',
      query_id: 12,
      purpose: 'Every gateway error',
      outcome: { outcome: 'success', result_id: 'fedcba9876543210fedcba9876543210', rows: 4, total_rows: 5000, duration_ms: 310 },
    },
  }]);
  await page.clock.fastForward(5_000);
  await expect(steps).toHaveCount(4);
  const step = steps.nth(3);
  // The collapsed card says so; the viewer was never opened.
  await expect(step.locator('summary')).toContainText('4 of 5000 rows (cut)');
  await expect(step.locator('details')).not.toHaveAttribute('open', /.*/);
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

test.describe('a read asked for in a hidden tab waits for what came before it', () => {
  /** Open a board that holds five entries more than one read of four
   * brings, make its next poll answer 429 with Retry-After: 30, and ask
   * for a read in the hidden tab while that pause runs. */
  async function owedInsidePause(page: Pg, request: Ctl) {
    await asReader(request, 4);
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    expect(await boardReads(request)).toEqual([0, 7]);
    await write(request, Array.from({ length: 5 }, (_, i) => ({
      actor: AGENT,
      at: `2026-10-24T09:44:${String(i * 10).padStart(2, '0')}.000000Z`,
      payload: { kind: 'note', text: `Note ${i + 1} on gw-2.` },
    })));
    await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 429, retryAfter: 30 } });
    await page.clock.fastForward(5_000);
    await expect(main.locator('.inv-live')).toContainText('Retrying');
    expect(await boardReads(request)).toEqual([0, 7, 9]);
    await setHidden(page, true);
    await main.getByRole('button', { name: 'Refresh', exact: true }).click();
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([0, 7, 9]);
    return main;
  }

  test('a read owed inside a 429 pause waits it out, however often the tab shows, then reads as the person asked', async ({ page, request }) => {
    const main = await owedInsidePause(page, request);
    // Shown inside the pause: nothing reads yet.
    await setHidden(page, false);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([0, 7, 9]);
    // Hidden and shown again, ten seconds on: still inside it.
    await page.clock.fastForward(10_000);
    await setHidden(page, true);
    await setHidden(page, false);
    await page.clock.fastForward(19_000);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([0, 7, 9]);

    // The pause ends: the person's read, which follows its short first
    // page at once, where a poll would read one page.
    await page.clock.fastForward(1_000);
    await expect.poll(() => boardReads(request)).toEqual([0, 7, 9, 9, 13]);
    await expect(main.getByRole('list', { name: 'Activity' }).locator('> li')).toHaveCount(11);
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    await expect(main).toHaveAttribute('aria-busy', 'false');
  });

  test('a read owed when a 403 stops the board is dropped, not read when the tab shows', async ({ page, request }) => {
    await asReader(request);
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const live = main.locator('.inv-live');
    await expect(live).toHaveText('Live checked just now');
    await expect(main.getByRole('list', { name: 'Findings' }).locator('> li').nth(1)).toContainText('Pruned');
    await expect.poll(() => resultReads(request)).toEqual(CITED);

    // The poll is answered 403 and held on its way back; the tab hides
    // and the person asks for a read before it lands.
    await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 403 } });
    const letThrough = await holdNextRead(page);
    await page.clock.fastForward(5_000);
    await expect.poll(() => boardReads(request)).toEqual([0, 9]);
    await setHidden(page, true);
    await main.getByRole('button', { name: 'Refresh', exact: true }).click();
    letThrough();
    await expect(live).toContainText('Stopped');

    await setHidden(page, false);
    await expectNothingReadWhileHidden(page, request, [0, 9], CITED);
    await expect(live).toContainText('Stopped');
    await expect(main.locator('.inv-stale')).toContainText(NOT_PERMITTED);
  });
});

test.describe('every read in a visible tab waits out a 429 pause', () => {
  /** Open a board of `pageCap` entries per read, let an agent write five
   * notes so the next poll leaves some unread and Load more shows, then
   * make the poll after it answer 429 with Retry-After: 30. Answers the
   * reads so far once the pause has begun. */
  async function insidePause(page: Pg, request: Ctl, pageCap: number, mount: number[], knobs: object = {}) {
    await asReader(request, pageCap, knobs);
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    expect(await boardReads(request)).toEqual(mount);
    await write(request, notes(5));
    await page.clock.fastForward(5_000);
    await expect(main.getByRole('button', { name: 'Load more entries' })).toBeVisible();
    await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 429, retryAfter: 30 } });
    await page.clock.fastForward(5_000);
    await expect(main.locator('.inv-live')).toContainText('Retrying');
    return boardReads(request);
  }

  test('visible reads wait out a 429 pause and join into one', async ({ page, request }) => {
    // Four entries per read: the poll before the pause reads four of the
    // five notes, so the one left and the close's entry fit one read.
    const before = await insidePause(page, request, 4, [0, 7], { scenario: 'investigations-writer' });
    expect(before).toEqual([0, 7, 9, 13]);
    const main = page.locator('.inv-page');
    const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
    const waiting = main.getByRole('status').filter({ hasText: WAITING });
    await expect(activity).toHaveCount(10);
    await expect(waiting).toHaveCount(0);

    // Inside the pause, in the visible tab: Refresh three times, Load
    // more once, and a close that lands.
    const refresh = main.getByRole('button', { name: 'Refresh', exact: true });
    await refresh.click();
    await expect(waiting).toBeVisible();
    await refresh.click();
    await refresh.click();
    await main.getByRole('button', { name: 'Load more entries' }).click();
    await main.getByRole('button', { name: 'Close investigation' }).click();
    await expect(main.locator('.inv-write-note [role="status"]')).toHaveText('You closed the investigation.');
    expect(await writes(request)).toEqual([
      { method: 'PATCH', id: ID, body: { expected_seq: 14, status: 'closed' } },
    ]);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual(before);
    await expect(waiting).toBeVisible();

    // A second before the pause ends, nothing has read the board.
    await page.clock.fastForward(29_000);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual(before);
    await expect(waiting).toBeVisible();
    await expect(activity).toHaveCount(10);

    // The pause ends: one read, a person's from where the board stands,
    // brings the last note and the close, and says nothing waits.
    await page.clock.fastForward(1_000);
    await expect.poll(() => boardReads(request)).toEqual([...before, 13]);
    await expect(activity).toHaveCount(12);
    await expect(activity.nth(11)).toContainText('Closed the investigation.');
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    await expect(waiting).toHaveCount(0);

    // Nothing else was owed: the next read is the poll five seconds on.
    await page.clock.fastForward(4_000);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([...before, 13]);
    await page.clock.fastForward(1_000);
    await expect.poll(() => boardReads(request)).toEqual([...before, 13, 15]);
    expect(await writes(request)).toHaveLength(1);
  });

  test('a read that waited out a 429 pause reads on through its follow-up pages', async ({ page, request }) => {
    // Two entries per read: the three notes left take two reads.
    const before = await insidePause(page, request, 2, [0, 4, 7]);
    expect(before).toEqual([0, 4, 7, 9, 11]);
    const main = page.locator('.inv-page');
    const activity = main.getByRole('list', { name: 'Activity' }).locator('> li');
    await expect(activity).toHaveCount(8);

    await main.getByRole('button', { name: 'Refresh', exact: true }).click();
    await main.getByRole('button', { name: 'Load more entries' }).click();
    await main.getByRole('button', { name: 'Refresh', exact: true }).click();
    await expect(main.getByRole('status').filter({ hasText: WAITING })).toBeVisible();
    await page.clock.fastForward(29_000);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual(before);

    // The joined read, then the page that follows its short first read.
    await page.clock.fastForward(1_000);
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();
    expect(await boardReads(request)).toEqual([...before, 11, 13]);
    await expect(activity).toHaveCount(11);
    await expect(main.getByRole('status').filter({ hasText: WAITING })).toHaveCount(0);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([...before, 11, 13]);
  });

  test('a first read answered 429 waits out its pause when the person retries', async ({ page, request }) => {
    await asReader(request);
    await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 429, retryAfter: 30 } });
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const retry = main.getByRole('button', { name: 'Retry', exact: true });
    await expect(retry).toBeVisible();
    expect(await boardReads(request)).toEqual([0]);

    await retry.click();
    await expect(main.getByRole('status')).toHaveText(WAITING);
    await page.clock.fastForward(29_000);
    await page.waitForTimeout(300);
    expect(await boardReads(request)).toEqual([0]);
    await expect(main.getByRole('status')).toHaveText(WAITING);

    await page.clock.fastForward(1_000);
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();
    expect(await boardReads(request)).toEqual([0, 0]);
    await expect(main.getByText(WAITING)).toHaveCount(0);
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
  });
});

test('a 403 stop removes every read control and sends no read', async ({ page, request }) => {
  // A writer's board of four entries per read, five notes past it.
  await asReader(request, 4, { scenario: 'investigations-writer' });
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const live = main.locator('.inv-live');
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  await expect(live).toHaveText('Live checked just now');
  await write(request, notes(5));
  await page.clock.fastForward(5_000);
  await expect(main.getByRole('button', { name: 'Load more entries' })).toBeVisible();
  expect(await boardReads(request)).toEqual([0, 7, 9]);

  // A close whose answer is lost: its notice offers to refresh the board.
  expect((await request.post('/__ctl/investigation/drop-write')).ok()).toBe(true);
  await main.getByRole('button', { name: 'Close investigation' }).click();
  await expect(main.locator('.inv-write-note [role="status"]')).toContainText('may or may not have taken effect');
  await expect(main.getByRole('button', { name: 'Refresh', exact: true })).toBeVisible();
  await expect(main.getByRole('button', { name: 'Refresh the board' })).toBeVisible();

  // A keep whose request never got an answer: the viewer offers one too.
  const finding = main.getByRole('list', { name: 'Findings' }).locator('> li').nth(0);
  await finding.getByRole('button', { name: 'Open Row 0 of result fedcba98 among its rows' }).click();
  const viewer = page.getByRole('dialog', { name: 'Result fedcba98' });
  await viewer.getByRole('button', { name: 'Keep row 0' }).click();
  await viewer.getByRole('textbox', { name: 'Why keep row 0?' }).fill('pool exhausted at 64/64');
  await page.route((url) => url.pathname === `/api/v1/investigations/${ID}/entries`, (route) => route.abort('failed'), { times: 1 });
  await page.keyboard.press('Enter');
  await expect(viewer.locator('.inv-write-note [role="status"]')).toContainText('may or may not have been kept');
  await expect(viewer.getByRole('button', { name: 'Refresh the board' })).toBeVisible();

  // The next poll is refused for want of a grant.
  await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 403 } });
  await page.clock.fastForward(5_000);
  await expect(live).toContainText('Stopped');
  expect(await boardReads(request)).toEqual([0, 7, 9, 13]);
  // No control on the page reads the board any more, hidden or not.
  for (const name of ['Refresh', 'Refresh the board', 'Load more entries']) {
    await expect(page.getByRole('button', { name, exact: true, includeHidden: true }), name).toHaveCount(0);
  }
  // Both notices stay, and neither names a control that is gone.
  await expect(main.locator('.inv-write-note [role="status"]')).toHaveText(
    "The answer to your close was lost, so it may or may not have taken effect. The board stopped reading, so reload the page to see the investigation's status.",
  );
  await expect(viewer.locator('.inv-write-note [role="status"]')).toHaveText(
    'The answer to your keep of row 0 was lost, so it may or may not have been kept. The board stopped reading, so reload the page and look for it in the activity before you keep it again.',
  );
  // Neither the tab showing again nor time passing reads it.
  await setHidden(page, true);
  await page.clock.fastForward(60_000);
  await setHidden(page, false);
  await page.waitForTimeout(300);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 7, 9, 13]);
  await expect(live).toContainText('Stopped');
  expect(await writes(request)).toHaveLength(1);

  // A reload is a new visit, which reads the whole log again.
  await page.reload();
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();
  expect(await boardReads(request)).toEqual([0, 7, 9, 13, 0, 7, 11]);
  await expect(live).toHaveText('Live checked just now');
  await expect(main.getByRole('button', { name: 'Refresh', exact: true })).toBeVisible();
});

test('a 401 drops the owed read before sign-in', async ({ page, request }) => {
  await asReader(request);
  await freezeClock(page);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  await expect(main.locator('.inv-live')).toHaveText('Live checked just now');

  // The next poll is answered 401 and held on its way back; the tab
  // hides and the person asks for a read, which is owed.
  await request.post('/__ctl/investigation/fail', { data: { count: 1, status: 401 } });
  const letThrough = await holdNextRead(page);
  await page.clock.fastForward(5_000);
  await expect.poll(() => boardReads(request)).toEqual([0, 9]);
  await setHidden(page, true);
  await main.getByRole('button', { name: 'Refresh', exact: true }).click();

  // The 401 lands and the page goes to sign-in. The sign-in document is
  // answered 204, which a browser does not navigate to, so the board stays
  // on the page and shows that the read owed went nowhere: not when the
  // tab shows, not as time runs on.
  const signIn: string[] = [];
  await page.route((url) => url.pathname === '/login', async (route) => {
    signIn.push(route.request().url());
    await route.fulfill({ status: 204 });
  });
  letThrough();
  await expect.poll(() => signIn.length).toBe(1);
  expect(new URL(signIn[0]).searchParams.get('return_to')).toBe(`/investigations/${ID}`);
  await setHidden(page, false);
  await page.waitForTimeout(300);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  await page.clock.fastForward(60_000);
  await page.waitForTimeout(300);
  expect(await boardReads(request)).toEqual([0, 9]);
  expect(new URL(page.url()).pathname).toBe(`/investigations/${ID}`);
  expect(signIn).toHaveLength(1);
});

test.describe('the board reads no evidence while its tab is hidden', () => {
  test('rows cited by the first read, landing once the tab is hidden, are read when it shows', async ({ page, request }) => {
    await asReader(request);
    await freezeClock(page);
    await page.route((url) => url.pathname === `/api/v1/investigations/${ID}` && url.searchParams.get('since') === '0', async (route) => {
      const response = await route.fetch();
      await setHidden(page, true);
      await route.fulfill({ response });
    }, { times: 1 });
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const findings = main.getByRole('list', { name: 'Findings' }).locator('> li');
    await expect(findings).toHaveCount(2);
    await expect(main.locator('.inv-live')).toContainText('Paused');
    await expectNothingReadWhileHidden(page, request, [0], []);
    await expect(findings.nth(0).getByText('Loading row…')).toHaveCount(2);
    await expect(findings.nth(1).getByText('Loading row…')).toHaveCount(1);

    await setHidden(page, false);
    await expect(findings.nth(0).locator('.inv-row-scroll')).toHaveCount(2);
    await expect(findings.nth(1)).toContainText('Pruned');
    expect(await resultReads(request)).toEqual(CITED);
  });

  test('a row cited by a poll that lands once the tab is hidden is read when it shows', async ({ page, request }) => {
    await asReader(request);
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const findings = main.getByRole('list', { name: 'Findings' }).locator('> li');
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    await expect(findings.nth(1)).toContainText('Pruned');
    await expect.poll(() => resultReads(request)).toEqual(CITED);

    // The agent cites a row of another result; the poll that brings the
    // finding is on its way back when the tab hides.
    await write(request, [{
      actor: AGENT,
      at: '2026-10-24T09:44:00.000000Z',
      payload: {
        kind: 'finding',
        statement: 'January had the same lock wait in front of the 502s.',
        status: 'hypothesis',
        references: [{ result_id: PAGED, row: 4 }],
        counterevidence: [],
      },
    }]);
    const letThrough = await holdNextRead(page);
    await page.clock.fastForward(5_000);
    await expect.poll(() => boardReads(request)).toEqual([0, 9]);
    await setHidden(page, true);
    letThrough();
    await expect(findings).toHaveCount(3);
    await expectNothingReadWhileHidden(page, request, [0, 9], CITED);
    await expect(findings.nth(2)).toContainText('Loading row…');

    await setHidden(page, false);
    await expect(findings.nth(2).getByRole('region', { name: 'Row 4 of result 7e57ab1e' })).toBeVisible();
    expect(await resultReads(request)).toEqual([[PRUNED, 1, 1], [PAGED, 4, 1], [KEPT, 0, 1], [KEPT, 2, 1]]);
  });

  test('Refresh in a hidden tab asks again for its failed rows only once the tab shows', async ({ page, request }) => {
    // The kept result's two cited rows fail to read.
    await asReader(request, undefined, { investigationResultFail: { id: KEPT, status: 500, count: 2 } });
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const first = main.getByRole('list', { name: 'Findings' }).locator('> li').nth(0);
    await expect(first.getByText('Could not load this row.', { exact: false })).toHaveCount(2);
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    await expect.poll(() => resultReads(request)).toEqual(CITED);

    await setHidden(page, true);
    await main.getByRole('button', { name: 'Refresh', exact: true }).click();
    await expectNothingReadWhileHidden(page, request, [0], CITED);
    await expect(first.getByText('Loading row…')).toHaveCount(2);

    await setHidden(page, false);
    await expect(first.locator('.inv-row-scroll')).toHaveCount(2);
    expect(await boardReads(request)).toEqual([0, 9]);
    expect(await resultReads(request)).toEqual([[PRUNED, 1, 1], [KEPT, 0, 1], [KEPT, 0, 1], [KEPT, 2, 1], [KEPT, 2, 1]]);
  });

  test('the viewer waits out a 429 in a hidden tab and reads again only once it shows', async ({ page, request }) => {
    await asReader(request);
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const first = main.getByRole('list', { name: 'Findings' }).locator('> li').nth(0);
    await expect(first.locator('.inv-row-scroll')).toHaveCount(2);
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    await expect.poll(() => resultReads(request)).toEqual(CITED);

    // The next result read answers 429 with Retry-After: 2. A reset while
    // the page is open serves the same board and starts the counts over.
    await asReader(request, undefined, { investigationThrottle: 1, investigationRetryAfter: '2' });
    await first.getByRole('button', { name: 'Open Row 0 of result fedcba98 among its rows' }).click();
    const viewer = page.getByRole('dialog', { name: 'Result fedcba98' });
    await expect(viewer.getByText('The server is limiting reads for your API key. Reading again shortly.')).toBeVisible();
    expect(await resultReads(request)).toEqual([[KEPT, 0, null]]);

    // The Retry-After passes while the tab is hidden.
    await setHidden(page, true);
    await page.clock.fastForward(2_000);
    await expectNothingReadWhileHidden(page, request, [], [[KEPT, 0, null]]);

    await setHidden(page, false);
    await expect(viewer.getByRole('region', { name: 'Rows of result fedcba98' })).toBeVisible();
    await expect(viewer.getByText('The server is limiting reads for your API key. Reading again shortly.')).toHaveCount(0);
    expect(await resultReads(request)).toEqual([[KEPT, 0, null], [KEPT, 0, null]]);
  });
});

test.describe('on a phone', () => {
  // The iPhone descriptor's viewport, scale, touch and user agent, on the
  // project's chromium: its webkit default would override the channel.
  const { defaultBrowserType: _webkit, ...iphone } = devices['iPhone 13'];
  test.use(iphone);

  test('the board grows above and below a reader halfway down it without moving what they read', async ({ page, request }, testInfo) => {
    await asReader(request);
    // A longer run than the fixture's three steps, so the reader can sit
    // in the middle of them.
    await write(request, Array.from({ length: 12 }, (_, i) => finishedStep(i + 1)));
    await freezeClock(page);
    await page.goto(`/investigations/${ID}`);
    const main = page.locator('.inv-page');
    const steps = main.getByRole('list', { name: 'Steps' }).locator('> li');
    const findings = main.getByRole('list', { name: 'Findings' }).locator('> li');
    await expect(steps).toHaveCount(15);
    // Every cited row the board holds has landed.
    await expect(findings.nth(0).locator('.inv-row-scroll')).toHaveCount(2);
    await expect(findings.nth(1)).toContainText('Pruned');

    // The reader scrolls to the fifth step and reads it 120 px below the
    // top of the page's scroller.
    const reading = steps.nth(4);
    const place = () => reading.evaluate((el) => {
      const scroller = el.closest('.inv-page')!;
      return {
        offset: el.getBoundingClientRect().top - scroller.getBoundingClientRect().top,
        top: el.getBoundingClientRect().top,
        scrollTop: scroller.scrollTop,
      };
    });
    await reading.evaluate((el) => {
      const scroller = el.closest('.inv-page')!;
      scroller.scrollTop += el.getBoundingClientRect().top - scroller.getBoundingClientRect().top - 120;
    });
    const before = await place();
    expect(Math.abs(before.offset - 120)).toBeLessThanOrEqual(1);
    await page.screenshot(steadyShot(page, capturePath(testInfo, 'board-phone-before.png')));

    // The next poll is armed only when the board's last read ends, so
    // the agent writes once the first read has ended and nothing else
    // runs: the board says it is live, which it sets in the same task
    // that arms the poll, and the stub has served that one read alone. A
    // read still in flight across the fastForward below would re-arm the
    // poll after it, and the board would not read again.
    await expect(main.locator('.inv-live')).toHaveText('Live checked just now');
    expect(await boardReads(request)).toEqual([0]);

    // The agent records a finding, which lands above the reader in the
    // latest finding, the activity and the findings, and starts a step,
    // which lands below them.
    const statement = 'The 08:58 resize shrank the gw-2 pool from 64 to 32 connections; checkout traffic did not grow.';
    await write(request, [
      {
        actor: AGENT,
        at: '2026-10-24T09:44:00.000000Z',
        payload: { kind: 'finding', statement, status: 'supported', references: [{ result_id: KEPT, row: 1 }], counterevidence: [] },
      },
      {
        actor: AGENT,
        at: '2026-10-24T09:44:30.000000Z',
        payload: {
          kind: 'step',
          executed: 'service=payments-gateway host=gw-2 message="pool resized" earliest="2026-10-24T08:00:00.000000Z" latest="2026-10-24T10:00:00.000000Z"',
          bounds: { earliest: '2026-10-24T08:00:00.000000Z', latest: '2026-10-24T10:00:00.000000Z' },
          request_id: 'req-03',
          purpose: 'Confirm the resize on gw-2',
        },
      },
    ]);
    await page.clock.fastForward(5_000);
    // The poll asked from the board's cursor, after both writes landed.
    await expect.poll(() => boardReads(request)).toEqual([0, 21]);

    // Both arrived, and the row the new finding cites has landed too.
    await expect(steps).toHaveCount(16);
    await expect(steps.nth(15)).toContainText('Confirm the resize on gw-2');
    await expect(findings).toHaveCount(3);
    await expect(findings.nth(2).locator('.inv-row-scroll')).toHaveCount(1);
    await expect(main.locator('[data-anchor="latest"]')).toContainText(statement);
    await expect(main.locator('.inv-hd [role="status"]')).toHaveText('2 new entries.');

    // What the reader read is where it was; the page scrolled by what
    // grew above it.
    const after = await place();
    expect(Math.abs(after.top - before.top)).toBeLessThanOrEqual(1);
    expect(after.scrollTop - before.scrollTop).toBeGreaterThan(100);
    await page.screenshot(steadyShot(page, capturePath(testInfo, 'board-phone-after.png')));
  });
});

/** Screenshot options that hold the pixels still: no animation or caret,
 * and the status bar masked whole, because its label names the worker's
 * own stub port. */
function steadyShot(page: Pg, path: string) {
  return {
    path,
    animations: 'disabled' as const,
    caret: 'hide' as const,
    mask: [page.locator('footer.statusbar')],
  };
}
