// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Readings of the filter rail at wide widths (issue #231, ADR-0044), and
// the held query that keeps an answer pending, shared by every spec that
// asserts where the rail stands. A spec file cannot import another spec
// file, so they live here.

import { expect } from './fixtures';
import { SEL } from './selectors';
import type { Locator, Page } from '@playwright/test';

/** fleet-ui's `--facets-w`, the open rail. */
export const OPEN_WIDTH = 224;
/** trawl-web-ui's `--facet-strip-w`, the closed rail. */
export const STRIP_WIDTH = 32;

/** The rail's state, read the way a reader sees it: open or not, and
 * how wide it is drawn. Width within a pixel, per the acceptance test.
 * One reading without the other would pass a rail that is open but
 * drawn as a strip, or the reverse. */
export async function expectRail(page: Page, open: boolean) {
  const rail = page.locator(SEL.filterRail);
  if (open) {
    await expect(rail).toHaveAttribute('open', '');
  } else {
    await expect(rail).not.toHaveAttribute('open');
  }
  const want = open ? OPEN_WIDTH : STRIP_WIDTH;
  await expect
    .poll(async () => Math.abs((await rail.boundingBox())!.width - want), {
      message: `the rail should be ${want}px wide`,
    })
    .toBeLessThanOrEqual(1);
}

/** Count every `toggle` the rail's <details> fires from here on, and
 * return a reader.
 *
 * The browser queues `toggle` as a task after the `open` attribute
 * changes, so both ends wait two animation frames and a task. Before
 * the count starts, that lets a toggle the preceding automatic change
 * already queued fire uncounted. Before the reader reads, it lets a
 * toggle the last change queued be counted.
 *
 * One listener per document, whatever the number of calls: a second
 * call starts the count again from zero instead of adding a listener
 * that counts every toggle twice. `toggle` does not bubble, so the
 * listener catches it on the way down, in the capture phase. */
export async function watchToggles(page: Page): Promise<() => Promise<number>> {
  await page.locator(SEL.filterRail).evaluate(async (_el, rail) => {
    await new Promise<void>((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve))),
    );
    const w = window as unknown as { __railToggles: number; __railTogglesWatched?: true };
    w.__railToggles = 0;
    if (!w.__railTogglesWatched) {
      w.__railTogglesWatched = true;
      document.addEventListener(
        'toggle',
        (event) => {
          if ((event.target as Element).matches(rail)) w.__railToggles += 1;
        },
        true,
      );
    }
  }, SEL.filterRail);
  return () =>
    page.evaluate(
      () =>
        new Promise<number>((resolve) =>
          requestAnimationFrame(() =>
            requestAnimationFrame(() =>
              setTimeout(() => resolve((window as unknown as { __railToggles: number }).__railToggles)),
            ),
          ),
        ),
    );
}

/** Hold the next `/api/v1/query` until `release` is called. `arrived`
 * settles once the request is parked, so the page is provably pending. */
export async function holdNextQuery(page: Page) {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let arrive!: () => void;
  const arrived = new Promise<void>((resolve) => {
    arrive = resolve;
  });
  await page.route(
    '**/api/v1/query',
    async (route) => {
      arrive();
      await gate;
      await route.continue();
    },
    { times: 1 },
  );
  return { arrived, release };
}

/** `text` is drawn where a reader can see it: one run of it inside
 * `locator` has a box of its own, inside the element's box, in a visible
 * element. `toHaveText` passes on text nobody can see, and `toBeVisible`
 * on an element whose text is clipped away or hidden. */
export async function expectTextShown(locator: Locator, text: string) {
  await expect(locator).toBeVisible();
  await expect
    .poll(
      () =>
        locator.evaluate((el, text) => {
          const box = el.getBoundingClientRect();
          const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
          for (let node = walker.nextNode(); node; node = walker.nextNode()) {
            const at = node.textContent!.indexOf(text);
            if (at < 0) continue;
            const range = document.createRange();
            range.setStart(node, at);
            range.setEnd(node, at + text.length);
            const run = range.getBoundingClientRect();
            return (
              run.width > 0 &&
              run.height > 0 &&
              run.left >= box.left - 1 &&
              run.right <= box.right + 1 &&
              run.top >= box.top - 1 &&
              run.bottom <= box.bottom + 1 &&
              node.parentElement!.checkVisibility({ opacityProperty: true, visibilityProperty: true })
            );
          }
          return false;
        }, text),
      { message: `"${text}" should be drawn inside the element` },
    )
    .toBe(true);
}
