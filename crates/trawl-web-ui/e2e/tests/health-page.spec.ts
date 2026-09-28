// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import type { APIRequestContext, Locator, Page, TestInfo } from '@playwright/test';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
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

// Ingestion, Storage, and Disk and retention each carry the phase label.
async function diagnosticPhase(page: Page, label: string) {
  await expect(page.locator(SEL.healthDiagnosticState)).toHaveText([label, label, label]);
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

test('the connected label names no version until health returns one', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let intercepted!: () => void;
  const interception = new Promise<void>(resolve => { intercepted = resolve; });
  await page.route('**/api/v1/health', async route => {
    // Fetch first so the harness counter still sees the request.
    const response = await route.fetch();
    intercepted();
    await held;
    await route.fulfill({ response });
  });
  try {
    await page.goto('/search');
    await interception;
    const host = new URL(page.url()).host;
    const label = page.locator(SEL.statusLabel);
    await expect(label).toHaveText(`Connected (${host})`);
    release();
    await expect(label).toHaveText(`Connected (${host} vhealth-fixture-163)`);
    expect((await state(request)).healthHits.health).toBe(1);
  } finally {
    release();
  }
});

test('the connected label keeps the host alone when health fails', async ({ page, request }) => {
  await setup(request, 'health-viewer');
  let failed = false;
  // 502, not 503: a 503 body parses as a health report.
  await page.route('**/api/v1/health', async route => {
    await route.fulfill({ status: 502, json: { error: 'upstream unavailable' } });
    failed = true;
  });
  await page.goto('/search');
  await expect.poll(() => failed).toBe(true);
  const host = new URL(page.url()).host;
  const label = page.locator(SEL.statusLabel);
  // The label reads the same while pending, so wait for the settled failure.
  await expect(label).toHaveAttribute('data-health', 'error');
  await expect(label).toHaveText(`Connected (${host})`);
});

test('ingest refusal shows an amber chip and keeps the session connected', async ({ page, request }) => {
  await setup(request, 'health-ingest-refusing');
  await page.goto('/search');
  const host = new URL(page.url()).host;
  const chip = page.locator(SEL.statusIngestRefusing);
  await expect(chip).toBeVisible();
  await expect(chip).toHaveText('Ingest refusing');
  // Back-pressure is not an outage: the label keeps its connected
  // semantics, and nothing signs the session out or reports it broken.
  const label = page.locator(SEL.statusLabel);
  await expect(label).toHaveText(`Connected (${host} vhealth-fixture-163)`);
  await expect(label).toHaveAttribute('data-health', 'ok');
  await expect(page.locator('.auth-notice')).toHaveCount(0);
  await expect(page).toHaveURL(/\/search/);

  await page.goto('/settings/health');
  const health = page.locator(SEL.healthSection);
  await expect(health.getByRole('heading')).toHaveText('Server is degraded');
  const capacity = health.locator(SEL.healthCheck).filter({ hasText: 'Ingest capacity' });
  await expect(capacity.locator('.health-check-key')).toHaveText('ingest_capacity');
  const badge = capacity.locator('dd .bdg');
  await expect(badge).toHaveText('Refusing');
  await expect(badge).toHaveClass(/\bwarn\b/);
  // A warning, not a failure: the row is not painted as an error.
  await expect(capacity.locator('dd')).not.toHaveClass(/health-check-error/);
  await expect(
    health.getByText('Overall state', { exact: true }).locator('xpath=ancestor::div[1]').locator('dd'),
  ).toHaveText('Degraded');
  await expect(chip).toBeVisible();
  await expect(page.locator(SEL.statusLabel)).toHaveText(`Connected (${host} vhealth-fixture-163)`);
});

test('the footer re-reads health every 30 s, one read at a time, and the chip clears', async ({ page, request }) => {
  await setup(request, 'health-ingest-refusing');
  await page.clock.install();
  let reads = 0;
  let inFlight = 0;
  let maxInFlight = 0;
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  await page.route('**/api/v1/health', async route => {
    const read = ++reads;
    inFlight += 1;
    maxInFlight = Math.max(maxInFlight, inFlight);
    try {
      const response = await route.fetch();
      if (read === 1) {
        await route.fulfill({ response });
        return;
      }
      // Later reads: compaction freed space, so admission reopened.
      if (read === 2) await held;
      const body = await response.json();
      body.status = 'ok';
      body.checks.ingest_capacity = 'ok';
      await route.fulfill({ response, json: body });
    } finally {
      inFlight -= 1;
    }
  });
  try {
    await page.goto('/search');
    const chip = page.locator(SEL.statusIngestRefusing);
    await expect(chip).toBeVisible();
    expect(reads).toBe(1);

    // The next read starts 30 s after the first settled, and is held.
    await page.clock.fastForward(30_000);
    await expect.poll(() => reads).toBe(2);
    // While it is held, no timer may start a second request beside it.
    await page.clock.fastForward(90_000);
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    expect(reads).toBe(2);
    await expect(chip).toBeVisible();

    release();
    await expect(chip).toHaveCount(0);
    await expect(page.locator(SEL.statusLabel)).toHaveAttribute('data-health', 'ok');

    await page.clock.fastForward(30_000);
    await expect.poll(() => reads).toBe(3);
    await expect(chip).toHaveCount(0);
    expect(maxInFlight).toBe(1);
  } finally {
    release();
  }
});

// The Disk and retention card (ADR-0042). Each case loads one capacity
// fixture over the base snapshot: a slice of the snapshot holding the
// capacity object with the Parquet and WAL fields it was assembled beside.
// The harness merges `dashboardSnapshot` shallowly, so each field rides
// along whole. Each fixture is producer output: trawl-server's
// src/metrics/capacity_fixtures.rs fails when one differs from what
// capacity::assemble emits, and tests/e2e_wire_fixture_contract.rs decodes
// each one and pins it to the state its case asserts.
const capacity = (name: string) => JSON.parse(readFileSync(`${__dirname}/../harness/wire/health-capacity-${name}.json`, 'utf8'));

test.describe('Disk and retention', () => {
  const OBSERVED = 'Observed: 7 days, 2026-09-19 to 2026-09-25.';
  const LAB_OBSERVED = 'Observed: 6 days, 2026-09-20 to 2026-09-25.';
  const WITHHELD = {
    history: 'Reach withheld: not enough observed days yet.',
    repin: 'Reach withheld: a repin ran during the latest samples, so they may count files twice or miss free space.',
    measurement: 'Reach withheld: measurement unavailable; the disk or Parquet sample is not complete.',
  };

  async function open(page: Page, request: APIRequestContext, fixture: string) {
    await setup(request, 'health-admin', { dashboardSnapshot: capacity(fixture) });
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.goto('/settings/health');
    await diagnosticPhase(page, 'Live');
    const disk = page.locator(SEL.healthDisk);
    await expect(disk.getByRole('heading', { name: 'Disk and retention', exact: true })).toBeVisible();
    return disk;
  }
  const group = (disk: Locator, name: string) => disk.locator(`[data-group="${name}"]`);
  const env = (disk: Locator, name: string) => disk.locator(`[data-env="${name}"]`);
  const reach = (disk: Locator, name: string) => env(disk, name).locator('.health-disk-reach');

  // A withheld reach names its reason and carries no digit at all.
  async function withheld(disk: Locator, names: string[], text: string) {
    await expect(disk.locator('.health-disk-reach')).toHaveCount(names.length);
    for (const name of names) {
      await expect(reach(disk, name)).toHaveText(text);
      expect(await reach(disk, name).textContent()).not.toMatch(/\d/);
    }
  }

  // No verdict: no incident role or tone class, no reassurance word, and
  // every reach sentence drawn in the same ink as the Storage card's
  // reading, whether it projects, withholds, or names a filling disk.
  async function noVerdict(page: Page, disk: Locator) {
    await expect(disk.locator('[role="alert"], [role="status"], .badge, .health-check-error, [class*="danger"], [class*="warn"], [class*="success"], [class*="ok"]')).toHaveCount(0);
    await expect(disk).not.toContainText(/\b(safe|healthy|ok|fine|good)\b/i);
    const reading = await page.locator(`${SEL.healthStorage} [data-source="parquet"] > p`).first().evaluate(el => getComputedStyle(el).color);
    const colors = await disk.locator('.health-disk-reach, .health-disk-list dd > p, .health-disk-group > p:not(.health-note)').evaluateAll(els => [...new Set(els.map(el => getComputedStyle(el).color))]);
    expect(colors).toEqual([reading]);
  }

  // A capture of the card alone, with a sidecar naming what it claims.
  // TRAWL_HEALTH_CAPTURE_DIR collects them for visual-evidence/. Health
  // owns its scroll container, so an element screenshot cannot stitch a
  // card taller than the viewport: scroll the card to the top of that
  // scroller, grow the viewport until the whole card is on screen, and
  // clip a viewport capture to the card's box. The width is untouched.
  async function capture(page: Page, testInfo: TestInfo, disk: Locator, name: string, claim: string) {
    const width = page.viewportSize()!.width;
    const settle = () => page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    await disk.evaluate(el => el.scrollIntoView({ block: 'start' }));
    await settle();
    const scroller = page.locator(SEL.healthPage);
    let box = (await disk.boundingBox())!;
    let floor = (await scroller.boundingBox())!;
    // The card must sit inside the scroller's own rect: the shell's footer
    // lies below it. A taller viewport leaves the scroller less to scroll,
    // so the card settles lower each round; grow until the box fits.
    for (let round = 0; round < 4 && box.y + box.height > floor.y + floor.height; round++) {
      const overshoot = box.y + box.height - (floor.y + floor.height);
      await page.setViewportSize({ width, height: Math.ceil(page.viewportSize()!.height + overshoot + 24) });
      await disk.evaluate(el => el.scrollIntoView({ block: 'start' }));
      await settle();
      box = (await disk.boundingBox())!;
      floor = (await scroller.boundingBox())!;
    }
    expect(box.y + box.height).toBeLessThanOrEqual(floor.y + floor.height);
    const file = process.env.TRAWL_HEALTH_CAPTURE_DIR
      ? path.join(process.env.TRAWL_HEALTH_CAPTURE_DIR, name)
      : testInfo.outputPath(name);
    await mkdir(path.dirname(file), { recursive: true });
    await page.screenshot({ path: file, clip: box, animations: 'disabled' });
    await testInfo.attach(name, { path: file, contentType: 'image/png' });
    const bytes = readFileSync(file);
    await writeFile(file.replace(/\.png$/, '.json'), JSON.stringify({
      file: name, claim, viewport: page.viewportSize(), sha256: createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length,
    }, null, 2));
  }

  test('a complete sample shows headroom rows and each projected form', async ({ page, request }, testInfo) => {
    const disk = await open(page, request, 'complete');
    const headroom = group(disk, 'headroom');
    await expect(headroom.locator('dl > div')).toHaveCount(2);
    const data = headroom.locator('[data-roles="data wal"]');
    await expect(data.locator('dt')).toHaveText('Data + WAL');
    await expect(data).toContainText('Complete measurement: 26.0 GB available of 250.0 GB. Sample age: 2s at this snapshot.');
    await expect(data).toContainText('Deletion floor: 1.1 GB.');
    await expect(data).not.toContainText('Deficit');
    const spill = headroom.locator('[data-roles="spill"]');
    await expect(spill.locator('dt')).toHaveText('Spill');
    await expect(spill).toContainText('Complete measurement: 64.4 GB available of 68.7 GB. Sample age: 2s at this snapshot.');
    await expect(spill).not.toContainText('floor');
    await expect(headroom).toContainText('never added across filesystems');
    const pressure = group(disk, 'pressure');
    await fact(pressure, 'Removed by age', '14');
    await fact(pressure, 'Removed by pressure', '0');
    await fact(pressure, 'Pressure attempts', '0');
    await fact(pressure, 'Last sweep', 'Completed, 754s ago');
    await expect(pressure).toContainText('since process start');
    await expect(group(disk, 'reach').locator('dl > div')).toHaveCount(5);
    // One fraction of each policy per end: every projected env reads as
    // whole days at both ends.
    await expect(env(disk, 'prod')).toContainText('Policy: 90 days. Oldest date: 2026-08-15. Stored: 111.1 GB.');
    await expect(reach(disk, 'prod')).toHaveText(`Reach: about 38–52 of 90 days if the observed days repeat. ${OBSERVED}`);
    await expect(reach(disk, 'staging')).toHaveText(`Reach: about 12–17 of 30 days if the observed days repeat. ${OBSERVED}`);
    await expect(env(disk, 'lab')).toContainText('Policy: 7 days. Oldest date: 2026-09-20. Stored: 480 MB.');
    await expect(reach(disk, 'lab')).toHaveText(`Reach: about 3–4 of 7 days if the observed days repeat. ${LAB_OBSERVED}`);
    await expect(env(disk, 'archive')).toContainText('Policy: keep forever. Oldest date: 2026-03-14. Stored: 20.0 GB.');
    await expect(reach(disk, 'archive')).toHaveText(`Mean growth: 120 MB per day. ${OBSERVED}`);
    await expect(env(disk, 'k8s')).toContainText('Policy: 14 days. Oldest date: 2026-09-24. Stored: 36 MB.');
    await expect(reach(disk, 'k8s')).toHaveText(WITHHELD.history);
    await expect(disk.locator('.health-disk-excluded')).toHaveText('Excludes growth of k8s: not enough history yet.');
    await expect(disk).toContainText('holds only if those days repeat');
    await expect(disk).not.toContainText(/guarantee/i);
    await noVerdict(page, disk);
    await capture(page, testInfo, disk, 'disk-retention-complete-1440.png',
      'Complete sample with a floor: two headroom rows and no deficit, a completed sweep, three envs projected in whole days from one shared fraction per end, keep-forever growth, and one env withheld for history with the growth-excluded note');
    // The card reads on a phone width without a horizontal scroll.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(reach(disk, 'prod')).toBeVisible();
    // The page's width settles a frame or more after the resize (under load
    // it read 190px of the 390px viewport), so poll until it has.
    await expect.poll(() => page.locator(SEL.healthPage).evaluate(el => el.scrollWidth <= el.clientWidth)).toBe(true);
    // Every ISO date is one unbreakable run: it never wraps at its hyphen.
    const isoDates = (await disk.textContent())!.match(/\d{4}-\d{2}-\d{2}/g) ?? [];
    expect(isoDates.length).toBeGreaterThan(0);
    const dates = disk.locator('.health-disk-date');
    await expect(dates).toHaveCount(isoDates.length);
    expect(await dates.allTextContents()).toEqual(isoDates);
    expect(await dates.evaluateAll(els => els.map(el => el.getClientRects().length))).toEqual(isoDates.map(() => 1));
    await capture(page, testInfo, disk, 'disk-retention-complete-390.png', 'The same complete sample at a 390px phone width: every sentence wraps inside the card');
  });

  test('a failed attempt keeps its aged rows and withholds every reach without a number', async ({ page, request }, testInfo) => {
    const disk = await open(page, request, 'failed-retained');
    const headroom = group(disk, 'headroom');
    await expect(headroom.locator('dl > div')).toHaveCount(2);
    const data = headroom.locator('[data-roles="data spill"]');
    await expect(data.locator('dt')).toHaveText('Data + Spill');
    await expect(data).toContainText('Collection failed; last complete reading: 800 MB available of 500.0 GB. Sample age: 3600s at this snapshot.');
    await expect(data).toContainText('Deletion floor: 1.1 GB. Deficit: 274 MB below the floor.');
    await expect(headroom.locator('[data-roles="wal"]')).toContainText('Collection failed; last complete reading: 60.0 GB available of 64.0 GB. Sample age: 3600s at this snapshot.');
    await fact(group(disk, 'pressure'), 'Last sweep', 'Failed, 41s ago');
    // Global: the rate-less k8s is withheld for the measurement too, and
    // nothing is excluded from a projection that did not run.
    await withheld(disk, ['archive', 'edge', 'k8s', 'prod'], WITHHELD.measurement);
    // A partition dated after today, from a fast agent clock, is still the
    // env's oldest date on disk.
    await expect(env(disk, 'edge')).toContainText('Policy: 90 days. Oldest date: 2026-09-28. Stored: 3 MB.');
    await expect(disk.locator('.health-disk-excluded')).toHaveCount(0);
    await noVerdict(page, disk);
    await capture(page, testInfo, disk, 'disk-retention-failed-retained-1440.png',
      'Failed headroom attempt with a retained sample: rows read "Collection failed; last complete reading" with a 3600s age and a deficit, and every reach, a future-dated env\'s included, is withheld as measurement unavailable with no digit and no growth-excluded note');
  });

  test('a repin in flight withholds every reach as suppressed without a number', async ({ page, request }, testInfo) => {
    const disk = await open(page, request, 'repin-suppressed');
    const data = group(disk, 'headroom').locator('[data-roles="data wal spill"]');
    await expect(data).toContainText('Complete measurement: 90.0 GB available of 250.0 GB. Sample age: 3s at this snapshot.');
    await expect(data).toContainText('Deletion floor: 1.1 GB.');
    await fact(group(disk, 'pressure'), 'Last sweep', 'Suppressed, 300s ago');
    await withheld(disk, ['k8s', 'prod', 'staging'], WITHHELD.repin);
    await expect(disk.locator('.health-disk-excluded')).toHaveCount(0);
    await noVerdict(page, disk);
    await capture(page, testInfo, disk, 'disk-retention-repin-suppressed-1440.png',
      'A repin in flight with both samples complete: the last sweep was suppressed, and every reach is withheld as retention suppressed with no digit and no growth-excluded note');
  });

  test('a floor of 0 reads as off, and the disk fills first at the largest observed day', async ({ page, request }, testInfo) => {
    const disk = await open(page, request, 'floor-zero');
    const data = group(disk, 'headroom').locator('[data-roles="data wal spill"]');
    await expect(data.locator('dt')).toHaveText('Data + WAL + Spill');
    await expect(data).toContainText('Pressure deletion off (floor 0).');
    await expect(data).not.toContainText('Deletion floor');
    const pressure = group(disk, 'pressure');
    await fact(pressure, 'Removed by pressure', '0');
    await fact(pressure, 'Pressure attempts', '0');
    await fact(pressure, 'Last sweep', 'Completed, 120s ago');
    const fills = 'Reach if the observed days repeat: at the largest observed day, the disk fills before retention is reached;';
    await expect(reach(disk, 'prod')).toHaveText(`${fills} at the mean day, the full 90 days. ${OBSERVED}`);
    await expect(reach(disk, 'lab')).toHaveText(`${fills} at the mean day, the full 7 days. ${LAB_OBSERVED}`);
    await expect(env(disk, 'fresh')).toContainText('Policy: 30 days. Oldest date: 2026-09-25. Stored: 12 MB.');
    await expect(reach(disk, 'fresh')).toHaveText(WITHHELD.history);
    expect(await reach(disk, 'fresh').textContent()).not.toMatch(/\d/);
    await expect(disk.locator('.health-disk-excluded')).toHaveText('Excludes growth of fresh: not enough history yet.');
    await noVerdict(page, disk);
    await capture(page, testInfo, disk, 'disk-retention-floor-zero-1440.png',
      'Floor of 0 reads as pressure deletion off with no pressure attempts; both projected envs read the disk filling first at the largest observed day and the full policy at the mean day, and a fresh env is withheld for history with the growth-excluded note');
  });

  test('pressure evidence shows the counts, the deficit, and an exhausted sweep', async ({ page, request }, testInfo) => {
    const disk = await open(page, request, 'pressure');
    const data = group(disk, 'headroom').locator('[data-roles="data wal spill"]');
    await expect(data).toContainText('Complete measurement: 600 MB available of 100.0 GB. Sample age: 1s at this snapshot.');
    await expect(data).toContainText('Deletion floor: 1.1 GB. Deficit: 474 MB below the floor.');
    const pressure = group(disk, 'pressure');
    await fact(pressure, 'Removed by age', '30');
    await fact(pressure, 'Removed by pressure', '12');
    await fact(pressure, 'Pressure attempts', '5');
    await fact(pressure, 'Last sweep', 'Ran out of candidates below the floor, 61s ago');
    await expect(pressure).toContainText('Counts are since process start.');
    // Evidence is counts and an outcome: no bytes figure anywhere in it.
    await expect(pressure).not.toContainText(/\d(\.\d+)? [KMG]?B\b/);
    // The exhausted sweep left each env today's partition alone, so no
    // env has a rate and every finite one is left out of the growth.
    await expect(env(disk, 'prod')).toContainText('Policy: 90 days. Oldest date: 2026-09-27. Stored: 4.1 GB.');
    await withheld(disk, ['archive', 'lab', 'prod'], WITHHELD.history);
    await expect(disk.locator('.health-disk-excluded')).toHaveText('Excludes growth of lab, prod: not enough history yet.');
    await noVerdict(page, disk);
    await capture(page, testInfo, disk, 'disk-retention-pressure-1440.png',
      'Under pressure: one row for all three roles with a 474 MB deficit, 12 pressure removals over 5 attempts, and a sweep that ran out of candidates below the floor; each env holds today alone, so every reach is withheld for history and the finite envs are excluded');
  });

  // An empty environment list is measured only when a complete Parquet
  // scan established it (ADR-0033). Before the first scan, or after one
  // that failed with nothing retained, the reach group says so in the
  // Storage card's own words, and never "No stored date partitions".
  for (const { fixture, line, headroom, claim } of [
    {
      fixture: 'awaiting',
      line: 'Awaiting measurement',
      headroom: 'Awaiting measurement',
      claim: 'Before the first measurement: headroom and reach both read "Awaiting measurement", no sweep yet, and the empty environment list is not presented as measured',
    },
    {
      fixture: 'scan-failed',
      line: 'Measurement unavailable; collection failed',
      headroom: 'Complete measurement: 180.0 GB available of 250.0 GB. Sample age: 4s at this snapshot.',
      claim: 'A Parquet scan that failed with nothing retained: headroom still reads its complete row, and reach reads "Measurement unavailable; collection failed" instead of a measured empty list',
    },
  ]) {
    test(`an empty list without a complete scan reads as ${fixture}, not as measured`, async ({ page, request }, testInfo) => {
      const disk = await open(page, request, fixture);
      const parquet = page.locator(`${SEL.healthStorage} [data-source="parquet"] > p`).first();
      await expect(parquet).toHaveText(line);
      const reachGroup = group(disk, 'reach');
      await expect(reachGroup.locator('dl')).toHaveCount(0);
      await expect(reachGroup.locator('h3 + p')).toHaveText(line);
      await expect(disk).not.toContainText('No stored date partitions');
      await expect(disk.locator('.health-disk-excluded')).toHaveCount(0);
      await expect(group(disk, 'headroom')).toContainText(headroom);
      await fact(group(disk, 'pressure'), 'Last sweep', 'No sweep yet since process start');
      await noVerdict(page, disk);
      await capture(page, testInfo, disk, `disk-retention-${fixture}-1440.png`, claim);
    });
  }

  test('the card is admin-gated with the rest of the diagnostics', async ({ page, request }) => {
    await setup(request, 'health-viewer', { dashboardSnapshot: capacity('pressure') });
    await page.goto('/settings/health');
    await rows(page);
    await expect(page.locator(SEL.healthDisk)).toHaveCount(0);
    await expect(page.locator(SEL.healthDiagnostics)).toHaveCount(0);
  });
});
