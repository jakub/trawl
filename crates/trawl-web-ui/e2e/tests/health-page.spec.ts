// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import type { APIRequestContext, Locator, Page } from '@playwright/test';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { test, expect } from '../fixtures';
import { SEL } from '../selectors';

async function state(request: APIRequestContext) {
  return (await request.get('/__ctl/state')).json();
}
async function setup(request: APIRequestContext, scenario: string, options = {}) {
  // Counters survive resets. Wait for the previous page's socket to close;
  // a leaked connection must fail here instead of being reset to zero.
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(0);
  const baseline = (await state(request)).dashboard.opens;
  await request.post('/__ctl/reset', { data: { scenario, ...options } });
  return baseline;
}
async function rows(page: Page) {
  await expect(page.locator(SEL.healthRow)).toHaveCount(4);
  await expect(page.locator(SEL.healthOwnRow)).toHaveCount(2);
}
async function confirm(page: Page, id = 101) {
  await page.locator(`${SEL.healthRow}[data-query-id="${id}"]`).getByRole('button', { name: 'Cancel', exact: true }).click();
  const dialog = page.locator(SEL.healthConfirm);
  await expect(dialog).toBeVisible();
  return dialog;
}

for (const width of [1440, 720]) {
  for (const identity of ['health-admin', 'health-viewer']) {
    test(`${identity} at ${width}px places visible cards above full-width Queries`, async ({ page, request }) => {
      await setup(request, identity);
      await page.setViewportSize({ width, height: 1000 });
      const longQuery = `service=api message="${'a-long-unbroken-query-value'.repeat(30)}"`;
      await page.route('**/api/v1/queries', async route => {
        const response = await route.fetch();
        const body = await response.json();
        body.active[0].query = longQuery;
        await route.fulfill({ response, json: body });
      });
      await page.goto('/settings/health');
      await rows(page);
      const cards = identity === 'health-admin'
        ? [SEL.healthSection, SEL.healthCapacity, SEL.healthLive]
        : [SEL.healthSection];
      const boxes = [];
      for (const selector of cards) {
        await expect(page.locator(selector)).toBeVisible();
        boxes.push((await page.locator(selector).boundingBox())!);
      }
      const queries = (await page.locator(SEL.healthQueries).boundingBox())!;
      expect(queries.y).toBeGreaterThanOrEqual(Math.max(...boxes.map(b => b.y + b.height)));
      const left = Math.min(...boxes.map(b => b.x));
      const right = Math.max(...boxes.map(b => b.x + b.width));
      expect(Math.abs(queries.x - left)).toBeLessThanOrEqual(1);
      expect(Math.abs(queries.x + queries.width - right)).toBeLessThanOrEqual(1);
      for (let i = 1; i < boxes.length; i++) {
        if (width >= 1100) {
          expect(Math.abs(boxes[i].y - boxes[0].y)).toBeLessThanOrEqual(1);
          expect(boxes[i].x).toBeGreaterThanOrEqual(boxes[i - 1].x + boxes[i - 1].width);
        } else {
          expect(boxes[i].y).toBeGreaterThanOrEqual(boxes[i - 1].y + boxes[i - 1].height);
          expect(Math.abs(boxes[i].width - queries.width)).toBeLessThanOrEqual(1);
        }
      }
      const table = page.getByRole('region', { name: 'Queries table', exact: true });
      const frame = page.getByRole('region', { name: 'Queries', exact: true });
      await expect(frame.getByRole('heading', { name: 'Queries', exact: true })).toBeVisible();
      await expect(frame.getByRole('button', { name: 'Refresh queries', exact: true })).toBeVisible();
      expect(await frame.evaluate(el => {
        const style = getComputedStyle(el);
        const heading = el.querySelector('h2')!;
        const table = el.querySelector('table')!;
        return Number.parseFloat(style.borderTopWidth) > 0
          && style.backgroundColor !== 'rgba(0, 0, 0, 0)'
          && heading.closest('.fleet-table-frame') === el
          && table.closest('.fleet-table-frame') === el;
      }), 'the heading and table share one visible card frame').toBe(true);
      await expect(table.getByRole('columnheader')).toHaveText(['Query', 'User', 'State', 'Elapsed', 'Action']);
      for (const header of await table.getByRole('columnheader').all()) {
        expect(await header.evaluate(el => el.tagName)).toBe('TH');
        await expect(header).toHaveAttribute('scope', 'col');
      }
      const queryText = table.getByText(longQuery, { exact: true });
      await expect(queryText).toBeVisible();
      expect(await queryText.evaluate(el => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return range.getClientRects().length;
      })).toBeGreaterThan(1);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    });
  }
}

test('query text is inert and Cancel is a separate keyboard action with confirmation', async ({ page, request }) => {
  await setup(request, 'health-cancel');
  await page.goto('/settings/health');
  await rows(page);
  const row = page.locator(`${SEL.healthRow}[data-query-id="101"]`);
  await row.getByRole('cell').first().click();
  await expect(page.locator(SEL.healthConfirm)).toHaveCount(0);
  expect((await state(request)).cancelRequests).toEqual([]);
  const cancel = row.getByRole('button', { name: 'Cancel', exact: true });
  await cancel.focus();
  await expect(cancel).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.healthConfirm)).toBeVisible();
  expect((await state(request)).cancelRequests).toEqual([]);
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.healthConfirm)).toHaveCount(0);
});

test('a delayed refresh cannot restore an active query after a newer empty report', async ({ page, request }) => {
  await setup(request, 'health-cancel');
  let reads = 0;
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let delivered!: () => void;
  const delivery = new Promise<void>(resolve => { delivered = resolve; });
  await page.route('**/api/v1/queries', async route => {
    const read = ++reads;
    const response = await route.fetch();
    if (read === 2) {
      await held;
      await route.fulfill({ response });
      delivered();
    } else if (read >= 3) {
      await route.fulfill({ response, json: { active: [], recent: [], retained: [] } });
    } else {
      await route.fulfill({ response });
    }
  });
  await page.goto('/settings/health');
  await rows(page);
  await page.locator(SEL.healthQueriesRefresh).click();
  await expect.poll(() => reads).toBe(2);
  await page.locator(SEL.healthQueriesRefresh).click();
  await expect(page.locator(SEL.healthQueries)).toContainText('No active or recent queries.');
  release();
  await delivery;
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  await expect(page.locator(SEL.healthQueries)).toContainText('No active or recent queries.');
  await expect(page.locator(SEL.healthRow)).toHaveCount(0);
  await expect(page.locator(SEL.healthQueries).getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
  expect((await state(request)).cancelRequests).toEqual([]);
});

test('a delayed health failure cannot replace a newer healthy report', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  let armed = false;
  let reads = 0;
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let delivered!: () => void;
  const delivery = new Promise<void>(resolve => { delivered = resolve; });
  await page.route('**/api/v1/health', async route => {
    const read = armed ? ++reads : 0;
    const response = await route.fetch();
    if (read === 1) {
      await held;
      await route.fulfill({ status: 502, json: { error: 'old report failed' } });
      delivered();
    } else {
      await route.fulfill({ response });
    }
  });
  await page.goto('/settings/health');
  const health = page.locator(SEL.healthSection);
  await expect(health.getByRole('heading')).toHaveText('Server is healthy');
  armed = true;
  await page.locator(SEL.healthRefresh).click();
  await expect.poll(() => reads).toBe(1);
  await page.locator(SEL.healthRefresh).click();
  await expect.poll(() => reads).toBe(2);
  await expect(health.getByRole('heading')).toHaveText('Server is healthy');
  release();
  await delivery;
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  await expect(health.getByRole('heading')).toHaveText('Server is healthy');
  await expect(health.getByRole('alert')).toHaveCount(0);
});

test('non-admin request silence: health 200 and queries without admin traffic or DOM', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthSection)).toContainText('health-fixture-163');
  await expect(page.locator(SEL.healthSection)).toContainText('duckdb');
  await expect(page.locator(SEL.healthSection)).toContainText('auth_db');
  await expect(page.locator(SEL.healthSection)).toContainText('storage_db');
  await rows(page);
  await page.locator(SEL.healthRefresh).click();
  await expect.poll(async () => (await state(request)).healthHits.health).toBeGreaterThanOrEqual(2);
  // Allow effects and the stub's reconnect interval to run before proving silence.
  await page.waitForTimeout(350);
  const hits = (await state(request)).healthHits;
  expect(hits.stats ?? 0, 'non-admin must not request stats').toBe(0);
  expect(hits.dashboard ?? 0, 'non-admin must not request dashboard').toBe(0);
  expect(hits.stream ?? 0, 'non-admin must not request dashboard stream').toBe(0);
  await expect(page.locator(SEL.healthCapacity)).toHaveCount(0);
  await expect(page.locator(SEL.healthLive)).toHaveCount(0);
  await expect(page.locator(SEL.healthDiagnostics)).toHaveCount(0);
  await expect(page.locator(SEL.healthFooterWal)).toHaveCount(0);
  await expect(page.locator(SEL.healthQueries).getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
});

test('health 503 renders the named failed subsystem', async ({ page, request }) => {
  await setup(request, 'health-degraded');
  const healthResponse = page.waitForResponse(r => r.url().endsWith('/api/v1/health') && r.status() === 503);
  await page.goto('/settings/health');
  await healthResponse;
  const health = page.locator(SEL.healthSection);
  for (const [name, value] of [
    ['Overall state', 'Unavailable'],
    ['duckdb', 'error'],
    ['auth_db', 'Healthy'],
    ['storage_db', 'Healthy'],
    ['data_path', 'Healthy'],
  ]) {
    // The key now sits in a span inside the dt, so the row is the
    // nearest ancestor div rather than the matched node's parent.
    await expect(
      health.getByText(name, { exact: true }).locator('xpath=ancestor::div[1]').locator('dd'),
    ).toHaveText(value);
  }
  await expect(health).toContainText('health-fixture-163');
});

test('admin shares one stream with the footer across direct navigation, away, Back and Forward', async ({ page, request }) => {
  const baseline = await setup(request, 'health-admin');
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await expect(page.locator(SEL.healthCapacity)).toContainText('1,234');
  await expect(page.locator(SEL.healthFooterHot)).toContainText('731');
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(1);
  await page.evaluate(() => { (window as any).__healthNavigation = true; });
  await page.locator(SEL.healthAwayLink).click();
  await expect(page).toHaveURL(/\/search\/schema$/);
  await page.goBack();
  await expect(page.locator(SEL.healthPage)).toBeVisible();
  await page.goForward();
  await expect(page).toHaveURL(/\/search\/schema$/);
  await page.goBack();
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  expect(await page.evaluate(() => (window as any).__healthNavigation)).toBe(true);
  const counts = (await state(request)).dashboard;
  expect(counts.open).toBe(1);
  expect(counts.opens - baseline).toBe(1);
  expect(counts.max).toBe(1);
});

test('manual refresh re-reads health and capacity, with a separate queries refresh', async ({ page, request }) => {
  await setup(request, 'health-admin');
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await rows(page);
  const before = (await state(request)).healthHits;
  await page.locator(SEL.healthRefresh).click();
  await expect.poll(async () => (await state(request)).healthHits.stats).toBe(before.stats + 1);
  await expect.poll(async () => (await state(request)).healthHits.health).toBe(before.health + 1);
  await page.locator(SEL.healthQueriesRefresh).click();
  await expect.poll(async () => (await state(request)).healthHits.queries).toBe(before.queries + 1);
});

test('Health rail points to the real page', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page.locator(SEL.railHealthLink)).toHaveAttribute('href', '/settings/health');
  await page.locator(SEL.railHealthLink).click();
  await expect(page).toHaveURL(/\/settings\/health$/);
  await expect(page.locator(SEL.healthPage)).toBeVisible();
});

test('bootstrap 503 waits and recovers on the first stream snapshot', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardHold: true, dashboardBootstrap: 'waiting' });
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Waiting for first snapshot');
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(1);
  await request.post('/__ctl/dashboard/release');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await expect(page.locator(SEL.healthLive)).toContainText('731');
});

test('a dropped stream is stale until fresh data arrives', async ({ page, request }) => {
  await setup(request, 'health-admin');
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await request.post('/__ctl/dashboard/drop');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Stale; reconnecting');
  await expect(page.locator(SEL.healthFooterHot)).toHaveCount(0);
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(1);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Stale; reconnecting');
  await request.post('/__ctl/dashboard/release');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
});

test('a late bootstrap cannot replace an authoritative stream snapshot', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardBootstrap: 'held' });
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await expect(page.locator(SEL.healthLive)).toContainText('731');
  await expect.poll(async () => (await state(request)).dashboard.pending).toBe(1);
  const reply = await request.post('/__ctl/dashboard/bootstrap-release');
  expect((await reply.json()).count).toBe(1);
  await page.waitForTimeout(250);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await expect(page.locator(SEL.healthLive)).toContainText('731');
  await expect(page.locator(SEL.healthLive)).not.toContainText('old-bootstrap-host');
});

for (const identity of ['health-no-query', 'health-admin-no-query']) {
  test(`${identity} does not fetch or render queries without query permission`, async ({ page, request }) => {
    await setup(request, identity);
    await page.goto('/settings/health');
    await expect(page.locator(SEL.healthSection)).toBeVisible();
    await page.waitForTimeout(250);
    expect((await state(request)).healthHits.queries ?? 0).toBe(0);
    await expect(page.locator(SEL.healthQueries)).toHaveCount(0);
  });
}

for (const [identity, ids] of [['health-viewer', []], ['health-cancel', [101]], ['health-admin', [101, 102]]] as const) {
  test(`${identity} cancel authority covers only active queries by own flag`, async ({ page, request }) => {
    await setup(request, identity);
    await page.goto('/settings/health');
    await rows(page);
    for (const id of [101, 102, 201, 202]) {
      const row = page.locator(`${SEL.healthRow}[data-query-id="${id}"]`);
      const [label, tone] = id < 200 ? ['Running', 'info']
        : id === 201 ? ['Succeeded', 'success'] : ['Timed out', 'danger'];
      await expect(row.locator('.bdg')).toHaveText(label);
      await expect(row.locator('.bdg')).toHaveClass(`bdg ${tone}`);
      await expect(row.getByRole('button', { name: 'Cancel', exact: true })).toHaveCount((ids as readonly number[]).includes(id) ? 1 : 0);
    }
  });
}

test('confirmation sends one DELETE and reports the accepted request', async ({ page, request }) => {
  await setup(request, 'health-cancel');
  await page.goto('/settings/health');
  await rows(page);
  let dialog = await confirm(page);
  expect((await state(request)).cancelRequests).toEqual([]);
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  expect((await state(request)).cancelRequests).toEqual([]);
  dialog = await confirm(page);
  await dialog.getByRole('button', { name: 'Cancel query', exact: true }).click();
  await expect(page.locator(SEL.healthQueries)).toContainText('Cancellation requested');
  await page.waitForTimeout(250);
  expect((await state(request)).cancelRequests).toEqual([101]);
});

for (const [outcome, message] of [['finished', 'No work was cancelled'], ['unknown', 'Cancellation outcome unknown. Refresh queries before trying again.']]) {
  test(`cancel ${outcome} is not success`, async ({ page, request }) => {
    await setup(request, 'health-cancel', { cancelOutcome: outcome });
    await page.goto('/settings/health');
    await rows(page);
    const dialog = await confirm(page);
    await dialog.getByRole('button', { name: 'Cancel query', exact: true }).click();
    await expect(page.locator(SEL.healthQueries)).toContainText(message);
    await expect(page.locator(SEL.healthQueries)).not.toContainText('Cancellation requested');
    expect((await state(request)).cancelRequests).toEqual([101]);
  });
}

test('leaving the authenticated shell closes its dashboard stream without reconnecting', async ({ page, request }) => {
  await setup(request, 'health-admin');
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await page.goto('/login');
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(0);
  const opens = (await state(request)).dashboard.opens;
  await page.waitForTimeout(650);
  expect((await state(request)).dashboard.opens).toBe(opens);
});

// Observe real browser error delivery. The counter advances in the native
// EventSource event, not when the control server merely sends a response.
async function observeDashboardErrors(page: Page) {
  await page.addInitScript(() => {
    const NativeEventSource = window.EventSource;
    (window as any).__healthStreamErrors = [];
    window.EventSource = class extends NativeEventSource {
      constructor(url: string | URL, options?: EventSourceInit) {
        super(url, options);
        if (String(url).includes('/dashboard/stream')) {
          this.addEventListener('error', () => {
            (window as any).__healthStreamErrors.push(this.readyState);
          });
        }
      }
    };
  });
}

async function afterDashboardError(page: Page, readyState: number) {
  await expect.poll(() => page.evaluate(() => (window as any).__healthStreamErrors)).toContain(readyState);
  // Let the application's listener and reactive render finish after the
  // observed error dispatch. Frame boundaries prove the update ran.
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

test('regression: no snapshot plus bootstrap 503 stays waiting across stream reconnect', async ({ page, request }) => {
  const baseline = await setup(request, 'health-admin', { dashboardHold: true, dashboardBootstrap: 'waiting' });
  await observeDashboardErrors(page);
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Waiting for first snapshot');
  await expect.poll(async () => (await state(request)).dashboard.open).toBe(1);
  await request.post('/__ctl/dashboard/drop');
  await afterDashboardError(page, 0);
  await expect.poll(async () => (await state(request)).dashboard.opens).toBe(baseline + 2);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Waiting for first snapshot');
  await expect(page.locator(SEL.healthFooterHot)).toHaveCount(0);
  await request.post('/__ctl/dashboard/release');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Live');
  await expect(page.locator(SEL.healthLive)).toContainText('731');
});

test('regression: bootstrap 403 stays forbidden after a later terminal stream failure', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardBootstrap: 'forbidden', dashboardTerminalHold: true });
  await observeDashboardErrors(page);
  await page.goto('/settings/health');
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Forbidden');
  await expect.poll(async () => (await state(request)).dashboard.terminalPending).toBe(1);
  const release = await request.post('/__ctl/dashboard/terminal-release');
  expect((await release.json()).count).toBe(1);
  await afterDashboardError(page, 2);
  await expect(page.locator(SEL.healthLiveState)).toHaveText('Forbidden');
  await expect(page.locator(SEL.healthFooterHot)).toHaveCount(0);
});

for (const status of [500, 502, 504, 403]) {
  test(`regression: cancel HTTP ${status} reports ${status === 403 ? 'definite refusal' : 'unknown outcome'}`, async ({ page, request }) => {
    await setup(request, 'health-cancel', { cancelOutcome: `http-${status}` });
    await page.goto('/settings/health');
    await rows(page);
    const dialog = await confirm(page);
    await dialog.getByRole('button', { name: 'Cancel query', exact: true }).click();
    const message = status === 403
      ? 'Cancellation failed, server returned 403.'
      : 'Cancellation outcome unknown. Refresh queries before trying again.';
    await expect(page.locator(SEL.healthQueries)).toContainText(message);
    await expect(page.locator(SEL.healthQueries)).not.toContainText('Cancellation requested');
    expect((await state(request)).cancelRequests).toEqual([101]);
  });
}

test('query-only readers can focus and scroll the queries table with the keyboard', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  await page.route('**/api/v1/queries', async route => {
    const response = await route.fetch();
    const body = await response.json();
    // Exercise a long display name alongside the narrow table viewport.
    body.active[0].user = 'queryreader'.repeat(12);
    await route.fulfill({ response, json: body });
  });
  await page.setViewportSize({ width: 640, height: 900 });
  await page.goto('/settings/health');
  await rows(page);
  await expect(page.locator(SEL.healthQueries).getByRole('button', { name: 'Cancel', exact: true })).toHaveCount(0);
  const region = page.locator(SEL.healthQueryScroll);
  await expect(region).toBeVisible();
  await expect(region).toHaveAttribute('role', 'region');
  await expect(page.getByRole('region', { name: 'Queries', exact: true })).toHaveCount(1);
  await expect(page.getByRole('region', { name: 'Queries table', exact: true })).toHaveCount(1);
  await expect(region).toHaveAccessibleName('Queries table');
  await expect.poll(() => region.evaluate(el => el.scrollWidth > el.clientWidth)).toBe(true);
  await expect(page.locator(SEL.healthQueries).getByText('Scroll horizontally for more columns.', { exact: true })).toBeVisible();
  await page.locator(SEL.healthQueriesRefresh).focus();
  await page.keyboard.press('Tab');
  await expect(region).toBeFocused();
  expect(await region.evaluate(el => {
    const style = getComputedStyle(el);
    return style.outlineStyle !== 'none' && Number.parseFloat(style.outlineWidth) > 0;
  })).toBe(true);
  await page.keyboard.press('ArrowRight');
  await expect.poll(() => region.evaluate(el => el.scrollLeft)).toBeGreaterThan(0);
});

async function fact(section: Locator, label: string, value: string) {
  const row = section.locator('dl > div').filter({ has: section.page().locator('dt', { hasText: new RegExp(`^${label}$`) }) });
  await expect(row.locator('dd')).toHaveText(value);
}

async function diagnosticPhase(page: Page, label: string) {
  await expect(page.locator(SEL.healthDiagnosticState)).toHaveText([label, label]);
}

for (const width of [1440, 720]) {
  test(`diagnostic band capture at ${width}px preserves cards and query controls`, async ({ page, request }, testInfo) => {
    await setup(request, 'health-admin', { dashboardSnapshot: { compaction_errors: 71 } });
    await page.setViewportSize({ width, height: 1000 });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    await rows(page);
    const ingestion = page.locator(SEL.healthIngestion);
    const storage = page.locator(SEL.healthStorage);
    await fact(page.locator(SEL.healthLive), 'HTTP ingest rate', '12.5 events/s');
    for (const [label, value] of [
      ['HTTP rejected events', '41'], ['Syslog configuration', 'Enabled'],
      ['UDP received', '113'], ['TCP received', '227'], ['Syslog rate', '3.5 events/s'],
      ['Parse errors', '17'], ['Backpressure drops', '29'], ['Active TCP connections', '5'],
    ]) await fact(ingestion, label, value);
    await fact(storage, 'Successful compaction cycles', '40');
    await fact(storage, 'Compaction error tally', '71');
    await fact(storage, 'Last successful cycle', '15s ago');
    await expect(storage.locator('[data-source="wal"]')).toContainText('7 files');
    await expect(storage.locator('[data-source="parquet"]')).toContainText('20 files, 8 KB');
    await expect(storage.locator('[data-source="wal"]')).toContainText('7 files, 2 KB');
    await expect(storage.locator('[data-source="wal"]')).toContainText('Sample age: 2s at this snapshot');
    await expect(ingestion).toContainText('since process startup');
    await expect(ingestion).toContainText('Received messages do not prove persistence');
    await expect(storage).toContainText('Includes active WAL files');
    await expect(storage).toContainText('Excludes saved report files');
    await expect(storage).toContainText('exceed their count');
    // Historical nonzero totals stay facts, without incident styling or roles.
    await expect(page.locator(SEL.healthDiagnostics).locator('[role="alert"], .health-check-error, [class*="danger"], [class*="warning"]')).toHaveCount(0);
    const cards = (await page.locator(SEL.healthCards).boundingBox())!;
    const a = (await ingestion.boundingBox())!;
    const b = (await storage.boundingBox())!;
    const queries = (await page.locator(SEL.healthQueries).boundingBox())!;
    expect(a.y).toBeGreaterThanOrEqual(cards.y + cards.height);
    expect(queries.y).toBeGreaterThanOrEqual(Math.max(a.y + a.height, b.y + b.height));
    if (width >= 1100) {
      expect(Math.abs(a.y - b.y)).toBeLessThanOrEqual(1);
      expect(b.x).toBeGreaterThanOrEqual(a.x + a.width);
    } else {
      expect(b.y).toBeGreaterThanOrEqual(a.y + a.height);
      expect(Math.abs(a.width - queries.width)).toBeLessThanOrEqual(1);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
    await page.locator(SEL.healthQueriesRefresh).click();
    await rows(page);
    const dialog = await confirm(page);
    await page.keyboard.press('Escape');
    await expect(dialog).toHaveCount(0);
    // Health owns its scroll container. Capture overlapping viewport slices
    // of that actual scroller, without resizing or changing production CSS.
    const scroller = page.locator(SEL.healthPage);
    await page.locator(SEL.healthQueryScroll).evaluate(el => { el.scrollLeft = 0; });
    await scroller.evaluate(el => { el.scrollTop = 0; el.scrollLeft = 0; });
    const frames: { name: string; width: number; scrollTop: number; clientHeight: number; scrollHeight: number }[] = [];
    const sections = await scroller.evaluate(el => {
      const origin = el.getBoundingClientRect().top;
      return ['.health-cards', '.health-ingestion', '.health-storage', '.health-queries'].map(selector => {
        const section = el.querySelector(selector)!;
        const rect = section.getBoundingClientRect();
        return { selector, top: rect.top - origin + el.scrollTop, bottom: rect.bottom - origin + el.scrollTop };
      });
    });
    for (let part = 1; part <= 20; part++) {
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      const geometry = await scroller.evaluate(el => ({ scrollTop: el.scrollTop, clientHeight: el.clientHeight, scrollHeight: el.scrollHeight }));
      const name = `health-diagnostics-${width}-part-${part}.png`;
      const capture = process.env.TRAWL_HEALTH_CAPTURE_DIR
        ? path.join(process.env.TRAWL_HEALTH_CAPTURE_DIR, name)
        : testInfo.outputPath(name);
      await mkdir(path.dirname(capture), { recursive: true });
      await page.screenshot({ path: capture, animations: 'disabled' });
      await testInfo.attach(name, { path: capture, contentType: 'image/png' });
      frames.push({ name, width, ...geometry });
      if (geometry.scrollTop + geometry.clientHeight >= geometry.scrollHeight - 1) break;
      const next = geometry.scrollTop + Math.max(1, geometry.clientHeight - 100);
      await scroller.evaluate((el, next) => { el.scrollTop = next; }, next);
    }
    expect(frames[0].scrollTop).toBe(0);
    for (let part = 1; part < frames.length; part++) {
      expect(frames[part].scrollTop).toBeGreaterThan(frames[part - 1].scrollTop);
      expect(frames[part].scrollTop).toBeLessThanOrEqual(frames[part - 1].scrollTop + frames[part - 1].clientHeight);
    }
    const last = frames[frames.length - 1];
    expect(last.scrollTop + last.clientHeight).toBeGreaterThanOrEqual(last.scrollHeight - 1);
    for (const section of sections) {
      expect(section.top).toBeGreaterThanOrEqual(0);
      expect(section.bottom).toBeLessThanOrEqual(last.scrollTop + last.clientHeight + 1);
    }
    expect(page.viewportSize()).toEqual({ width, height: 1000 });
    const manifestPath = process.env.TRAWL_HEALTH_CAPTURE_DIR
      ? path.join(process.env.TRAWL_HEALTH_CAPTURE_DIR, `health-diagnostics-${width}.json`)
      : testInfo.outputPath(`health-diagnostics-${width}.json`);
    await writeFile(manifestPath, JSON.stringify({ width, viewportHeight: 1000, scroller: SEL.healthPage, sections, frames }, null, 2));

  });
}

for (const enabled of [false, true]) {
  test(`syslog configured ${enabled ? 'enabled and idle' : 'disabled'} has truthful readings`, async ({ page, request }) => {
    await setup(request, 'health-admin', { dashboardSnapshot: {
      syslog_enabled: enabled, syslog_events_udp: 0, syslog_events_tcp: 0,
      syslog_rate: 0, syslog_parse_errors: 0, syslog_dropped: 0, syslog_tcp_connections: 0,
    } });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    const ingestion = page.locator(SEL.healthIngestion);
    await fact(ingestion, 'Syslog configuration', enabled ? 'Enabled' : 'Disabled');
    if (enabled) {
      for (const label of ['UDP received', 'TCP received', 'Parse errors', 'Backpressure drops', 'Active TCP connections']) await fact(ingestion, label, '0');
      await fact(ingestion, 'Syslog rate', '0.0 events/s');
    } else {
      for (const label of ['UDP received', 'TCP received', 'Syslog rate', 'Parse errors', 'Backpressure drops', 'Active TCP connections']) {
        await expect(ingestion.locator('dt').filter({ hasText: new RegExp(`^${label}$`) })).toHaveCount(0);
      }
    }
  });
}

for (const age of [null, 0]) {
  test(`last successful compaction age ${age} differs from never reported`, async ({ page, request }) => {
    await setup(request, 'health-admin', { dashboardSnapshot: { last_compaction_secs: age } });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    await fact(page.locator(SEL.healthStorage), 'Last successful cycle', age === null ? 'No successful cycle reported since startup' : '0s ago');
  });
}

for (const width of [900, 960]) {
  test(`retained WAL footer stays within its row at ${width}px`, async ({ page, request }) => {
    await setup(request, 'health-admin', { dashboardSnapshot: {
      wal_files: 1234, wal_bytes: 4_200_000_000,
      wal_measurement: { status: 'failed', sample_age_secs: 3600 },
    } });
    await page.setViewportSize({ width, height: 1000 });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    const wal = page.locator(SEL.healthFooterWal);
    await expect(wal).toContainText('failed; last 1234 / 4.2 GB; age 3600s');
    await expect(wal).toHaveAttribute('title', 'WAL failed; last 1234 / 4.2 GB; age 3600s');
    const footer = (await page.locator('.statusbar').boundingBox())!;
    const reading = (await wal.boundingBox())!;
    expect(reading.y).toBeGreaterThanOrEqual(footer.y);
    expect(reading.y + reading.height).toBeLessThanOrEqual(footer.y + footer.height);
    const theme = (await page.locator(SEL.themeControl).boundingBox())!;
    expect(theme.x).toBeGreaterThanOrEqual(0);
    expect(theme.x + theme.width).toBeLessThanOrEqual(width);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(width);
  });
}

for (const sample of [
  { status: 'not_configured', age: null, files: 0, bytes: 0, footer: 'not configured', expected: 'Not configured' },
  { status: 'not_sampled', age: null, files: 0, bytes: 0, footer: 'awaiting measurement', expected: 'Awaiting measurement' },
  { status: 'complete', age: 0, files: 0, bytes: 0, footer: '0 / 0 B; age 0s', expected: 'Complete measurement: 0 files' },
  { status: 'failed', age: null, files: 0, bytes: 0, footer: 'failed; unavailable', expected: 'Measurement unavailable; collection failed' },
  { status: 'failed', age: 123, files: 37, bytes: 913, footer: 'failed; last 37 / 913 B; age 123s', expected: 'Collection failed; last complete totals: 37 files' },
]) {
  test(`storage ${sample.status} with sample age ${sample.age} renders truthful totals`, async ({ page, request }) => {
    const metadata = { status: sample.status, sample_age_secs: sample.age };
    await setup(request, 'health-admin', { dashboardSnapshot: {
      wal_files: sample.files, wal_bytes: sample.bytes, wal_measurement: metadata,
      parquet_files: sample.files, parquet_bytes: sample.bytes, parquet_measurement: metadata,
    } });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    await expect(page.locator(SEL.healthFooterWal)).toHaveText(`WAL ${sample.footer}`);
    await expect(page.locator(SEL.healthFooterWal)).toHaveAttribute('title', `WAL ${sample.footer}`);
    await expect(page.locator(SEL.themeControl)).toBeVisible();
    for (const source of ['wal', 'parquet']) {
      const reading = page.locator(`${SEL.healthStorage} [data-source="${source}"] > p`).first();
      await expect(reading).toContainText(sample.expected);
      if (sample.age === null) {
        await expect(reading).not.toContainText('0 files');
        await expect(reading).not.toContainText('Sample age');
      } else {
        await expect(reading).toContainText(`Sample age: ${sample.age}s at this snapshot`);
      }
    }
    // A stream drop preserves the displayed sample, including a collection
    // failure. Neither a successful stream nor a reconnect rewrites its age.
    const before = await page.locator(`${SEL.healthStorage} .health-storage-source`).allTextContents();
    await request.post('/__ctl/dashboard/drop');
    await diagnosticPhase(page, 'Stale; reconnecting');
    await expect(page.locator(SEL.healthFooterWal)).toHaveCount(0);
    // Dwell beyond two seconds with the server holding snapshots. An immediate
    // successful poll cannot detect a client-side sample-age ticker.
    await page.waitForTimeout(2200);
    await diagnosticPhase(page, 'Stale; reconnecting');
    expect(await page.locator(`${SEL.healthStorage} .health-storage-source`).allTextContents()).toEqual(before);
    await request.post('/__ctl/dashboard/release');
    await diagnosticPhase(page, 'Live');
    expect(await page.locator(`${SEL.healthStorage} .health-storage-source`).allTextContents()).toEqual(before);
    await expect(page.locator(SEL.healthFooterWal)).toHaveText(`WAL ${sample.footer}`);
    if (sample.status === 'failed') {
      await request.post('/__ctl/dashboard/release', { data: { snapshot: {
        wal_files: 0, wal_bytes: 0, wal_measurement: { status: 'complete', sample_age_secs: 0 },
        parquet_files: 0, parquet_bytes: 0, parquet_measurement: { status: 'complete', sample_age_secs: 0 },
      } } });
      for (const source of ['wal', 'parquet']) {
        await expect(page.locator(`${SEL.healthStorage} [data-source="${source}"]`)).toContainText('Complete measurement: 0 files, 0 B. Sample age: 0s at this snapshot');
      }
      await expect(page.locator(SEL.healthFooterWal)).toHaveText('WAL 0 / 0 B; age 0s');
    }
  });
}

test('WAL failure and complete Parquet readings stay independent', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardSnapshot: {
    wal_files: 7, wal_bytes: 91, wal_measurement: { status: 'failed', sample_age_secs: 120 },
    parquet_files: 8, parquet_bytes: 72, parquet_measurement: { status: 'complete', sample_age_secs: 3 },
  } });
  await page.goto('/settings/health');
  await diagnosticPhase(page, 'Live');
  const wal = page.locator(`${SEL.healthStorage} [data-source="wal"] > p`).first();
  const parquet = page.locator(`${SEL.healthStorage} [data-source="parquet"] > p`).first();
  await expect(wal).toHaveText('Collection failed; last complete totals: 7 files, 91 B. Sample age: 120s at this snapshot.');
  await expect(parquet).toHaveText('Complete measurement: 8 files, 72 B. Sample age: 3s at this snapshot.');
  await expect(page.locator(SEL.healthFooterWal)).toHaveText('WAL failed; last 7 / 91 B; age 120s');
  await request.post('/__ctl/dashboard/drop');
  await diagnosticPhase(page, 'Stale; reconnecting');
  // The one existing Live operations status owns announcements; the adjacent
  // visible labels remain ordinary accessible text.
  for (const label of await page.locator(SEL.healthDiagnosticState).all()) {
    await expect(label).toBeVisible();
    await expect(label).not.toHaveAttribute('role', 'status');
  }
});

test('diagnostics wait without fabricated values before their first snapshot', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardHold: true, dashboardBootstrap: 'waiting' });
  await page.goto('/settings/health');
  await diagnosticPhase(page, 'Waiting for first snapshot');
  await expect(page.locator(SEL.healthDiagnostics).locator('dl, .health-storage-source')).toHaveCount(0);
  await request.post('/__ctl/dashboard/release');
  await diagnosticPhase(page, 'Live');
});

test('bootstrap totals remain visible after a terminal stream failure', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardTerminalHold: true });
  await observeDashboardErrors(page);
  await page.goto('/settings/health');
  await diagnosticPhase(page, 'Snapshot; waiting for live updates');
  await expect(page.locator(SEL.healthFooterWal)).toHaveCount(0);
  await expect(page.locator(SEL.healthStorage)).toContainText('7 files');
  await expect.poll(async () => (await state(request)).dashboard.terminalPending).toBe(1);
  await request.post('/__ctl/dashboard/terminal-release');
  await afterDashboardError(page, 2);
  await diagnosticPhase(page, 'Live updates failed');
  await expect(page.locator(SEL.healthStorage)).toContainText('Sample age: 2s at this snapshot');
  await expect(page.locator(SEL.healthStorage)).toContainText('Complete measurement: 7 files');
});

test('forbidden diagnostics have no retained readings', async ({ page, request }) => {
  await setup(request, 'health-admin', { dashboardBootstrap: 'forbidden', dashboardTerminalHold: true });
  await page.goto('/settings/health');
  await diagnosticPhase(page, 'Forbidden');
  await expect(page.locator(SEL.healthDiagnostics).locator('dl, .health-storage-source')).toHaveCount(0);
});

test('a late bootstrap from a prior identity cannot replace new diagnostic facts', async ({ page, request }) => {
  await setup(request, 'health-admin');
  let identityName = 'previous-admin';
  await page.route('**/api/auth/me', async route => {
    const response = await route.fetch();
    await route.fulfill({ response, json: { ...(await response.json()), name: identityName } });
  });
  let releaseOld!: () => void;
  const held = new Promise<void>(resolve => { releaseOld = resolve; });
  let oldSeen = false;
  let released = false;
  let first = true;
  await page.route('**/api/v1/dashboard', async route => {
    if (!first) { await route.continue(); return; }
    first = false;
    const response = await route.fetch();
    const body = await response.json();
    body.ingest_rejected = 888888;
    body.wal_files = 777777;
    oldSeen = true;
    await held;
    await route.fulfill({ response, json: body });
    released = true;
  });
  try {
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    await expect.poll(() => oldSeen).toBe(true);
    await page.evaluate(() => { (window as any).__identitySameDocument = true; });
    const navigateInApp = async (href: string) => page.evaluate(href => {
      const link = document.createElement('a');
      link.href = href;
      document.body.append(link);
      link.click();
      link.remove();
    }, href);
    // Router navigation drops the old shell without dropping the document or
    // its pending fetch. A full page reload would not exercise alive cleanup.
    await navigateInApp('/login');
    await expect(page).toHaveURL(/\/login$/);
    await expect.poll(async () => (await state(request)).dashboard.open).toBe(0);
    identityName = 'next-admin';
    await setup(request, 'health-admin', { dashboardSnapshot: { ingest_rejected: 909, wal_files: 303 } });
    await navigateInApp('/settings/health');
    await diagnosticPhase(page, 'Live');
    expect(await page.evaluate(() => (window as any).__identitySameDocument)).toBe(true);
    await fact(page.locator(SEL.healthIngestion), 'HTTP rejected events', '909');
    const lateResponse = page.waitForResponse(async response => response.url().endsWith('/api/v1/dashboard') && response.status() === 200 && (await response.json()).ingest_rejected === 888888);
    releaseOld();
    await lateResponse;
    await expect.poll(() => released).toBe(true);
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    await fact(page.locator(SEL.healthIngestion), 'HTTP rejected events', '909');
    await expect(page.locator(SEL.healthStorage)).toContainText('303 files');
    await expect(page.locator(SEL.healthDiagnostics)).not.toContainText('888888');
    await expect(page.locator(SEL.healthDiagnostics)).not.toContainText('777777');
    expect((await state(request)).dashboard.open).toBe(1);
  } finally {
    releaseOld();
  }
});
