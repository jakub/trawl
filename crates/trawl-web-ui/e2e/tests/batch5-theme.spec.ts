// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';
import { selectTheme } from '../theme';

test('mounted chart recolors through both themes without losing visibility or data', async ({ page }) => {
  await page.addInitScript(() => {
    const stroke = CanvasRenderingContext2D.prototype.stroke;
    (window as any).themeStrokes = [];
    CanvasRenderingContext2D.prototype.stroke = function (...args: any[]) {
      (window as any).themeStrokes.push(this.strokeStyle);
      return (stroke as any).apply(this, args);
    };
    const Observer = window.MutationObserver;
    (window as any).themeObservers = new Set();
    window.MutationObserver = class extends Observer {
      observe(target: Node, options?: MutationObserverInit) {
        if (options?.attributeFilter?.includes('data-theme')) (window as any).themeObservers.add(this);
        super.observe(target, options);
      }
      disconnect() {
        (window as any).themeObservers.delete(this);
        super.disconnect();
      }
    };
  });
  const labels = ['alpha', 'beta', 'delta', 'epsilon', 'gamma', 'zeta'];
  await page.route('**/api/v1/query', route => route.fulfill({ json: {
    columns: [{ name: '_time' }, ...labels.map(name => ({ name }))],
    rows: [['2026-09-01T00:00:00Z', 1, 9, 2, 8, 5, 3], ['2026-09-01T00:01:00Z', 9, 1, 8, 2, 5, 7], ['2026-09-01T00:02:00Z', 1, 9, 2, 8, 5, 3]],
    truncated: false, pagination: { limit: 50, offset: 0, returned: 3 },
  } }));
  await page.goto('/search?q=' + encodeURIComponent('service=nginx | timechart ' + labels.map(label => `count() as ${label}`).join(', ')));
  await page.getByRole('tab', { name: 'Visualization' }).click();
  await expect(page.locator('.chart .series-key line')).toHaveCount(6);
  await page.evaluate(() => { (window as any).originalCanvas = document.querySelector('.chart canvas'); });
  const beta = page.locator('.chart .u-series').filter({ has: page.locator('.u-label', { hasText: 'beta' }) });
  await beta.locator('.u-label').click();
  await expect(beta).toHaveClass(/u-off/);
  for (const theme of ['dark', 'light']) {
    await page.evaluate(() => { (window as any).themeStrokes = []; });
    await selectTheme(page, theme === 'dark' ? 'Dark' : 'Light');
    await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
    await expect.poll(async () => page.evaluate(() => {
      const tokens = ['--accent', '--teal', '--red', '--yellow', '--green', '--ink'];
      const style = getComputedStyle(document.documentElement);
      return [...document.querySelectorAll('.chart .series-key line')].every((line, i) => line.getAttribute('stroke') === style.getPropertyValue(tokens[i]).trim());
    })).toBe(true);
    await expect.poll(async () => page.evaluate(() => {
      const context = document.createElement('canvas').getContext('2d')!;
      return [...document.querySelectorAll('.chart .u-series:not(.u-off) .series-key line')].every(line => {
        context.strokeStyle = line.getAttribute('stroke')!;
        return (window as any).themeStrokes.includes(context.strokeStyle);
      });
    })).toBe(true);
    await expect(beta).toHaveClass(/u-off/);
    expect(await page.evaluate(() => (window as any).originalCanvas === document.querySelector('.chart canvas'))).toBe(true);
  }
  await beta.locator('.u-label').click();
  await expect(beta).not.toHaveClass(/u-off/);
  const plot = await page.locator('.chart .u-over').boundingBox();
  await page.mouse.move(plot!.x + plot!.width / 2, plot!.y + plot!.height / 2);
  await expect(beta.locator('.u-value')).toHaveText('1');
  await expect(page.locator('.chart .u-series').filter({ has: page.locator('.u-label', { hasText: 'zeta' }) }).locator('.u-value')).toHaveText('7');
  expect(await page.evaluate(() => (window as any).themeObservers.size)).toBe(1);
  await page.getByRole('tab', { name: /^Events/ }).click();
  expect(await page.evaluate(() => (window as any).themeObservers.size)).toBe(0);
});

test('mounted ingest bars update their palette and retain time semantics', async ({ page, request }) => {
  await page.addInitScript(() => {
    const fill = CanvasRenderingContext2D.prototype.fill;
    (window as any).themeFills = [];
    CanvasRenderingContext2D.prototype.fill = function (...args: any[]) {
      (window as any).themeFills.push(this.fillStyle);
      return (fill as any).apply(this, args);
    };
  });
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema?svc=nginx');
  await expect(page.locator('.ig-chart canvas')).toHaveCount(1);
  const barFill = () => page.evaluate(() => {
    const accent = getComputedStyle(document.documentElement).getPropertyValue('--accent').trim();
    const context = document.createElement('canvas').getContext('2d')!;
    context.fillStyle = /^#[0-9a-f]{6}$/i.test(accent) ? `${accent}cc` : accent;
    return context.fillStyle;
  });
  const initial = await barFill();
  await page.evaluate(() => { (window as any).ingestCanvas = document.querySelector('.ig-chart canvas'); });
  await page.evaluate(() => { (window as any).themeFills = []; });
  // Open the real account menu programmatically while the drawer remains
  // mounted; pointer interaction outside the drawer would dismiss it.
  await page.locator(SEL.topbarUser).dispatchEvent('click');
  await page.getByRole('menuitemradio', { name: 'Dark', exact: true }).dispatchEvent('click');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await expect.poll(barFill).not.toBe(initial);
  await expect.poll(async () => page.evaluate(fill => (window as any).themeFills.includes(fill), await barFill())).toBe(true);
  const plot = (await page.locator('.ig-chart .u-over').boundingBox())!;
  await page.mouse.move(plot.x + plot.width * 0.9, plot.y + plot.height / 2);
  const tooltip = page.locator('.ig-tooltip');
  await expect(tooltip).toBeVisible();
  await expect(tooltip.locator('.ig-tooltip-time')).toContainText(' – ');
  const darkBackground = await tooltip.evaluate(el => getComputedStyle(el).backgroundColor);
  await page.evaluate(() => { (window as any).themeFills = []; });
  await page.locator(SEL.topbarUser).dispatchEvent('click');
  await page.getByRole('menuitemradio', { name: 'Light', exact: true }).dispatchEvent('click');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.mouse.move(plot.x + plot.width * 0.9, plot.y + plot.height / 2);
  await expect.poll(barFill).toBe(initial);
  await expect.poll(async () => page.evaluate(fill => (window as any).themeFills.includes(fill), await barFill())).toBe(true);
  await expect.poll(() => tooltip.evaluate(el => getComputedStyle(el).backgroundColor)).not.toBe(darkBackground);
  expect(await page.evaluate(() => (window as any).ingestCanvas === document.querySelector('.ig-chart canvas'))).toBe(true);
});
