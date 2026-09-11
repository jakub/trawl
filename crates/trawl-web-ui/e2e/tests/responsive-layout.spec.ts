// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
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

for (const width of [320, 720, 1024, 1440]) {
  test(`responsive search keeps navigation and actions reachable at ${width}px`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
    await page.setViewportSize({ width, height: 900 });
    await page.goto('/search?q=service%3Dnginx');
    await expect(page.locator('.results-table tbody tr')).toHaveCount(8);
    await noPageOverflow(page);
    for (const selector of ['.topbar .jump', '.topbar .user', '.dsl-editor', '.run', '.tabs .save', '.tabs .export']) {
      await insideViewport(page.locator(selector));
    }
    for (const link of await page.locator('.rail a, .topbar .mode').all()) await insideViewport(link);
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
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
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
  await expect(summary).not.toBeVisible();
  await expect(needle).toHaveValue('0');
  await expect(panel.locator('.v.selected .n')).toHaveText('web-01');
  await needle.focus();
  await page.setViewportSize({ width: 320, height: 900 });
  await expect(needle).toBeFocused();
  await expect(needle).toBeVisible();
  await noPageOverflow(page);
});

test('responsive page headers and labelled list scrolling keep controls and columns reachable', async ({ page, request }) => {
  await page.setViewportSize({ width: 320, height: 900 });
  for (const path of ['/search/history', '/search/schema', '/jobs/nets', '/jobs/runs']) {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
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
  await request.post('/__ctl/reset', { data: { scenario: 'health-admin' } });
  await page.goto('/settings/health');
  await insideViewport(page.getByRole('button', { name: 'Refresh', exact: true }));
  await noPageOverflow(page);
  await request.post('/__ctl/reset', { data: { scenario: 'unauth' } });
  await page.goto('/login');
  await insideViewport(page.locator('.login-card'));
  for (const control of await page.locator('.login-card input, .login-card button').all()) await insideViewport(control);
  await noPageOverflow(page);
});

test('scroll instructions follow actual overflow as the viewport and results change', async ({ page, request }) => {
  await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
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
  await expect(page.locator('.results-empty')).toBeVisible();
  await expect(page.locator('.overflow-hint')).toHaveCount(0);
  await page.locator('.dsl-editor').getByRole('textbox').fill('service=nginx');
  await page.locator('.run').click();
  await expect(page.locator('.results-table tbody tr')).toHaveCount(8);
  await expect(page.locator('.overflow-hint')).toBeVisible();
});

for (const width of [320, 720]) {
  test(`responsive Repin keeps footer visible through normal and forced plans at ${width}x450`, async ({ page, request }) => {
    await request.post('/__ctl/reset', { data: { scenario: 'corpus' } });
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
