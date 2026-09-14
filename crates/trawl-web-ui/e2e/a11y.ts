// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Assertions about what a control is to a keyboard and a screen reader,
// shared by the specs ADR-0029 added. One copy per spec drifted the
// moment one of them learned something the others did not.

import { expect } from './fixtures';

type Loc = import('@playwright/test').Locator;

/** The fleet-ui focus ring, as the browser computes it. Two readings:
 * `:focus-visible` is the state, the outline is the pixels. A ring rule
 * that stopped matching leaves the first true and the second `none`.
 * ADR-0032 moved the ring off box-shadow onto `outline` so it never
 * competes with the elevation shadow a plane already paints, so the
 * pixels are read as an outline style and width — a control that paints
 * an elevation shadow would satisfy a box-shadow reading with no ring at
 * all. */
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
