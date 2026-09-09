// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

test('global Runs zero-match filter preserves the unfiltered page window', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/jobs/runs');
  await expect(page.locator('.tbl-body .tbl-row')).toHaveCount(3);
  const footer = page.locator('.results-footer');
  await expect(footer.locator('.results-summary')).toHaveText('1–3 of 3');
  await expect(footer.getByRole('button', { name: 'Next' })).toBeDisabled();

  await page.getByPlaceholder('Filter by net…').fill('no-such-net');
  await expect(page.locator('.tbl-body .tbl-row')).toHaveCount(0);
  await expect.soft(footer.locator('.results-summary')).toContainText('1–3 of 3');
  await expect.soft(footer).toContainText('0 matches on this page');
  await expect.soft(footer.getByRole('button', { name: 'Next' })).toBeDisabled();
});

test('net drawer preserves its unfiltered three-row page window', async ({ page, request }) => {
  await resetScenario(request, 'pagination');
  await page.goto('/jobs/nets?net=1&ntab=runs');
  const drawer = page.locator(SEL.drawerPanel);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await expect(drawer.locator('.results-summary')).toHaveText('1–3 of 3');
  await expect(drawer.getByRole('button', { name: 'Prev' })).toBeDisabled();
  await expect(drawer.getByRole('button', { name: 'Next' })).toBeDisabled();
});
