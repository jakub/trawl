// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, type Page, type TestInfo } from '@playwright/test';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { installThemeProbe, settleTheme, themeProbe, type ProbeOptions } from '../theme-probe';

function namespace(info: TestInfo): string {
  return info.project.name === 'workbench' ? 'fleet-ui-demo:prefs' : 'trawl.ui';
}

async function appearance(page: Page) {
  return page.evaluate(() => ({
    background: getComputedStyle(document.body).backgroundColor,
    scheme: getComputedStyle(document.documentElement).colorScheme,
    inlineScheme: document.documentElement.style.colorScheme,
  }));
}

async function styled(page: Page): Promise<void> {
  await expect.poll(async () => page.evaluate(() => !!document.body
    && !!getComputedStyle(document.documentElement).getPropertyValue('--bg').trim())).toBe(true);
  await settleTheme(page);
}

async function holdWasm(page: Page) {
  let release!: () => void;
  let held = false;
  const gate = new Promise<void>(resolve => { release = resolve; });
  await page.route(/\.wasm(?:\?|$)/, async route => {
    held = true;
    await gate;
    await route.continue();
  });
  return { release, held: () => held };
}

test('built bootstrap is blocking, namespaced, content-hashed and served with production CSP and immutable JavaScript headers', async ({ page, request }, info) => {
  const hold = await holdWasm(page);
  try {
    const response = await page.goto('/login', { waitUntil: 'commit' });
    await styled(page);
    const contract = await page.evaluate(() => {
      const script = document.querySelector<HTMLScriptElement>('script[src*="theme-bootstrap-"]')!;
      const nodes = [...document.querySelectorAll('script, link')];
      const laterAssets = nodes.filter(node => node.matches('link[rel="stylesheet"], script[type="module"], link[rel="modulepreload"]'));
      return {
        count: document.querySelectorAll('script[src*="theme-bootstrap-"]').length,
        src: script.getAttribute('src')!, key: script.dataset.storageKey,
        type: script.getAttribute('type'), async: script.hasAttribute('async'), defer: script.hasAttribute('defer'),
        hasStyles: laterAssets.some(node => node.matches('link[rel="stylesheet"]')),
        hasWasmLoader: laterAssets.some(node => node.matches('script[type="module"]')),
        beforeStylesAndWasm: laterAssets
          .every(node => nodes.indexOf(script) < nodes.indexOf(node)),
      };
    });
    expect(contract).toMatchObject({ count: 1, key: namespace(info), async: false, defer: false, hasStyles: true, hasWasmLoader: true, beforeStylesAndWasm: true });
    expect([null, '', 'text/javascript']).toContain(contract.type);
    const url = new URL(contract.src, page.url());
    expect(url.origin).toBe(new URL(page.url()).origin);
    expect(url.pathname).toMatch(/\/theme-bootstrap-[a-f0-9]{8,}\.js$/);
    const asset = await request.get(url.href, { headers: { 'Accept-Encoding': 'identity' } });
    expect(asset.status()).toBe(200);
    const manifest = JSON.parse(readFileSync(process.env.THEME_ASSET_MANIFEST
      ?? path.resolve(__dirname, '../../../../e2e-artifacts/theme-builds/theme-assets.json'), 'utf8'));
    const expectedAsset = info.project.name === 'workbench' ? manifest.workbench : manifest.trawl;
    expect({ src: contract.src, key: contract.key, sha256: createHash('sha256').update(await asset.body()).digest('hex') }).toEqual(expectedAsset);
    expect(asset.headers()['content-type']).toMatch(/^(text|application)\/javascript/);
    expect(asset.headers()['cache-control']).toBe('public, max-age=31536000, immutable');
    expect(response!.headers()['content-security-policy']).toContain("script-src 'self'");
    expect(response!.headers()['x-content-type-options']).toBe('nosniff');
    await info.attach('emitted-index.html', { body: await response!.body(), contentType: 'text/html' });
    await info.attach('bootstrap.js', { body: await asset.body(), contentType: 'text/javascript' });
    await info.attach('asset-contract.json', { body: JSON.stringify({ contract, documentHeaders: response!.headers(), assetHeaders: asset.headers() }, null, 2), contentType: 'application/json' });
  } finally { hold.release(); }
});

const cases: { id: string; os: 'light' | 'dark'; raw: string | null; expected: 'light' | 'dark'; storage?: ProbeOptions['storage']; media?: ProbeOptions['media'] }[] = [
  { id: 'missing-os-light', os: 'light', raw: null, expected: 'light' },
  { id: 'missing-os-dark', os: 'dark', raw: null, expected: 'dark' },
  { id: 'system-os-light', os: 'light', raw: '{"theme":"system"}', expected: 'light' },
  { id: 'system-os-dark', os: 'dark', raw: '{"theme":"system"}', expected: 'dark' },
  { id: 'fixed-light-os-dark', os: 'dark', raw: '{"theme":"light"}', expected: 'light' },
  { id: 'fixed-dark-os-light', os: 'light', raw: '{"theme":"dark"}', expected: 'dark' },
  { id: 'corrupt-os-dark', os: 'dark', raw: '{broken', expected: 'dark' },
  { id: 'blocked-storage-os-dark', os: 'dark', raw: '{"theme":"light"}', storage: 'blocked', expected: 'dark' },
  { id: 'read-failure-os-dark', os: 'dark', raw: '{"theme":"light"}', storage: 'read-fails', expected: 'dark' },
  { id: 'unavailable-media', os: 'dark', raw: null, media: 'unavailable', expected: 'light' },
  { id: 'throwing-media', os: 'dark', raw: null, media: 'throws', expected: 'light' },
];

for (const entry of cases) {
  test(`first styled appearance and runtime agree: ${entry.id}`, async ({ page }, info) => {
    const errors: Error[] = [];
    page.on('pageerror', error => errors.push(error));
    await page.emulateMedia({ colorScheme: entry.os });
    await installThemeProbe(page, { key: namespace(info), raw: entry.raw, storage: entry.storage, media: entry.media });
    const hold = await holdWasm(page);
    try {
      await page.goto('/login', { waitUntil: 'commit' });
      await styled(page);
      await expect.poll(hold.held).toBe(true);
      await expect(page.getByLabel('API key', { exact: true })).toHaveCount(0);
      const before = await appearance(page);
      expect(before.scheme).toBe(entry.expected);
      expect(before.background).not.toBe('rgba(0, 0, 0, 0)');
      expect(before.inlineScheme).toBe('');
      // Compare against CSS's own expected token under an independent attribute
      // sample; restore before releasing Wasm. This checks actual painted color.
      const expectedBackground = await page.evaluate(expected => {
        const previous = document.documentElement.getAttribute('data-theme');
        document.documentElement.setAttribute('data-theme', expected);
        const background = getComputedStyle(document.body).backgroundColor;
        if (previous === null) document.documentElement.removeAttribute('data-theme');
        else document.documentElement.setAttribute('data-theme', previous);
        return background;
      }, entry.expected);
      expect(before.background).toBe(expectedBackground);
      await info.attach('before-wasm.png', { body: await page.screenshot(), contentType: 'image/png' });
      hold.release();
      await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
      await settleTheme(page);
      expect(await appearance(page)).toEqual(before);
      expect((await themeProbe(page)).writes).toEqual([]);
      expect(errors).toEqual([]);
      await info.attach('after-install.png', { body: await page.screenshot(), contentType: 'image/png' });
      await info.attach('appearance.json', { body: JSON.stringify({ before, after: await appearance(page) }), contentType: 'application/json' });
    } finally { hold.release(); }
  });
}

test('changed media at handoff uses the runtime current state', async ({ page }, info) => {
  await installThemeProbe(page, { key: namespace(info), raw: null });
  const hold = await holdWasm(page);
  try {
    await page.goto('/login', { waitUntil: 'commit' });
    await styled(page);
    expect((await appearance(page)).scheme).toBe('light');
    await page.emulateMedia({ colorScheme: 'dark' });
    expect((await appearance(page)).scheme).toBe('light');
    hold.release();
    await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
    await expect.poll(async () => (await appearance(page)).scheme).toBe('dark');
    expect((await themeProbe(page)).writes).toEqual([]);
  } finally { hold.release(); }
});

test('changed stored preference at handoff is reread without an initialization write', async ({ page }, info) => {
  const key = namespace(info);
  await installThemeProbe(page, { key, raw: '{"theme":"light"}' });
  const hold = await holdWasm(page);
  try {
    await page.goto('/login', { waitUntil: 'commit' });
    await styled(page);
    expect((await appearance(page)).scheme).toBe('light');
    await page.evaluate(key => localStorage.setItem(key, '{"theme":"dark"}'), key);
    const writes = (await themeProbe(page)).writes;
    hold.release();
    await expect(page.getByLabel('API key', { exact: true })).toBeVisible();
    await expect.poll(async () => (await appearance(page)).scheme).toBe('dark');
    expect((await themeProbe(page)).writes).toEqual(writes);
  } finally { hold.release(); }
});

test('disabled JavaScript retains the visible static Light fallback', async ({ browser, baseURL }, info) => {
  const context = await browser.newContext({ javaScriptEnabled: false, colorScheme: 'dark' });
  try {
    const page = await context.newPage();
    await page.goto(`${baseURL}/login`);
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
    await expect(page.locator('html')).toHaveCSS('color-scheme', 'light');
    await expect(page.locator('body')).not.toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');
    await info.attach('javascript-disabled-static-light.png', { body: await page.screenshot(), contentType: 'image/png' });
  } finally { await context.close(); }
});

test('failed bootstrap retains static Light before Wasm', async ({ page }, info) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.route('**/theme-bootstrap-*.js', route => route.abort());
  const hold = await holdWasm(page);
  try {
    await page.goto('/login', { waitUntil: 'commit' });
    await styled(page);
    expect((await appearance(page)).scheme).toBe('light');
    expect((await appearance(page)).background).not.toBe('rgba(0, 0, 0, 0)');
    await info.attach('bootstrap-failed-static-light.png', { body: await page.screenshot(), contentType: 'image/png' });
  } finally { hold.release(); }
});
