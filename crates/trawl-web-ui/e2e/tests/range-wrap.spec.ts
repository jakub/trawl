// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// F06: at 320 and 390 px an absolute range used to overflow its track
// and cover Haul, so the button was on screen, enabled, and unclickable.
//
// A bounding-box comparison is not enough evidence here. Two boxes can
// fail to intersect while a third element still sits over the button, so
// the proof is what the browser itself would hit: `elementFromPoint` at
// the centre of Haul must resolve inside Haul, and a trial click — which
// runs Playwright's full actionability check without firing the query —
// must succeed. The trigger is asserted separately to be showing its
// whole value rather than an ellipsis, since "no overlap" is also what a
// clipped trigger produces.

import type { Locator } from '@playwright/test';
import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

// Literal link with an absolute range: the widest trigger value the app
// renders, and the one the finding measured at 363.89 px.
const ABSOLUTE_URL = '/search?q=service%3Dnginx&page=0&r=2026-01-01T00:00:00Z..now';

for (const width of [320, 390]) {
  test(`absolute range wraps inside its trigger at ${width}px and leaves Haul clickable`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.setViewportSize({ width, height: 900 });
    await page.goto(ABSOLUTE_URL);

    const trigger = page.locator(SEL.dateRangeTrigger);
    await expect(trigger).toBeVisible();

    // The whole value is laid out: nothing scrolls away inside the
    // trigger in either axis.
    const clipped = await trigger.evaluate((el) => ({
      x: el.scrollWidth > el.clientWidth + 1,
      y: el.scrollHeight > el.clientHeight + 1,
    }));
    expect(clipped).toEqual({ x: false, y: false });

    const run = page.locator(SEL.runButton);
    await expect(run).toBeVisible();
    const box = await run.boundingBox();
    expect(box, 'Haul must have a layout box').not.toBeNull();

    const onTarget = await page.evaluate(({ x, y }) => {
      const hit = document.elementFromPoint(x, y);
      return hit !== null && hit.closest('button.run') !== null;
    }, { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2 });
    expect(onTarget, 'the centre of Haul must belong to Haul').toBe(true);

    await run.click({ trial: true });
  });
}

// The console used to clip its own popover: `.console { overflow: hidden }`
// cut `.dr-pop` — which RangeDialog renders absolutely inside `.daterange`,
// with no portal — off at the console's bottom edge, so the lower presets,
// the Absolute inputs and Apply were unreachable at desktop widths. Below
// 600px the popover is `position: fixed`, so only the wide layout proves it.
test('the range dialog is fully visible inside the console at 1440px', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.goto('/search?q=service%3Dnginx&page=0');

  await page.locator(SEL.dateRangeTrigger).click();
  const pop = page.locator('.dr-pop');
  await expect(pop).toBeVisible();

  // What the browser would hit at the centre of a control is the only
  // proof that nothing clips or covers it; a bounding box alone is not.
  const hits = async (target: Locator, closest: string, name: string) => {
    await expect(target).toBeVisible();
    const box = await target.boundingBox();
    expect(box, `${name} must have a layout box`).not.toBeNull();
    const inViewport = await page.evaluate(
      ({ x, y, w, h }) => y >= 0 && x >= 0 && y + h <= window.innerHeight && x + w <= window.innerWidth,
      { x: box!.x, y: box!.y, w: box!.width, h: box!.height },
    );
    expect(inViewport, `${name} must be inside the viewport`).toBe(true);
    const onTarget = await page.evaluate(
      ({ x, y, sel }) => {
        const hit = document.elementFromPoint(x, y);
        return hit !== null && hit.closest(sel) !== null;
      },
      { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2, sel: closest },
    );
    expect(onTarget, `the centre of ${name} must belong to it`).toBe(true);
  };

  await hits(page.locator(SEL.quickRangeOption).last(), SEL.quickRangeOption, 'the last preset');

  await page.locator(SEL.absoluteTab).click();
  await hits(page.locator(SEL.dateRangeApply), SEL.dateRangeApply, 'Apply');
});
