// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { expect } from './fixtures';

type Loc = import('@playwright/test').Locator;

/** A control that paints an elevation shadow would satisfy a box-shadow
 * reading with no ring at all, so the ring is read as an outline (ADR-0032). */
export async function expectFocusRing(control: Loc): Promise<void> {
  await expect(control).toBeFocused();
  expect(await control.evaluate((el) => el.matches(':focus-visible'))).toBe(true);
  const style = await control.evaluate((el) => {
    const s = getComputedStyle(el);
    return { style: s.outlineStyle, width: parseFloat(s.outlineWidth) };
  });
  expect(style.style).not.toBe('none');
  expect(style.width).toBeGreaterThan(0);
}
