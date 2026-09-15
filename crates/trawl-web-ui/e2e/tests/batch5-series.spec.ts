// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';

for (const deviceScaleFactor of [1, 2]) {
  test.describe(`series at ${deviceScaleFactor}x`, () => {
    test.use({ deviceScaleFactor });
    test('six crossing series have matching distinct dash indicators and usable hover values', async ({ page }) => {
      await page.addInitScript(() => {
        const stroke = CanvasRenderingContext2D.prototype.stroke;
        (window as any).seriesStrokes = [];
        CanvasRenderingContext2D.prototype.stroke = function (...args: any[]) {
          (window as any).seriesStrokes.push({ color: this.strokeStyle, dash: this.getLineDash() });
          return (stroke as any).apply(this, args);
        };
      });
      const labels = ['alpha', 'beta', 'gamma', 'delta', 'epsilon', 'zeta'];
      const values = [[1, 9, 5, 2, 8, 3], [9, 1, 5, 8, 2, 7], [1, 9, 5, 2, 8, 3]];
      await page.route('**/api/v1/query', route => route.fulfill({ json: {
        columns: [{ name: '_time' }, ...labels.map(name => ({ name }))],
        rows: values.map((row, i) => [`2026-09-01T00:0${i}:00Z`, ...row]),
        truncated: false, pagination: { limit: 50, offset: 0, returned: 3 },
      } }));
      await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart ' + labels.map(label => `count() as ${label}`).join(', ')));
      await page.getByRole('tab', { name: 'Visualization' }).click();
      await expect(page.locator('.chart .u-legend')).toContainText('Result position');
      await expect(page.locator('.chart .series-key line')).toHaveCount(6);
      const indicators = await page.locator('.chart .series-key line').evaluateAll(lines => lines.map(line => ({
        color: line.getAttribute('stroke'), dash: line.getAttribute('stroke-dasharray') || '',
      })));
      expect(new Set(indicators.map(x => x.dash)).size).toBe(6);
      expect(new Set(indicators.map(x => x.color)).size).toBe(6);
      for (const indicator of indicators) {
        expect(await page.evaluate(indicator => {
          const context = document.createElement('canvas').getContext('2d')!;
          context.strokeStyle = indicator.color!;
          const dash = indicator.dash.split(' ').filter(Boolean).map(Number);
          return (window as any).seriesStrokes.some((draw: any) => draw.color === context.strokeStyle && JSON.stringify(draw.dash) === JSON.stringify(dash.map(n => n * devicePixelRatio)));
        }, indicator)).toBe(true);
      }
      const plot = await page.locator('.chart .u-over').boundingBox();
      await page.mouse.move(plot!.x + plot!.width / 2, plot!.y + plot!.height / 2);
      for (const [index, label] of labels.entries()) {
        const row = page.locator('.chart .u-series').filter({ has: page.locator('.u-label', { hasText: label }) });
        await expect(row.locator('.u-value')).toHaveText(String(values[1][index]));
      }
    });

  });
}

test('ingest charts show the containing hour in a bounded tooltip', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.route('**/api/v1/query', route => {
    if (!route.request().postData()?.includes('timechart')) return route.fallback();
    return route.fulfill({ json: {
      columns: [{ name: '_time' }, { name: 'count' }],
      rows: [['2026-09-01T00:00:00Z', 2], ['2026-09-01T01:00:00Z', 1]],
      truncated: false, pagination: { limit: 50, offset: 0, returned: 2 },
    } });
  });
  await page.goto('/search/schema?svc=nginx');
  await expect(page.locator('.ig-chart canvas')).toHaveCount(1);
  await expect(page.getByRole('img', { name: 'Hourly ingest chart. Total events in the displayed hours: 3.' })).toBeVisible();
  await expect(page.locator('.ig-chart .u-legend')).toHaveCount(0);
  await expect(page.locator('.ig-chart .series-key')).toHaveCount(0);
  const plot = (await page.locator('.ig-chart .u-over').boundingBox())!;
  const tooltip = page.locator('.ig-tooltip');
  for (const fraction of [23.2 / 24, 23.8 / 24]) {
    await page.mouse.move(plot.x + plot.width * fraction, plot.y + plot.height / 2);
    await expect(tooltip).toBeVisible();
    await expect(tooltip).toContainText('Events: 1');
    await expect(tooltip.locator('.ig-tooltip-time')).toHaveText('Sep 1, 01:00 AM – Sep 1, 02:00 AM');
  }
  for (const fraction of [0.01, 0.99]) {
    await page.mouse.move(plot.x + plot.width * fraction, plot.y + plot.height / 2);
    const tip = (await tooltip.boundingBox())!;
    expect(tip.x).toBeGreaterThanOrEqual(plot.x - 1);
    expect(tip.x + tip.width).toBeLessThanOrEqual(plot.x + plot.width + 1);
  }
  await page.mouse.move(plot.x - 10, plot.y - 10);
  await expect(tooltip).toBeHidden();
  for (const width of [1000, 800, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await expect.poll(() => page.locator('.ig-chart').evaluate(el =>
      Math.abs(el.clientWidth - el.querySelector('.uplot')!.clientWidth)
    )).toBeLessThanOrEqual(1);
  }
  await page.goto('/search/schema');
  await expect(tooltip).toHaveCount(0);
  await page.goto('/search/schema?svc=nginx');
  await expect(tooltip).toHaveCount(1);
});
