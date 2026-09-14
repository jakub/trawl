// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';

const wire = (name: string) => JSON.parse(readFileSync(`${__dirname}/../harness/wire/${name}.json`, 'utf8'));

for (const [path, label, columns] of [
  ['/search/history', 'Search history', 4],
  ['/jobs/nets', 'Saved queries', 4],
  ['/jobs/runs', 'Recent runs', 5],
  ['/search/schema', 'Services', 6],
] as const) {
  test(`management table relationships and local scroll: ${label}`, async ({ page, request }, testInfo) => {
    await resetScenario(request, 'corpus');
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(path);
    const table = page.getByRole('table', { name: label, exact: true });
    await expect(table).toBeVisible();
    await expect(table.getByRole('columnheader')).toHaveCount(columns);
    const row = table.locator('tbody > tr').first();
    await expect(row.locator(':scope > td')).toHaveCount(columns);
    const tracks = await table.evaluate(el => {
      const headers = [...el.querySelectorAll('thead th')];
      const cells = [...el.querySelectorAll('tbody > tr:first-child > td')];
      return headers.map((header, i) => ({
        header: header.getBoundingClientRect().left,
        cell: cells[i].getBoundingClientRect().left,
        headerInset: getComputedStyle(header).paddingInlineStart,
        cellInset: getComputedStyle(cells[i]).paddingInlineStart,
        scope: header.getAttribute('scope'),
        fits: header.scrollWidth <= header.clientWidth,
      }));
    });
    for (const track of tracks) {
      expect(track.scope).toBe('col');
      expect(track.fits).toBe(true);
      expect(Math.abs(track.header - track.cell)).toBeLessThan(1);
      expect(track.headerInset).toBe(track.cellInset);
    }
    await page.screenshot({ path: testInfo.outputPath('desktop.png') });
    for (const width of [390, 320]) {
      await page.setViewportSize({ width, height: 900 });
      const region = page.getByRole('region', { name: label, exact: true });
      await expect(region).toHaveAttribute('tabindex', '0');
      await expect(page.getByText('Scroll horizontally for more columns.', { exact: true })).toBeVisible();
      expect(await region.evaluate(el => el.scrollWidth > el.clientWidth)).toBe(true);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      await page.screenshot({ path: testInfo.outputPath(`narrow-${width}.png`) });
      await region.focus();
      await page.keyboard.press('ArrowRight');
      await expect.poll(() => region.evaluate(el => el.scrollLeft)).toBeGreaterThan(0);
    }
  });
}

test('long History query keeps metadata and native save action at the start', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const response = wire('history');
  response.entries = [{ ...response.entries[0], query: `host=${'longvalue'.repeat(180)} suffix` }];
  await page.route('**/api/v1/history?*', route => route.fulfill({ json: response }));
  await page.goto('/search/history');
  const row = page.getByRole('table', { name: 'Search history' }).locator('tbody > tr');
  await expect(row).toHaveCount(1);
  const tops = await row.locator('td').evaluateAll(cells => cells.map(cell => {
    const child = cell.firstElementChild;
    const range = document.createRange();
    range.selectNodeContents(child ?? cell);
    return range.getBoundingClientRect().top;
  }));
  expect(Math.max(...tops) - Math.min(...tops)).toBeLessThan(5);
  await row.locator('.row-stretch').focus();
  await page.keyboard.press('Tab');
  const save = row.getByRole('button', { name: 'Save as net' });
  await expect(save).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('dialog')).toBeVisible();
});

test('Nets renders distinct actual statuses at the same timestamp and keeps native sorting', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const original = wire('saved-queries').queries[0];
  const time = original.created_at;
  const statuses = ['success', 'error', 'timeout', 'unknown'];
  await page.route('**/api/v1/saved', route => route.fulfill({ json: { queries: statuses.map((status, i) => ({
    ...original, id: i + 1, name: `net ${i}`, schedule: {
      id: i + 1, saved_query_id: i + 1, interval: '5m', interval_secs: 300,
      enabled: true, created_at: time, updated_at: time, total_runs: 1, next_fire_at: time,
      last_run: { id: i + 1, query: original.query, status, started_at: time },
    },
  })) } }));
  await page.goto('/jobs/nets');
  const table = page.getByRole('table', { name: 'Saved queries' });
  for (const status of statuses) await expect(table.locator('.run-status').filter({ hasText: new RegExp(`^${status}$`) })).toBeVisible();
  await expect(table.locator('.status-dot')).toHaveCount(4);
  for (const dot of await table.locator('.status-dot').all()) await expect(dot).toHaveAttribute('aria-hidden', 'true');
  const sort = table.getByRole('button', { name: /^Sort by Name/ });
  await sort.focus();
  await page.keyboard.press('Enter');
  await expect(sort.locator('xpath=ancestor::th')).toHaveAttribute('aria-sort', 'descending');
  await expect(table.locator('tbody tr').first().getByRole('link')).toHaveText('net 3');
  await table.locator('tbody tr').first().getByRole('button', { name: 'Actions', exact: true }).click();
  await expect(page.getByRole('menu')).toBeVisible();
  await expect(page).not.toHaveURL(/net=/);
});

test('service freshness states expose the actual daily count and date', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const original = wire('service-schema-corpus').services[0];
  const day = new Date().toISOString().slice(0, 10);
  const services = [
    { ...original, name: 'recent', daily_event_counts: [{ date: day, count: 1 }] },
    { ...original, name: 'old', daily_event_counts: [{ date: '2020-01-01', count: 2 }] },
    { ...original, name: 'empty', daily_event_counts: [] },
  ];
  await page.route('**/api/v1/schema/services', route => route.fulfill({ json: { ...wire('service-schema-corpus'), services } }));
  await page.goto('/search/schema');
  await expect(page.locator('.freshness').filter({ hasText: `1 event on ${day}` })).toBeVisible();
  await expect(page.locator('.freshness').filter({ hasText: '2 events on 2020-01-01' })).toBeVisible();
  await expect(page.locator('.freshness').filter({ hasText: 'No daily activity recorded' })).toBeVisible();
  await page.getByRole('link', { name: 'recent', exact: true }).click();
  await expect(page.locator('.service-freshness')).toHaveText(`1 event on ${day}`);
});

test('Net run preview stays a native button and expands into a spanning table cell', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/jobs/nets?net=1&ntab=runs');
  const table = page.getByRole('table', { name: 'Net runs', exact: true });
  await expect(table.getByRole('columnheader')).toHaveCount(5);
  const button = table.locator('tbody tr').first().getByRole('button');
  await button.focus();
  await page.keyboard.press('Space');
  await expect(button).toHaveAttribute('aria-expanded', 'true');
  await expect(table.locator('tbody > tr > td[colspan="5"]')).toBeVisible();
  await page.keyboard.press('Space');
  await expect(button).toHaveAttribute('aria-expanded', 'false');
});

test('live-tail wraps the whole message including unbroken suffixes', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const message = `start ${'error details '.repeat(90)}${'x'.repeat(200)} UNIQUE-END`;
  // This geometry fixture sends one event. Delay reconnection beyond the
  // 20-second test timeout so the closed response cannot replay that event.
  await page.route('**/api/v1/stream?*', route => route.fulfill({
    contentType: 'text/event-stream', body: `retry: 60000\nevent: data\ndata: ${JSON.stringify({ _time: '2026-09-10T12:00:00Z', message })}\n\n`,
  }));
  await page.setViewportSize({ width: 390, height: 900 });
  await page.goto('/search/schema?svc=nginx&stab=tail');
  const msg = page.locator('.tl-row .msg').first();
  await expect(page.locator('.tl-row')).toHaveCount(1);
  await expect(msg).toHaveText(message);
  const geometry = await msg.evaluate(el => {
    const range = document.createRange();
    range.setStart(el.firstChild!, el.textContent!.indexOf('UNIQUE-END'));
    range.setEnd(el.firstChild!, el.textContent!.length);
    const suffix = range.getBoundingClientRect();
    const box = el.getBoundingClientRect();
    return { fits: el.scrollWidth <= el.clientWidth, height: box.height, suffixFits: suffix.right <= box.right + 1 && suffix.bottom <= box.bottom + 1 };
  });
  expect(geometry.fits).toBe(true);
  expect(geometry.height).toBeGreaterThan(60);
  expect(geometry.suffixFits).toBe(true);
  const stream = page.getByRole('region', { name: 'Live tail messages', exact: true });
  // This finite response disconnects after its one event. Retry is now a
  // reachable control between Pause and the retained messages.
  const retry = page.getByRole('button', { name: 'Retry live stream', exact: true });
  await expect(retry).toBeVisible();
  await page.getByRole('button', { name: 'Pause', exact: true }).focus();
  await page.keyboard.press('Tab');
  await expect(retry).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(stream).toBeFocused();
  await page.keyboard.press('End');
  await expect.poll(() => stream.evaluate(el => el.scrollTop)).toBeGreaterThan(0);
  await expect.poll(() => msg.evaluate(el => {
    const range = document.createRange();
    range.setStart(el.firstChild!, el.textContent!.indexOf('UNIQUE-END'));
    range.setEnd(el.firstChild!, el.textContent!.length);
    return range.getBoundingClientRect().bottom <= el.closest('.tl-stream')!.getBoundingClientRect().bottom;
  })).toBe(true);
  await expect(page.locator('.tl-row')).toHaveCount(1);
});
