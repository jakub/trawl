// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The answers ADR-0066 lets an endpoint read itself before the one
// classifier sees them (issue #386): a health 503 that IS a report, the
// repin POST's two 409 bodies, a repin job read's 404, logout's empty
// 204, and export's binary success. Anything else these endpoints answer
// is an ordinary failure, so a health 503 that carries the envelope is a
// refusal, not a report.

import { readFileSync } from 'node:fs';
import type { Page, Route } from '@playwright/test';
import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

type Reply = Parameters<Route['fulfill']>[0];

const LIMIT = 'The server is at its request limit and did not process this request.';

async function openHealth(page: Page, reply: Reply) {
  await page.route(url => url.pathname === '/api/v1/health', route => route.fulfill(reply));
  await page.goto('/settings/health');
  return page.locator(SEL.healthSection);
}

test('a health 503 whose body is a report renders the degraded report', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const health = await openHealth(page, { status: 503, json: {
    status: 'degraded',
    version: 'health-503-degraded',
    checks: { duckdb: 'error', auth_db: 'ok', storage_db: 'ok', data_path: 'ok', ingest_capacity: 'ok' },
  } });
  await expect(health.getByRole('heading')).toHaveText('Server is degraded');
  await expect(health).toContainText('health-503-degraded');
  await expect(health.getByText('duckdb', { exact: true }).locator('xpath=ancestor::div[1]').locator('dd')).toHaveText('error');
  await expect(health.getByRole('alert')).toHaveCount(0);
});

test('a health 503 that carries the envelope is a refusal in the server\'s words', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const health = await openHealth(page, { status: 503, json: { error: { code: 'request_limit_reached', message: LIMIT } } });
  await expect(health.getByRole('heading')).toHaveText('Health report unavailable');
  await expect(health.getByRole('alert')).toHaveText(`Report unavailable: ${LIMIT}`);
  // A refusal is never read as a report of any kind.
  await expect(health.locator('dl.health-checks')).toHaveCount(0);
});

test('a health 503 that is neither a report nor the envelope is a bare failure', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  const health = await openHealth(page, { status: 503, contentType: 'text/html', body: '<h1>RAW-GATEWAY-SENTINEL</h1>' });
  await expect(health.getByRole('heading')).toHaveText('Health report unavailable');
  await expect(health.getByRole('alert')).toHaveText('Report unavailable, server returned 503. Refresh to try again.');
  await expect(page.locator('body')).not.toContainText('RAW-GATEWAY-SENTINEL');
});

// ---- repin ----------------------------------------------------------------

const FIELD = 'duration';
const field = JSON.parse(readFileSync('harness/wire/catalog-field.json', 'utf8'));
const running = JSON.parse(readFileSync('harness/wire/repin-status-running.json', 'utf8')).job;
const BUSY = 'another repin (job 4100) holds the one-running slot';

/** A schema writer on a degraded field's case file, with every repin
 * read and write scripted here. */
async function repinPage(page: Page, onPost: (body: { dry_run: boolean }) => Reply) {
  await page.route('**/api/auth/me', route => route.fulfill({ json: {
    name: 'e2e', roles: ['schema-writer'], permissions: ['query', 'schema_read', 'schema_write'],
    exp: Math.floor(Date.now() / 1000) + 3600,
  } }));
  await page.route('**/api/v1/schema/field?*', route => route.fulfill({ json: {
    ...field,
    name: FIELD,
    verdict: { since: '2026-08-01T00:00:00Z', services: 2, episodes: 4,
      rows_shelved: 120, samples: ['123', 'oops'], suggested_to: 'BIGINT' },
  } }));
  const script = { posts: [] as Array<{ dry_run: boolean }>, jobReads: [] as number[] };
  await page.route(url => url.pathname === '/api/v1/schema/repin/status', route => {
    const id = new URL(route.request().url()).searchParams.get('id');
    if (id === null) return route.fulfill({ json: { job: null } });
    script.jobReads.push(Number(id));
    // The route holds no job with that id.
    return route.fulfill({ status: 404, json: { error: { code: 'not_found', message: 'repin job not found' } } });
  });
  await page.route('**/api/v1/schema/repin', route => {
    const body = route.request().postDataJSON() as { dry_run: boolean };
    script.posts.push(body);
    return route.fulfill(onPost(body));
  });
  await page.goto(`/search/schema?field=${FIELD}`);
  await page.getByRole('button', { name: 'Repin this field', exact: true }).click();
  const modal = page.getByRole('dialog', { name: 'Repin field', exact: true });
  await expect(modal).toBeVisible();
  return { modal, script };
}

const plan = { ...running, field: FIELD, to_type: 'BIGINT', dry_run: true, status: 'succeeded', id: 4212, files_done: 12 };

test('a repin 409 that carries a plan asks for a forced plan', async ({ page }) => {
  const { modal, script } = await repinPage(page, body => body.dry_run
    ? { json: { job: plan } }
    : { status: 409, json: { job: { ...plan, dry_run: false, id: 4213, status: 'refused_needs_force', projected_nulls: 37 } } });
  await modal.getByRole('button', { name: 'Run repin', exact: true }).click();
  await expect(modal.getByRole('button', { name: 'Get forced plan', exact: true })).toBeEnabled();
  await expect(modal.locator('.rp-kv').filter({ hasText: 'Values the new pin cannot keep' })).toContainText('37');
  await expect(modal.getByRole('alert')).toHaveCount(0);
  expect(script.posts.map(p => p.dry_run)).toEqual([true, false]);
});

test('a repin 409 that carries the envelope says the slot is held, in the server\'s words', async ({ page }) => {
  const { modal, script } = await repinPage(page, () =>
    ({ status: 409, json: { error: { code: 'bad_request', message: BUSY } } }));
  await expect(modal.getByRole('alert')).toContainText('Another repin is already running');
  await expect(modal.getByText(BUSY, { exact: true })).toBeVisible();
  await expect(modal.getByRole('button', { name: 'Check again', exact: true })).toBeEnabled();
  await expect(modal.getByRole('button', { name: /Get plan|Run repin|Run forced repin/ })).toHaveCount(0);
  expect(script.posts.map(p => p.dry_run)).toEqual([true]);
});

test('a repin job read answered 404 is no job: the plan is asked for again', async ({ page }) => {
  // The plan's scan outlasts the server's answer bound (202), and the job
  // read by its id answers 404.
  const { modal, script } = await repinPage(page, () =>
    ({ status: 202, json: { job: { ...plan, status: 'running', files_done: 0 } } }));
  await expect(modal.getByText("The plan's scan outlasted the server's answer", { exact: false }))
    .toBeVisible({ timeout: 10_000 });
  await expect(modal.getByRole('button', { name: 'Retry plan', exact: true })).toBeEnabled();
  await expect(modal.getByRole('button', { name: /Run repin/ })).toHaveCount(0);
  expect(script.jobReads).toEqual([4212]);
});

// ---- logout and export ----------------------------------------------------

test('a logout answered 204 with no body signs out cleanly', async ({ page }) => {
  let logouts = 0;
  await page.route('**/api/auth/logout', route => {
    logouts += 1;
    return route.fulfill({ status: 204, body: '' });
  });
  await page.goto('/search/history?note=before-logout');
  await expect(page.locator(SEL.historyPage)).toBeVisible();
  await page.locator(SEL.topbarUser).click();
  await page.getByRole('menuitem', { name: 'Sign Out', exact: true }).click();
  // Explicit logout carries no return destination.
  await expect(page).toHaveURL(url => url.pathname === '/login' && url.search === '');
  await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
  await expect(page.getByText('Your session may still be active', { exact: false })).toHaveCount(0);
  expect(logouts).toBe(1);
});

test('export still downloads its binary answer under its server-given name', async ({ page }) => {
  let exports = 0;
  await page.route(url => url.pathname === '/api/v1/export', route => {
    exports += 1;
    return route.fulfill({
      status: 200,
      headers: { 'content-type': 'text/csv; charset=utf-8', 'content-disposition': 'attachment; filename="health-failures.csv"' },
      body: 'service,count\nnginx,1\n',
    });
  });
  await page.goto('/search?q=service%3Dnginx');
  await page.getByRole('button', { name: 'Export', exact: true }).click();
  const download = page.waitForEvent('download');
  await page.getByRole('dialog').getByRole('button', { name: 'Download', exact: true }).click();
  const file = await download;
  expect(file.suggestedFilename()).toBe('health-failures.csv');
  expect(readFileSync((await file.path())!, 'utf8')).toBe('service,count\nnginx,1\n');
  await expect(page.locator(SEL.toastAny)).toContainText('Exported');
  expect(exports).toBe(1);
});
