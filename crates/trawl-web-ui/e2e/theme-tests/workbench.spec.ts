// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '@playwright/test';
import { SEL } from '../selectors';
import { installThemeProbe, settleTheme, themeProbe } from '../theme-probe';

test('login choices recolor the same mounted backdrop, then disposal removes the sole listener', async ({ page }) => {
  const errors: Error[] = [];
  page.on('pageerror', error => errors.push(error));
  await installThemeProbe(page, { key: 'fleet-ui-demo:prefs', raw: null });
  await page.addInitScript(() => {
    const uniform = WebGL2RenderingContext.prototype.uniform4fv;
    (window as any).__themeUniforms = [];
    WebGL2RenderingContext.prototype.uniform4fv = function (...args) {
      (window as any).__themeUniforms.push(Array.from(args[1]));
      return uniform.apply(this, args);
    };
  });
  await page.goto('/login');
  await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
  const canvasSelector = `${SEL.atmosphere} canvas`;
  await expect(page.locator(canvasSelector)).toHaveCount(1);
  const original = await page.locator(canvasSelector).elementHandle();
  const choices = page.getByRole('group', { name: 'Demo theme', exact: true });
  let previousUniforms = '';
  for (const [choice, theme] of [['Dark', 'dark'], ['Light', 'light'], ['System', 'light']] as const) {
    await page.evaluate(() => { (window as any).__themeUniforms = []; });
    await choices.getByRole('button', { name: choice, exact: true }).click();
    await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
    await expect(choices.getByRole('button', { name: choice, exact: true })).toHaveAttribute('aria-pressed', 'true');
    expect(await original!.evaluate((canvas, selector) => canvas === document.querySelector(selector), canvasSelector)).toBe(true);
    if (choice !== 'System') {
      await expect.poll(() => page.evaluate(() => (window as any).__themeUniforms.length)).toBeGreaterThan(0);
      const uniforms = await page.evaluate(() => JSON.stringify((window as any).__themeUniforms));
      expect(uniforms).not.toBe(previousUniforms);
      previousUniforms = uniforms;
    }
  }
  await page.evaluate(() => { (window as any).__themeUniforms = []; });
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await expect.poll(() => page.evaluate(() => (window as any).__themeUniforms.length)).toBeGreaterThan(0);
  expect(await page.evaluate(() => JSON.stringify((window as any).__themeUniforms))).not.toBe(previousUniforms);
  expect(await original!.evaluate((canvas, selector) => canvas === document.querySelector(selector), canvasSelector)).toBe(true);
  expect(await themeProbe(page)).toMatchObject({ registrations: 1, removals: 0, active: 1 });
  await page.getByRole('button', { name: 'Dispose demo', exact: true }).click();
  await expect(page.getByRole('group', { name: 'Demo theme', exact: true })).toHaveCount(0);
  await expect(page.locator(SEL.atmosphere)).toHaveCount(0);
  await expect.poll(async () => (await themeProbe(page)).active).toBe(0);
  const disposed = await themeProbe(page);
  expect(disposed.removals).toBe(1);
  await page.emulateMedia({ colorScheme: 'light' });
  await settleTheme(page);
  expect(await themeProbe(page)).toEqual(disposed);
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  // Storage is changed only by the test while no installation exists. A new
  // installation must reread it rather than revive the disposed session state.
  await page.evaluate(() => localStorage.setItem('fleet-ui-demo:prefs', '{"theme":"light"}'));
  const beforeInstall = await themeProbe(page);
  await page.getByRole('button', { name: 'Install demo', exact: true }).click();
  await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  const reinstalled = await themeProbe(page);
  expect(reinstalled).toMatchObject({ registrations: 2, removals: 1, active: 1 });
  expect(reinstalled.writes).toEqual(beforeInstall.writes);
  expect(errors).toEqual([]);
});

test('losing and restoring identity does not reopen the account menu', async ({ page }) => {
  await page.goto('/');
  const trigger = page.locator(SEL.topbarUser);
  await trigger.click();
  await expect(page.getByRole('menuitemradio', { name: 'System', exact: true })).toBeFocused();
  // Preserve the open menu until identity changes, without outside-mousedown
  // dismissal masking the behavior under test.
  await page.getByRole('button', { name: 'Clear demo identity', exact: true }).dispatchEvent('click');
  await expect(page.getByRole('menu')).toHaveCount(0);
  await expect(trigger).toBeDisabled();
  await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  await page.getByRole('button', { name: 'Restore demo identity', exact: true }).click();
  await expect(trigger).toBeEnabled();
  await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  await expect(page.getByRole('menu')).toHaveCount(0);
});
