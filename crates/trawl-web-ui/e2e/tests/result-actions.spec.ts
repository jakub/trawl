// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import fs from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

const aggregate = JSON.parse(fs.readFileSync('harness/wire/result-actions.json', 'utf8'));
const raw = JSON.parse(fs.readFileSync('harness/wire/query-rows.json', 'utf8'));
const routeQuery = '**/api/v1/query';

test('result audit: numeric sorting and only original aggregate group Include', async ({ page }) => {
  await page.route(routeQuery, route => route.fulfill({ json: aggregate }));
  await page.goto('/search?q=' + encodeURIComponent('* | stats count() by host'));
  await page.getByRole('button', { name: 'Sort by count', exact: true }).click();
  await expect(page.locator('.results-table tbody tr').first()).toContainText('hundred');
  await page.getByRole('button', { name: 'Sort by count', exact: true }).click();
  await expect(page.locator('.results-table tbody tr').first()).toContainText('null');
  await page.getByRole('button', { name: 'Show details for result 1', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Include count = 100', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Show context', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Find similar', exact: true })).toHaveCount(0);
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText('* | stats count()');
  await page.locator('.results').getByRole('button', { name: 'Include host = hundred', exact: true }).click();
  const q = new URL(page.url()).searchParams.get('q')!;
  expect(q).toContain('stats count() by host');
  const filters = new URL(page.url()).searchParams.get('f')!;
  expect(JSON.parse(Buffer.from(filters.slice(3), 'base64url').toString())).toContainEqual({field: 'host',value: 'hundred',op: '+'});
});

for (const query of [
  '* | let host = lower(host) | stats count() by host',
  '* | stats count() by host | let host = lower(host)',
  '* | stats count() by host | rename host as other',
  '| from saved "example" | stats count() by host',
]) {
  test(`result audit: suppress transformed or saved input actions: ${query}`, async ({ page }) => {
    await page.route(routeQuery, route => route.fulfill({ json: aggregate }));
    await page.goto('/search?q=' + encodeURIComponent(query));
    await page.getByRole('button', { name: 'Show details for result 1', exact: true }).click();
    await expect(page.getByRole('button', { name: /^Include / })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Copy _raw', exact: true })).toHaveCount(0);
  });
}

test('result audit: historical context owns absolute range and clears facets and page', async ({ page }) => {
  const queries: string[] = [];
  await page.route(routeQuery, async route => {
    queries.push(route.request().postDataJSON().query);
    await route.fulfill({ json: raw });
  });
  await page.goto('/search?q=*&r=24h&page=2&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0');
  await page.getByRole('button', { name: 'Show details for result 1', exact: true }).click();
  await page.getByRole('button', { name: 'Show context', exact: true }).click();
  await expect.poll(() => queries.length).toBe(2);
  const url = new URL(page.url());
  expect(url.searchParams.get('r')).toBe('2026-09-01T09:59:30Z..2026-09-01T10:00:30Z');
  expect(url.searchParams.get('f')).toBeNull();
  expect(url.searchParams.get('page')).toBe('0');
  expect(url.searchParams.get('mode')).toBeNull();
  expect(queries[1]).toContain('host="web-01"');
  expect(queries[1]).not.toContain('last=');
});

test('result audit: live aggregate Events replaces exact table while Visualization stays separate', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await page.goto('/search?q=' + encodeURIComponent('* | stats count() by host') + '&mode=live');
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  const send = async (host: string, count: number) => {
    expect((await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify({ columns: ['host', 'count'], rows: [{host, count}] }) } })).ok()).toBe(true);
  };
  await send('first', 100);
  await expect(page.locator('.results-table')).toContainText('first');
  await send('latest', 20);
  await expect(page.locator('.results-table')).toContainText('latest');
  await expect(page.locator('.results-table')).not.toContainText('first');
  await expect(page.locator('.chart')).toHaveCount(0);
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.results-table')).toHaveCount(0);
});

test('result audit: service tail refusal, retry, pause, resume and teardown', async ({ page, request }) => {
  await resetScenario(request, 'populated');
  let refuse = true;
  await page.route('**/api/v1/stream?*', route => refuse ? route.fulfill({ status: 403 }) : route.continue());
  await page.goto('/search/schema?svc=nginx&stab=tail');
  await expect(page.locator('.sd-tail .lbl').first()).toHaveText('Disconnected');
  await expect(page.getByText('Waiting for events…', { exact: true })).toHaveCount(0);
  refuse = false;
  await page.getByRole('button', { name: 'Retry live stream', exact: true }).click();
  await expect(page.locator('.sd-tail .lbl').first()).toHaveText('Tailing');
  await page.getByRole('button', { name: 'Pause', exact: true }).click();
  await expect(page.locator('.sd-tail .lbl').first()).toHaveText('Paused');
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(0);
  await page.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect(page.locator('.sd-tail .lbl').first()).toHaveText('Tailing');
  await page.keyboard.press('Escape');
  await expect(page.locator('.sd-tail')).toHaveCount(0);
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(0);
});

test('result audit: stats completion after Unicode and quoted pipe', async ({ page }) => {
  await page.goto('/search');
  await page.locator(SEL.cmContent).click();
  await page.keyboard.insertText('message="日本🐟|" | stats cou');
  await page.keyboard.press('Control+Space');
  await expect(page.getByRole('option', { name: /count\(\)/ })).toBeVisible();
  await page.getByRole('option', { name: /count\(\)/ }).click();
  await expect(page.locator(SEL.cmContent)).toContainText('stats count()');
});


test('result audit: pending query cannot retarget retained aggregate row actions', async ({ page }) => {
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let pending = false;
  await page.route(routeQuery, async route => {
    const q = route.request().postDataJSON().query as string;
    if (q.includes('service=new')) { pending = true; await held; }
    await route.fulfill({ json: aggregate }).catch(() => {});
  });
  try {
    await page.goto('/search?q=' + encodeURIComponent('* | stats count() by host') + '&r=1h');
    await page.getByRole('button', { name: 'Show details for result 1', exact: true }).click();
    await page.locator(SEL.cmContent).click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText('service=new');
    await page.keyboard.press('Control+Enter');
    await expect.poll(() => pending).toBe(true);
    await expect(page.locator('.facets').getByRole('button', { name: /^Include / })).toHaveCount(0);
    await page.locator('.results').getByRole('button', { name: 'Include host = hundred', exact: true }).click();
    const q = new URL(page.url()).searchParams.get('q')!;
    expect(q).toContain('stats count() by host');
    expect(new URL(page.url()).searchParams.get('r')).toBe('1h');
    expect(q).not.toContain('service=new');
    const filters = new URL(page.url()).searchParams.get('f')!;
    expect(JSON.parse(Buffer.from(filters.slice(3), 'base64url').toString())).toContainEqual({field: 'host',value: 'hundred',op: '+'});
  } finally { release(); }
});
