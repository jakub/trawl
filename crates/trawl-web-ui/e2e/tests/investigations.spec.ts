// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The read-only investigation pages (ADR-0059), over the recorded wire
// bodies in harness/wire/investigation*.json and result-*.json, whose
// shapes and cases e2e_wire_fixture_contract.rs pins with the trawl-api
// types. The `investigations` scenario signs in a reader holding `query`
// and `investigation_read`; every other scenario holds neither, and the
// stub answers an investigation read under it with 403 and counts it.
//
// The phone capture is evidence for #310. With TRAWL_E2E_EVIDENCE set it
// is written to docs/evidence/issue-310/investigation-phone.png (CP9
// commits it at the final SHA); otherwise to this test's output dir. The
// clock is fixed and the page is captured whole at the phone's width, so
// two runs at one SHA write the same pixels.

import { join, resolve } from 'node:path';
import { devices } from '@playwright/test';
import { test, expect, resetScenario, trackToasts, toastCount } from '../fixtures';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const ID = '5f0c1a2b3c4d5e6f7a8b9c0d1e2f3a4b';
const KEPT = 'fedcba9876543210fedcba9876543210';
const PRUNED = '0a1b2c3d4e5f60718293a4b5c6d7e8f9';
// harness/wire/investigation-read-many.json: four findings citing 32
// rows each, 96 distinct.
const MANY = '9e8d7c6b5a4f30211203f4e5d6c7b8a9';
// A few minutes after the fixture's last write, so relative labels read
// the same on every run.
const NOW = new Date('2026-10-24T09:45:00Z');
const NOT_PERMITTED = 'You do not have permission to read investigations.';

const EVIDENCE_DIR = resolve(__dirname, '../../../../docs/evidence/issue-310');

function capturePath(testInfo: import('@playwright/test').TestInfo, name: string): string {
  return process.env.TRAWL_E2E_EVIDENCE ? join(EVIDENCE_DIR, name) : testInfo.outputPath(name);
}

async function reads(request: Ctl): Promise<{
  listReads: number;
  reads: Array<{ id: string; since: number; limit: number }>;
  resultReads: Array<{ id: string; offset: number; limit: number }>;
  maxResultsInFlight: number;
}> {
  return (await (await request.get('/__ctl/state')).json()).investigation;
}

async function asReader(request: Ctl, pageCap?: number) {
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'investigations', investigationPageCap: pageCap ?? null },
  });
  expect(response.ok()).toBe(true);
}

/** Neither the document nor the page's own scroller is wider than the
 * viewport: anything wide scrolls inside its own box. */
async function expectNoHorizontalOverflow(page: Pg) {
  const overflow = await page.evaluate(() => {
    const doc = document.documentElement;
    const scroller = document.querySelector('.inv-page')!;
    return {
      document: doc.scrollWidth - doc.clientWidth,
      page: scroller.scrollWidth - scroller.clientWidth,
    };
  });
  expect(overflow).toEqual({ document: 0, page: 0 });
}

/** Grow the viewport's height, never its width, until the page's own
 * scroller holds everything, then capture it whole. */
async function captureWhole(page: Pg, path: string) {
  for (let i = 0; i < 3; i += 1) {
    const extra = await page.evaluate(() => {
      const scroller = document.querySelector('.inv-page')!;
      return scroller.scrollHeight - scroller.clientHeight;
    });
    if (extra <= 0) break;
    const size = page.viewportSize()!;
    await page.setViewportSize({ width: size.width, height: size.height + extra });
  }
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot(steadyShot(page, path));
}

/** Screenshot options that hold the pixels still: no animation or caret,
 * and the status bar masked whole, because its label names the worker's
 * own stub port and the label's width moves what follows it. */
function steadyShot(page: Pg, path: string) {
  return {
    path,
    animations: 'disabled' as const,
    caret: 'hide' as const,
    mask: [page.locator('footer.statusbar')],
  };
}

test.describe('on a phone', () => {
  // The iPhone descriptor's viewport, scale, touch and user agent, on the
  // project's chromium: its webkit default would override the channel.
  const { defaultBrowserType: _webkit, ...iphone } = devices['iPhone 13'];
  test.use(iphone);

  test('the list is one row per investigation, each a link to it', async ({ page, request }, testInfo) => {
    await asReader(request);
    await page.clock.setFixedTime(NOW);
    await page.goto('/investigations');

    const table = page.getByRole('table', { name: 'Investigations' });
    const rows = table.locator('tbody tr');
    await expect(rows).toHaveCount(3);
    await expect(rows.nth(0)).toContainText('502s on checkout since 09:00');
    await expect(rows.nth(0)).toContainText('Open');
    await expect(rows.nth(0)).toContainText('claude-agent');
    await expect(rows.nth(2)).toContainText('Closed');
    // ADR-0029: one control per row, a link, because the row is a place.
    for (let i = 0; i < 3; i += 1) {
      await expect(rows.nth(i).locator('a, button')).toHaveCount(1);
    }
    await expect(page.getByText('3 investigations')).toBeVisible();
    await expectNoHorizontalOverflow(page);
    await page.screenshot(steadyShot(page, capturePath(testInfo, 'investigations-phone.png')));

    // The fixed clock holds animation frames, so the row link is
    // activated from the keyboard, the way a reader without a pointer
    // would.
    await rows.nth(0).getByRole('link', { name: '502s on checkout since 09:00' }).focus();
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(`/investigations/${ID}`);
    await expect(page.getByRole('heading', { level: 1, name: '502s on checkout since 09:00' })).toBeVisible();
  });

  test('an investigation reads question, latest finding, findings with their rows, then the log', async ({ page, request }, testInfo) => {
    await asReader(request);
    await trackToasts(page);
    await page.clock.setFixedTime(NOW);
    await page.goto(`/investigations/${ID}`);

    const main = page.locator('.inv-page');
    await expect(main.getByRole('heading', { level: 1 })).toHaveText('502s on checkout since 09:00');
    await expect(main.getByText('Why does checkout answer 502 since 09:00, and is it still happening?')).toBeVisible();
    await expect(main.getByText('service=shop-api and its upstream payment pool, 08:00 to 10:00 UTC')).toBeVisible();

    // The sections come in the page's order.
    const headings = await main.getByRole('heading', { level: 2 }).allTextContents();
    expect(headings).toEqual(['Question', 'Latest finding', 'Findings', 'Steps and notes']);

    // The latest finding is the one written last, with its status.
    const latest = main.locator('section', { has: page.getByRole('heading', { name: 'Latest finding' }) });
    await expect(latest).toContainText('The resize halved the pool on gw-2 only');
    await expect(latest).toContainText('Hypothesis');

    // Each cited row of the readable result renders in the results
    // table's markup, read once with a one-row page.
    const findings = main.getByRole('list', { name: 'Findings' }).locator('> li');
    await expect(findings).toHaveCount(2);
    const first = findings.nth(0);
    await expect(first).toContainText('Supported');
    await expect(first).toContainText('Still open: Did the 08:58 pool resize shrink the pool');
    const row0 = first.getByRole('region', { name: 'Row 0 of result fedcba98' });
    const row2 = first.getByRole('region', { name: 'Row 2 of result fedcba98' });
    await expect(row0.locator('table.results-table td')).toHaveCount(4);
    await expect(row0).toContainText('upstream connect timeout to payments:8443 after 3000ms; pool exhausted');
    await expect(row2).toContainText('upstream connect timeout to payments:8443 after 3000ms');
    await expect(row0.locator('th')).toHaveText(['_time', 'host', 'status', 'message']);

    // A pruned result shows as pruned in the row's place, never a toast.
    const second = findings.nth(1);
    await expect(second.locator('.inv-pruned')).toContainText('Pruned');
    await expect(second.getByRole('region')).toHaveCount(0);
    expect(await toastCount(page)).toBe(0);

    const results = (await reads(request)).resultReads;
    expect(results.filter((r) => r.id === KEPT).map((r) => [r.offset, r.limit]).sort())
      .toEqual([[0, 1], [2, 1]]);
    expect(results.filter((r) => r.id === PRUNED)).toEqual([{ id: PRUNED, offset: 1, limit: 1 }]);

    // The log is every step and note in the order they were written: the
    // note sits between the second and third steps although the second
    // step's interrupted outcome was written last.
    const log = main.getByRole('list', { name: 'Steps and notes' }).locator('> li');
    await expect(log).toHaveCount(4);
    await expect(log.nth(0)).toContainText('Find the first 502');
    await expect(log.nth(0)).toContainText('Succeeded');
    await expect(log.nth(1)).toContainText('Widen to the whole day');
    await expect(log.nth(1)).toContainText('Interrupted');
    await expect(log.nth(2)).toContainText('Paged the payments on-call');
    await expect(log.nth(3)).toContainText('Count pool errors per gateway host');
    await expect(log.nth(3)).toContainText('2026-10-24T08:30:00.000000Z → 2026-10-24T09:30:00.000000Z');
    await expect(main.getByText('Every entry is loaded.')).toBeVisible();

    // Long executed text wraps inside its box.
    for (const executed of await main.locator('pre.inv-executed').all()) {
      const fits = await executed.evaluate((el) => el.scrollWidth <= el.clientWidth);
      expect(fits).toBe(true);
    }
    await expectNoHorizontalOverflow(page);

    // Read-only: Refresh is the page's one button, and nothing offers a
    // write the session cannot make.
    await expect(main.getByRole('button')).toHaveText(['Refresh']);

    await expect(main).toMatchAriaSnapshot(`
      - paragraph:
        - link "Investigations":
          - /url: /investigations
      - heading "502s on checkout since 09:00" [level=1]
      - paragraph: Open Opened by claude-agent · updated 2026-10-24 09:41 UTC by claude-agent
      - button "Refresh"
      - region "Question":
        - heading "Question" [level=2]
        - paragraph: Why does checkout answer 502 since 09:00, and is it still happening?
        - heading "Scope" [level=3]
        - paragraph: service=shop-api and its upstream payment pool, 08:00 to 10:00 UTC
      - region "Latest finding":
        - heading "Latest finding" [level=2]
        - text: Hypothesis claude-agent · 2026-10-24 09:36 UTC
        - paragraph: The resize halved the pool on gw-2 only; gw-1 kept its old size.
      - region "Findings":
        - heading "Findings" [level=2]
        - list "Findings":
          - listitem:
            - article:
              - text: Supported claude-agent · 2026-10-24 09:24 UTC
              - paragraph: "The payment upstream pool is exhausted: every 502 follows an upstream connect timeout to payments:8443."
              - paragraph: "Still open: Did the 08:58 pool resize shrink the pool, or did traffic grow?"
              - heading "Evidence" [level=3]
              - list:
                - listitem:
                  - paragraph: Row 0 of result fedcba98
                  - region "Row 0 of result fedcba98":
                    - table:
                      - rowgroup:
                        - row "_time host status message"
                      - rowgroup:
                        - row "2026-10-24 09:03:40 gw-2 502 upstream connect timeout to payments:8443 after 3000ms; pool exhausted (64/64 in use)"
                - listitem:
                  - paragraph: Row 2 of result fedcba98
                  - region "Row 2 of result fedcba98":
                    - table:
                      - rowgroup:
                        - row "_time host status message"
                      - rowgroup:
                        - row "2026-10-24 09:01:12 gw-2 502 upstream connect timeout to payments:8443 after 3000ms"
          - listitem:
            - article:
              - text: Hypothesis claude-agent · 2026-10-24 09:36 UTC
              - paragraph: The resize halved the pool on gw-2 only; gw-1 kept its old size.
              - heading "Evidence" [level=3]
              - list:
                - listitem:
                  - paragraph: Row 1 of result 0a1b2c3d
                  - paragraph: Pruned This result's evidence file is gone. The finding still cites the row.
      - region "Steps and notes":
        - heading "Steps and notes" [level=2]
        - list "Steps and notes":
          - listitem:
            - text: Step Succeeded 3 rows · 0.182s
            - paragraph: Find the first 502 and what the gateway logged beside it
            - code: service=shop-api status>=500 earliest="2026-10-24T08:00:00.000000Z" latest="2026-10-24T10:00:00.000000Z" | sort -_time, -_ingested
            - paragraph: Bounds 2026-10-24T08:00:00.000000Z → 2026-10-24T10:00:00.000000Z
            - paragraph: claude-agent · 2026-10-24 09:13 UTC
          - listitem:
            - text: Step Interrupted
            - paragraph: Widen to the whole day to see when the 502 rate changed
            - code: service=shop-api earliest="2026-10-24T00:00:00.000000Z" latest="2026-10-24T10:00:00.000000Z" | timechart span=1m count by status
            - paragraph: Bounds 2026-10-24T00:00:00.000000Z → 2026-10-24T10:00:00.000000Z
            - paragraph: claude-agent · 2026-10-24 09:18 UTC
          - listitem:
            - text: Note
            - paragraph: Paged the payments on-call. They confirm a pool resize deploy went out at 08:58.
            - paragraph: jakub · 2026-10-24 09:20 UTC
          - listitem:
            - text: Step Succeeded 6 rows · 1.240s
            - paragraph: Count pool errors per gateway host before and after the resize
            - code: service=payments-gateway (message="pool exhausted" OR message="connect timeout" OR message="upstream reset") earliest="2026-10-24T08:30:00.000000Z" latest="2026-10-24T09:30:00.000000Z" | stats count by host, message | sort -count
            - paragraph: Bounds 2026-10-24T08:30:00.000000Z → 2026-10-24T09:30:00.000000Z
            - paragraph: claude-agent · 2026-10-24 09:30 UTC
        - text: Every entry is loaded.
    `);

    await captureWhole(page, capturePath(testInfo, 'investigation-phone.png'));
    // The capture never widened the page: it is the phone's width.
    expect(page.viewportSize()!.width).toBe(iphone.viewport.width);
    await expectNoHorizontalOverflow(page);
  });
});

test('Load more and Refresh read the entries changed since the last read, and every control takes Tab', async ({ page, request }) => {
  // Four entries per read: the first page ends at the third step's
  // outcome, and the second brings the last finding and the second
  // step's interrupted outcome.
  await asReader(request, 4);
  await page.goto(`/investigations/${ID}`);
  const main = page.locator('.inv-page');
  const log = main.getByRole('list', { name: 'Steps and notes' }).locator('> li');
  await expect(log).toHaveCount(3);
  await expect(main.getByRole('list', { name: 'Findings' }).locator('> li')).toHaveCount(1);
  const more = main.getByRole('button', { name: 'Load more entries' });
  await expect(more).toBeVisible();
  // The rail marks the section for the page under it.
  await expect(page.locator('nav.rail .grp > a[aria-current="page"]')).toHaveAttribute('title', 'Investigations');

  // Every control on the page is reachable by Tab, in reading order.
  await main.getByRole('link', { name: 'Investigations' }).focus();
  const wanted = [
    'button:Refresh',
    'region:Row 0 of result fedcba98',
    'region:Row 2 of result fedcba98',
    'button:Load more entries',
  ];
  const reached: string[] = [];
  for (let i = 0; i < 20 && reached.length < wanted.length; i += 1) {
    await page.keyboard.press('Tab');
    const focused = await page.evaluate(() => {
      const el = document.activeElement as HTMLElement | null;
      if (!el || !el.closest('.inv-page')) return null;
      const role = el.getAttribute('role') ?? el.tagName.toLowerCase();
      const name = el.getAttribute('aria-label') ?? el.textContent?.trim() ?? '';
      return `${role}:${name}`;
    });
    if (focused) reached.push(focused);
  }
  expect(reached).toEqual(wanted);

  await page.keyboard.press('Enter');
  await expect(log).toHaveCount(4);
  await expect(log.nth(1)).toContainText('Interrupted');
  await expect(main.getByRole('list', { name: 'Findings' }).locator('> li')).toHaveCount(2);
  await expect(more).toHaveCount(0);
  await expect(main.getByText('Every entry is loaded.')).toBeVisible();

  await main.getByRole('button', { name: 'Refresh' }).click();
  await expect.poll(async () => (await reads(request)).reads.length).toBe(3);
  expect((await reads(request)).reads.map((r) => r.since)).toEqual([0, 7, 9]);
  // Nothing changed since 9, so the page reads as it did.
  await expect(log).toHaveCount(4);
  // Each cited row was read once in all of that.
  expect((await reads(request)).resultReads).toHaveLength(3);
});

test('a board citing 128 rows reads them four at a time, each once, and waits out a 429 instead of failing a row', async ({ page, request }) => {
  // The first six result reads answer 429 with Retry-After: 1, and every
  // result answer is held 40 ms so the page's reads overlap at the stub.
  const response = await request.post('/__ctl/reset', {
    data: { scenario: 'investigations', investigationBoard: 'many', investigationThrottle: 6, investigationResultDelayMs: 40 },
  });
  expect(response.ok()).toBe(true);
  await trackToasts(page);
  await page.goto(`/investigations/${MANY}`);
  const main = page.locator('.inv-page');
  await expect(main.getByRole('list', { name: 'Findings' }).locator('> li')).toHaveCount(4);

  // Every cited row ends loaded, the refused ones included: none shows a
  // failure or offers Retry.
  await expect(main.locator('.inv-row-scroll')).toHaveCount(128, { timeout: 15_000 });
  await expect(main.getByText('Could not load this row.')).toHaveCount(0);
  await expect(main.getByRole('button', { name: /^Retry / })).toHaveCount(0);
  expect(await toastCount(page)).toBe(0);

  const state = await reads(request);
  // Each of the 96 distinct rows was answered once; the six refused
  // reads were asked again, and nothing else.
  expect(new Set(state.resultReads.map((r) => `${r.id}:${r.offset}`)).size).toBe(96);
  expect(state.resultReads).toHaveLength(96 + 6);
  // Never more than four at once, and more than one: the reads overlap.
  expect(state.maxResultsInFlight).toBeLessThanOrEqual(4);
  expect(state.maxResultsInFlight).toBeGreaterThan(1);
});

test('without investigation_read there is no rail entry, and the pages say so and ask nothing', async ({ page, request }) => {
  // The default session holds `query` alone.
  await resetScenario(request, 'default');
  await page.goto('/search');
  await expect(page.locator('nav.rail .grp > a[title="Search"]')).toBeVisible();
  await expect(page.locator('nav.rail a[href="/investigations"]')).toHaveCount(0);

  await page.goto('/investigations');
  await expect(page.getByRole('heading', { level: 1, name: 'Investigations' })).toBeVisible();
  await expect(page.getByRole('status').filter({ hasText: NOT_PERMITTED })).toBeVisible();
  await expect(page.locator('.inv-page').getByRole('button')).toHaveCount(0);
  await expect(page.locator('nav.rail a[href="/investigations"]')).toHaveCount(0);

  await page.goto(`/investigations/${ID}`);
  await expect(page.getByRole('status').filter({ hasText: NOT_PERMITTED })).toBeVisible();
  await expect(page.locator('.inv-page').getByRole('button')).toHaveCount(0);

  expect(await reads(request)).toEqual({ listReads: 0, reads: [], resultReads: [], maxResultsInFlight: 0 });
});

test('an id the installation does not hold reads as not found', async ({ page, request }) => {
  await asReader(request);
  await page.goto('/investigations/ffffffffffffffffffffffffffffffff');
  await expect(page.getByRole('heading', { level: 1, name: 'Investigation not found' })).toBeVisible();
  // Not an id at all: refused before any request.
  await page.goto('/investigations/not-an-id');
  await expect(page.getByRole('heading', { level: 1, name: 'Investigation not found' })).toBeVisible();
  expect((await reads(request)).reads.map((r) => r.id)).toEqual(['ffffffffffffffffffffffffffffffff']);
});
