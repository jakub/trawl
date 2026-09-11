// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';

test('failed edits stay available, announce through the existing host and leave focus in the form', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
  await page.route('**/api/v1/saved/1', route => route.fulfill({ status: 503, json: { error: 'Unavailable' } }));
  await page.goto('/jobs/nets?net=1');
  const host = page.locator('.toasts');
  await expect(host).toHaveAttribute('role', 'status');
  await expect(host).toHaveAttribute('aria-live', 'polite');
  await expect(host).toHaveAttribute('aria-atomic', 'false');
  await expect(host.locator('.toast')).toHaveCount(0);
  await host.evaluate(e => e.setAttribute('data-original-host', 'yes'));
  await page.getByRole('button', { name: 'Edit', exact: true }).click();
  const query = page.locator('.sd-drawer textarea');
  await query.fill('service=nginx | head 10');
  await page.clock.install();
  await page.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(host.locator('.toast.error')).toContainText('Update failed');
  await query.focus();
  await page.clock.fastForward(6000);
  await expect(host.locator('.toast.error')).toBeVisible();
  await expect(host).toHaveAttribute('data-original-host', 'yes');
  await expect(query).toBeFocused();
  await expect(query).toHaveValue('service=nginx | head 10');
  await host.getByRole('button', { name: 'Dismiss notification' }).click();
  await expect(host.locator('.toast')).toHaveCount(0);
});

test('routine notification expiry pauses for pointer and keyboard interaction', async ({ page, context }) => {
  test.setTimeout(30_000);
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/search');
  const share = page.locator('.editor-tools').getByRole('button', { name: 'Share', exact: true });
  await expect(share).toBeVisible();
  await share.click();
  const toast = page.locator('.toast');
  await expect(toast).toHaveCount(1);
  await toast.hover();
  // Exercise real elapsed time: virtual-clock jumps can run the expiry
  // before Leptos has processed the pointer/focus effect.
  await page.waitForTimeout(4700);
  await expect(toast).toHaveCount(1);
  const dismiss = toast.getByRole('button', { name: 'Dismiss notification' });
  await dismiss.focus();
  await page.mouse.move(1, 1);
  await page.waitForTimeout(4700);
  await expect(dismiss).toBeFocused();
  await share.focus();
  await expect(toast).toHaveCount(0, { timeout: 6000 });
});

test('persistent error stacks remain bounded and every dismiss control is reachable', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
  await page.setViewportSize({ width: 320, height: 450 });
  await page.route('**/api/v1/saved/1', route => route.fulfill({ status: 503, json: { error: 'Unavailable' } }));
  await page.goto('/jobs/nets?net=1');
  await page.getByRole('button', { name: 'Edit', exact: true }).click();
  const save = page.getByRole('button', { name: 'Save', exact: true });
  for (let i = 1; i <= 8; i++) {
    await save.click();
    await expect(page.locator('.toast.error')).toHaveCount(i);
  }
  const host = page.locator('.toasts');
  const box = await host.boundingBox();
  expect(box!.y).toBeGreaterThanOrEqual(0);
  expect(box!.y + box!.height).toBeLessThanOrEqual(450);
  expect(await host.evaluate(e => e.scrollHeight > e.clientHeight)).toBe(true);
  const last = host.getByRole('button', { name: 'Dismiss notification' }).last();
  await last.focus();
  expect(await host.evaluate(e => e.scrollTop)).toBeGreaterThan(0);
  await last.press('Enter');
  await expect(page.locator('.toast.error')).toHaveCount(7);
});

for (const [path, endpoint, filter] of [
  ['/search/history?hpage=0', '/api/v1/history', 'Filter history'],
  ['/search/schema', '/api/v1/schema/services', 'Filter services'],
  ['/jobs/nets', '/api/v1/saved', 'Filter nets'],
  ['/jobs/runs', '/api/v1/runs', 'Filter by net'],
] as const) {
  test(`resource retry preserves page context on ${path}`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
    let fail = true, attempts = 0;
    await page.route(`**${endpoint}*`, route => {
      if (new URL(route.request().url()).pathname !== endpoint) return route.continue();
      attempts++;
      return fail ? route.fulfill({ status: 503, json: { error: 'Unavailable' } }) : route.continue();
    });
    await page.goto(path);
    await expect(page.locator('.tbl').getByRole('button', { name: 'Retry', exact: true })).toBeVisible();
    const input = page.getByPlaceholder(new RegExp(filter));
    await input.fill('nginx');
    const url = page.url(), before = attempts;
    fail = false;
    await page.locator('.tbl').getByRole('button', { name: 'Retry', exact: true }).click();
    await expect(page.locator('.tbl .load-hint.error')).toHaveCount(0);
    await expect.poll(() => attempts).toBe(before + 1);
    await expect(input).toHaveValue('nginx');
    expect(page.url()).toBe(url);
  });
}

test('query retry keeps the executed URL and unsent editor buffer', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
  let fail = true;
  const queries: unknown[] = [];
  await page.route('**/api/v1/query', route => {
    queries.push(route.request().postDataJSON());
    return fail ? route.fulfill({ status: 503, json: { error: 'Unavailable' } }) : route.continue();
  });
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator('.results').getByRole('button', { name: 'Retry' })).toBeVisible();
  const editor = page.locator('.dsl-editor').getByRole('textbox');
  await editor.fill('service=postgres');
  const url = page.url();
  fail = false;
  await page.locator('.results').getByRole('button', { name: 'Retry' }).click();
  await expect(page.locator('.results-table tbody tr')).toHaveCount(8);
  expect(queries).toHaveLength(2);
  expect(queries[1]).toEqual(queries[0]);
  expect(page.url()).toBe(url);
  await expect(editor).toHaveText('service=postgres');
});

test('active-net count distinguishes pending, failure and a confirmed zero', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let fail = true, calls = 0;
  await page.route('**/api/v1/saved', async route => {
    calls++;
    if (calls === 1) await held;
    return fail ? route.fulfill({ status: 503, json: { error: 'Unavailable' } }) : route.continue();
  });
  await page.goto('/jobs/runs');
  const card = page.locator('.stat-card').filter({ hasText: 'Active nets' });
  await expect(card).toContainText('Loading active nets');
  await expect(page.locator('.tbl-row')).toHaveCount(2);
  release();
  await expect(card).toContainText("Couldn't load active nets");
  await expect(card.locator('.value')).not.toHaveText('0');
  fail = false;
  await card.getByRole('button', { name: 'Retry' }).click();
  await expect(card.locator('.value')).toHaveText('0');
  expect(calls).toBe(2);
});

test('404 offers a native return link and saved-run action promises a rerun', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
  await page.goto('/not-a-route');
  const home = page.getByRole('link', { name: 'Go to Search' });
  await expect(home).toHaveAttribute('href', '/search');
  await home.press('Enter');
  await expect(page).toHaveURL(/\/search$/);
  await page.goto('/jobs/nets?net=1&ntab=runs');
  await page.locator('.sd-drawer .row-stretch').first().click();
  const rerun = page.getByRole('button', { name: 'Run query again', exact: true });
  await expect(rerun).toBeVisible();
  const queries: Record<string, unknown>[] = [];
  await page.route('**/api/v1/query', route => {
    queries.push(route.request().postDataJSON());
    return route.fulfill({ path: `${__dirname}/../harness/wire/query-rows.json` });
  });
  await rerun.click();
  await expect(page).toHaveURL(/\/search\?q=/);
  await expect.poll(() => queries.length).toBe(1);
  expect(queries[0].query).toBe('_severity>=error last=1h | stats count() by host');
});

for (const [tab, queryPart, errorLabel, readySelector] of [
  ['overview', 'timechart', 'histogram', '.sd-card.chart canvas'],
  ['overview', 'dc(', 'cardinality', '.topfields .tf'],
  ['fields', 'dc(', 'cardinality', '.sf-row'],
] as const) {
  test(`service ${tab} retries ${errorLabel} without closing the drawer`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
    let fail = true, calls = 0;
    await page.route('**/api/v1/query', route => {
      if (!route.request().postDataJSON().query.includes(queryPart)) return route.continue();
      calls++;
      return fail ? route.fulfill({ status: 503, json: { error: 'Unavailable' } }) : route.continue();
    });
    await page.goto(`/search/schema?svc=nginx&stab=${tab}`);
    const error = page.locator('.sd-drawer .load-hint.error').filter({ hasText: errorLabel });
    await expect(error).toBeVisible();
    const url = page.url(), before = calls;
    fail = false;
    await error.getByRole('button', { name: 'Retry', exact: true }).click();
    await expect(error).toHaveCount(0);
    await expect(page.locator(readySelector).first()).toBeVisible();
    expect(calls).toBe(before + 1);
    expect(page.url()).toBe(url);
  });
}

for (const detail of [false, true]) {
  test(`net ${detail ? 'run result' : 'runs list'} retries in its open drawer`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
    const endpoint = `/api/v1/saved/1/runs${detail ? '/501' : ''}`;
    let fail = true, calls = 0;
    await page.route(`**${endpoint}*`, route => {
      if (new URL(route.request().url()).pathname !== endpoint) return route.continue();
      calls++;
      return fail ? route.fulfill({ status: 503, json: { error: 'Unavailable' } }) : route.continue();
    });
    await page.goto('/jobs/nets?net=1&ntab=runs');
    if (detail) await page.locator('.sd-drawer .row-stretch').first().click();
    const error = page.locator('.sd-drawer .load-hint.error');
    await expect(error).toBeVisible();
    const url = page.url(), before = calls;
    fail = false;
    await error.getByRole('button', { name: 'Retry', exact: true }).click();
    await expect(error).toHaveCount(0);
    await expect(page.locator(detail ? '.sd-drawer .run-preview table' : '.sd-drawer .row-stretch').first()).toBeVisible();
    expect(calls).toBe(before + 1);
    expect(page.url()).toBe(url);
  });
}
