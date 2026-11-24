// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// ADR-0058: the search URL the MCP server emits opens Search on a phone.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { devices } from '@playwright/test';
import { test, expect, lastCapturedQuery } from '../fixtures';

const fixture = JSON.parse(
  readFileSync(resolve(__dirname, '../../../../docs/evidence/issue-308/search-url.json'), 'utf8'),
) as { origin: string; url: string; executed_query: string; bounds: { earliest: string; latest: string } };

// The iPhone descriptor's viewport, scale, touch and user agent, on the
// project's chromium: its webkit default would override the channel.
const { defaultBrowserType: _webkit, ...iphone } = devices['iPhone 13'];
test.use(iphone);

test('the MCP search URL runs the captured query and bounds in Search on a phone', async ({ page, request }, testInfo) => {
  const link = new URL(fixture.url);
  expect(link.origin).toBe(fixture.origin);
  // The stub's default scenario answers any query with no rows.
  await page.goto(link.pathname + link.search);

  const sent = await lastCapturedQuery(request, 1);
  expect(sent.query).toContain(`earliest="${fixture.bounds.earliest}"`);
  expect(sent.query).toContain(`latest="${fixture.bounds.latest}"`);
  const [hullStart, hullEnd] = link.searchParams.get('r')!.split('..');
  const sentText = sent.query.replace(/\s+/g, ' ').trim();
  const executed = fixture.executed_query.replace(/\s+/g, ' ').trim();
  expect(sentText).toBe(`_time>="${hullStart}" _time<="${hullEnd}" ${executed}`);

  const viewport = page.viewportSize()!;
  expect(viewport.width).toBeLessThan(500);
  const editor = page.getByRole('textbox', { name: 'Search query' });
  await expect(editor).toContainText(`earliest="${fixture.bounds.earliest}"`);
  await expect(editor).toContainText('service=shop-api status>=500');
  await expect(page.getByRole('button', { name: `${hullStart} → ${hullEnd}` })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Search results' })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('search-url-phone.png'), fullPage: false });
});
