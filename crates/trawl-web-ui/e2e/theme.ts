// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { expect, type Page } from '@playwright/test';
import { SEL } from './selectors';

export type ThemeChoice = 'Light' | 'Dark' | 'System';

/** Select through the shipped account menu, including its close/restore contract. */
export async function selectTheme(page: Page, choice: ThemeChoice): Promise<void> {
  await page.locator(SEL.topbarUser).click();
  await page.getByRole('menuitemradio', { name: choice, exact: true }).click();
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
}
