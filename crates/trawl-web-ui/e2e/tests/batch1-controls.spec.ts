// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, capturedSavedRequests, CORPUS } from '../fixtures';
import { SEL } from '../selectors';

for (const modifier of ['Control', 'Meta']) {
  test(`named search and save dialog advertise ${modifier}+Enter and submit once`, async ({ page, request }) => {
    await resetScenario(request, 'saved-success');
    await page.goto('/search');
    await expect(page.getByRole('textbox', { name: 'Search query', exact: true })).toBeVisible();
    await page.locator(SEL.saveAction).click();
    const dialog = page.getByRole('dialog', { name: 'Save query as net', exact: true });
    await expect(dialog).toBeVisible();
    await expect(dialog.locator('.hint')).toContainText('Ctrl/⌘ + Enter');
    await expect(dialog.getByRole('button', { name: 'Save as net', exact: true })).toBeVisible();
    await dialog.getByLabel('Name', { exact: true }).fill('named schedule');
    await page.keyboard.press('Enter');
    await expect(dialog).toBeVisible();
    expect(await capturedSavedRequests(request)).toEqual([]);
    await page.keyboard.press(`${modifier}+Enter`);
    await expect(dialog).toHaveCount(0);
    expect(await capturedSavedRequests(request)).toHaveLength(1);
    await expect(page.locator(SEL.saveAction)).toBeFocused();
  });
}

for (const colorScheme of ['light', 'dark'] as const) {
  test(`schedule names, helper and visible switch focus in ${colorScheme}`, async ({ page, request }) => {
    await page.emulateMedia({ colorScheme });
    await resetScenario(request, 'corpus');
    await page.goto('/jobs/nets');
    if (colorScheme === 'dark') {
      await page.getByRole('button', { name: 'Theme light: switch to dark theme', exact: true }).click();
    }
    await page.goto(`/jobs/nets?net=${CORPUS.netId}&ntab=query`);
    await expect(page.locator('html')).toHaveAttribute('data-theme', colorScheme);
    const drawer = page.getByRole('dialog', { name: /errors by host/ });
    await expect(drawer).toBeVisible();
    await drawer.getByRole('button', { name: 'Edit', exact: true }).click();
    await expect(drawer.getByRole('textbox', { name: 'Query', exact: true })).toBeVisible();
    await drawer.getByRole('button', { name: '+ Add Schedule', exact: true }).click();
    const max = drawer.getByRole('spinbutton', { name: 'Max runs', exact: true });
    await expect(max).toHaveAccessibleDescription('(blank = unlimited)');
    await expect(drawer.getByRole('textbox', { name: 'Interval', exact: true })).toBeVisible();
    await expect(drawer.getByRole('button', { name: 'Save schedule', exact: true })).toBeVisible();
    await max.focus();
    await page.keyboard.press('Tab');
    const toggle = drawer.getByRole('checkbox', { name: 'Schedule enabled', exact: true });
    await expect(toggle).toBeFocused();
    const track = drawer.locator('.toggle-slider');
    await expect(track).toHaveCSS('outline-style', 'solid');
    await expect(track).toHaveCSS('outline-width', '2px');
    // The outline is offset from the track, so both of its edges touch
    // the actual schedule card background. Canvas converts computed OKLCH
    // and color-mix values to the sRGB pixels used for contrast.
    const contrast = await track.evaluate((el) => {
      const card = el.closest('.sd-card');
      if (!card) throw new Error('Focused switch is outside the schedule card');
      const canvas = document.createElement('canvas');
      canvas.width = canvas.height = 1;
      const ctx = canvas.getContext('2d');
      if (!ctx) throw new Error('Canvas color conversion unavailable');
      ctx.fillStyle = getComputedStyle(card).backgroundColor;
      ctx.fillRect(0, 0, 1, 1);
      const background = [...ctx.getImageData(0, 0, 1, 1).data];
      if (background[3] !== 255) throw new Error('Schedule card background is not opaque');
      ctx.fillStyle = getComputedStyle(el).outlineColor;
      ctx.fillRect(0, 0, 1, 1);
      const outline = [...ctx.getImageData(0, 0, 1, 1).data];
      const luminance = (rgba: number[]) => {
        const linear = rgba.slice(0, 3).map((channel) => {
          const c = channel / 255;
          return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
        });
        return linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722;
      };
      const a = luminance(outline);
      const b = luminance(background);
      return { outline, background, ratio: (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05) };
    });
    console.log(`Schedule switch ${colorScheme}: ${JSON.stringify(contrast)}`);
    expect(contrast.ratio, `focused switch against schedule card in ${colorScheme}`).toBeGreaterThanOrEqual(3);
    const checked = await toggle.isChecked();
    await page.keyboard.press('Space');
    await expect(toggle).toBeChecked({ checked: !checked });
    await page.emulateMedia({ forcedColors: 'active' });
    await expect(track).toHaveCSS('outline-style', 'solid');
    await expect(track).toHaveCSS('outline-width', '2px');
    await page.keyboard.press('Tab');
    const save = drawer.getByRole('button', { name: 'Save schedule', exact: true });
    await expect(save).toBeFocused();
    await expect(save).toHaveCSS('outline-style', 'solid');
    await page.keyboard.press('Escape');
    await expect(drawer).toHaveCount(0);
    const search = page.getByRole('link', { name: 'Search', exact: true }).first();
    await search.focus();
    await expect(search).toHaveCSS('outline-style', 'solid');
  });
}

test('reduced motion stops actual overlays, toasts and live tail pulses', async ({ page, request, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await resetScenario(request, 'corpus');
  await page.goto('/search');
  await page.locator(SEL.editorTool).filter({ hasText: /^Share$/ }).click();
  await expect(page.locator('.toast').first()).toHaveCSS('animation-name', 'none');
  await page.locator(SEL.saveAction).click();
  await expect(page.locator('.modal')).toHaveCSS('animation-name', 'none');
  await expect(page.locator('.modal-scrim')).toHaveCSS('animation-name', 'none');
  await page.keyboard.press('Escape');
  await page.goto(`/search/schema?svc=${CORPUS.service}&stab=tail`);
  await expect(page.locator('.sd-drawer')).toHaveCSS('animation-name', 'none');
  await expect(page.locator('.sd-scrim')).toHaveCSS('animation-name', 'none');
  await expect(page.locator('.pulse > span')).toHaveCSS('animation-name', 'none');
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await expect(page.locator('.pulse > span')).toHaveCSS('animation-name', 'pulse-ring');
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await expect(page.locator('.pulse > span')).toHaveCSS('animation-name', 'none');
});

test('forced colors retains a visible navigation outline', async ({ page }) => {
  await page.emulateMedia({ forcedColors: 'active' });
  await page.goto('/search');
  const search = page.getByRole('link', { name: 'Search', exact: true }).first();
  await search.focus();
  await expect(search).toHaveCSS('outline-style', 'solid');
  await expect(search).toHaveCSS('outline-width', '2px');
});

test('Tab draws focus on the visible schedule switch track', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}&ntab=query`);
  const drawer = page.locator('.sd-drawer');
  await drawer.getByRole('button', { name: '+ Add Schedule', exact: true }).click();
  await drawer.getByRole('spinbutton').focus();
  await page.keyboard.press('Tab');
  await expect(drawer.getByRole('checkbox')).toBeFocused();
  await expect(drawer.locator('.toggle-slider')).toHaveCSS('outline-style', 'solid');
  await expect(drawer.locator('.toggle-slider')).toHaveCSS('outline-width', '2px');
});
