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

test('ingest charts retain their time legend label', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema?svc=nginx');
  await expect(page.locator('.ig-chart canvas')).toHaveCount(1);
  await expect(page.locator('.ig-chart .u-label').first()).toHaveText('time');
  await expect(page.locator('.ig-chart .series-key')).toHaveCount(0);
});
