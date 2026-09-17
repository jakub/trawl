// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import { SEL } from '../selectors';
import { selectTheme } from '../theme';
import { themeFixtures } from '../theme-fixtures';
import { installThemeProbe, settleTheme, themeProbe } from '../theme-probe';

test('shared fixture table exercises the actual content-hashed bootstrap without writes', async ({ page, request }) => {
  // This single test loads the real asset for all 160 shared parsing cases.
  test.setTimeout(60_000);
  const html = await (await request.get('/')).text();
  await page.route('**/__theme-fixtures', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><html data-theme="light"><head></head><body></body></html>' }));
  await page.goto('/__theme-fixtures');
  // Read the built DOM attribute to decode any HTML escaping of the asset URL.
  const src = await page.evaluate(html => {
    return new DOMParser().parseFromString(html, 'text/html').querySelector<HTMLScriptElement>('script[src*="theme-bootstrap-"]')!.getAttribute('src')!;
  }, html);
  for (const fixture of themeFixtures) {
    const result = await page.evaluate(async ({ fixture, src }) => {
      const key = 'trawl.ui';
      fixture.raw === null ? localStorage.removeItem(key) : localStorage.setItem(key, fixture.raw);
      document.documentElement.dataset.theme = 'unset';
      const match = window.matchMedia;
      const write = Storage.prototype.setItem;
      const remove = Storage.prototype.removeItem;
      const clear = Storage.prototype.clear;
      let writes = 0;
      Storage.prototype.setItem = function (...args) { writes++; return write.apply(this, args); };
      Storage.prototype.removeItem = function (...args) { writes++; return remove.apply(this, args); };
      Storage.prototype.clear = function () { writes++; return clear.call(this); };
      window.matchMedia = (() => {
        if (fixture.media === 'throws') throw new Error('media unavailable');
        if (fixture.media === 'unavailable') return null;
        return { matches: fixture.media === 'dark' };
      }) as typeof window.matchMedia;
      const script = document.createElement('script');
      script.src = src;
      script.dataset.storageKey = key;
      try {
        await new Promise<void>((resolve, reject) => {
          script.onload = () => resolve();
          script.onerror = () => reject(new Error('bootstrap failed to load'));
          document.head.append(script);
        });
        return { resolved: document.documentElement.dataset.theme, raw: localStorage.getItem(key), writes };
      } finally {
        window.matchMedia = match;
        Storage.prototype.setItem = write;
        Storage.prototype.removeItem = remove;
        Storage.prototype.clear = clear;
        script.remove();
      }
    }, { fixture, src });
    expect(result, fixture.id).toEqual({ resolved: fixture.resolved, raw: fixture.raw, writes: 0 });
  }
});

for (const colorScheme of ['light', 'dark'] as const) {
  test(`System remains checked under OS ${colorScheme}, initialization and Enter write nothing`, async ({ page }, testInfo) => {
    await page.emulateMedia({ colorScheme });
    await installThemeProbe(page, { raw: null });
    await page.goto('/search');
    await expect(page.locator(SEL.topbarUser)).toBeEnabled();
    await page.locator(SEL.topbarUser).focus();
    await page.keyboard.press('Enter');
    const system = page.getByRole('menuitemradio', { name: 'System', exact: true });
    await expect(system).toBeFocused();
    await expect(system).toHaveAttribute('aria-checked', 'true');
    await expect(page.locator('html')).toHaveAttribute('data-theme', colorScheme);
    await expect(page.locator('.user-menu')).toHaveCSS('opacity', '1');
    const capture = testInfo.outputPath(`system-${colorScheme}.png`);
    await page.screenshot({ path: capture, animations: 'disabled' });
    await testInfo.attach(`system-${colorScheme}`, { path: capture, contentType: 'image/png' });
    await page.keyboard.press('Enter');
    await settleTheme(page);
    expect((await themeProbe(page)).writes).toEqual([]);
    await expect(page.locator(SEL.topbarUser)).toBeFocused();
  });
}

test('System follows live OS changes, fixed overrides ignore them, and snapshots retain system', async ({ page }) => {
  await installThemeProbe(page, { raw: null });
  await page.goto('/search');
  await expect(page.locator(SEL.topbarUser)).toBeEnabled();
  expect((await themeProbe(page)).active).toBe(1);
  await page.emulateMedia({ colorScheme: 'dark' });
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  expect((await themeProbe(page)).writes).toEqual([]);
  await selectTheme(page, 'Light');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.emulateMedia({ colorScheme: 'light' });
  await page.emulateMedia({ colorScheme: 'dark' });
  await settleTheme(page);
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  expect((await themeProbe(page)).writes.map(raw => JSON.parse(raw).theme)).toEqual(['light']);
  await selectTheme(page, 'Dark');
  await page.emulateMedia({ colorScheme: 'light' });
  await settleTheme(page);
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await selectTheme(page, 'System');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.locator(SEL.sidebarCollapse).click();
  await expect.poll(async () => (await themeProbe(page)).writes.length).toBe(4);
  const snapshots = (await themeProbe(page)).writes.map(raw => JSON.parse(raw));
  expect(snapshots.map(snapshot => snapshot.theme)).toEqual(['light', 'dark', 'system', 'system']);
  expect(snapshots[3]).toMatchObject({ sidebar: 'collapsed', rowstyle: 'bordered', details: 'inline', rows: 'compact' });
});

for (const choice of ['Light', 'Dark'] as const) {
  test(`saved ${choice} opens checked and Enter preserves it without a write`, async ({ page }) => {
    await installThemeProbe(page, { raw: JSON.stringify({ theme: choice.toLowerCase() }) });
    await page.goto('/search');
    await expect(page.locator(SEL.topbarUser)).toBeEnabled();
    await page.locator(SEL.topbarUser).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByRole('menuitemradio', { name: choice, exact: true })).toBeFocused();
    await expect(page.getByRole('menuitemradio', { name: choice, exact: true })).toHaveAttribute('aria-checked', 'true');
    await expect(page.getByRole('menuitemradio', { name: 'System', exact: true })).toHaveAttribute('aria-checked', 'false');
    await page.keyboard.press('Enter');
    await settleTheme(page);
    expect((await themeProbe(page)).writes).toEqual([]);
    await expect(page.getByRole('menu')).toHaveCount(0);
    await expect(page.locator(SEL.topbarUser)).toBeFocused();
  });
}

for (const storage of ['blocked', 'read-fails', 'quota'] as const) {
  test(`${storage} storage leaves the session usable and does not retry on OS changes or reselect`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await installThemeProbe(page, { raw: null, storage });
    await page.goto('/search');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await selectTheme(page, 'Light');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    const before = (await themeProbe(page)).writes;
    await selectTheme(page, 'Light');
    await page.emulateMedia({ colorScheme: 'light' });
    await settleTheme(page);
    expect((await themeProbe(page)).writes).toEqual(before);
    expect(before.length).toBe(storage === 'blocked' ? 0 : 1);
    await selectTheme(page, 'System');
    const systemAttempts = (await themeProbe(page)).writes;
    await page.emulateMedia({ colorScheme: 'dark' });
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await selectTheme(page, 'System');
    await settleTheme(page);
    expect((await themeProbe(page)).writes).toEqual(systemAttempts);
    if (storage === 'quota') expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBeNull();
  });
}

for (const media of ['unavailable', 'throws'] as const) {
  test(`${media} media access resolves System to Light`, async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await installThemeProbe(page, { raw: null, media });
    await page.goto('/search');
    await expect(page.locator(SEL.topbarUser)).toBeEnabled();
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    expect((await themeProbe(page)).writes).toEqual([]);
    await selectTheme(page, 'Dark');
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  });
}

test('runtime samples after registering its listener and preserves malformed storage', async ({ page }) => {
  await installThemeProbe(page, { raw: '{broken', changeDuringRegistration: true });
  await page.goto('/search');
  await expect(page.locator(SEL.topbarUser)).toBeEnabled();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  expect(await page.evaluate(() => localStorage.getItem('trawl.ui'))).toBe('{broken');
  expect(await themeProbe(page)).toMatchObject({ registrations: 1, active: 1, writes: [] });
});
