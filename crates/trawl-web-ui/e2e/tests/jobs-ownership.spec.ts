// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, SCHEDULE } from '../fixtures';
import { SEL } from '../selectors';
import { readFile } from 'node:fs/promises';

test('Nets keeps its open menu and focused item through polling and a clock tick', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let reads = 0;
  await page.route('**/api/v1/saved', async route => {
    const response = await route.fetch();
    const body = await response.json();
    reads++;
    // The list shows names, cadence and last run, not query text, so the
    // live cell under observation is the name link.
    body.queries[0].name = `polled-net-${reads}`;
    await route.fulfill({ response, json: body });
  });
  await page.goto('/jobs/nets');
  await page.locator(SEL.actionsMenuTrigger).first().click();
  const item = page.locator(SEL.actionsMenuItem).first();
  await item.focus();
  const mounted = await item.elementHandle();
  const before = reads;
  const polled = page.locator('.nets-table .row-stretch', { hasText: /^polled-net-/ });
  await page.clock.fastForward(5_000);
  await expect.poll(() => reads).toBeGreaterThan(before);
  await expect(polled).toHaveText('polled-net-2');
  await expect(item).toBeFocused();
  await page.clock.fastForward(25_000);
  await expect(polled).toHaveText('polled-net-3');
  await expect(item).toBeFocused();
  expect(await mounted!.evaluate(el => el.isConnected)).toBe(true);
});

test('global run links keep focus while their live cells update', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let reads = 0;
  await page.route('**/api/v1/runs?*', async route => {
    const response = await route.fetch();
    const body = await response.json();
    reads++;
    if (reads > 1) body.runs[0].status = 'running';
    await route.fulfill({ response, json: body });
  });
  await page.goto('/jobs/runs');
  const link = page.locator('.runs-table .row-stretch').first();
  await link.focus();
  const mounted = await link.elementHandle();
  await page.clock.fastForward(30_000);
  await expect(page.locator('.runs-table tbody tr').first()).toContainText('Running');
  await expect(link).toBeFocused();
  expect(await mounted!.evaluate(el => el.isConnected)).toBe(true);
});

for (const surface of ['global', 'drawer']) {
  test(`${surface} pager stays focused and accepts a page change during a background read`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'pagination', pagination: { runsTotal: 43 } } });
    let reads = 0;
    let release: (() => void) | undefined;
    const offsets: string[] = [];
    const endpoint = surface === 'global' ? '**/api/v1/runs?*' : '**/api/v1/saved/1/runs?*';
    await page.route(endpoint, async route => {
      reads++;
      offsets.push(new URL(route.request().url()).searchParams.get('offset')!);
      const response = await route.fetch();
      if (reads === 2) await new Promise<void>(resolve => { release = resolve; });
      await route.fulfill({ response });
    });
    await page.goto(surface === 'global' ? '/jobs/runs' : '/jobs/nets?net=1&ntab=runs');
    const root = surface === 'global' ? page.locator('.page') : page.locator(SEL.drawerPanel);
    const footer = root.locator('.results-footer');
    await expect(footer.locator('.results-summary')).toHaveText('1–20 of 43');
    const next = footer.getByRole('button', { name: 'Next' });
    await next.focus();
    const mounted = await next.elementHandle();
    await expect.poll(() => Boolean(release), { timeout: 8000 }).toBe(true);
    await expect(footer.locator('.results-summary')).toHaveText('1–20 of 43');
    await expect(next).toBeEnabled();
    await expect(next).toBeFocused();
    expect(await mounted!.evaluate(el => el.isConnected)).toBe(true);
    await next.click();
    await expect(next).toBeDisabled();
    expect(reads).toBe(2);
    if (surface === 'global') await expect(footer.locator('.results-summary')).toHaveText('Loading…');
    release!();
    await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
    expect(offsets).toEqual(['0', '0', '20']);
  });
}

test('inactive Runs retains its preview but performs no reads, and terminal results stay cached', async ({ page, request }) => {
  test.setTimeout(30000);
  await resetScenario(request, 'schedule');
  let lists = 0, details = 0;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`, async route => { lists++; await route.continue(); });
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs/${SCHEDULE.pagedRunId}`, async route => { details++; await route.continue(); });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await expect(drawer).toBeVisible();
  await page.waitForTimeout(5200);
  expect(lists).toBe(0);
  await drawer.getByRole('tab', { name: 'Runs', exact: true }).click();
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  const preview = page.locator(SEL.netRunPreview);
  await preview.getByRole('button', { name: 'Next' }).click();
  await expect(preview.locator(SEL.resultsSummary)).toHaveText('21–40 of 45');
  await drawer.getByRole('tab', { name: 'Query + Schedule', exact: true }).click();
  const before = lists;
  await page.waitForTimeout(5200);
  expect(lists).toBe(before);
  expect(details).toBe(1);
  await drawer.getByRole('tab', { name: 'Runs', exact: true }).click();
  await expect.poll(() => lists).toBe(before + 1);
  await expect(preview.locator(SEL.resultsSummary)).toHaveText('21–40 of 45');
  await page.waitForTimeout(5200);
  expect(lists).toBeGreaterThan(before + 1);
  expect(details).toBe(1);
});

test('an expanded running result learns completion off-page and then stops downloading', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  let lists = 0, details = 0;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    lists++;
    if (lists === 1) body.runs[0].status = 'running';
    else body.runs = body.runs.slice(1);
    await route.fulfill({ response, json: body });
  });
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs/${SCHEDULE.pagedRunId}`, async route => {
    const response = await route.fetch();
    const body = await response.json();
    details++;
    if (details === 1) { body.status = 'running'; body.result = null; }
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  await expect(page.getByText('No result data (error or still running)', { exact: true })).toBeVisible();
  await expect(page.getByText('Expanded run outside this page')).toBeVisible({ timeout: 8000 });
  await expect(page.locator(SEL.netRunPreview).locator(SEL.resultsSummary)).toHaveText('1–20 of 45');
  expect(details).toBe(2);
  await page.waitForTimeout(5500);
  expect(details).toBe(2);
});

for (const path of ['/jobs/nets', '/jobs/runs']) {
  test(`${path} sends an expired session to Login`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    const endpoint = path === '/jobs/nets' ? '**/api/v1/saved' : '**/api/v1/runs?*';
    await page.route(endpoint, route => route.fulfill({ status: 401, json: { error: 'Unauthorized' } }));
    await page.goto(path);
    await expect(page).toHaveURL(/\/login$/);
    await expect(page.getByText('Retrying automatically.', { exact: false })).toHaveCount(0);
  });
}

for (const surface of ['nets', 'runs', 'drawer']) {
  test(`${surface} retains focus when a live refresh moves the focused row`, async ({ page, request }) => {
    await resetScenario(request, surface === 'drawer' ? 'schedule' : 'corpus');
    await page.clock.install();
    let reads = 0;
    const endpoint = surface === 'nets' ? '**/api/v1/saved'
      : surface === 'runs' ? '**/api/v1/runs?*' : `**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`;
    await page.route(endpoint, async route => {
      const response = await route.fetch();
      const body = await response.json();
      reads++;
      if (surface === 'nets') {
        const first = { ...body.queries[0], name: reads === 1 ? 'A moving net' : 'Z moving net' };
        const second = { ...first, id: first.id + 1000, name: 'B stationary net' };
        body.queries = [first, second];
      } else {
        const first = { ...body.runs[0], net_name: 'A moving run', error_message: 'A moving run' };
        const second = { ...first, id: first.id + 1000, net_name: 'B stationary run', error_message: 'B stationary run' };
        body.runs = reads === 1 ? [first, second] : [second, first];
        body.total = 2;
      }
      await route.fulfill({ response, json: body });
    });
    await page.goto(surface === 'drawer' ? `/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs` : `/jobs/${surface}`);
    const table = page.locator(surface === 'drawer' ? '.run-preview-table' : `.${surface}-table`);
    await expect(table.locator('tbody tr')).toHaveCount(2);
    const control = surface === 'nets'
      ? table.locator(SEL.actionsMenuTrigger).first()
      : table.locator('.row-stretch').first();
    await control.focus();
    const mounted = await control.elementHandle();
    if (surface === 'nets') {
      await control.click();
      await page.locator(SEL.actionsMenuItem).first().focus();
    }
    const focused = await page.evaluateHandle(() => document.activeElement);
    await page.clock.fastForward(5_000);
    if (surface === 'drawer') await expect(table.locator('tbody tr').first()).toContainText('B stationary run');
    else await expect(table.locator('.row-stretch').first()).toHaveText(surface === 'nets' ? 'B stationary net' : 'B stationary run');
    expect(await mounted!.evaluate(el => el.isConnected)).toBe(true);
    expect(await focused.evaluate(el => el === document.activeElement)).toBe(true);
    if (surface === 'nets') await expect(page.locator(SEL.actionsMenuItem).first()).toBeVisible();
  });
}

for (const behavior of ['remove the focused row', 'retain newer focus']) {
  test(`a live refresh can ${behavior} without restoring obsolete focus`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.clock.install();
    let reads = 0;
    let release: (() => void) | undefined;
    await page.route('**/api/v1/saved', async route => {
      const response = await route.fetch();
      const body = await response.json();
      reads++;
      const first = { ...body.queries[0], name: reads === 1 ? 'A moving net' : 'Z moving net' };
      const second = { ...first, id: first.id + 1000, name: 'B stationary net' };
      body.queries = reads > 1 && behavior === 'remove the focused row' ? [second] : [first, second];
      if (reads === 2) await new Promise<void>(resolve => { release = resolve; });
      await route.fulfill({ response, json: body });
    });
    await page.goto('/jobs/nets');
    const table = page.locator('.nets-table');
    await expect(table.locator('tbody tr')).toHaveCount(2);
    const initial = table.locator('.row-stretch').first();
    await initial.focus();
    const mounted = await initial.elementHandle();
    await page.clock.fastForward(5_000);
    await expect.poll(() => Boolean(release)).toBe(true);
    const nextFocus = table.locator('thead button').last();
    if (behavior === 'retain newer focus') await nextFocus.focus();
    release!();
    await expect(table.locator('.row-stretch').first()).toHaveText('B stationary net');
    if (behavior === 'retain newer focus') await expect(nextFocus).toBeFocused();
    else {
      expect(await mounted!.evaluate(el => el.isConnected)).toBe(false);
      await expect(page.locator('body')).toBeFocused();
    }
  });
}

test('Runs failed page transition exposes Retry over retained success and keeps same-page rows on poll failure', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'pagination', pagination: { runsTotal: 43 } } });
  await page.clock.install();
  let reads = 0;
  let release: (() => void) | undefined;
  await page.route('**/api/v1/runs?*', async route => {
    reads++;
    if (reads === 2 || reads === 4) {
      await route.fulfill({ status: 503, json: { error: 'Runs temporarily unavailable' } });
      return;
    }
    const response = await route.fetch();
    if (reads === 3) await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ response });
  });
  await page.goto('/jobs/runs');
  const frame = page.getByRole('region', { name: 'Recent runs', exact: true });
  const rows = frame.locator('tbody tr');
  const footer = frame.locator('.results-footer');
  await expect(rows).toHaveCount(20);
  await footer.getByRole('button', { name: 'Next' }).click();
  await expect(frame.getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
  await expect(frame).toContainText("Couldn't load runs");
  await expect(footer.locator('.results-summary')).toHaveText('Runs unavailable');
  await expect(rows).toHaveCount(0);
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await frame.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect.poll(() => Boolean(release)).toBe(true);
  await expect(frame).toHaveAttribute('aria-busy', 'true');
  await expect(frame.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0);
  await expect(footer.locator('.results-summary')).toHaveText('Loading…');
  await expect(rows).toHaveCount(0);
  release!();
  await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
  await expect(frame).toHaveAttribute('aria-busy', 'false');
  const before = await rows.allTextContents();
  await page.clock.fastForward(5_000);
  await expect.poll(() => reads).toBe(4);
  await expect(page.getByRole('status').filter({ hasText: 'Refresh failed' })).toBeVisible();
  expect(await rows.allTextContents()).toEqual(before);
  await expect(footer.locator('.results-summary')).toHaveText('21–40 of 43');
  await expect(footer.getByRole('button', { name: 'Prev' })).toBeEnabled();
  await expect(frame.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0);
});

test('Runs competing sorts hide old rows and discard the older response before the latest order arrives', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.clock.install();
  const requests: string[] = [];
  const releases: Array<() => void> = [];
  await page.route('**/api/v1/runs?*', async route => {
    const key = new URL(route.request().url()).searchParams.get('sort')!;
    requests.push(key);
    const response = await route.fetch();
    const body = await response.json();
    body.runs.forEach((run: { net_name: string }) => { run.net_name = `${key} response`; });
    if (requests.length > 1) await new Promise<void>(resolve => { releases.push(resolve); });
    await route.fulfill({ response, json: body });
  });
  await page.goto('/jobs/runs');
  const frame = page.getByRole('region', { name: 'Recent runs', exact: true });
  const rows = frame.locator('tbody tr');
  const summary = frame.locator('.results-summary');
  await expect(rows.first()).toContainText('started response');
  await page.getByPlaceholder('Filter by net…').fill('response');
  await expect(summary).toHaveText('1–3 of 3 · 3 matches on this page');
  await frame.getByRole('button', { name: 'Sort by Net', exact: true }).click();
  await expect.poll(() => releases.length).toBe(1);
  await expect(summary).toHaveText('Loading…');
  await frame.getByRole('button', { name: 'Sort by Rows', exact: true }).click();
  await expect(frame).toHaveAttribute('aria-busy', 'true');
  await expect(rows).toHaveCount(0);
  releases[0]();
  await expect.poll(() => releases.length).toBe(2);
  expect(requests).toEqual(['started', 'net', 'rows']);
  await expect(summary).toHaveText('Loading…');
  await expect(rows).toHaveCount(0);
  await expect(frame).toHaveAttribute('aria-busy', 'true');
  releases[1]();
  await expect(rows.first()).toContainText('rows response');
  await expect(summary).toHaveText('1–3 of 3 · 3 matches on this page');
  await expect(frame).not.toContainText('net response');
  await expect(frame).toHaveAttribute('aria-busy', 'false');
});

test('Runs selection owns its name and result through paging and a delayed A-to-B response', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  // The corpus endpoint slices before responding, so offset 20 has no
  // source row. Build every fabricated page from a complete wire record.
  const run = JSON.parse(await readFile(`${__dirname}/../harness/wire/runs-all.json`, 'utf8')).runs[0];
  await page.route('**/api/v1/runs?*', async route => {
    const response = await route.fetch();
    const body = await response.json();
    const offset = Number(new URL(route.request().url()).searchParams.get('offset'));
    body.runs = offset === 0 ? [
      { ...run, id: 503, net_id: 2, net_name: 'Selected A' },
      { ...run, id: 502, net_id: 2, net_name: 'Selected B' },
    ] : [{ ...run, id: 999, net_id: 9, net_name: 'Other page' }];
    body.total = 21;
    await route.fulfill({ response, json: body });
  });
  let releaseA: (() => void) | undefined;
  let detailReads = 0;
  await page.route('**/api/v1/saved/2/runs/*', async route => {
    detailReads++;
    const id = Number(new URL(route.request().url()).pathname.split('/').at(-1));
    const body = JSON.parse(await readFile(`${__dirname}/../harness/wire/run-result-paged.json`, 'utf8'));
    body.id = id;
    body.query = id === 503 ? 'query from A' : 'query from B';
    if (id === 503) await new Promise<void>(resolve => { releaseA = resolve; });
    await route.fulfill({ json: body });
  });
  await page.goto('/jobs/runs');
  await page.getByRole('link', { name: 'Selected A', exact: true }).click();
  await expect.poll(() => Boolean(releaseA)).toBe(true);
  const detail = page.locator(SEL.runDetail);
  const mountedA = await detail.elementHandle();
  await page.getByRole('link', { name: 'Selected B', exact: true }).click();
  await expect(detail.locator('.receipt')).toContainText('query from B');
  expect(await mountedA!.evaluate(el => el.isConnected)).toBe(false);
  const completedA = page.waitForResponse(response => new URL(response.url()).pathname === '/api/v1/saved/2/runs/503');
  releaseA!();
  await (await completedA).finished();
  await expect(detail.locator('.receipt')).not.toContainText('query from A');
  await detail.locator('.data-area').getByRole('button', { name: 'Next →', exact: true }).click();
  await expect(detail.locator('.results-summary')).toHaveText('21–40 of 45');
  const mountedB = await detail.elementHandle();
  await page.getByRole('region', { name: 'Recent runs', exact: true }).getByRole('button', { name: 'Next →', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText('Other page');
  await expect(detail.locator('.sd-ttl')).toContainText('Selected B');
  await expect(detail.locator('.results-summary')).toHaveText('21–40 of 45');
  expect(await mountedB!.evaluate(el => el.isConnected)).toBe(true);
  await page.locator('.runs-table thead').getByRole('button', { name: 'Sort by Net', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText('Selected B');
  await expect(detail.locator('.sd-ttl')).toContainText('Selected B');
  await expect(detail.locator('.results-summary')).toHaveText('21–40 of 45');
  expect(await mountedB!.evaluate(el => el.isConnected)).toBe(true);
  expect(detailReads).toBe(2);
});

test('Runs completed empty response reports zero entries', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'pagination', pagination: { runsTotal: 0 } } });
  await page.goto('/jobs/runs');
  const frame = page.getByRole('region', { name: 'Recent runs', exact: true });
  await expect(frame.locator('.results-summary')).toHaveText('0 entries');
  await expect(frame).toHaveAttribute('aria-busy', 'false');
  await expect(frame.locator('tbody tr')).toHaveCount(0);
  await expect(frame.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(frame.getByRole('button', { name: 'Next' })).toBeDisabled();
});
