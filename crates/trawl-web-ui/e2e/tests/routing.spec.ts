// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import { SEL, COPY } from '../selectors';

test('/ redirects to /search and mounts the DSL editor', async ({ page }) => {
  await page.goto('/');
  await expect(page).toHaveURL(/\/search$/);
  await expect(page.locator(SEL.dslEditor)).toBeVisible();
});

test('rail link to history navigates without a full page load', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  // Tag `window` so a full reload (which replaces `window`) is detectable.
  await page.evaluate(() => {
    (window as any).__e2e_marker = true;
  });

  await page.locator(SEL.railHistoryLink).click();
  await expect(page.locator('h1')).toHaveText(COPY.historyH1);
  await expect(page).toHaveURL(/\/search\/history$/);

  const markerSurvived = await page.evaluate(() => (window as any).__e2e_marker === true);
  expect(markerSurvived).toBe(true);
});

test('an unknown route renders the 404 page', async ({ page }) => {
  await page.goto('/definitely-not-a-route');
  await expect(page.locator(SEL.notFoundHeading)).toHaveText(COPY.notFoundHeading);
  await expect(page.locator(SEL.notFoundSubtitle)).toHaveText(COPY.notFoundSubtitle);
});
