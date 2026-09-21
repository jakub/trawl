// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A run that succeeded but whose stored result file is gone (issue #227).
// The server answers 409 with a sentence naming the run; the drawer has
// to SHOW that sentence. The two strings it must not show are the ones
// it used to: "No result data (error or still running)", which calls a
// finished run unfinished, and the generic "Couldn't load" wrapper,
// which calls a read that answered a failure.

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

/** One named row of a run drawer's execution receipt, by its term. */
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

  // Neither of the sentences that would misdescribe the run. Scoped to
  // the preview: the runs table beside it carries a FAILED run whose own
  // error message is a "Couldn't load" the server wrote.
  await expect(preview.getByText('No result data (error or still running)')).toHaveCount(0);
  await expect(preview.getByText("Couldn't load", { exact: false })).toHaveCount(0);
  // And no table pretending the run held rows.
  await expect(preview.locator(SEL.previewRow)).toHaveCount(0);

  // Terminal: the 5s poll does not keep re-reading a run whose answer
  // cannot change on its own.
  await page.waitForLoadState('networkidle');
  const before = await runDetailReadCount(request);
  await page.waitForTimeout(6000);
  expect(await runDetailReadCount(request)).toBe(before);

  // The explicit control asks again, and the answer is the same.
  await preview.getByRole('button', { name: 'Check again' }).click();
  await expect.poll(() => runDetailReadCount(request)).toBeGreaterThan(before);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);
});

// The runs page mounts the same preview beside its own execution
// receipt. The 409 carries no summary, so a receipt fed only by that
// read would go blank exactly when the page has to say what the run
// was — while the list the page already holds knows all of it. The
// list is a LOADED PAGE, though: reading on past the selected run has
// to leave the drawer beside it intact.
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

  // Reading on: the run leaves the loaded page, and the drawer keeps the
  // last listing it saw under this selection — the same retention the
  // net name has always had.
  const list = page.getByRole('region', { name: 'Recent runs table', exact: true });
  await list.getByRole('button', { name: 'Next →', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText('Other page');
  await expect(page.locator('.runs-table')).not.toContainText(CORPUS.netName);
  await filled();

  await list.getByRole('button', { name: '← Prev', exact: true }).click();
  await expect(page.locator('.runs-table')).toContainText(CORPUS.netName);
  await filled();
});

// The list is a snapshot, and a row read while the run was still going
// says `running`. The server answers this 409 only for a run whose query
// SUCCEEDED and wrote a file it can no longer find, so pairing that row
// with the refusal unreconciled would print "report run N succeeded,
// but…" beside a Running badge. Until the read has answered at all, the
// receipt has nothing confirmed to print and prints nothing.
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

  // The stale row is on screen and the run's own read is still open.
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
  // Only the outcome was stale. A run still going had recorded no
  // duration and no rows, and the receipt does not invent either.
  await expect(field('Duration')).toHaveText('—');
  await expect(field('Rows recorded')).toHaveText('—');
  await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);
});

// The other order: the run was selected while it was STILL GOING, so the
// first read is a 200 that records `running` here, and only the next poll
// finds the file gone. The refusal carries no summary of its own, so it
// has to evict the one it supersedes — a receipt still reading the older
// answer says Running beside a sentence that says the run succeeded, and
// nothing later repairs it because the 409 ends the polling.
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

// The net drawer keeps an expanded run on screen after its list has
// moved past it, out of the last summary it saw for that run. A run
// whose stored result is gone has a 409 for its last READ, and a 409
// carries no summary — so the row has to survive one.
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

  // The list moves past the expanded run. The row is the drawer's now,
  // not the page's, and the refusal is not a reason to drop it.
  listed = false;
  const before = lists;
  await page.clock.fastForward(5_000);
  await expect.poll(() => lists).toBeGreaterThan(before);
  await expect(page.getByText('Expanded run outside this page')).toHaveCount(1);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);
  await expect(preview.locator(SEL.runUnavailable)).toHaveText(SENTENCE);
});
