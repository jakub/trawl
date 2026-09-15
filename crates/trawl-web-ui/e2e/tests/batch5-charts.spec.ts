// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, trackIntervals, intervalCount } from '../fixtures';

const countResult = {
  columns: [{ name: '_time' }, { name: 'count' }],
  rows: [['2026-09-01T00:00:00Z', 2], ['2026-09-01T00:01:00Z', 4]],
  truncated: false, pagination: { limit: 50, offset: 0, returned: 2 },
};

test('snapshot chart handles completed, unsupported, empty and failed responses without stale canvas', async ({ page }) => {
  let body: typeof countResult = countResult;
  let failed = false;
  let release: (() => void) | undefined;
  let held = false;
  await page.route('**/api/v1/query', async route => {
    if (held) await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ status: failed ? 500 : 200, json: failed ? { error: 'Query failed' } : body });
  });
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await expect(page.locator('.chart-note')).toBeVisible();
  const submit = async (query: string) => {
    await page.locator('.cm-content').click();
    await page.keyboard.press('Control+a');
    await page.keyboard.insertText(query);
    await page.keyboard.press('Control+Enter');
  };
  held = true;
  await submit('service=other | timechart count()');
  await expect(page.getByText('Loading snapshot visualization…')).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  body = { ...countResult, columns: [{ name: 'host' }, { name: 'count' }] };
  held = false;
  release?.();
  await expect(page.getByText('Visualization requires a _time column', { exact: false })).toBeVisible();
  body = { ...countResult, rows: [] };
  await submit('service=empty | timechart count()');
  await expect(page.getByText('No rows returned for this visualization.')).toBeVisible();
  body = countResult;
  await submit('service=again | timechart count()');
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  failed = true;
  await submit('service=failed | timechart count()');
  await expect(page.getByText('Snapshot query failed.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  failed = false;
  await page.getByRole('button', { name: 'Retry snapshot' }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
});

test('live raw visualization explains its supported query and preserves live connection', async ({ page, request }) => {
  const opensBefore = (await (await request.get('/__ctl/state')).json()).sse.opens;
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Live event queries appear in Events.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  expect(new URL(page.url()).searchParams.get('mode')).toBe('live');
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.opens).toBe(opensBefore + 1);
});

test('chart refuses lossy metrics and supports multiple integer metrics', async ({ page }) => {
  let body = { ...countResult, rows: [['2026-09-01T00:00:00Z', -2], ['2026-09-01T00:01:00Z', 1.5]] };
  await page.route('**/api/v1/query', route => route.fulfill({ json: body }));
  await page.goto('/search?q=service%3Dnginx');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('This chart supports non-negative integer metrics only.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  body = { ...countResult, columns: [{ name: '_time' }, { name: 'count' }, { name: 'total' }], rows: [['2026-09-01T00:00:00Z', 2, 3], ['2026-09-01T00:01:00Z', 4, 5]] };
  await page.goto('/search?q=' + encodeURIComponent('service=other | timechart count() as count, count() as total'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await expect(page.locator('.u-legend')).toContainText('total');
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
});

test('live aggregation waits, charts in Visualization and shows exact rows in Events', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()&mode=live');
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  expect((await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify({ columns: ['_time', 'count'], rows: [{ _time: '2026-09-01T00:00:00Z', count: 2 }, { _time: '2026-09-01T00:01:00Z', count: 4 }] }) } })).ok()).toBe(true);
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  await expect(page.locator('.results-table tbody tr')).toHaveCount(2);
  await expect(page.locator('.results-table tbody tr').first()).toContainText('2026-09-01T00:00:00Z');
});

test('grouped snapshots explain the unsupported shape for equal or unequal series', async ({ page }) => {
  let rows: (string | number)[][] = [
    ['2026-09-01T00:00:00Z', 'a', 2], ['2026-09-01T00:01:00Z', 'a', 3],
    ['2026-09-01T00:00:00Z', 'b', 4], ['2026-09-01T00:01:00Z', 'b', 5],
  ];
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: 'host' }, { name: 'count' }], rows } }));
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()%20by%20host');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  await expect(page.getByText('Grouped results are not supported by this chart.', { exact: false })).toBeVisible();
  rows = rows.slice(0, 3);
  await page.goto('/search?q=service%3Dother%20%7C%20timechart%20count()%20by%20host');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Grouped results are not supported by this chart.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
});

test('supported snapshot chart fits initial narrow viewport and resizes with its pane', async ({ page }) => {
  await page.route('**/api/v1/query', route => route.fulfill({ json: countResult }));
  await page.setViewportSize({ width: 320, height: 800 });
  await page.goto('/search?q=service%3Dnginx');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  const fits = async () => page.locator('.chart').evaluate(host => {
    const plot = host.querySelector('.uplot')!;
    const style = getComputedStyle(host);
    return plot.getBoundingClientRect().width <= host.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight) + 1;
  });
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await expect.poll(fits).toBe(true);
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect.poll(fits).toBe(true);
  await page.setViewportSize({ width: 320, height: 800 });
  await expect.poll(fits).toBe(true);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('subsecond events fall inside their histogram tooltip intervals', async ({ page }) => {
  const events = [['2026-09-01T00:00:00.100Z', 9], ['2026-09-01T00:00:00.900Z', 17]];
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: '_severity' }], rows: events } }));
  await page.goto('/search?q=service%3Dnginx');
  const tips = page.locator('.histo .bar .tip');
  await expect(tips).toHaveCount(48);
  const populated = (await tips.allTextContents())
    .map(text => text.match(/^(.*?) – (.*?) · (\d+) events(?: · (\d+) errors)?$/))
    .filter(match => match && Number(match[3]) > 0)
    .map(match => [match![1], match![2], match![3], match![4] ?? '0']);
  expect(populated).toHaveLength(2);
  for (const [time, severity] of events) {
    const interval = populated.find(cells => Date.parse(cells[0].replace(' ', 'T') + 'Z') <= Date.parse(String(time)) && Date.parse(String(time)) <= Date.parse(cells[1].replace(' ', 'T') + 'Z'));
    expect(interval, `no interval contains ${time}`).toBeDefined();
    expect(Number(interval![2])).toBe(1);
    expect(Number(interval![3])).toBe(Number(severity) >= 17 ? 1 : 0);
  }
});

test('disjoint grouped times are explicitly refused instead of paired by occurrence', async ({ page }) => {
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: 'host' }, { name: 'count' }], rows: [
    ['2026-09-01T00:00:00Z', 'a', 1], ['2026-09-01T00:01:00Z', 'b', 10], ['2026-09-01T00:02:00Z', 'a', 2], ['2026-09-01T00:03:00Z', 'b', 20],
  ] } }));
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()%20by%20host');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Grouped results are not supported by this chart.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
});

test('numeric group keys are never plotted as metrics', async ({ page }) => {
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: 'status' }, { name: 'count' }], rows: [['2026-09-01T00:00:00Z', 200, 1], ['2026-09-01T00:00:00Z', 500, 2]] } }));
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()%20by%20status');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Grouped results are not supported by this chart.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
});

test('rejected live aggregation reports failure and can retry', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  let failed = true;
  await page.route('**/api/v1/stream?*', route => failed
    ? route.fulfill({ status: 400, json: { error: 'unsupported stream pipeline' } })
    : route.continue());
  await page.goto('/search?q=service%3Dnginx%20%7C%20pivot%20count()%20on%20status%20by%20host&mode=live');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Live stream unavailable.', { exact: false })).toBeVisible();
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toHaveCount(0);
  failed = false;
  await page.getByRole('button', { name: 'Retry live stream' }).click();
  await expect(page.getByText('Live stream unavailable.', { exact: false })).toHaveCount(0);
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
});


test('malformed live snapshots clear stale charts and valid snapshots recover', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  const priorOpens = (await (await request.get('/__ctl/state')).json()).sse.opens;
  await trackIntervals(page);
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()&mode=live');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  const send = async (data: string) => expect((await request.post('/__ctl/stream/frame', { data: { data } })).ok()).toBe(true);
  const valid = JSON.stringify({ columns: ['_time', 'count'], rows: [{ _time: '2026-09-01T00:00:00Z', count: 2 }] });
  await send(valid);
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await send('{');
  await expect(page.getByText('Live stream returned an unreadable snapshot.', { exact: false })).toBeVisible();
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toHaveCount(0);
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  await send(valid);
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await expect(page.getByRole('button', { name: 'Retry live stream' })).toHaveCount(0);
  await send(JSON.stringify({ columns: ['count', 'count'], rows: [] }));
  await expect(page.getByText('Live stream returned an unreadable snapshot.', { exact: false })).toBeVisible();
  await page.getByRole('button', { name: 'Retry live stream' }).click();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.opens).toBe(priorOpens + 2);
  expect(await intervalCount(page, 16)).toBe(1);
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await send(valid);
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await page.locator('a[href="/search/history"]').first().click();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(0);
  expect(await intervalCount(page, 16)).toBe(0);
});

test('disconnected live charts show reconnecting and recover on a fresh snapshot', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  const priorOpens = (await (await request.get('/__ctl/state')).json()).sse.opens;
  let attempts = 0;
  let release: (() => void) | undefined;
  await page.route('**/api/v1/stream?*', async route => {
    attempts += 1;
    if (attempts > 1) await new Promise<void>(resolve => { release = resolve; });
    await route.continue();
  });
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()&mode=live');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  const send = async () => expect((await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify({ columns: ['_time', 'count'], rows: [{ _time: '2026-09-01T00:00:00Z', count: 2 }] }) } })).ok()).toBe(true);
  await send();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  expect((await request.post('/__ctl/stream/drop')).ok()).toBe(true);
  await expect(page.getByText('Live stream disconnected.', { exact: false })).toBeVisible();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  await expect.poll(() => !!release).toBe(true);
  release?.();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.opens).toBe(priorOpens + 2);
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await send();
  await expect(page.locator('.chart canvas')).toHaveCount(1);
});


test('live chart updates data and series while its supported hint stays unchanged', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()&mode=live');
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  const send = async (columns: string[], rows: Record<string, string | number>[]) => {
    expect((await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify({ columns, rows }) } })).ok()).toBe(true);
  };
  await send(['_time', 'count'], [{ _time: '2026-09-01T00:00:00Z', count: 2 }]);
  const canvas = page.locator('.chart canvas');
  await expect(canvas).toHaveCount(1);
  const before = await canvas.evaluate(node => (node as HTMLCanvasElement).toDataURL());
  await send(['_time', 'count'], [{ _time: '2026-09-01T00:00:00Z', count: 20 }, { _time: '2026-09-01T00:01:00Z', count: 4 }]);
  await expect.poll(() => canvas.evaluate(node => (node as HTMLCanvasElement).toDataURL())).not.toBe(before);
  await send(['_time', 'count', 'total'], [{ _time: '2026-09-01T00:00:00Z', count: 2, total: 3 }]);
  await expect(page.locator('.u-legend')).toContainText('total');
  await expect(canvas).toHaveCount(1);
  await expect(page.locator('.chart-note')).toBeVisible();
});
