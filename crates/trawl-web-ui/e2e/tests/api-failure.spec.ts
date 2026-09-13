// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

test('a 500 from /api/v1/query surfaces an inline error, not a redirect to /login', async ({ page, request }) => {
  await resetScenario(request, 'query-500');

  await page.goto('/search');
  await page.locator(SEL.cmContent).click();
  await page.keyboard.type('service=nginx');
  await page.locator(SEL.runButton).click();

  const err = page.locator(SEL.loadHintError);
  await expect(err).toBeVisible();
  await expect(err).toContainText(COPY.loadHintErrorPrefix);

  await expect(page).toHaveURL(/\/search/);
  await expect(page).not.toHaveURL(/\/login/);
});

test('a failed snapshot reads Error in the footer and a re-haul recovers it', async ({ page, request }) => {
  await resetScenario(request, 'query-500');
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.statusLabel)).toHaveText('Error');

  // Re-hauling the SAME text is the only way back from a failed page
  // zero, so it re-runs the request rather than writing an identical
  // link and notifying nothing.
  await resetScenario(request, 'default');
  await page.locator(SEL.runButton).click();
  await expect(page.locator(SEL.statusLabel)).toContainText('Connected');
  await expect(page.locator(SEL.footerCount)).toContainText('Last 0');
});
