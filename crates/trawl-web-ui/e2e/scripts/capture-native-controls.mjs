// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Evidence tooling, not a test: drives the built SPA against the same
// stub harness the suite uses and writes the ADR-0028 surfaces to PNG,
// light and dark, plus the geometry numbers that a picture alone cannot
// settle.
//
// It is here rather than in visual-evidence/ because it is worth
// re-running: the two tab strips gained a nested `.tablist` box, and
// "did the underline move" is a question every later change to that
// chrome has to answer again.
//
// The dark pass is not a `colorScheme` emulation. fleet-ui's theme comes
// from localStorage through `theme::runtime`, never from
// prefers-color-scheme, so the only honest way to reach dark is the
// account menu's own theme item — which makes the dark set evidence that
// the item works as well as evidence of how dark looks.
//
// Usage, from the repo root (the SPA must already be built):
//   node crates/trawl-web-ui/e2e/scripts/capture-native-controls.mjs [outdir]

import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const E2E_DIR = path.resolve(SCRIPT_DIR, '..');
const ROOT_DIR = path.resolve(E2E_DIR, '..', '..', '..');
const PORT = Number(process.env.E2E_PORT ?? 8129);
const BASE = `http://127.0.0.1:${PORT}`;
const OUT = path.resolve(
  process.argv[2] ?? path.join(ROOT_DIR, 'visual-evidence', 'issue-159'),
);

/** Screenshot a rectangle that covers every element named, with padding,
 * so an absolutely-positioned panel outside its wrapper's own box (the
 * menu) is still in frame. */
async function shotUnion(page, selectors, file, pad = 12) {
  const rect = await page.evaluate(
    ({ selectors }) => {
      const boxes = selectors.map((sel) => {
        const el = document.querySelector(sel);
        if (!el) throw new Error(`capture: ${sel} is not mounted`);
        return el.getBoundingClientRect();
      });
      return {
        x: Math.min(...boxes.map((b) => b.left)),
        y: Math.min(...boxes.map((b) => b.top)),
        right: Math.max(...boxes.map((b) => b.right)),
        bottom: Math.max(...boxes.map((b) => b.bottom)),
      };
    },
    { selectors },
  );
  await page.screenshot({
    path: path.join(OUT, file),
    clip: {
      x: Math.max(0, rect.x - pad),
      y: Math.max(0, rect.y - pad),
      width: rect.right - rect.x + pad * 2,
      height: rect.bottom - rect.y + pad * 2,
    },
  });
  return file;
}

/** The two geometry questions the nested `.tablist` raises, answered in
 * device pixels by the browser rather than by reading the stylesheet.
 *
 * `underlineOverlap` is the tab's border-box bottom minus the strip's
 * OWN bottom edge: each tab carries `margin-bottom: -1px` so its 2px
 * underline laps the strip's 1px bottom border. 1 means the tab's box
 * ends 1px past the strip's, which is the lap. 0 or less means the
 * underline now floats above the border. */
async function stripGeometry(page, stripSel, tabSel) {
  return page.evaluate(
    ({ stripSel, tabSel }) => {
      const strip = document.querySelector(stripSel);
      const tablist = strip.querySelector('[role="tablist"]');
      const tabs = [...strip.querySelectorAll(tabSel)];
      const meta = strip.querySelector('.meta');
      const r = (el) => (el ? el.getBoundingClientRect() : null);
      const sb = r(strip);
      return {
        strip: { top: sb.top, bottom: sb.bottom, height: sb.height },
        tablist: r(tablist),
        tabs: tabs.map((t) => {
          const b = r(t);
          return { name: t.textContent.trim(), top: b.top, bottom: b.bottom, left: b.left };
        }),
        meta: meta ? { left: r(meta).left, right: r(meta).right } : null,
        underlineOverlap: tabs.map((t) => Number((r(t).bottom - sb.bottom).toFixed(2))),
      };
    },
    { stripSel, tabSel },
  );
}

/** The pre-#159 layout, measured rather than reasoned about: unwrap the
 * `[role="tablist"]` box in place, which is exactly the flat DOM these
 * strips had before (same buttons, same classes, no wrapper), and
 * re-measure. `.tablist`'s two rules then match nothing, so what comes
 * back is the old geometry under the current stylesheet. Destructive to
 * the DOM, so it runs last, after every screenshot. */
async function flattenedGeometry(page, stripSel, tabSel) {
  await page.evaluate(
    ({ stripSel }) => {
      const list = document.querySelector(`${stripSel} [role="tablist"]`);
      if (!list) throw new Error(`capture: no tablist inside ${stripSel}`);
      list.replaceWith(...list.childNodes);
    },
    { stripSel },
  );
  return stripGeometry(page, stripSel, tabSel);
}

/** Wait out any CSS entrance animation on the elements named, by their
 * own `finished` promises rather than a sleep: the menu, the modal and
 * the drawer all fade in, and a frame grabbed mid-fade shows the page
 * bleeding through a half-opaque panel. `reducedMotion: 'reduce'` does
 * not help — fleet-ui's stylesheet has no reduced-motion rule (ADR-0028
 * names that as out of scope). */
async function settle(page, selectors) {
  await page.evaluate(
    async ({ selectors }) => {
      const els = selectors.flatMap((sel) => [...document.querySelectorAll(sel)]);
      await Promise.all(els.flatMap((el) => el.getAnimations().map((a) => a.finished)));
    },
    { selectors },
  );
}

async function openAccountMenu(page) {
  await page.locator('.topbar button.user').focus();
  await page.keyboard.press('Enter');
  await page.locator('.user-menu [role="menu"]').waitFor({ state: 'visible' });
}

async function capture(browser, theme) {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    colorScheme: theme,
    reducedMotion: 'reduce',
    locale: 'en-US',
    timezoneId: 'UTC',
  });
  const page = await context.newPage();
  const files = [];

  await page.goto(`${BASE}/search`);
  await page.locator('.topbar button.user').waitFor({ state: 'visible' });

  if (theme === 'dark') {
    // The theme item is the first item and already holds focus.
    await openAccountMenu(page);
    await page.keyboard.press('Enter');
    await page.waitForFunction(
      () => document.documentElement.getAttribute('data-theme') === 'dark',
    );
  }
  const applied = await page.evaluate(() =>
    document.documentElement.getAttribute('data-theme'),
  );
  if (applied !== theme) throw new Error(`capture: data-theme is ${applied}, wanted ${theme}`);

  // 1. The account menu, open, focus ring on the first item.
  await openAccountMenu(page);
  await settle(page, ['.user-menu', '.user-menu *']);
  files.push(await shotUnion(page, ['.topbar .user-wrap', '.user-menu'], `account-menu-${theme}.png`));
  await page.keyboard.press('Escape');

  // 2. The results strip, focus on an UNSELECTED tab (arrow right off
  //    the selected one) — the roving contract's whole point.
  const wsTabs = page.locator('.tabs [role="tab"]');
  await wsTabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  files.push(await shotUnion(page, ['.tabs'], `results-tabs-${theme}.png`, 8));
  const results = await stripGeometry(page, '.tabs', '[role="tab"]');

  // 3. The modal header and its close button, focused. Opened from the
  //    KEYBOARD: Chromium only paints `:focus-visible` when the last
  //    interaction was a keypress, so a mouse-opened dialog would show
  //    a focused close button with no ring on it.
  await page.locator('.tabs .action.export').focus();
  await page.keyboard.press('Enter');
  await page.locator('.modal').waitFor({ state: 'visible' });
  await settle(page, ['.modal', '.modal *']);
  await page.locator('.modal .m-hd button.x').focus();
  files.push(await shotUnion(page, ['.modal .m-hd'], `modal-header-${theme}.png`, 8));
  await page.keyboard.press('Escape');
  await page.locator('.modal').waitFor({ state: 'detached' });

  // 4. A toast and its dismiss button, focused (keyboard again, same
  //    reason).
  await page.locator('.tabs .action.save').focus();
  await page.keyboard.press('Enter');
  await page.locator('.toast').first().waitFor({ state: 'visible' });
  await settle(page, ['.toast', '.toast *']);
  await page.locator('.toast button.x').first().focus();
  files.push(await shotUnion(page, ['.toast'], `toast-${theme}.png`, 8));

  // 5. The service drawer strip, focus on an unselected tab. Needs the
  //    populated corpus, so the scenario flips first.
  await context.request.post(`${BASE}/__ctl/reset`, { data: { scenario: 'populated' } });
  await page.goto(`${BASE}/search/schema?svc=nginx&stab=overview`);
  await page.locator('.sd-drawer').waitFor({ state: 'visible' });
  if (theme === 'dark') {
    await page.waitForFunction(
      () => document.documentElement.getAttribute('data-theme') === 'dark',
    );
  }
  await settle(page, ['.sd-drawer', '.sd-drawer *', '.sd-scrim']);
  const dTabs = page.locator('.sd-tabs [role="tab"]');
  await dTabs.nth(0).focus();
  await page.keyboard.press('ArrowRight');
  files.push(await shotUnion(page, ['.sd-tabs'], `drawer-tabs-${theme}.png`, 8));
  const drawer = await stripGeometry(page, '.sd-tabs', '[role="tab"]');

  // The before/after, on the same page and the same stylesheet. Each
  // strip is measured on the route that mounts it, and the flattening is
  // the last thing that happens to that page.
  const drawerFlat = await flattenedGeometry(page, '.sd-tabs', '[role="tab"]');
  await page.goto(`${BASE}/search`);
  await page.locator('.tabs [role="tablist"]').waitFor({ state: 'visible' });
  const resultsNested = await stripGeometry(page, '.tabs', '[role="tab"]');
  const resultsFlat = await flattenedGeometry(page, '.tabs', '[role="tab"]');

  await context.close();
  return {
    files,
    geometry: { results, drawer },
    ab: { resultsNested, resultsFlat, drawerNested: drawer, drawerFlat },
  };
}

async function waitForHealth() {
  for (let i = 0; i < 100; i += 1) {
    try {
      const res = await fetch(`${BASE}/__ctl/health`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`capture: stub server never became healthy on ${BASE}`);
}

const server = spawn('node', ['harness/server.mjs'], {
  cwd: E2E_DIR,
  env: { ...process.env, E2E_PORT: String(PORT) },
  stdio: ['ignore', 'inherit', 'inherit'],
});

try {
  await waitForHealth();
  await fs.mkdir(OUT, { recursive: true });
  const browser = await chromium.launch();
  const report = {};
  for (const theme of ['light', 'dark']) {
    const out = await capture(browser, theme);
    report[theme] = out;
    console.log(`${theme}: ${out.files.join(', ')}`);
  }
  await browser.close();
  console.log(JSON.stringify(report, null, 2));
} finally {
  server.kill('SIGTERM');
}
