// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Run now and a run's stored rows need `query` beside `saved_query`
// (issue #444, ADR-0018 as amended on 2026-11-11). A session with
// `saved_query` alone still lists a net's runs, but is offered no Run now
// in the net drawer or the nets list, its run rows do not expand, and the
// browser never asks for a run's rows: not from the drawer, not from the
// runs page. The last test is the control: with both permissions every
// one of those is there.

import { test, expect, resetScenario, CORPUS, SCHEDULE } from '../fixtures';
import { SEL, COPY } from '../selectors';
import type { Page } from '@playwright/test';

const SAVED_ONLY = ['saved_query'];
const QUERY_AND_SAVED = ['query', 'saved_query'];

/** The single-run read: one run's summary and its stored rows. */
const RUN_READ = /\/api\/v1\/saved\/\d+\/runs\/\d+(\?|$)/;

/** Serve the session with exactly `permissions`, and record every
 * single-run read the browser sends. */
async function sessionWith(page: Page, permissions: string[]): Promise<string[]> {
  await page.route('**/api/auth/me', async (route) => {
    const response = await route.fetch();
    const json = await response.json();
    json.permissions = permissions;
    await route.fulfill({ response, json });
  });
  const reads: string[] = [];
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname;
    if (RUN_READ.test(path)) reads.push(path);
  });
  return reads;
}

const runNow = (scope: import('@playwright/test').Locator) =>
  scope.getByRole('button', { name: COPY.netRunNow, exact: true });

test('run_now_is_absent_without_query: the net drawer offers no Run now', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await sessionWith(page, SAVED_ONLY);
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  // The drawer header is up, with the control that does not need `query`
  // to be offered, before the absence is read.
  await expect(drawer.getByRole('button', { name: '▶ Search' })).toBeVisible();
  await expect(runNow(drawer)).toHaveCount(0);
});

test('run_now_is_absent_without_query: the nets list offers no Run now', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await sessionWith(page, SAVED_ONLY);
  await page.goto('/jobs/nets');
  const row = page.locator(SEL.tableRow).filter({ hasText: SCHEDULE.windowedNetName });
  await expect(row).toHaveCount(1);
  await expect(runNow(page.locator(SEL.tableRow))).toHaveCount(0);
});

test("run_now_is_absent_without_query: the net drawer's run rows list but do not expand or read", async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const reads = await sessionWith(page, SAVED_ONLY);
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  const rows = page.locator(SEL.netRunRow);
  await expect(rows).toHaveCount(3);

  // Clicking where a readable row's control sits expands nothing and
  // reads nothing.
  await rows.first().locator('td').first().click();
  await page.waitForLoadState('networkidle');
  expect(reads).toEqual([]);
  await expect(page.locator(SEL.netRunPreview)).toHaveCount(0);
  // A row is plain text, not a disclosure control.
  await expect(rows.locator(SEL.rowStretch)).toHaveCount(0);
  await expect(rows.locator('[aria-expanded]')).toHaveCount(0);
});

test('run_now_is_absent_without_query: the runs page shows no preview and reads no rows', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  const reads = await sessionWith(page, SAVED_ONLY);
  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  // The receipt stands on the list's record of the run.
  await expect(detail.locator('#receipt-title')).toBeVisible();
  await page.waitForLoadState('networkidle');
  expect(reads).toEqual([]);
  await expect(detail.locator('.run-preview')).toHaveCount(0);
  await expect(detail.getByText("Reading a run's rows needs the query permission.")).toBeVisible();
});

test('run_now_is_absent_without_query: with query too, Run now is offered and a run row reads its rows', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const reads = await sessionWith(page, QUERY_AND_SAVED);

  await page.goto('/jobs/nets');
  await expect(
    runNow(page.locator(SEL.tableRow).filter({ hasText: SCHEDULE.windowedNetName })),
  ).toBeVisible();

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  const drawer = page.locator(SEL.drawerPanel);
  await expect(runNow(drawer)).toBeVisible();
  const rows = page.locator(SEL.netRunRow);
  await expect(rows).toHaveCount(3);
  expect(reads).toEqual([]);
  await rows.first().locator(SEL.rowStretch).click();
  await expect(page.locator(SEL.netRunPreview).locator(SEL.previewRow).first()).toBeVisible();
  expect(reads.length).toBeGreaterThan(0);

  await resetScenario(request, 'corpus');
  reads.length = 0;
  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  await expect(detail.locator('.run-preview').locator(SEL.previewRow)).toHaveCount(CORPUS.runWithResultRows);
  await expect(detail.getByText("Reading a run's rows needs the query permission.")).toHaveCount(0);
  expect(reads).toContain(`/api/v1/saved/${CORPUS.netId}/runs/${CORPUS.runWithResult}`);
});
