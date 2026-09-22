// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, trackIntervals, intervalCount } from '../fixtures';

for (const tab of ['Events', 'Visualization']) {
  test(`rejected raw live stream exposes retry in ${tab} and preserves search context`, async ({ page, request }, testInfo) => {
    await resetScenario(request, 'stream-burst');
    await trackIntervals(page);
    let failed = true;
    const queries: string[] = [];
    await page.route('**/api/v1/stream?*', route => {
      queries.push(new URL(route.request().url()).searchParams.get('query')!);
      return failed ? route.fulfill({ status: 400, json: { error: 'unavailable' } }) : route.continue();
    });
    await page.goto('/search?q=service%3Dnginx&mode=live&f=v1.W3sib3AiOiIrIiwiZmllbGQiOiJob3N0IiwidmFsdWUiOiJ3ZWItMDEifV0');
    await page.getByRole('tab', { name: new RegExp(`^${tab}`) }).click();
    await expect(page.getByText('Live stream unavailable.', { exact: false })).toBeVisible();
    await expect(page.getByText('Streaming', { exact: false })).toHaveCount(0);
    await page.screenshot({ path: testInfo.outputPath(`rejected-${tab.toLowerCase()}.png`) });
    const editor = page.locator('.dsl-editor').getByRole('textbox');
    await editor.fill('service=postgres');
    const url = page.url();
    failed = false;
    await page.getByRole('button', { name: 'Retry live stream' }).click();
    await expect(page.getByRole('button', { name: 'Retry live stream' })).toHaveCount(0);
    await page.getByRole('tab', { name: /^Events/ }).click();
    await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
    expect(queries).toEqual(['host="web-01" service=nginx', 'host="web-01" service=nginx']);
    expect(page.url()).toBe(url);
    await expect(editor).toHaveText('service=postgres');
    expect(await intervalCount(page, 16)).toBe(1);
    await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(1);
    await page.locator('a[href="/search/history"]').first().click();
    await expect.poll(async () => (await (await request.get('/__ctl/state')).json()).sse.open).toBe(0);
    expect(await intervalCount(page, 16)).toBe(0);
    const opens = (await (await request.get('/__ctl/state')).json()).sse.opens;
    await page.waitForTimeout(500);
    expect((await (await request.get('/__ctl/state')).json()).sse.opens).toBe(opens);
  });
}

test('a disconnected raw live stream retains received rows and recovers after retry', async ({ page, request }, testInfo) => {
  await resetScenario(request, 'stream-burst');
  let failed = false;
  await page.route('**/api/v1/stream?*', route => failed
    ? route.fulfill({ status: 400, json: { error: 'unavailable' } }) : route.continue());
  await page.goto('/search?q=service%3Dnginx&mode=live');
  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
  const receivedTable = await page.locator('.results-table').elementHandle();
  failed = true;
  expect((await request.post('/__ctl/stream/drop')).ok()).toBe(true);
  await expect(page.getByRole('button', { name: 'Retry live stream' })).toBeVisible();
  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
  await expect(page.locator('.results-table tbody tr')).toHaveCount(5000);
  expect(await receivedTable!.evaluate(table => table.isConnected)).toBe(true);
  await expect(page.getByText('Live stream unavailable.', { exact: false })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('disconnected-events.png') });
  await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Retry live stream' })).toBeVisible();
  await expect(page.locator('.results-table')).toHaveCount(0);
  await page.getByRole('tab', { name: /^Events/ }).click();
  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
  failed = false;
  await page.getByRole('button', { name: 'Retry live stream' }).click();
  await expect(page.getByText('burst-5999', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Retry live stream' })).toHaveCount(0);
});
