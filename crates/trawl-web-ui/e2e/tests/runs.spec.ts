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

const sentenceFor = (runId: number) =>
  `report run ${runId} succeeded, but its stored result is unavailable; `
  + 'no older run was substituted';

const SENTENCE = sentenceFor(SCHEDULE.pagedRunId);

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
// was — while the list the page already holds knows all of it.
test('unavailable result keeps the receipt', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await armRunUnavailable(request, CORPUS.runWithResult);

  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  await expect(detail.locator(SEL.runUnavailable)).toHaveText(sentenceFor(CORPUS.runWithResult));

  // `tone_vocab::run_status_label("success")`, which is what the title
  // badge calls a run that finished.
  await expect(detail.locator('.sd-ttl')).toContainText('Succeeded');

  // The receipt prints the recorded status verbatim; the badge above is
  // where the reader-facing label lives.
  const field = (name: string) => detail.locator('.receipt .fieldlist > div')
    .filter({ has: page.getByText(name, { exact: true }) })
    .locator('dd');
  await expect(field('Outcome')).toHaveText('success');
  await expect(field('Rows recorded')).toHaveText(String(CORPUS.runWithResultRows));
  await expect(field('Query')).toHaveText(CORPUS.runWithResultQuery);
  await expect(field('Duration')).not.toHaveText('—');
});
