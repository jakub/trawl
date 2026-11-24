// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import {
  test,
  expect,
  resetScenario,
  armRunUnavailable,
  runDetailReadCount,
  CORPUS,
  SCHEDULE,
} from '../fixtures';
import { SEL } from '../selectors';
import type { Locator, Page } from '@playwright/test';
import { readFile } from 'node:fs/promises';

const sentenceFor = (runId: number) =>
  `report run ${runId} succeeded, but its stored result is unavailable; `
  + 'no older run was substituted';

const SENTENCE = sentenceFor(SCHEDULE.pagedRunId);

const receiptOf = (page: Page, detail: Locator) => (name: string) =>
  detail.locator('.receipt .fieldlist > div')
    .filter({ has: page.getByText(name, { exact: true }) })
    .locator('dd');

/** The `tone_vocab::run_status_label` the title badge prints for a run
 * the server says succeeded. */
const SUCCEEDED = 'Succeeded';

test('unavailable result names the run', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await armRunUnavailable(request, SCHEDULE.pagedRunId);

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();

  const preview = page.locator(SEL.netRunPreview);
  await expect(preview).toHaveCount(1);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);

  // Scoped to the preview: the runs table beside it carries a FAILED run
  // whose own error message is a "Couldn't load" the server wrote.
  await expect(preview.getByText('No result data (error or still running)')).toHaveCount(0);
  await expect(preview.getByText("Couldn't load", { exact: false })).toHaveCount(0);
  await expect(preview.locator(SEL.previewRow)).toHaveCount(0);

  // Terminal: the 5s poll does not keep re-reading a run whose answer
  // cannot change on its own.
  await page.waitForLoadState('networkidle');
  const before = await runDetailReadCount(request);
  await page.waitForTimeout(6000);
  expect(await runDetailReadCount(request)).toBe(before);

  await preview.getByRole('button', { name: 'Check again' }).click();
  await expect.poll(() => runDetailReadCount(request)).toBeGreaterThan(before);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);
});

test('unavailable result keeps the receipt', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await armRunUnavailable(request, CORPUS.runWithResult);

  // Two pages over one complete wire record, so the selected run can be
  // paged away from. The corpus list is two rows and cannot page.
  const listed = JSON.parse(await readFile(`${__dirname}/../harness/wire/runs-all.json`, 'utf8')).runs[0];
  await page.route('**/api/v1/runs?*', async route => {
    if (new URL(route.request().url()).pathname !== '/api/v1/runs') {
      await route.fallback();
      return;
    }
    const response = await route.fetch();
    const body = await response.json();
    const offset = Number(new URL(route.request().url()).searchParams.get('offset'));
    body.runs = offset === 0 ? [listed] : [{ ...listed, id: 999, net_id: 9, net_name: 'Other page' }];
    body.total = 21;
    await route.fulfill({ response, json: body });
  });

  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  const field = receiptOf(page, detail);
  await expect(detail.locator(SEL.runUnavailable)).toHaveText(sentenceFor(CORPUS.runWithResult));

  // The title badge speaks `tone_vocab`; the receipt prints the recorded
  // status verbatim.
  const filled = async () => {
    await expect(detail.locator('.sd-ttl .bdg')).toHaveText(SUCCEEDED);
    await expect(field('Outcome')).toHaveText('success');
    await expect(field('Rows recorded')).toHaveText(String(CORPUS.runWithResultRows));
    await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);
    await expect(field('Duration')).not.toHaveText('—');
  };
  await filled();

  const list = page.getByRole('region', { name: 'Recent runs table', exact: true });
  await list.getByRole('button', { name: 'Next →', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText('Other page');
  await expect(page.locator('.runs-table')).not.toContainText(CORPUS.netName);
  await filled();

  await list.getByRole('button', { name: '← Prev', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText(CORPUS.netName);
  await filled();
});

test('unavailable result reconciles a stale running row', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await armRunUnavailable(request, CORPUS.runWithResult);

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

  let reads = 0;
  let release: (() => void) | undefined;
  await page.route(`**/api/v1/saved/${CORPUS.netId}/runs/${CORPUS.runWithResult}`, async route => {
    const response = await route.fetch();
    reads++;
    await new Promise<void>(resolve => { release = resolve; });
    await route.fulfill({ response });
  });

  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  const field = receiptOf(page, detail);

  await expect(page.locator('.runs-table')).toContainText('Running');
  await expect.poll(() => reads).toBeGreaterThan(0);
  await expect(field('Outcome')).toHaveText('—');
  await expect(field('Query')).toHaveText('—');
  await expect(detail.locator('.sd-ttl .bdg')).toHaveCount(0);

  release!();
  await expect(detail.locator(SEL.runUnavailable)).toHaveText(sentenceFor(CORPUS.runWithResult));
  const badge = detail.locator('.sd-ttl .bdg');
  await expect(badge).toHaveText(SUCCEEDED);
  await expect(badge).toHaveClass(/success/);
  await expect(field('Outcome')).toHaveText('success');
  await expect(field('Duration')).toHaveText('—');
  await expect(field('Rows recorded')).toHaveText('—');
  await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);
});

test('unavailable result replaces a running summary', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await armRunUnavailable(request, CORPUS.runWithResult);
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

  // Hit one is the run still going; from hit two the harness's own armed
  // 409 answers, so the sentence stays the server's.
  const running = JSON.parse(await readFile(`${__dirname}/../harness/wire/run-result.json`, 'utf8'));
  running.status = 'running';
  running.finished_at = null;
  running.duration_ms = null;
  running.row_count = null;
  delete running.result;
  let reads = 0;
  await page.route(`**/api/v1/saved/${CORPUS.netId}/runs/${CORPUS.runWithResult}`, async route => {
    reads++;
    if (reads === 1) {
      await route.fulfill({ json: running });
      return;
    }
    await route.fulfill({ response: await route.fetch() });
  });

  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  const field = receiptOf(page, detail);
  await expect(detail.locator('.sd-ttl .bdg')).toHaveText('Running');
  await expect(field('Outcome')).toHaveText('running');
  await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);

  // The five-second poll of the same mounted drawer, not a new selection.
  await page.clock.fastForward(5_000);
  await expect.poll(() => reads).toBeGreaterThan(1);
  await expect(detail.locator(SEL.runUnavailable)).toHaveText(sentenceFor(CORPUS.runWithResult));
  const badge = detail.locator('.sd-ttl .bdg');
  await expect(badge).toHaveText(SUCCEEDED);
  await expect(badge).toHaveClass(/success/);
  await expect(field('Outcome')).toHaveText('success');
  await expect(field('Duration')).toHaveText('—');
  await expect(field('Rows recorded')).toHaveText('—');
  await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);
});

test('unavailable result keeps the expanded row after the list moves on', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await armRunUnavailable(request, SCHEDULE.pagedRunId);
  await page.clock.install();

  let lists = 0;
  let listed = true;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`, async route => {
    if (new URL(route.request().url()).pathname !== `/api/v1/saved/${SCHEDULE.windowedNetId}/runs`) {
      await route.fallback();
      return;
    }
    const response = await route.fetch();
    const body = await response.json();
    lists++;
    if (!listed) body.runs = body.runs.filter((run: { id: number }) => run.id !== SCHEDULE.pagedRunId);
    await route.fulfill({ response, json: body });
  });

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  const preview = page.locator(SEL.netRunPreview);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);

  listed = false;
  const before = lists;
  await page.clock.fastForward(5_000);
  await expect.poll(() => lists).toBeGreaterThan(before);
  await expect(page.getByText('Expanded run outside this page')).toHaveCount(1);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);
});

test('unavailable result settles the expanded row of a running run', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await armRunUnavailable(request, SCHEDULE.pagedRunId);
  await page.clock.install();

  let lists = 0;
  let listed = true;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs?*`, async route => {
    if (new URL(route.request().url()).pathname !== `/api/v1/saved/${SCHEDULE.windowedNetId}/runs`) {
      await route.fallback();
      return;
    }
    const response = await route.fetch();
    const body = await response.json();
    lists++;
    body.runs = listed
      ? body.runs.map((run: { id: number; status: string; finished_at: unknown; duration_ms: unknown; row_count: unknown }) =>
          run.id === SCHEDULE.pagedRunId
            ? { ...run, status: 'running', finished_at: null, duration_ms: null, row_count: null }
            : run)
      : body.runs.filter((run: { id: number }) => run.id !== SCHEDULE.pagedRunId);
    await route.fulfill({ response, json: body });
  });

  const running = JSON.parse(await readFile(`${__dirname}/../harness/wire/run-result-paged.json`, 'utf8'));
  running.status = 'running';
  running.finished_at = null;
  running.duration_ms = null;
  running.row_count = null;
  delete running.result;
  let reads = 0;
  await page.route(`**/api/v1/saved/${SCHEDULE.windowedNetId}/runs/${SCHEDULE.pagedRunId}`, async route => {
    reads++;
    if (reads === 1) {
      await route.fulfill({ json: running });
      return;
    }
    await route.fulfill({ response: await route.fetch() });
  });

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  const row = page.locator(SEL.netRunRow).filter({ has: page.locator('[aria-expanded="true"]') });
  await expect(row.locator('td').nth(1)).toHaveText('Running');
  // The FIRST read has to have landed and been rendered before the clock
  // moves: the poll below skips a tick while a read is still in flight,
  // and the row's cell above is the list's word, not the read's.
  await expect(page.locator(SEL.netRunPreview))
    .toContainText('No result data (error or still running)');

  listed = false;
  const before = lists;
  // Two periods, so neither poll can land on the far edge of the jump.
  await page.clock.fastForward(11_000);
  await expect.poll(() => lists).toBeGreaterThan(before);
  await expect.poll(() => reads).toBeGreaterThan(1);
  await expect(page.locator(SEL.netRunPreview).locator(SEL.runUnavailable)).toHaveText(SENTENCE);
  await expect(page.getByText('Expanded run outside this page')).toHaveCount(1);
  await expect(row.locator('td').nth(1)).toHaveText(SUCCEEDED);
});
