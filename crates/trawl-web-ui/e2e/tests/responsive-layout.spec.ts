// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { expectRail } from '../filter-rail';
import { COPY, SEL } from '../selectors';
import type { Locator, Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';

async function insideViewport(locator: Locator) {
  await expect(locator).toBeVisible();
  const box = await locator.boundingBox();
  const viewport = locator.page().viewportSize()!;
  expect(box!.width).toBeGreaterThan(0);
  expect(box!.x).toBeGreaterThanOrEqual(-1);
  expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width + 1);
  expect(box!.y).toBeGreaterThanOrEqual(-1);
  expect(box!.y + box!.height).toBeLessThanOrEqual(viewport.height + 1);
}
async function noPageOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(page.viewportSize()!.width);
}
/** Two frames and a task: a media query change the viewport caused has
 * reached the app, and whatever it set off has run, so an assertion
 * that something did NOT happen is not read too early. */
async function settle(page: Page) {
  await page.evaluate(() => new Promise<void>((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve)))));
}

for (const width of [320, 720, 900, 1024, 1440]) {
  test(`responsive search keeps navigation and actions reachable at ${width}px`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.setViewportSize({ width, height: 900 });
    await page.goto('/search?q=service%3Dnginx');
    await expect(page.locator('.results-table tbody tr')).toHaveCount(8);
    await noPageOverflow(page);
    for (const selector of ['.topbar .jump', '.topbar .user', '.dsl-editor', '.run', '.tabs .export']) {
      await insideViewport(page.locator(selector));
    }
    // The console's Save as Net is the one save entry on this page.
    await insideViewport(page.locator(SEL.editorTool).filter({ hasText: COPY.saveAsNetTool }));
    if (width >= 900) {
      for (const link of await page.locator(SEL.paletteRailLink).all()) await insideViewport(link);
      for (const control of await page.locator('nav.rail .bot a, nav.rail .bot button').all()) {
        await insideViewport(control);
      }
      // The sidebar is docked here, so a toggle would be a control that
      // opens nothing. At exactly 900 the CSS used to reveal it while
      // `Shell` still gated the overlay on 899.98 — visible and dead.
      await expect(page.locator(SEL.navToggle)).toBeHidden();
    } else {
      // Below 900px navigation is an overlay the command bar opens.
      const toggle = page.locator(SEL.navToggle);
      await insideViewport(toggle);
      await toggle.click();
      // The overlay slides in from 20px off the left edge. Measure it at
      // rest, by its own animation's `finished` promise: this suite does
      // not run under emulated reduced motion, so a box read on the
      // frame after the click is a frame of the entrance, not the
      // layout. (`animation-fill-mode` is `none`, so an element that
      // never animates resolves immediately.)
      await page.locator('nav.rail.overlay').evaluate(async (el) => {
        await Promise.all(el.getAnimations().map((a) => a.finished));
      });
      for (const link of await page.locator(SEL.paletteRailLink).all()) await insideViewport(link);
      // The bottom slot too: Help and the collapse control live outside
      // the destination list the palette selector names, and a sweep
      // that skips them is a sweep of half the sidebar.
      for (const control of await page.locator('nav.rail .bot a, nav.rail .bot button').all()) {
        await insideViewport(control);
      }
      await page.keyboard.press('Escape');
      await expect(toggle).toBeFocused();
    }
    await expect(page.locator('.topbar .user')).toHaveAccessibleName('e2e');
    await page.locator('.dr-trigger').click();
    await insideViewport(page.locator('.dr-pop'));
    await page.keyboard.press('Escape');
    await expect(page.locator('.dr-trigger')).toBeFocused();
    await page.locator('.topbar .jump').click();
    await expect(page.locator('.command-palette')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('.topbar .jump')).toBeFocused();
    await page.locator('.tabs .export').click();
    await insideViewport(page.locator('.modal'));
    await page.keyboard.press('Escape');
    await expect(page.locator('.tabs .export')).toBeFocused();
  });
}

test('responsive filters preserve field search, expanded groups and URL filters across layouts', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto('/search?q=service%3Dnginx');
  const panel = page.locator('.facet-panel'), summary = panel.locator('summary');
  await expect(panel).not.toHaveAttribute('open', '');
  await summary.press('Enter');
  await expect(panel).toHaveAttribute('open', '');
  await page.getByRole('button', { name: 'Show 1 more values for host' }).click();
  const needle = panel.locator('input');
  await needle.fill('0');
  await page.getByRole('button', { name: 'Include host = web-01', exact: true }).click();
  await expect(page).toHaveURL(/f=/);
  await expect(summary).toContainText('1 active');
  await summary.click();
  await expect(needle).not.toBeVisible();
  await summary.press('Enter');
  await expect(needle).toHaveValue('0');
  await expect(page.getByRole('button', { name: 'Include host = cache-01', exact: true })).toBeVisible();
  await page.setViewportSize({ width: 1440, height: 900 });
  // Wide, the summary is the open rail's header row (ADR-0044).
  await expect(summary).toBeVisible();
  await expectRail(page, true);
  await expect(needle).toHaveValue('0');
  await expect(panel.locator('.v.selected .n')).toHaveText('web-01');
  // Clear the compact disclosure state before testing focus-driven reopening.
  await page.setViewportSize({ width: 720, height: 900 });
  await expect(summary).toBeVisible();
  await Promise.all([
    panel.evaluate(el => new Promise<void>(resolve => el.addEventListener('toggle', () => resolve(), { once: true }))),
    summary.click(),
  ]);
  await expect(panel).not.toHaveAttribute('open', '');
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect(needle).toBeVisible();
  await needle.focus();
  await page.setViewportSize({ width: 320, height: 900 });
  await expect(panel).toHaveAttribute('open', '');
  await expect(needle).toBeFocused();
  await expect(needle).toBeVisible();
  await noPageOverflow(page);
});

// Below 900px the rail is what it was before ADR-0044: a disclosure a
// countable page leaves closed, whose own open state is not the wide
// rail's hand choice.
test('responsive filters: a countable page leaves the narrow disclosure closed, and a narrow close leaves the wide rail automatic', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  const panel = page.locator(SEL.filterRail), summary = page.locator(SEL.filterRailSummary);
  await settle(page);
  await expect(panel).not.toHaveAttribute('open');
  await summary.click();
  await expect(panel).toHaveAttribute('open', '');
  await summary.click();
  await expect(panel).not.toHaveAttribute('open');
  // Wide, on the same countable page, the rail opens by itself.
  await page.setViewportSize({ width: 1440, height: 900 });
  await expectRail(page, true);
});

test('responsive filters keep the value search and group state across 720, 1440 and 320', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  const panel = page.locator(SEL.filterRail), summary = page.locator(SEL.filterRailSummary);
  await summary.click();
  await expect(panel).toHaveAttribute('open', '');
  const needle = page.locator(SEL.facetFilterInput);
  await needle.fill('0');
  // One group collapsed, and one expanded past its first five values.
  const status = panel.getByRole('button', { name: /^status, \d+ values?$/ });
  await status.click();
  await expect(status).toHaveAttribute('aria-expanded', 'false');
  await panel.getByRole('button', { name: 'Show 1 more values for host' }).click();
  const hosts = panel.locator(SEL.facetGroup)
    .filter({ has: page.getByRole('button', { name: 'host, 6 values', exact: true }) })
    .locator(SEL.facetValue);
  await expect(hosts).toHaveCount(6);
  for (const width of [1440, 320]) {
    await page.setViewportSize({ width, height: 900 });
    if (width >= 900) {
      await expectRail(page, true);
    } else {
      await expect(panel).toHaveAttribute('open', '');
    }
    await expect(needle).toBeVisible();
    await expect(needle).toHaveValue('0');
    await expect(status).toHaveAttribute('aria-expanded', 'false');
    await expect(hosts).toHaveCount(6);
  }
  await noPageOverflow(page);
});

// Narrowing reopens the disclosure for focus in its content, above, so
// the focused control is never hidden. The <summary> is the one part a
// closed disclosure still shows, so focus there reopens nothing.
test('responsive filters leave the narrow disclosure closed for focus on its summary', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto('/search?q=service%3Dnginx');
  await expect(page.locator(SEL.resultsRow)).toHaveCount(8);
  await expectRail(page, true);
  const panel = page.locator(SEL.filterRail), summary = page.locator(SEL.filterRailSummary);
  // A wide press closes the rail and leaves focus where it was.
  await summary.press('Enter');
  await expectRail(page, false);
  await expect(summary).toBeFocused();
  await page.setViewportSize({ width: 720, height: 900 });
  // Narrow now: the summary is the disclosure's full-width row again.
  await expect.poll(async () => (await summary.boundingBox())!.width).toBeGreaterThan(600);
  await settle(page);
  await expect(panel).not.toHaveAttribute('open');
  await expect(summary).toBeFocused();
});

test('responsive page headers and labelled list scrolling keep controls and columns reachable', async ({ page, request }) => {
  await page.setViewportSize({ width: 320, height: 900 });
  for (const path of ['/search/history', '/search/schema', '/jobs/nets', '/jobs/runs']) {
    await resetScenario(request, 'corpus');
    await page.goto(path);
    await expect(page.locator('.tbl-row').first()).toBeVisible();
    await noPageOverflow(page);
    for (const action of await page.locator('.page-hd input, .page-hd button, .page-hd select').all()) await insideViewport(action);
    const scroller = page.locator('.tbl-scroll');
    await expect(scroller).toHaveRole('region');
    await expect(scroller).toHaveAccessibleName(/Search history|Services|Saved queries|Recent runs/);
    await expect(page.locator('.overflow-hint')).toBeVisible();
    await scroller.focus();
    const old = await page.locator('.fleet-table thead').evaluate(e => e.getBoundingClientRect().x);
    await page.keyboard.press('ArrowRight');
    await expect.poll(() => scroller.evaluate(e => e.scrollLeft)).toBeGreaterThan(0);
    expect(await page.locator('.fleet-table thead').evaluate(e => e.getBoundingClientRect().x)).toBeLessThan(old);
  }
  await resetScenario(request, 'health-admin');
  await page.goto('/settings/health');
  await insideViewport(page.getByRole('button', { name: 'Refresh', exact: true }));
  await noPageOverflow(page);
  await resetScenario(request, 'unauth');
  await page.goto('/login');
  await insideViewport(page.locator('.login-card'));
  for (const control of await page.locator('.login-card input, .login-card button').all()) await insideViewport(control);
  await noPageOverflow(page);
});

test('scroll instructions follow actual overflow as the viewport and results change', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  for (const [path, width] of [['/jobs/runs', 720], ['/search/history', 1024]] as const) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto(path);
    await expect(page.locator('.tbl-row').first()).toBeVisible();
    expect(await page.locator('.tbl-scroll').evaluate(e => e.scrollWidth === e.clientWidth)).toBe(true);
    await expect(page.locator('.overflow-hint')).toHaveCount(0);
    await page.setViewportSize({ width: 320, height: 900 });
    await expect(page.locator('.overflow-hint')).toBeVisible();
    await page.setViewportSize({ width: 1440, height: 900 });
    await expect(page.locator('.overflow-hint')).toHaveCount(0);
  }
  await page.setViewportSize({ width: 320, height: 900 });
  await page.goto('/search');
  // A search that has not run yet is the Quick start guide, which has no
  // table and so nothing to scroll sideways.
  await expect(page.locator('.search-quick-start').getByRole('heading', { name: 'Quick start', exact: true })).toBeVisible();
  await expect(page.locator('.overflow-hint')).toHaveCount(0);
  await page.locator('.dsl-editor').getByRole('textbox').fill('service=nginx');
  await page.locator('.run').click();
  await expect(page.locator('.results-table tbody tr')).toHaveCount(8);
  await expect(page.locator('.overflow-hint')).toBeVisible();
});

for (const width of [320, 720]) {
  test(`responsive Repin keeps footer visible through normal and forced plans at ${width}x450`, async ({ page, request }) => {
    await resetScenario(request, 'corpus');
    await page.setViewportSize({ width, height: 450 });
    await page.route('**/api/auth/me', async route => {
      const response = await route.fetch(), json = await response.json();
      json.permissions = ['query', 'schema_read', 'schema_write'];
      await route.fulfill({ json });
    });
    await page.route('**/api/v1/schema/field?**', async route => {
      const response = await route.fetch(), json = await response.json();
      json.verdict = { since: '2026-09-01T10:00:00Z', services: 2, episodes: 8, rows_shelved: 150, samples: ['12.5', 'example'], suggested_to: 'DOUBLE' };
      await route.fulfill({ json });
    });
    let executes = 0;
    await page.route('**/api/v1/schema/repin', async route => {
      const input = route.request().postDataJSON();
      const json = JSON.parse(await readFile(`${__dirname}/../harness/wire/repin-status-running.json`, 'utf8'));
      Object.assign(json.job, { dry_run: input.dry_run, force: input.force, to_type: input.to, status: input.dry_run ? 'succeeded' : 'refused_needs_force', finished_at: '2026-09-10T20:00:00Z', projected_nulls: 150 });
      if (!input.dry_run) executes++;
      await route.fulfill({ status: input.dry_run ? 200 : 409, json });
    });
    await page.goto('/search/schema?field=duration');
    const opener = page.getByRole('button', { name: 'Repin this field' });
    await opener.click();
    const body = page.locator('.modal .m-body'), footer = page.locator('.modal .m-ft');
    const run = page.getByRole('button', { name: 'Run repin', exact: true });
    await expect(run).toBeEnabled();
    await insideViewport(footer);
    await insideViewport(page.locator('.modal .m-hd'));
    for (const choice of await page.locator('.modal .seg-opt').all()) await insideViewport(choice);
    expect(await body.evaluate(e => e.scrollHeight > e.clientHeight)).toBe(true);
    await body.hover();
    await page.mouse.wheel(0, 900);
    await expect.poll(() => body.evaluate(e => e.scrollTop)).toBeGreaterThan(0);
    await insideViewport(footer);
    await run.click();
    await page.getByRole('button', { name: 'Get forced plan' }).click();
    const checkbox = page.locator('.rp-force input[type=checkbox]');
    await checkbox.focus();
    await page.keyboard.press('Space');
    await expect(checkbox).toBeChecked();
    await insideViewport(checkbox);
    await insideViewport(footer);
    await expect(page.getByRole('button', { name: 'Run forced repin' })).toBeEnabled();
    expect(executes).toBe(1);
    await page.keyboard.press('Escape');
    await expect(page.locator('.modal')).toHaveCount(0);
    await expect(opener).toBeFocused();
    await expect(page.locator('.sd-drawer')).toBeVisible();
    await noPageOverflow(page);
  });
}

// The overlay's open flag survived the viewport leaving compact, so the
// next narrowing re-opened it by itself over a page nobody had asked it
// about.
test('the nav overlay does not re-open by itself after a wide detour', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.setViewportSize({ width: 720, height: 900 });
  await page.goto('/search?q=service%3Dnginx');

  await page.locator(SEL.navToggle).click();
  await expect(page.locator('nav.rail.overlay')).toHaveCount(1);

  await page.setViewportSize({ width: 1200, height: 900 });
  await expect(page.locator('nav.rail.overlay')).toHaveCount(0);
  await expect(page.locator(SEL.navToggle)).toBeHidden();

  await page.setViewportSize({ width: 720, height: 900 });
  await expect(page.locator(SEL.navToggle)).toBeVisible();
  await expect(page.locator('nav.rail.overlay')).toHaveCount(0);
  await expect(page.locator('.nav-scrim')).toHaveCount(0);
});

test('Runs crosses the 1099/1100 docking boundary without remounting or fetching its paged result', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();
  let reads = 0;
  await page.route('**/api/v1/saved/2/runs/503', async route => {
    reads++;
    const body = JSON.parse(await readFile(`${__dirname}/../harness/wire/run-result-paged.json`, 'utf8'));
    // Wide stored data exercises the preview's own scroll region even in
    // the wide viewport, where the docked column itself remains narrow.
    body.result.columns.push({ name: 'wide_value' });
    body.result.rows.forEach((row: unknown[]) => row.push('stored-column-'.repeat(60)));
    await route.fulfill({ json: body });
  });
  await page.setViewportSize({ width: 1100, height: 900 });
  await page.goto('/jobs/runs?run=503&net=2');
  const detail = page.locator(SEL.runDetail);
  await expect(detail.locator('.results-summary')).toHaveText('1–20 of 45');
  await detail.locator('.data-area').getByRole('button', { name: 'Next →', exact: true }).click();
  await expect(detail.locator('.results-summary')).toHaveText('21–40 of 45');
  const mounted = await detail.elementHandle();
  const preview = await detail.getByRole('region', { name: 'Stored result rows', exact: true }).elementHandle();
  for (const width of [1099, 1100, 1440, 720, 320, 1100]) {
    await page.setViewportSize({ width, height: 900 });
    if (width >= 1100) {
      await expect(detail).toHaveClass(/sd-docked/);
      await expect(page.locator('.page-split > .sd-host')).toHaveCount(1);
      await expect(page.locator('.page-split > .sd-scrim')).toHaveCount(0);
      const list = await page.locator('.page-split > .list-sheet').boundingBox();
      const panel = await detail.boundingBox();
      expect(panel!.x).toBeGreaterThanOrEqual(list!.x + list!.width);
    } else {
      await expect(detail).not.toHaveClass(/sd-docked/);
      await expect(page.locator('.page-split > .sd-scrim')).toHaveCount(1);
      await expect(detail).toHaveCSS('position', 'fixed');
    }
    await expect(detail.locator('.sd-tabs')).toBeHidden();
    await expect(detail.locator('.results-summary')).toHaveText('21–40 of 45');
    const data = await detail.locator('.data-area').boundingBox();
    const receipt = await detail.locator('.receipt').boundingBox();
    expect(receipt!.y).toBeGreaterThanOrEqual(data!.y + data!.height - 1);
    const scroller = detail.getByRole('region', { name: 'Stored result rows' });
    await expect(scroller).toHaveAttribute('tabindex', '0');
    expect(await scroller.evaluate(el => el.scrollWidth > el.clientWidth)).toBe(true);
    await noPageOverflow(page);
    expect(await mounted!.evaluate(el => el.isConnected)).toBe(true);
    expect(await preview!.evaluate(el => el.isConnected)).toBe(true);
    expect(reads).toBe(1);
    await expect(page).toHaveURL(/run=503&net=2$/);
  }
});
