// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import { SEL } from '../selectors';

test('typing a query and pressing Ctrl+Enter submits it and syncs the URL', async ({ page, request }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.dslEditor)).toBeVisible();

  await page.locator(SEL.cmContent).click();
  await page.keyboard.type('service=nginx');
  await page.keyboard.press('Control+Enter');

  await expect(page).toHaveURL(/[?&]q=service%3Dnginx(&|$)/);

  const state = await (await request.get('/__ctl/state')).json();
  const last = state.queries.at(-1);
  expect(last, `captured queries: ${JSON.stringify(state.queries)}`).toBeTruthy();
  expect(last.query).toContain('service=nginx');
});
