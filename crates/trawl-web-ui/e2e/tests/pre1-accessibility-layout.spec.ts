// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, lastCapturedQuery, capturedQueryCount } from '../fixtures';
import { SEL } from '../selectors';

for (const theme of ['light', 'dark']) {
  for (const width of [320, 390, 768, 1440]) {
    test(`absolute range keeps Haul reachable at ${width}px in ${theme}`, async ({ page, request }) => {
      await page.setViewportSize({ width, height: 900 });
      await page.goto('/search');
      await page.evaluate(theme => document.documentElement.setAttribute('data-theme', theme), theme);
      await page.locator(SEL.dateRangeTrigger).click();
      await page.locator(SEL.absoluteTab).click();
      await page.locator(SEL.dateRangeFrom).fill('2026-08-01T00:00:00Z');
      await page.locator(SEL.dateRangeTo).fill('2026-08-02T00:00:00Z');
      await page.locator(SEL.dateRangeApply).click();
      await expect(page.locator(SEL.rangeDialog)).toHaveCount(0);
      const range = page.locator(SEL.dateRangeTrigger);
      const run = page.locator(SEL.runButton);
      const box = await run.boundingBox();
      expect(box).not.toBeNull();
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(width);
      expect(await range.getAttribute('title')).toBe(await range.innerText());
      await expect(range).toHaveAttribute('title', /2026-08-01.*2026-08-02/);
      const before = await capturedQueryCount(request);
      await page.locator(SEL.cmContent).fill('service=nginx');
      await run.click();
      const query = await lastCapturedQuery(request, before + 1);
      expect(query.query).toContain('_time>="2026-08-01T00:00:00Z"');
      expect(query.query).toContain('_time<="2026-08-02T00:00:00Z"');
    });
  }

  test(`secondary text and degraded badges retain contrast in ${theme}`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.goto('/search?q=service%3Dnginx');
    await page.evaluate(theme => document.documentElement.setAttribute('data-theme', theme), theme);
    await expect(page.locator('.results-table thead th').first()).toBeVisible();
    await expect(page.locator('.facets .v .c').first()).toBeVisible();
    async function contrast(selector: string) {
      return page.locator(selector).evaluateAll(elements => {
        const ctx = document.createElement('canvas').getContext('2d')!;
        function rgba(color: string) {
          ctx.clearRect(0, 0, 1, 1); ctx.fillStyle = color; ctx.fillRect(0, 0, 1, 1);
          return Array.from(ctx.getImageData(0, 0, 1, 1).data).map((v, i) => i === 3 ? v / 255 : v);
        }
        function over(fg: number[], bg: number[]) {
          return fg.slice(0, 3).map((v, i) => v * fg[3] + bg[i] * (1 - fg[3])).concat(1);
        }
        function luminance(rgb: number[]) {
          return rgb.slice(0, 3).map(v => v / 255).map(v => v <= .04045 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4)
            .reduce((n, v, i) => n + v * [.2126, .7152, .0722][i], 0);
        }
        return elements.filter(el => el.getClientRects().length).map(el => {
          const ancestors: Element[] = [];
          for (let n: Element | null = el; n; n = n.parentElement) ancestors.unshift(n);
          let bg = [255, 255, 255, 1];
          for (const n of ancestors) bg = over(rgba(getComputedStyle(n).backgroundColor), bg);
          // Facet bars are a painted sibling behind the count. Measure that
          // tinted surface too, including when a shorter bar stops before it.
          const bar = el.closest('.v')?.querySelector('.bar');
          const backgrounds = bar ? [bg, over(rgba(getComputedStyle(bar).backgroundColor), bg)] : [bg];
          const color = rgba(getComputedStyle(el).color);
          return Math.min(...backgrounds.map(bg => {
            const l1 = luminance(over(color, bg)), l2 = luminance(bg);
            return (Math.max(l1, l2) + .05) / (Math.min(l1, l2) + .05);
          }));
        });
      });
    }
    for (const selector of ['.results-table thead th', '.facets .v .c']) {
      const ratios = await contrast(selector);
      expect(ratios.length).toBeGreaterThan(0);
      expect(Math.min(...ratios), selector).toBeGreaterThanOrEqual(4.5);
    }
    await page.goto('/search/schema?svc=nginx');
    await page.evaluate(theme => document.documentElement.setAttribute('data-theme', theme), theme);
    await expect(page.locator('.bdg.warn').first()).toBeVisible();
    const ratios = await contrast('.bdg.warn');
    expect(ratios.length).toBeGreaterThan(0);
    expect(Math.min(...ratios)).toBeGreaterThanOrEqual(4.5);
  });
}

test('keyboard shortcuts bypass the rail and facets without changing the search', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.cmContent)).toBeVisible();
  const url = page.url();
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to main content', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator('#fleet-main-content')).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to query editor', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.cmContent)).toBeFocused();
  await page.getByRole('link', { name: 'Skip to results', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('tab', { name: /^Events/ })).toBeFocused();
  await page.keyboard.press('ArrowRight');
  await expect(page.getByRole('tab', { name: 'Visualization' })).toBeFocused();
  expect(page.url()).toBe(url);
  await expect(page.locator(SEL.cmContent)).toHaveText('service=nginx');
});

test('drawer uses valid dialog semantics and keeps the background reachable', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema?svc=nginx');
  const drawer = page.locator(SEL.drawerPanel);
  await expect(drawer).toBeVisible();
  await expect(drawer).toHaveJSProperty('tagName', 'DIV');
  await expect(drawer).toHaveAttribute('role', 'dialog');
  await expect(drawer).not.toHaveAttribute('aria-modal', 'true');
  await page.locator(SEL.railHistoryLink).focus();
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/\/search\/history$/);
  await expect(drawer).toHaveCount(0);
});
