// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, SCHEDULE } from '../fixtures';
import { SEL } from '../selectors';

test('missing net and service links explain the missing target', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto('/jobs/nets?net=999999');
  await expect(page.getByText('Net not found.', { exact: false })).toBeVisible();
  await expect(page.getByRole('link', { name: 'Back to Nets', exact: true })).toBeVisible();
  await page.goto('/search/schema?svc=missing-service');
  await expect(page.getByText('Service not found.', { exact: false })).toBeVisible();
});

test('jobs refresh retains dirty drafts across remote edits, tabs and deletion', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  let revision = 0;
  let reads = 0;
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    reads++;
    if (revision === 2) body.queries = body.queries.filter((q: any) => q.id !== SCHEDULE.windowedNetId);
    if (revision === 1) {
      const net = body.queries.find((q: any) => q.id === SCHEDULE.windowedNetId);
      net.query = '* | limit 23';
      net.schedule.max_runs = 77;
    }
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await drawer.getByRole('button', { name: 'Edit', exact: true }).click();
  await drawer.getByRole('textbox', { name: 'Query', exact: true }).fill('* | limit 19');
  await drawer.locator(SEL.windowLagInput).fill('9m');
  revision = 1;
  const before = reads;
  await expect.poll(() => reads, { timeout: 8000 }).toBeGreaterThan(before);
  await expect(drawer.getByRole('textbox', { name: 'Query', exact: true })).toHaveValue('* | limit 19');
  await expect(drawer.locator(SEL.windowLagInput)).toHaveValue('9m');
  await expect(drawer.locator('#net-max-runs')).toHaveValue('77');
  await drawer.getByRole('tab', { name: 'Runs', exact: true }).click();
  await drawer.getByRole('tab', { name: 'Query + Schedule', exact: true }).click();
  await expect(drawer.getByRole('textbox', { name: 'Query', exact: true })).toHaveValue('* | limit 19');
  revision = 2;
  await expect(drawer.getByText('This net no longer exists.', { exact: false })).toBeVisible({ timeout: 8000 });
  await expect(drawer.getByRole('button', { name: 'Save', exact: true })).toBeDisabled();
  await expect(drawer.getByRole('textbox', { name: 'Query', exact: true })).toHaveValue('* | limit 19');
});

test('net run polling keeps expanded preview and its current page', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  const preview = page.locator(SEL.netRunPreview);
  await preview.getByRole('button', { name: 'Next' }).click();
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`21–40 of ${SCHEDULE.pagedRunRows}`);
  await page.waitForTimeout(5500);
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`21–40 of ${SCHEDULE.pagedRunRows}`);
});

test('global runs refresh new rows and status while retaining data on failures', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  let generation = 0;
  await page.route('**/api/v1/runs?*', async route => {
    if (generation === 2) return route.fulfill({ status: 503, body: 'unavailable' });
    const response = await route.fetch();
    const body = await response.json();
    if (generation === 1 && body.runs.length) {
      body.runs[0].status = 'running';
      body.runs.unshift({ ...body.runs[0], net_name: 'new-live-net', id: 999991 });
      body.total++;
    }
    await route.fulfill({ response, json: body });
  });
  await page.route('**/api/v1/runs/stats', async route => {
    const response = await route.fetch();
    const body = await response.json();
    if (generation > 0) { body.success_count = 3; body.total_runs = 3; }
    await route.fulfill({ response, json: body });
  });
  await page.goto('/jobs/runs');
  await expect(page.locator('.runs-table tbody tr')).not.toHaveCount(0);
  generation = 1;
  await expect(page.getByRole('link', { name: 'new-live-net' })).toBeVisible({ timeout: 8000 });
  await expect(page.locator('.stat-card').filter({ hasText: 'Success rate' }).locator('.value')).toHaveText('100%');
  await expect(page.locator('.results-summary')).toHaveText('1–3 of 3');
  generation = 2;
  await expect(page.getByText('Refresh failed.', { exact: false })).toBeVisible({ timeout: 8000 });
  await expect(page.getByRole('link', { name: 'new-live-net' })).toBeVisible();
});

test('hidden jobs pause polling, visible jobs refresh immediately, and cleanup ignores a late read', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.addInitScript(() => {
    const intervals = new Map<number, number>();
    (window as any).jobIntervals = intervals;
    const originalSet = window.setInterval.bind(window);
    const originalClear = window.clearInterval.bind(window);
    window.setInterval = ((handler: TimerHandler, ms?: number, ...args: any[]) => {
      const id = originalSet(handler, ms, ...args);
      intervals.set(id, ms ?? 0);
      return id;
    }) as typeof window.setInterval;
    window.clearInterval = (id?: number) => { intervals.delete(id!); originalClear(id); };
  });
  const timerCount = () => page.evaluate(() => [...(window as any).jobIntervals.values()].filter(ms => ms === 5000).length);
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  let reads = 0;
  let release: (() => void) | undefined;
  let hold = false;
  await page.route('**/api/v1/runs?*', async route => {
    reads++;
    const response = await route.fetch();
    if (hold) await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ response });
  });
  await page.goto('/jobs/runs');
  await expect.poll(() => reads).toBe(1);
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await page.waitForTimeout(5500);
  expect(reads).toBe(1);
  expect(await timerCount()).toBe(0);
  hold = true;
  await page.evaluate(() => {
    Object.defineProperty(document, 'hidden', { configurable: true, value: false });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect.poll(() => reads).toBe(2);
  // Runs polls its list and its stats; the nets feed left with the
  // retired "Active nets" card.
  expect(await timerCount()).toBe(2);
  await expect.poll(() => Boolean(release)).toBe(true);
  await page.getByRole('link', { name: 'Nets', exact: true }).click();
  release!();
  await page.waitForTimeout(5500);
  expect(reads).toBe(2);
  expect(await timerCount()).toBe(1);
  expect(errors).toEqual([]);
});

test('relative run ages advance on the shared clock without waiting for a response', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const start = new Date('2026-09-13T12:00:00Z');
  await page.clock.install({ time: start });
  let reads = 0;
  await page.route('**/api/v1/runs?*', async route => {
    reads++;
    const response = await route.fetch();
    const body = await response.json();
    body.runs[0].started_at = new Date(start.getTime() - 59_000).toISOString();
    await route.fulfill({ response, json: body });
  });
  await page.goto('/jobs/runs');
  const when = page.locator('.runs-table tbody tr').first().locator('td').nth(2);
  await expect(when).toHaveText('just now');
  await page.route('**/api/v1/runs?*', route => route.fulfill({ status: 503, body: 'offline' }));
  await page.clock.fastForward(30_000);
  await expect(when).toHaveText('1m ago');
  expect(reads).toBe(1);
});

test('reordering keeps the expanded run when it moves outside the current page', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  let moved = false;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    if (moved) body.runs = body.runs.slice(1).reverse();
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  const preview = page.locator(SEL.netRunPreview);
  await preview.getByRole('button', { name: 'Next' }).click();
  moved = true;
  await expect(page.getByText('Expanded run outside this page')).toBeVisible({ timeout: 8000 });
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`21–40 of ${SCHEDULE.pagedRunRows}`);
});

test('a local mutation supersedes an in-flight read without overlapping requests', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  let reads = 0;
  let release: (() => void) | undefined;
  let saved: any;
  let changed = false;
  await page.route('**/api/v1/saved', async route => {
    const response = await route.fetch();
    const body = await response.json();
    reads++;
    const net = body.queries.find((q: any) => q.id === SCHEDULE.windowedNetId);
    saved = { ...net };
    if (reads === 2) {
      net.name = 'obsolete-read';
      await new Promise<void>(resolve => { release = resolve; });
    } else if (changed) net.name = 'current-read';
    await route.fulfill({ response, json: body });
  });
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}`, async route => {
    changed = true;
    await route.fulfill({ json: { ...saved, query: '* | limit 19', name: 'current-read' } });
  });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  await expect.poll(() => Boolean(release), { timeout: 8000 }).toBe(true);
  await page.waitForTimeout(5100);
  expect(reads).toBe(2);
  const drawer = page.locator(SEL.drawerPanel);
  await drawer.getByRole('button', { name: 'Edit', exact: true }).click();
  await drawer.getByRole('textbox', { name: 'Query', exact: true }).fill('* | limit 19');
  await drawer.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(drawer.getByRole('textbox', { name: 'Query', exact: true })).toHaveCount(0);
  await page.evaluate(() => {
    (window as any).sawObsolete = false;
    new MutationObserver(() => {
      if (document.body.textContent?.includes('obsolete-read')) (window as any).sawObsolete = true;
    }).observe(document.body, { subtree: true, childList: true, characterData: true });
  });
  release!();
  await expect(drawer.locator('button.name')).toHaveText('current-read');
  expect(await page.evaluate(() => (window as any).sawObsolete)).toBe(false);
});

test('polling preserves a non-first runs page and its filter', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'pagination', pagination: { runsTotal: 43 } } });
  const offsets: string[] = [];
  await page.route('**/api/v1/runs?*', async route => {
    offsets.push(new URL(route.request().url()).searchParams.get('offset')!);
    await route.continue();
  });
  await page.goto('/jobs/runs');
  await page.getByRole('button', { name: 'Next' }).click();
  await expect(page.locator('.results-summary')).toHaveText('21–40 of 43');
  await page.getByPlaceholder('Filter by net…').fill('absent-net');
  await expect.poll(() => offsets.filter(x => x === '20').length, { timeout: 8000 }).toBe(2);
  await expect(page.getByPlaceholder('Filter by net…')).toHaveValue('absent-net');
  await expect(page.locator('.results-summary')).toHaveText('21–40 of 43 · 0 matches on this page');
  expect(offsets.at(-1)).toBe('20');
});

test('a remote rename does not replace an unfinished name draft', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  let changed = false;
  let reads = 0;
  await page.route('**/api/v1/saved', async route => {
    const response = await route.fetch();
    const body = await response.json();
    reads++;
    if (changed) body.queries.find((q: any) => q.id === SCHEDULE.windowedNetId).name = 'remote-name';
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await drawer.locator('button.name').click();
  await drawer.locator('input.name-edit').fill('unfinished-name');
  changed = true;
  await expect.poll(() => reads, { timeout: 8000 }).toBe(2);
  await expect(drawer.locator('input.name-edit')).toHaveValue('unfinished-name');
  await page.keyboard.press('Escape');
  await expect(drawer.locator('button.name')).toHaveText('remote-name');
});

for (const target of ['net', 'service']) {
  test(`${target} deep link distinguishes pending and failed lookup from not found`, async ({ page, request }) => {
    await resetScenario(request, 'schedule');
    let release: (() => void) | undefined;
    await page.route(target === 'net' ? '**/api/v1/saved' : '**/api/v1/schema/services', async route => {
      await new Promise<void>(resolve => { release = resolve; });
      await route.fulfill({ status: 503, body: 'unavailable' });
    });
    await page.goto(target === 'net' ? '/jobs/nets?net=999999' : '/search/schema?svc=absent');
    await expect(page.getByText(`Loading ${target}…`, { exact: false })).toBeVisible();
    await expect.poll(() => Boolean(release)).toBe(true);
    release!();
    await expect(page.getByText(`Could not load this ${target}. Please retry.`, { exact: false })).toBeVisible();
    await expect(page.getByText(/not found\. It may have been deleted/)).toHaveCount(0);
  });
}
