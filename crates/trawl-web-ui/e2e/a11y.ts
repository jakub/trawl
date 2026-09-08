// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Assertions about what a control is to a keyboard and a screen reader,
// shared by the specs ADR-0029 added. One copy per spec drifted the
// moment one of them learned something the others did not.

import { expect } from './fixtures';

type Loc = import('@playwright/test').Locator;

/** The fleet-ui focus ring, as the browser computes it. Two readings:
 * `:focus-visible` is the state, the box-shadow is the pixels. A ring
 * rule that stopped matching leaves the first true and the second
 * `none`. */
export async function expectFocusRing(control: Loc): Promise<void> {
  await expect(control).toBeFocused();
  expect(await control.evaluate((el) => el.matches(':focus-visible'))).toBe(true);
  expect(await control.evaluate((el) => getComputedStyle(el).boxShadow)).not.toBe('none');
}
