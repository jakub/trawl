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
  SCHEDULE,
} from '../fixtures';
import { SEL } from '../selectors';

const SENTENCE =
  `report run ${SCHEDULE.pagedRunId} succeeded, but its stored result is unavailable; `
  + 'no older run was substituted';

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
