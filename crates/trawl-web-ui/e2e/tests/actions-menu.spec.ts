// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Direct Nets actions use native keyboard navigation and retain dialog focus.
import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

test('Nets actions are visible, tabbable and restore delete focus', async ({ page, request }) => {
  await resetScenario(request, 'populated');
  await page.goto('/jobs/nets');
  await expect(page.getByRole('columnheader', { name: 'Actions' })).toBeVisible();
  const actions = page.locator(SEL.netAction);
  await expect(actions).toHaveCount(3);
  for (const [index, name] of ['Open in search', 'Trigger run', 'Delete Net'].entries()) {
    await expect(actions.nth(index)).toBeVisible();
    await expect(actions.nth(index)).toHaveAttribute('type', 'button');
    await expect(actions.nth(index)).toHaveAttribute('title', name);
    await expect(actions.nth(index)).toHaveAccessibleName(name);
  }
  await actions.first().focus();
  await page.keyboard.press('Tab');
  await expect(actions.nth(1)).toBeFocused();
  await page.keyboard.press('Tab');
  const destructive = actions.nth(2);
  await expect(destructive).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  await expect(page).toHaveURL(/\/jobs\/nets$/);
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  await expect(destructive).toBeFocused();
});
