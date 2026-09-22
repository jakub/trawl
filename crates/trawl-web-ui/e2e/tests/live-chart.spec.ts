// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The Visualization tab under a live stream (issue #234, ADR-0038).
//
// Live feeds the SAME ladder, the same span resolver and the same
// categorical detector as a snapshot: a grouped live `timechart` draws
// one series per group on the bucket grid, a seventh group becomes a
// caption rather than a refusal, and a live `stats … by` draws columns
// that re-rank each frame. What makes this worth its own file is that
// every one of those answers arrives as a FRAME, so the assertions have
// to survive the chart being rebuilt underneath them.
//
// Three rules the shape of this file follows:
//
//  - Never assert on canvas identity. A frame that changes the label set
//    changes the chart OPTIONS, so the component destroys and remounts
//    uPlot; the surviving observables are the host's data attributes and
//    the DOM around it.
//  - Never sleep. Every wait is `expect`/`expect.poll` on the thing the
//    frame was supposed to change.
//  - One stream at a time. `POST /__ctl/stream/frame` broadcasts to every
//    open SSE response, so a leaked stream would feed a chart no longer
//    on screen. Each navigation waits for `sse.open` to settle at 1
//    before the first frame.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

/** The live lane's own `_time` spelling: RFC 3339 at a zero offset, which
 * is what chrono's `to_rfc3339()` writes (ADR-0038). */
const live = (minute: number) => `2026-09-01T00:0${minute}:00+00:00`;
/** The other admitted spelling of the same instant. One frame below uses
 * it, so the strict parser's second form is covered too. */
const liveZ = (minute: number) => `2026-09-01T00:0${minute}:00Z`;

const GROUPED = '/search?q='
  + encodeURIComponent('* | timechart span=1m count() by service')
  + '&mode=live';
const STATS = '/search?q='
  + encodeURIComponent('* | stats count() by service')
  + '&mode=live';

async function sseOpen(request: Ctl): Promise<number> {
  return (await (await request.get('/__ctl/state')).json()).sse.open;
}

/** Open a live query on the Visualization tab, with the chart host laid
 * out and exactly one stream connected — both preconditions of a frame
 * being drawn at all: a zero-width host mounts no chart. */
async function liveVisualization(page: Pg, request: Ctl, url: string) {
  await page.goto(url);
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByRole('tab', { name: 'Visualization' }))
    .toHaveAttribute('aria-selected', 'true');
  await expect.poll(() => page.locator(SEL.chartHost).evaluate(el => el.clientWidth))
    .toBeGreaterThan(0);
  await expect.poll(() => sseOpen(request)).toBe(1);
}

/** Broadcast one aggregation frame, as the server does: `columns` plus
 * row OBJECTS keyed by column name. */
async function frame(request: Ctl, columns: string[], rows: Record<string, unknown>[]) {
  const sent = await request.post('/__ctl/stream/frame', {
    data: { data: JSON.stringify({ columns, rows }) },
  });
  expect(sent.ok(), `frame: HTTP ${sent.status()}`).toBe(true);
  expect((await sent.json()).sent, 'exactly one stream is listening').toBe(1);
}

/** A grouped timechart frame: one row per (bucket, service) that has a
 * value, which is how a missing bucket reaches the client — as an absent
 * row, never a zero. */
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

  // Frame 1 — two services over two minutes.
  await frame(request, ['_time', 'service', 'count'], buckets(live, {
    nginx: [2, 4],
    postgres: [1, 3],
  }));
  await expect(host).toHaveAttribute('data-series', '2');
  await expect(host).toHaveAttribute('data-points', '2');
  await expect(host).toHaveAttribute('data-chart-type', 'line');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);

  // Frame 2 — a third service arrives, and it has no row for the middle
  // minute. The grid is the three minutes the frame spans, not the rows
  // any one service happens to carry.
  await frame(request, ['_time', 'service', 'count'], buckets(live, {
    nginx: [2, 4, 5],
    postgres: [1, 3, 2],
    redis: [7, null, 9],
  }));
  await expect(host).toHaveAttribute('data-series', '3');
  await expect(host).toHaveAttribute('data-points', '3');
  // The gap is a null: over the middle instant `redis` has no legend
  // value while the other two do, which is what a zero would erase.
  const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
  await page.mouse.move(plot.x + plot.width / 2, plot.y + plot.height / 2);
  const value = (label: string) => page.locator(`${SEL.chartHost} .u-series`)
    .filter({ has: page.locator('.u-label', { hasText: label }) })
    .locator('.u-value');
  await expect(value('nginx')).toHaveText('4');
  await expect(value('redis')).toHaveText('');

  // Frame 3 — seven services, ranked by their total over the frame. Six
  // draw and the caption names the one left out. This frame spells the
  // zero offset `Z`, the parser's other admitted form.
  await frame(request, ['_time', 'service', 'count'], buckets(liveZ, {
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
    .toHaveText('6 of 7 series drawn; the 1 smallest by total are not. Narrow service, or open Events.');
  await expect(page.locator(`${SEL.chartHost} .series-key`))
    .toHaveText(['svc-2', 'svc-3', 'svc-4', 'svc-5', 'svc-6', 'svc-7']);
});

test('a live stats by draws columns and re-ranks them on every frame', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  await liveVisualization(page, request, STATS);

  const host = page.locator(SEL.chartHost);
  const columns = ['service', 'count'];

  await frame(request, columns, [
    { service: 'nginx', count: 5 },
    { service: 'postgres', count: 9 },
    { service: 'redis', count: 1 },
  ]);
  // No `_time` column at all, so the shape picks the type: Column, one
  // series over the groups.
  await expect(host).toHaveAttribute('data-chart-type', 'column');
  await expect(host).toHaveAttribute('data-series', '1');
  await expect(host).toHaveAttribute('data-points', '3');
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);

  // Which group is first is readable through the hover card: the ordinal
  // axis labels are drawn on the canvas, but the card prints the label of
  // the column under the cursor. Groups are ranked by value, so the
  // first column is the largest.
  const firstLabel = async () => {
    const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
    await page.mouse.move(plot.x + plot.width / 6, plot.y + plot.height / 2);
    return page.locator(`${SEL.chartHost} .ig-tooltip-time`).innerText();
  };
  await expect.poll(firstLabel).toBe('postgres');

  // A frame that changes the order changes which column is first. The
  // label set is unchanged, so this is a data-only update — and the
  // answer still has to follow the values, not the arrival order.
  await frame(request, columns, [
    { service: 'nginx', count: 40 },
    { service: 'postgres', count: 9 },
    { service: 'redis', count: 1 },
  ]);
  await expect.poll(firstLabel).toBe('nginx');
});
