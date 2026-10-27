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
// A few minutes after the fixture's last write.
const NOW = new Date('2026-10-24T09:45:00Z');

/** Freeze the page's clock at `at` before the page loads. */
async function freezeClock(page: Pg, at = NOW) {
  await page.clock.install({ time: at.getTime() - 10_000 });
  await page.clock.pauseAt(at);
}

/** Sign in a reader; with `pageCap`, each read brings at most that many
 * entries, so a person's read follows itself. */
async function asReader(request: Ctl, pageCap?: number) {
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'investigations', investigationPageCap: pageCap ?? null },
  });
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
const KEPT = 'fedcba9876543210fedcba9876543210';

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
