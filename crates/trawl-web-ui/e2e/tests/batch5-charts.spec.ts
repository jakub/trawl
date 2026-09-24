// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, trackIntervals, intervalCount } from '../fixtures';
import { expectFocusRing } from '../a11y';
import { SEL } from '../selectors';

type Pg = import('@playwright/test').Page;

// The refusal sentences, copied verbatim from `Refusal::message` in
// crates/trawl-web-ui/src/series.rs (ADR-0038). Every one of them names
// a fix or the place to look instead, which is the property that makes
// them worth asserting whole rather than by substring.
const REFUSES = {
  pivot: 'Pivot results are not drawn as lines. Open Events for the table.',
  top: 'Top results are not drawn as lines. Open Events for the table.',
  rare: 'Rare results are not drawn as lines. Open Events for the table.',
  twoMetrics: 'Grouped charts draw one metric. Chart one metric, or open Events.',
  noTime: 'This result has no time axis. Choose Column for stats by, or open Events.',
  empty: 'No rows to draw.',
  badMetric: 'Visualization draws numeric metrics. Open Events for these values.',
};

// A snapshot `_time` cell as the server actually writes it: UTC wall
// clock, a space, no zone. The web UI sends `timezone: None` and the
// server defaults the offset to zero, so there is no suffix to carry
// (ADR-0038). The live lane spells the same instant `…T00:00:00+00:00`,
// and the chart's parser admits `Z` as well — the tests below cover all
// three spellings between them.
const snapshotTime = (minute: number) => `2026-09-01 00:0${minute}:00`;

const countResult = {
  columns: [{ name: '_time' }, { name: 'count' }],
  rows: [['2026-09-01T00:00:00Z', 2], ['2026-09-01T00:01:00Z', 4]],
  pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
};

/** A refused chart: the sentence in place of the canvas, nothing drawn,
 * and the Events tab one click away — the alternative every refusal
 * offers (`Refusal::offers_events`). */
async function refusalOffersEvents(page: Pg, sentence: string) {
  await expect(page.locator('.visualization .results-empty')).toHaveText(sentence);
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(0);
  await expect(page.locator(SEL.chartHost)).not.toHaveAttribute('data-points', /.*/);
  await page.getByRole('button', { name: 'Open Events', exact: true }).click();
  await expect(page.getByRole('tab', { name: /^Events/ })).toHaveAttribute('aria-selected', 'true');
}

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
  await expect(page.locator('.visualization .results-empty')).toHaveText(REFUSES.noTime);
  body = { ...countResult, rows: [], pagination: { limit: 50, offset: 0, returned: 0, total: 0 } };
  await submit('service=empty | timechart count()');
  await expect(page.locator('.visualization .results-empty')).toHaveText(REFUSES.empty);
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

test('floats and negatives draw, a text metric does not, and metrics draw one series each', async ({ page }) => {
  // Any number draws: integer, float, negative (ADR-0038). Only a cell
  // that is not a number at all stops the chart.
  let body: Record<string, unknown> = {
    ...countResult,
    rows: [[snapshotTime(0), -2], [snapshotTime(1), 1.5]],
  };
  await page.route('**/api/v1/query', route => route.fulfill({ json: body }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1m count()'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '2');
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '1');
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-chart-type', 'line');

  // A metric column holding text: no magnitude, so nothing to draw, and
  // the sentence sends the reader to the values themselves.
  body = {
    ...countResult,
    rows: [[snapshotTime(0), 'many'], [snapshotTime(1), 'more']],
  };
  await page.goto('/search?q=' + encodeURIComponent('service=text | timechart span=1m count()'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await refusalOffersEvents(page, REFUSES.badMetric);

  // An ungrouped timechart draws one series per metric column.
  body = {
    columns: [{ name: '_time' }, { name: 'count' }, { name: 'total' }],
    rows: [[snapshotTime(0), 2, 3], [snapshotTime(1), 4, 5]],
    pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
  };
  await page.goto('/search?q=' + encodeURIComponent('service=other | timechart span=1m count() as count, count() as total'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  await expect(page.locator('.u-legend')).toContainText('total');
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(0);
});

test('live aggregation waits, charts in Visualization and shows exact rows in Events', async ({ page, request }) => {
  await resetScenario(request, 'stream-chart');
  // A live stream asks for no window at all, so there is no coverage to
  // judge and the chart has nothing to refuse (ADR-0037).
  const streamRequest = page.waitForRequest(/\/api\/v1\/stream\?/);
  await page.goto('/search?q=service%3Dnginx%20%7C%20timechart%20count()&mode=live');
  const streamParams = new URL((await streamRequest).url()).searchParams;
  expect(streamParams.get('limit')).toBeNull();
  expect(streamParams.get('offset')).toBeNull();
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.getByText('Waiting for the first live aggregation snapshot.')).toBeVisible();
  await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
  expect((await request.post('/__ctl/stream/frame', { data: { data: JSON.stringify({ columns: ['_time', 'count'], rows: [{ _time: '2026-09-01T00:00:00Z', count: 2 }, { _time: '2026-09-01T00:01:00Z', count: 4 }] }) } })).ok()).toBe(true);
  await expect(page.locator('.chart canvas')).toHaveCount(1);
  await expect(page.locator('.visualization')).not.toContainText('were fetched');
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.locator('.chart canvas')).toHaveCount(0);
  await expect(page.locator('.results-table tbody tr')).toHaveCount(2);
  await expect(page.locator('.results-table tbody tr').first()).toContainText('2026-09-01T00:00:00Z');
});

test('a grouped snapshot draws one series per group on the union of their buckets', async ({ page }) => {
  // Two hosts over the same two minutes, then the same query with one
  // host's rows dropped: the second host's series is shorter, which is
  // exactly what a shared bucket axis exists to align (ADR-0038).
  let rows: (string | number)[][] = [
    [snapshotTime(0), 'a', 2], [snapshotTime(1), 'a', 3],
    [snapshotTime(0), 'b', 4], [snapshotTime(1), 'b', 5],
  ];
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, { name: 'host' }, { name: 'count' }],
    rows,
    pagination: { limit: 50, offset: 0, returned: rows.length, total: rows.length },
  } }));
  const GROUPED = '/search?q=' + encodeURIComponent('service=nginx | timechart span=1m count() by host');
  await page.goto(GROUPED);
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(`${SEL.chartHost} .series-key`)).toHaveCount(2);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  // Two minutes, the union of both hosts' buckets.
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '2');
  // The axis is UTC, and the legend says so where the reader is.
  await expect(page.locator(`${SEL.chartHost} .u-legend`)).toContainText('UTC');

  rows = [[snapshotTime(0), 'a', 2], [snapshotTime(1), 'a', 3], [snapshotTime(0), 'b', 4]];
  await page.goto('/search?q=' + encodeURIComponent('service=other | timechart span=1m count() by host'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '2');
});

test('disjoint grouped times keep their own instants and a missing bucket is a gap', async ({ page }) => {
  // `a` at 00:00 and 00:03, `b` at 00:01. Paired by occurrence these
  // would be three positions; laid on the minute grid they are four
  // instants, and `a` has no value at two of them.
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, { name: 'host' }, { name: 'count' }],
    rows: [[snapshotTime(0), 'a', 1], [snapshotTime(1), 'b', 10], [snapshotTime(3), 'a', 2]],
    pagination: { limit: 50, offset: 0, returned: 3, total: 3 },
  } }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1m count() by host'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  // The GRID, not the row count: three rows, four minutes.
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '4');

  // And the gap is a null, not a zero. Over the SECOND instant the live
  // legend reads 10 for `b` and nothing at all for `a`: one cursor
  // position, two answers, which is the distinction a zero would erase.
  // (uPlot's legend prints an empty cell for a null, not a placeholder,
  // so `b`'s value is what makes the empty one mean "no value here".)
  const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
  await page.mouse.move(plot.x + plot.width / 3, plot.y + plot.height / 2);
  const value = (label: string) => page.locator(`${SEL.chartHost} .u-series`)
    .filter({ has: page.locator('.u-label', { hasText: label }) })
    .locator('.u-value');
  await expect(value('b')).toHaveText('10');
  await expect(value('a')).toHaveText('');
});

test('supported snapshot chart fits initial narrow viewport and resizes with its pane', async ({ page }) => {
  await page.route('**/api/v1/query', route => route.fulfill({ json: countResult }));
  await page.setViewportSize({ width: 320, height: 800 });
  // The roles come from the query, so a chart needs one that ends in a
  // timechart to have anything to draw.
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart count()'));
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
  const bars = page.locator('.histo .bar');
  const first = bars.first();
  const second = bars.nth(1);
  await first.focus();
  await page.keyboard.press('Shift+Tab');
  await page.keyboard.press('Tab');
  await expectFocusRing(first);
  await expect(first).toHaveAccessibleName(await tips.first().innerText());
  await expect(tips.first()).toHaveCSS('opacity', '1');
  await page.keyboard.press('Tab');
  await expect(second).toBeFocused();
  await expect(second).toHaveAccessibleName(await tips.nth(1).innerText());
  await expect(tips.nth(1)).toHaveCSS('opacity', '1');
  await expect(tips.first()).toHaveCSS('opacity', '0');
  await first.hover();
  await expect(tips.first()).toHaveCSS('opacity', '1');
  await first.click();
  await page.mouse.move(0, 0);
  await expect(tips.first()).toHaveCSS('opacity', '0');
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

test('histogram tooltip stays inside the strip at both edges', async ({ page }) => {
  // Events in the first and the last bucket, both errors, so the two
  // edge bars carry the longest tip text this strip draws.
  const events = [['2026-09-01T00:00:00.100Z', 17], ['2026-09-01T00:00:00.900Z', 17]];
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: '_severity' }], rows: events } }));
  await page.goto('/search?q=service%3Dnginx');
  const histo = page.locator('.histo');
  const bars = page.locator('.histo .bar');
  await expect(bars).toHaveCount(48);

  // Layout boxes are fractional and the page offers only whole-pixel
  // offsets to measure the tip against, so the two boxes may disagree
  // by sub-pixel rounding. Half a pixel is less than one device pixel
  // at DPR 1, so a tip that passes cannot visibly cross the strip edge.
  const ROUNDING = 0.5;
  const inside = async (label: string, index: number) => {
    const tip = bars.nth(index).locator('.tip');
    // The tip fades in over 120ms; measure it once it is fully shown.
    await expect(tip).toHaveCSS('opacity', '1');
    const [t, h] = [await tip.boundingBox(), await histo.boundingBox()];
    expect(t && h, label).toBeTruthy();
    expect(t!.x, `${label}: left edge`).toBeGreaterThanOrEqual(h!.x - ROUNDING);
    expect(t!.x + t!.width, `${label}: right edge`).toBeLessThanOrEqual(h!.x + h!.width + ROUNDING);
  };

  // 320px is the narrowest width the responsive suite checks. There the
  // strip is narrower than an error bucket's one-line label, so the tip
  // has to wrap to fit at all.
  for (const viewport of [{ width: 1400, height: 900 }, { width: 800, height: 900 }, { width: 320, height: 900 }]) {
    await page.setViewportSize(viewport);
    for (const index of [0, 47]) {
      const bar = bars.nth(index);
      const at = `${viewport.width}px, bar ${index}`;

      await bar.hover();
      await inside(`${at}, hover`, index);
      await page.mouse.move(0, 0);
      await expect(bar.locator('.tip')).toHaveCSS('opacity', '0');

      // Keyboard focus, arrived at by Tab so :focus-visible matches.
      await bar.focus();
      await page.keyboard.press(index === 0 ? 'Tab' : 'Shift+Tab');
      await page.keyboard.press(index === 0 ? 'Shift+Tab' : 'Tab');
      await expect(bar).toBeFocused();
      await inside(`${at}, focus`, index);
      await page.locator('body').click({ position: { x: 0, y: 0 } });
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      await expect(bar.locator('.tip')).toHaveCSS('opacity', '0');
    }
  }
});

test('a shown histogram tooltip is re-placed when the strip resizes', async ({ page }) => {
  const events = [['2026-09-01T00:00:00.100Z', 17], ['2026-09-01T00:00:00.900Z', 17]];
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: '_severity' }], rows: events } }));
  const histo = page.locator('.histo');
  const bars = page.locator('.histo .bar');
  // Whole-pixel rounding, as in the edge test above.
  const ROUNDING = 0.5;
  const fits = (index: number) => async () => {
    const [t, h] = [await bars.nth(index).locator('.tip').boundingBox(), await histo.boundingBox()];
    return !!t && !!h && t.x >= h.x - ROUNDING && t.x + t.width <= h.x + h.width + ROUNDING;
  };

  // The tip is placed when its bar gains focus. Resizing the window
  // afterwards moves the bar under a tip that is still shown, and focus
  // never moves, so nothing but the strip's own resize can re-place it.
  //
  // Not the edge bars: bar 0 and bar 47 keep their distance from their
  // own strip edge at any width, so a stale shift happens to stay right
  // for them. Bars 36 and 9 fit centred in the 1400px strip (about 954px)
  // and overflow the 800px one (about 778px) by roughly 17px on the right
  // and 24px on the left, so a shift measured at 1400px is wrong at 800px.
  for (const [index, from, to] of [[36, 1400, 800], [9, 1400, 800]] as const) {
    await page.setViewportSize({ width: from, height: 900 });
    await page.goto('/search?q=service%3Dnginx');
    await expect(bars).toHaveCount(48);
    const bar = bars.nth(index);
    await bar.focus();
    await page.keyboard.press('Shift+Tab');
    await page.keyboard.press('Tab');
    await expect(bar).toBeFocused();
    await expect(bar.locator('.tip')).toHaveCSS('opacity', '1');
    await expect.poll(fits(index), `bar ${index} at ${from}px`).toBe(true);

    await page.setViewportSize({ width: to, height: 900 });
    await expect(bar).toBeFocused();
    await expect(bar.locator('.tip')).toHaveCSS('opacity', '1');
    await expect.poll(fits(index), `bar ${index} after ${from}px → ${to}px`).toBe(true);
  }
});

test('a hovered histogram tooltip is re-placed when new bars render under it', async ({ page }) => {
  // The first response has one event at each end; the second adds one
  // in bucket 36's range, so the rerun draws new bars and new labels.
  // The pointer stays on bar 36 throughout, and the tip of the bar that
  // replaces it must still fit. This pins the outcome, not the route:
  // Chromium sends a mouseenter to the new bar under a still pointer,
  // which places its tip on its own, and the component also places every
  // tip after the bars render, which does not rely on that.
  let responses = 0;
  await page.route('**/api/v1/query', route => {
    responses += 1;
    const rows = [['2026-09-01T00:00:00.100Z', 17], ['2026-09-01T00:00:00.900Z', 17]];
    if (responses > 1) rows.push(['2026-09-01T00:00:00.860Z', 17]);
    return route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: '_severity' }], rows } });
  });
  await page.setViewportSize({ width: 800, height: 900 });
  await page.goto('/search?q=service%3Dnginx');
  const histo = page.locator('.histo');
  const bars = page.locator('.histo .bar');
  await expect(bars).toHaveCount(48);
  const ROUNDING = 0.5;
  const fits = async () => {
    const [t, h] = [await bars.nth(36).locator('.tip').boundingBox(), await histo.boundingBox()];
    return !!t && !!h && t.x >= h.x - ROUNDING && t.x + t.width <= h.x + h.width + ROUNDING;
  };
  await bars.nth(36).hover();
  await expect(bars.nth(36).locator('.tip')).toHaveCSS('opacity', '1');
  await expect.poll(fits).toBe(true);
  const before = await bars.nth(36).getAttribute('aria-label');

  await page.locator(SEL.cmContent).focus();
  await page.keyboard.press('Control+Enter');
  await expect.poll(() => responses).toBe(2);
  await expect(bars.nth(36)).not.toHaveAttribute('aria-label', before!);
  await expect(bars.nth(36).locator('.tip')).toHaveCSS('opacity', '1');
  await expect.poll(fits).toBe(true);
});

test('the deferred tip placement is skipped once the histogram unmounts', async ({ page, pageErrors }) => {
  // After the bars render, the histogram places every tip in an
  // animation frame, and nothing cancels that frame when the strip goes
  // away. Hold every frame the page asks for, unmount the histogram
  // while its placement is still queued, then run the queue: the late
  // callback must find the strip gone and do nothing.
  await page.addInitScript(() => {
    const native = window.requestAnimationFrame.bind(window);
    const cancel = window.cancelAnimationFrame.bind(window);
    const held = new Map<number, FrameRequestCallback>();
    let next = -1;
    const w = window as any;
    w.__holdFrames = true;
    w.__heldFrames = () => held.size;
    // Each held callback runs from its own task, as a frame would, so a
    // throw reaches the page as an uncaught error and not the caller.
    w.__releaseFrames = () => {
      w.__holdFrames = false;
      const queued = [...held.values()];
      held.clear();
      for (const cb of queued) setTimeout(() => cb(performance.now()), 0);
    };
    window.requestAnimationFrame = (cb: FrameRequestCallback) => {
      if (!w.__holdFrames) return native(cb);
      const id = next--;
      held.set(id, cb);
      return id;
    };
    window.cancelAnimationFrame = (id: number) => {
      if (id < 0) held.delete(id);
      else cancel(id);
    };
  });
  const panics: string[] = [];
  page.on('console', msg => { if (/panicked/.test(msg.text())) panics.push(msg.text()); });
  const events = [['2026-09-01T00:00:00.100Z', 17], ['2026-09-01T00:00:00.900Z', 17]];
  await page.route('**/api/v1/query', route => route.fulfill({ json: { ...countResult, columns: [{ name: '_time' }, { name: '_severity' }], rows: events } }));
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator('.histo .bar')).toHaveCount(48);
  expect(await page.evaluate(() => (window as any).__heldFrames()), 'the placement is queued').toBeGreaterThan(0);

  // The Visualization tab renders no histogram, so switching disposes it.
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.histo')).toHaveCount(0);

  await page.evaluate(() => (window as any).__releaseFrames());
  // Tasks queued after the released ones run after them, so once this
  // resolves every held callback has run.
  await page.evaluate(() => new Promise(resolve => setTimeout(() => resolve(null), 0)));
  expect(panics).toEqual([]);
  expect(pageErrors.errors.map(e => e.message)).toEqual([]);
});

test('numeric group keys are series, labelled by their own digits', async ({ page }) => {
  // `by status` over 200 and 500: the roles come from the query, so the
  // numbers in the group column are two series and not two metrics.
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, { name: 'status' }, { name: 'count' }],
    rows: [[snapshotTime(0), 200, 1], [snapshotTime(0), 500, 2]],
    pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
  } }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1m count() by status'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '1');
  await expect(page.locator(`${SEL.chartHost} .series-key`)).toHaveText(['200', '500']);
});

test('a grouped average with a null cell draws the gap', async ({ page }) => {
  // A null metric cell is "no measurement": a gap in the line, and a
  // grid instant all the same.
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, { name: 'pod' }, { name: 'avg_latency_ms' }],
    rows: [
      [snapshotTime(0), 'api-1', 1.5], [snapshotTime(1), 'api-1', null], [snapshotTime(2), 'api-1', 3.5],
      [snapshotTime(0), 'api-2', 2.0],
    ],
    pagination: { limit: 50, offset: 0, returned: 4, total: 4 },
  } }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart span=1m avg(latency_ms) by pod'));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator(`${SEL.chartHost} canvas`)).toHaveCount(1);
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-series', '2');
  await expect(page.locator(SEL.chartHost)).toHaveAttribute('data-points', '3');
  // The middle instant: `api-1` returned a null there, so its legend
  // cell is empty while `api-2`'s first bucket still reads 2.
  const plot = (await page.locator(`${SEL.chartHost} .u-over`).boundingBox())!;
  const value = (label: string) => page.locator(`${SEL.chartHost} .u-series`)
    .filter({ has: page.locator('.u-label', { hasText: label }) })
    .locator('.u-value');
  await page.mouse.move(plot.x + 2, plot.y + plot.height / 2);
  await expect(value('api-1')).toHaveText('1.5');
  await page.mouse.move(plot.x + plot.width / 2, plot.y + plot.height / 2);
  await expect(value('api-1')).toHaveText('');
});

test('every unchartable shape refuses with its own sentence and offers Events', async ({ page }) => {
  // One mock body per shape: what the ladder reads is the QUERY that
  // produced the rows, so the rows themselves only have to be a
  // plausible answer to it.
  const cases: { query: string; json: Record<string, unknown>; sentence: string }[] = [
    {
      query: 'service=nginx | pivot count() on status by host',
      json: {
        columns: [{ name: 'host' }, { name: '200' }, { name: '500' }],
        rows: [['web-01', 4, 1]],
        pagination: { limit: 50, offset: 0, returned: 1, total: 1 },
      },
      sentence: REFUSES.pivot,
    },
    {
      query: 'service=nginx | top 5 host',
      json: {
        columns: [{ name: 'host' }, { name: 'count' }],
        rows: [['web-01', 4], ['web-02', 1]],
        pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
      },
      sentence: REFUSES.top,
    },
    {
      query: 'service=nginx | rare 5 host',
      json: {
        columns: [{ name: 'host' }, { name: 'count' }],
        rows: [['web-09', 1], ['web-08', 2]],
        pagination: { limit: 50, offset: 0, returned: 2, total: 2 },
      },
      sentence: REFUSES.rare,
    },
    {
      query: 'service=nginx | timechart span=1m count(), avg(bytes) by host',
      json: {
        columns: [{ name: '_time' }, { name: 'host' }, { name: 'count' }, { name: 'avg_bytes' }],
        rows: [[snapshotTime(0), 'web-01', 4, 1200.5]],
        pagination: { limit: 50, offset: 0, returned: 1, total: 1 },
      },
      sentence: REFUSES.twoMetrics,
    },
  ];
  let json: Record<string, unknown> = cases[0].json;
  await page.route('**/api/v1/query', route => route.fulfill({ json }));
  for (const shape of cases) {
    json = shape.json;
    await page.goto('/search?q=' + encodeURIComponent(shape.query));
    await page.getByRole('tab', { name: 'Visualization' }).click();
    await refusalOffersEvents(page, shape.sentence);
  }
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
