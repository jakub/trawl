// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFileSync } from 'node:fs';
import type { Page, Route } from '@playwright/test';
import { test, expect, intervalCount, trackIntervals, trackToasts, toastCount } from '../fixtures';
import { TIMING } from '../selectors';

const FIELD = 'duration';
const OTHER = 'other_field';
const field = JSON.parse(readFileSync('harness/wire/catalog-field.json', 'utf8'));
const running = JSON.parse(readFileSync('harness/wire/repin-status-running.json', 'utf8')).job;
const BUSY = 'Fixture slot held';
const FALLBACK_BUSY = 'another repin already holds the one-running slot';
const UNKNOWN = "Couldn't read the repin status just now";

type Reply = Parameters<Route['fulfill']>[0];
type RepinRequest = { field: string; to: string; dry_run: boolean; force: boolean };
const free: Reply = { json: { job: null } };
const held: Reply = { json: { job: { ...running, field: OTHER } } };
const failed: Reply = { status: 503, json: { error: { code: 'unavailable', message: 'Fixture status unavailable' } } };

/** A read remains pending until the test releases it. No timer decides when
 * the response arrives, including responses delivered after modal disposal. */
function pending() {
  let release!: (reply: Reply) => void;
  let completed!: () => void;
  const response = new Promise<Reply>(resolve => { release = resolve; });
  const done = new Promise<void>(resolve => { completed = resolve; });
  return { response, release, completed, done };
}

async function prepare(page: Page) {
  await trackIntervals(page);
  await trackToasts(page);
  await page.route('**/api/auth/me', route => route.fulfill({ json: {
    name: 'e2e', roles: ['schema-writer'], permissions: ['query', 'schema_read', 'schema_write'],
    exp: Math.floor(Date.now() / 1000) + 3600,
  } }));
  await page.route('**/api/v1/schema/field?*', route => route.fulfill({ json: {
    ...field,
    name: new URL(route.request().url()).searchParams.get('name'),
    verdict: { since: '2026-08-01T00:00:00Z', services: 2, episodes: 4,
      rows_shelved: 120, samples: ['123', 'oops'], suggested_to: 'BIGINT' },
  } }));

  const script = {
    reads: 0,
    posts: [] as RepinRequest[],
    status: [] as (Reply | ReturnType<typeof pending>)[],
    postMode: 'busy' as 'busy' | 'plan' | 'lost',
    // `bad_request` is the server's supported ErrorResponse code for busy.
    // The audit's `conflict` spelling exercises the conservative fallback.
    busyCode: 'bad_request',
  };
  await page.route('**/api/v1/schema/repin/status', async route => {
    script.reads++;
    const next = script.status.shift() ?? free;
    if ('response' in next) {
      await route.fulfill(await next.response);
      next.completed();
    } else {
      await route.fulfill(next);
    }
  });
  await page.route('**/api/v1/schema/repin', async route => {
    const body = route.request().postDataJSON() as RepinRequest;
    script.posts.push(body);
    if (script.postMode === 'busy') {
      await route.fulfill({ status: 409, json: { error: { code: script.busyCode, message: BUSY } } });
    } else if (script.postMode === 'lost' && !body.dry_run) {
      await route.fulfill(failed);
    } else {
      await route.fulfill({ json: { job: { ...running, field: body.field, to_type: body.to,
        dry_run: true, status: 'succeeded', id: 4212, files_done: 12 } } });
    }
  });
  return script;
}

function modal(page: Page) {
  return page.getByRole('dialog', { name: 'Repin field', exact: true });
}

async function open(page: Page) {
  await page.getByRole('button', { name: 'Repin this field', exact: true }).click();
  await expect(modal(page)).toBeVisible();
}

async function expectNoBackgroundWork(page: Page) {
  expect(await intervalCount(page, TIMING.repinPollMs)).toBe(0);
  expect(await toastCount(page)).toBe(0);
}

for (const [name, reply, busyCode] of [
  ['free', free, 'bad_request'],
  ['held', held, 'bad_request'],
  ['failed', failed, 'bad_request'],
  ['audit conflict envelope', free, 'conflict'],
] as const) {
  test(`repin busy annotation finishes for ${name} without offering a write`, async ({ page }) => {
    const script = await prepare(page);
    script.busyCode = busyCode;
    const probe = pending();
    script.status.push(free, probe); // Drawer mount, then modal annotation.
    await page.goto(`/search/schema?field=${FIELD}`);
    await open(page);
    await expect.poll(() => script.reads).toBe(2);
    await expect(modal(page).getByRole('button', { name: 'Checking…', exact: true })).toBeDisabled();
    // The selector ignores changes while the scan/probe is in flight.
    await modal(page).getByRole('button', { name: 'DOUBLE', exact: true }).click();
    await expect(modal(page).getByRole('button', { name: 'BIGINT', exact: true })).toHaveAttribute('aria-pressed', 'true');
    probe.release(reply);
    await probe.done;
    await expect(modal(page).getByRole('button', { name: 'Check again', exact: true })).toBeEnabled();
    await expect(modal(page).getByText(busyCode === 'conflict' ? FALLBACK_BUSY : BUSY, { exact: true })).toBeVisible();
    if (name === 'held') await expect(modal(page).getByText(OTHER, { exact: true })).toBeVisible();
    else await expect(modal(page).getByText(OTHER, { exact: true })).toHaveCount(0);
    await expect(modal(page).getByRole('button', { name: /Get plan|Run repin|Run forced repin/ })).toHaveCount(0);
    expect(script.reads).toBe(2);
    expect(script.posts).toEqual([{ field: FIELD, to: 'BIGINT', dry_run: true, force: false }]);
    await expectNoBackgroundWork(page);
  });
}

test('repin operator rechecks held, unknown and free slots before requesting a new target plan', async ({ page }) => {
  const script = await prepare(page);
  await page.goto(`/search/schema?field=${FIELD}`);
  await open(page);
  const check = modal(page).getByRole('button', { name: 'Check again', exact: true });
  await expect(check).toBeEnabled();

  for (const [reply, result] of [[held, 'held'], [failed, 'unknown'], [free, 'free']] as const) {
    const probe = pending();
    script.status.push(probe);
    const before = script.reads;
    await check.click();
    await expect.poll(() => script.reads).toBe(before + 1);
    await expect(modal(page).getByRole('button', { name: 'Checking…', exact: true })).toBeDisabled();
    // Keyboard submission while probing must not issue another status read.
    await page.keyboard.press('Control+Enter');
    probe.release(reply);
    await probe.done;
    if (result === 'free') {
      await expect(modal(page).getByRole('button', { name: 'Get plan', exact: true })).toBeEnabled();
      await expect(modal(page).getByText(OTHER, { exact: true })).toHaveCount(0);
      await expect(modal(page).getByText(UNKNOWN, { exact: false })).toHaveCount(0);
    } else {
      await expect(check).toBeEnabled();
      await expect(modal(page).getByText(OTHER, { exact: true })).toBeVisible();
      if (result === 'unknown') await expect(modal(page).getByText(UNKNOWN, { exact: false })).toBeVisible();
      await expect(modal(page).getByRole('button', { name: /Get plan|Run repin/ })).toHaveCount(0);
    }
    expect(script.reads).toBe(before + 1);
    expect(script.posts).toHaveLength(1);
  }

  await modal(page).getByRole('button', { name: 'DOUBLE', exact: true }).click();
  await expect(modal(page).getByRole('button', { name: 'DOUBLE', exact: true })).toHaveAttribute('aria-pressed', 'true');
  script.postMode = 'plan';
  await modal(page).getByRole('button', { name: 'Get plan', exact: true }).click();
  await expect(modal(page).getByRole('button', { name: 'Run repin', exact: true })).toBeEnabled();
  expect(script.posts).toEqual([
    { field: FIELD, to: 'BIGINT', dry_run: true, force: false },
    { field: FIELD, to: 'DOUBLE', dry_run: true, force: false },
  ]);
  await expectNoBackgroundWork(page);
});

for (const dismissal of ['close', 'Escape', 'another field'] as const) {
  test(`repin late probe after ${dismissal} cannot change a newly opened modal`, async ({ page }) => {
    const script = await prepare(page);
    const probe = pending();
    script.status.push(free, probe);
    if (dismissal === 'another field') {
      await page.goto('/search/schema');
      await page.evaluate(() => {
        (window as any).__repinDocument = true;
        history.pushState({}, '', '/search/schema?field=duration');
        history.pushState({}, '', '/search/schema?field=status');
      });
      await page.goBack();
    } else {
      await page.goto(`/search/schema?field=${FIELD}`);
    }
    await open(page);
    await expect.poll(() => script.reads).toBe(2);
    await expect(modal(page).getByRole('button', { name: 'Checking…', exact: true })).toBeDisabled();

    if (dismissal === 'close') await modal(page).getByRole('button', { name: 'Close dialog', exact: true }).click();
    else if (dismissal === 'Escape') await page.keyboard.press('Escape');
    else await page.goForward();
    await expect(modal(page)).toHaveCount(0);
    if (dismissal === 'another field') {
      await expect(page.getByRole('dialog', { name: 'status', exact: true })).toBeVisible();
      expect(await page.evaluate(() => (window as any).__repinDocument)).toBe(true);
    }

    script.postMode = 'plan';
    await open(page);
    const run = modal(page).getByRole('button', { name: 'Run repin', exact: true });
    await expect(run).toBeEnabled();
    probe.release(held);
    await probe.done;
    // Two paint turns let the completed fetch run its Wasm continuation.
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    await expect(run).toBeEnabled();
    await expect(modal(page).getByText(OTHER, { exact: true })).toHaveCount(0);
    await expect(modal(page).getByRole('button', { name: /Checking|Check again|Get plan/ })).toHaveCount(0);
    expect(script.posts).toEqual([
      { field: FIELD, to: 'BIGINT', dry_run: true, force: false },
      { field: dismissal === 'another field' ? 'status' : FIELD, to: 'BIGINT', dry_run: true, force: false },
    ]);
    expect(script.reads).toBe(dismissal === 'another field' ? 3 : 2);
    await expectNoBackgroundWork(page);
  });
}

test('repin lost execution stays indeterminate after failed and empty probes', async ({ page }) => {
  const script = await prepare(page);
  script.postMode = 'lost';
  await page.goto(`/search/schema?field=${FIELD}`);
  await open(page);
  await expect(modal(page).getByRole('button', { name: 'Run repin', exact: true })).toBeEnabled();
  script.status.push(failed);
  await modal(page).getByRole('button', { name: 'Run repin', exact: true }).click();
  const check = modal(page).getByRole('button', { name: 'Check status', exact: true });
  await expect(check).toBeEnabled();
  const probe = pending();
  script.status.push(probe);
  await check.click();
  await expect(modal(page).getByRole('button', { name: 'Checking…', exact: true })).toBeDisabled();
  probe.release(free);
  await probe.done;
  await expect(check).toBeEnabled();
  await expect(modal(page).getByRole('button', { name: /Run repin|Get plan|Retry plan/ })).toHaveCount(0);
  await modal(page).getByRole('button', { name: 'DOUBLE', exact: true }).click();
  await expect(modal(page).getByRole('button', { name: 'BIGINT', exact: true })).toHaveAttribute('aria-pressed', 'true');
  expect(script.posts).toEqual([
    { field: FIELD, to: 'BIGINT', dry_run: true, force: false },
    { field: FIELD, to: 'BIGINT', dry_run: false, force: false },
  ]);
  expect(script.reads).toBe(3);
  await expectNoBackgroundWork(page);
});
