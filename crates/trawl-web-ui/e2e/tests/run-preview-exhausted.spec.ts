// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A stored run's preview when the server refuses the read for resources
// (ADR-0056, issue #304). The saved-run read runs on the budgeted query
// database, so a big stored result can answer 503 `query_resource_exhausted`.
// The same read spends the same budget again, so the preview shows the
// server's sentence with a manual "Check again" and never polls it again on
// its own, where any other failed read retries every five seconds.

import { readFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { test, expect, resetScenario, CORPUS, SCHEDULE } from '../fixtures';
import { SEL } from '../selectors';
import type { Locator, Page } from '@playwright/test';

const EXHAUSTED = JSON.parse(
  readFileSync(`${__dirname}/../harness/wire/query-resource-exhausted.json`, 'utf8'),
);

/** Longer than the five-second poll the preview uses for any other
 * failure, so a poll that should not happen has time to show itself. */
const QUIET_MS = 6_500;

/** One named row of a run drawer's execution receipt, by its term. */
const receiptOf = (page: Page, detail: Locator) => (name: string) =>
  detail.locator('.receipt .fieldlist > div')
    .filter({ has: page.getByText(name, { exact: true }) })
    .locator('dd');

const readUrl = (netId: number, runId: number) => `**/api/v1/saved/${netId}/runs/${runId}`;

test('an exhausted stored-run read shows the sentence and waits for Check again', async ({ page, request }) => {
  test.setTimeout(40_000);
  await resetScenario(request, 'schedule');

  let refuse = true;
  let reads = 0;
  await page.route(readUrl(SCHEDULE.windowedNetId, SCHEDULE.pagedRunId), async route => {
    reads++;
    if (refuse) {
      await route.fulfill({ status: 503, json: EXHAUSTED });
      return;
    }
    await route.fallback();
  });

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();

  const preview = page.locator(SEL.netRunPreview);
  await expect(preview.locator(SEL.runExhausted)).toHaveText(EXHAUSTED.error.message);
  const check = preview.getByRole('button', { name: 'Check again', exact: true });
  await expect(check).toBeVisible();

  // Not a load failure, not an empty result, and not the 409's line.
  await expect(preview.getByText('Retrying automatically', { exact: false })).toHaveCount(0);
  await expect(preview.getByText("Couldn't load", { exact: false })).toHaveCount(0);
  await expect(preview.getByText('No result data (error or still running)')).toHaveCount(0);
  await expect(preview.getByRole('button', { name: 'Retry', exact: true })).toHaveCount(0);
  await expect(preview.locator(SEL.runUnavailable)).toHaveCount(0);
  await expect(preview.locator(SEL.previewRow)).toHaveCount(0);

  // No automatic retry: the one read stays the only one past the poll
  // interval.
  expect(reads).toBe(1);
  await page.waitForTimeout(QUIET_MS);
  expect(reads).toBe(1);
  await expect(preview.locator(SEL.runExhausted)).toBeVisible();

  // The control asks again by hand, and a refusal is still a refusal.
  await check.click();
  await expect.poll(() => reads).toBe(2);
  await expect(preview.locator(SEL.runExhausted)).toHaveText(EXHAUSTED.error.message);

  // Once the server can answer, the same control brings the rows back.
  refuse = false;
  await check.click();
  await expect(preview.locator(SEL.previewRow).first()).toBeVisible();
  await expect(preview.locator(SEL.runExhausted)).toHaveCount(0);
});

// The drawer on the runs page prints an execution receipt out of the last
// read. A run watched to completion has `running` recorded from an earlier
// poll, and the first read of its finished file is the refusal. That
// refusal summarises nothing, so the stale `running` must not outlive it.
test('an exhausted read evicts the running summary it follows', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.clock.install();

  await page.route('**/api/v1/runs?*', async route => {
    if (new URL(route.request().url()).pathname !== '/api/v1/runs') {
      await route.fallback();
      return;
    }
    const response = await route.fetch();
    const body = await response.json();
    const row = body.runs.find((run: { id: number }) => run.id === CORPUS.runWithResult);
    row.status = 'running';
    row.finished_at = null;
    row.duration_ms = null;
    row.row_count = null;
    await route.fulfill({ response, json: body });
  });

  // Hit one is the run still going; from hit two the stored file is too
  // big for the query budget.
  const running = JSON.parse(await readFile(`${__dirname}/../harness/wire/run-result.json`, 'utf8'));
  running.status = 'running';
  running.finished_at = null;
  running.duration_ms = null;
  running.row_count = null;
  delete running.result;
  let reads = 0;
  await page.route(readUrl(CORPUS.netId, CORPUS.runWithResult), async route => {
    reads++;
    if (reads === 1) {
      await route.fulfill({ json: running });
      return;
    }
    await route.fulfill({ status: 503, json: EXHAUSTED });
  });

  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  const field = receiptOf(page, detail);
  await expect(detail.locator('.sd-ttl .bdg')).toHaveText('Running');
  await expect(field('Outcome')).toHaveText('running');

  await page.clock.fastForward(5_000);
  await expect.poll(() => reads).toBeGreaterThan(1);
  await expect(detail.locator(SEL.runExhausted)).toHaveText(EXHAUSTED.error.message);
  // The server confirmed nothing about the run, so neither badge nor
  // receipt claims an outcome.
  await expect(detail.locator('.sd-ttl .bdg')).toHaveCount(0);
  await expect(field('Outcome')).toHaveText('—');

  // The poll is off: a further interval sends no read.
  const settled = reads;
  await page.clock.fastForward(12_000);
  await page.waitForTimeout(500);
  expect(reads).toBe(settled);
});
