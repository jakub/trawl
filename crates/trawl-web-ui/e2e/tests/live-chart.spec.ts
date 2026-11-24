// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The Visualization tab under a live stream (ADR-0038).
//
// Never assert on canvas identity. A frame that changes the label set
// changes the chart OPTIONS, so the component destroys and remounts uPlot.
//
// One stream at a time. `POST /__ctl/stream/frame` broadcasts to every
// open SSE response, so each navigation waits for `sse.open` to settle at 1.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

const live = (minute: number) => `2026-09-01T00:0${minute}:00.000000Z`;

type Col = { name: string, type: string };
const TIME: Col = { name: '_time', type: 'timestamp' };
const SERVICE: Col = { name: 'service', type: 'string' };
const COUNT: Col = { name: 'count', type: 'integer' };

const GROUPED = '/search?q='
  + encodeURIComponent('* | timechart span=1m count() by service')
  + '&mode=live';
const STATS = '/search?q='
  + encodeURIComponent('* | stats count() by service')
  + '&mode=live';

async function sseOpen(request: Ctl): Promise<number> {
  return (await (await request.get('/__ctl/state')).json()).sse.open;
}

/** A zero-width host mounts no chart. */
async function liveVisualization(page: Pg, request: Ctl, url: string) {
  await page.goto(url);
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByRole('tab', { name: 'Visualization' }))
    .toHaveAttribute('aria-selected', 'true');
  await expect.poll(() => page.locator(SEL.chartHost).evaluate(el => el.clientWidth))
    .toBeGreaterThan(0);
  await expect.poll(() => sseOpen(request)).toBe(1);
}

async function frame(request: Ctl, columns: Col[], rows: Record<string, unknown>[]) {
  const cell = (type: string, value: unknown) =>
    value === undefined || value === null ? null : type === 'integer' ? String(value) : value;
  const positional = rows.map(row => columns.map(c => cell(c.type, row[c.name])));
  const sent = await request.post('/__ctl/stream/frame', {
    data: { data: JSON.stringify({ columns, rows: positional }) },
  });
  expect(sent.ok(), `frame: HTTP ${sent.status()}`).toBe(true);
  expect((await sent.json()).sent, 'exactly one stream is listening').toBe(1);
}

function buckets(
  spelling: (minute: number) => string,
  counts: Record<string, (number | null)[]>,
): Record<string, unknown>[] {
  const rows: Record<string, unknown>[] = [];
  for (const [service, series] of Object.entries(counts)) {
    series.forEach((count, minute) => {
      if (count !== null) rows.push({ _time: spelling(minute), service, count });
    });
  }
  return rows;
}

test('a grouped live timechart draws one series per service, and a seventh becomes a caption', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await liveVisualization(page, request, GROUPED);
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();

  const host = page.locator(SEL.chartHost);

  await frame(request, [TIME, SERVICE, COUNT], buckets(live, {
    nginx: [2, 4],
    postgres: [1, 3],
  }));
  await expect(host).toHaveAttribute('data-series', '2');
  await expect(host).toHaveAttribute('data-points', '2');
  await expect(host).toHaveAttribute('data-chart-type', 'line');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);

  await frame(request, [TIME, SERVICE, COUNT], buckets(live, {
    nginx: [2, 4, 5],
    postgres: [1, 3, 2],
    redis: [7, null, 9],
  }));
  await expect(host).toHaveAttribute('data-series', '3');
  await expect(host).toHaveAttribute('data-points', '3');
  const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
  await page.mouse.move(plot.x + plot.width / 2, plot.y + plot.height / 2);
  const value = (label: string) => page.locator(`${SEL.chartHost} .u-series`)
    .filter({ has: page.locator('.u-label', { hasText: label }) })
    .locator('.u-value');
  await expect(value('nginx')).toHaveText('4');
  await expect(value('redis')).toHaveText('');

  await frame(request, [TIME, SERVICE, COUNT], buckets(live, {
    'svc-1': [1, 1],
    'svc-2': [2, 2],
    'svc-3': [3, 3],
    'svc-4': [4, 4],
    'svc-5': [5, 5],
    'svc-6': [6, 6],
    'svc-7': [7, 7],
  }));
  await expect(host).toHaveAttribute('data-series', '6');
  await expect(page.locator('.visualization .chart-caption'))
    .toHaveText('6 of 7 series drawn; svc-1, the smallest by total, is not. Narrow service, or');
  await expect(page.locator(`${SEL.chartHost} .series-key`))
    .toHaveText(['svc-2', 'svc-3', 'svc-4', 'svc-5', 'svc-6', 'svc-7']);
});

test('a live stats by draws columns and re-ranks them on every frame', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await liveVisualization(page, request, STATS);

  const host = page.locator(SEL.chartHost);
  const columns = [SERVICE, COUNT];

  await frame(request, columns, [
    { service: 'nginx', count: 5 },
    { service: 'postgres', count: 9 },
    { service: 'redis', count: 1 },
  ]);
  await expect(host).toHaveAttribute('data-chart-type', 'column');
  await expect(host).toHaveAttribute('data-series', '1');
  await expect(host).toHaveAttribute('data-points', '3');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);

  // The ordinal axis labels are drawn on the canvas, but the card prints
  // the label of the column under the cursor.
  const firstLabel = async () => {
    const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
    await page.mouse.move(plot.x + plot.width / 6, plot.y + plot.height / 2);
    return page.locator(`${SEL.chartHost} .ig-tooltip-time`).innerText();
  };
  await expect.poll(firstLabel).toBe('postgres');

  await frame(request, columns, [
    { service: 'nginx', count: 40 },
    { service: 'postgres', count: 9 },
    { service: 'redis', count: 1 },
  ]);
  await expect.poll(firstLabel).toBe('nginx');
});
