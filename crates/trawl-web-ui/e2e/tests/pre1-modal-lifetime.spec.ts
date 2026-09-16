// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';

const created = JSON.parse(readFileSync(`${__dirname}/../harness/wire/saved-created.json`, 'utf8'));

for (const origin of ['search', 'history'] as const) {
  for (const outcome of ['success', 'failure'] as const) {
    test(`late Save ${outcome} from ${origin} preserves a replacement dialog`, async ({ page, request }) => {
      await resetScenario(request, 'corpus');
      let release!: () => void;
      const held = new Promise<void>(resolve => { release = resolve; });
      let calls = 0;
      await page.route('**/api/v1/saved', async route => {
        if (route.request().method() !== 'POST') return route.continue();
        calls += 1;
        const body = route.request().postDataJSON();
        await held;
        await route.fulfill(outcome === 'success'
          ? { json: { ...created, ...body } }
          : { status: 503, json: { error: { code: 'unavailable', message: 'Save unavailable' } } });
      });
      await page.goto(origin === 'search' ? '/search?q=service%3Dnginx' : '/search/history');
      const open = async () => {
        if (origin === 'search') await page.locator('.editor-tools').getByRole('button', { name: 'Save as Net', exact: true }).click();
        else await page.getByRole('button', { name: 'Save as Net', exact: true }).first().click();
      };
      await open();
      await page.getByLabel('Name', { exact: true }).fill('first-request');
      await page.getByRole('dialog').getByRole('button', { name: 'Save as Net', exact: true }).click();
      await expect.poll(() => calls).toBe(1);
      await page.keyboard.press('Escape');
      await expect(page.getByRole('dialog')).toHaveCount(0);
      await open();
      await page.getByLabel('Name', { exact: true }).fill('replacement-draft');
      release();
      await expect(page.locator('.toast')).toContainText(outcome === 'success' ? 'Saved as net' : "Couldn't save");
      await expect(page.getByRole('dialog')).toBeVisible();
      await expect(page.getByLabel('Name', { exact: true })).toHaveValue('replacement-draft');
      expect(calls).toBe(1);
    });
  }
}

for (const outcome of ['success', 'failure'] as const) {
  test(`late Export ${outcome} preserves a replacement dialog and reports once`, async ({ page }) => {
    let release!: () => void;
    const held = new Promise<void>(resolve => { release = resolve; });
    let calls = 0;
    const downloads: string[] = [];
    page.on('download', download => downloads.push(download.suggestedFilename()));
    await page.route('**/api/v1/export*', async route => {
      calls += 1;
      await held;
      await route.fulfill(outcome === 'success'
        ? { status: 200, body: 'service,count\nnginx,1\n', headers: { 'content-type': 'text/csv', 'content-disposition': 'attachment; filename="lifetime.csv"' } }
        : { status: 503, body: 'Export unavailable' });
    });
    await page.goto('/search?q=service%3Dnginx');
    await page.getByRole('button', { name: 'Export', exact: true }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Download', exact: true }).click();
    await expect.poll(() => calls).toBe(1);
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.getByRole('button', { name: 'Export', exact: true }).click();
    release();
    await expect(page.locator('.toast')).toContainText(outcome === 'success' ? 'Exported' : 'Export failed');
    await expect(page.getByRole('dialog')).toBeVisible();
    if (outcome === 'success') await expect.poll(() => downloads).toEqual(['lifetime.csv']);
    else expect(downloads).toEqual([]);
    expect(calls).toBe(1);
  });
}

test('Save completion survives navigation away from its page', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  let calls = 0;
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    calls += 1;
    await held;
    await route.fulfill({ json: { ...created, name: 'navigation-save' } });
  });
  await page.goto('/search/history');
  await page.getByRole('button', { name: 'Save as Net', exact: true }).first().click();
  await page.getByLabel('Name', { exact: true }).fill('navigation-save');
  await page.getByRole('dialog').getByRole('button', { name: 'Save as Net', exact: true }).click();
  await expect.poll(() => calls).toBe(1);
  await page.keyboard.press('Escape');
  await page.locator('.rail a[href="/search"]').click();
  release();
  await expect(page.locator('.toast')).toContainText('Saved as net');
  await expect(page).toHaveURL(/\/search$/);
});
