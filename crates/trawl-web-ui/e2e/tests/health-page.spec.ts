// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import type { APIRequestContext, Page } from '@playwright/test';
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
