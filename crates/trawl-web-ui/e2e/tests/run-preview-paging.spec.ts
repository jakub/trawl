// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import {
  test,
  expect,
  resetScenario,
  runDetailReadCount,
  SCHEDULE,
} from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';

async function expandPagedRun(page: import('@playwright/test').Page) {
  const row = page.locator(SEL.netRunRow).first();
  await row.locator(SEL.rowStretch).click();
  const preview = page.locator(SEL.netRunPreview);
  await expect(preview).toHaveCount(1);
  return preview;
}

test('the stored preview pages in the browser without a second read', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  await expect(page.locator(SEL.netRunRow)).toHaveCount(3);

  const preview = await expandPagedRun(page);
  const next = preview.locator(SEL.resultsFooter).getByRole('button', { name: 'Next' });
  const prev = preview.locator(SEL.resultsFooter).getByRole('button', { name: 'Prev' });

  await expect(preview.locator(SEL.previewRow)).toHaveCount(SCHEDULE.previewPageSize);
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`1–20 of ${SCHEDULE.pagedRunRows}`);
  await expect(preview.locator(SEL.previewRow).first()).toContainText('row-01');
  await expect(prev).toBeDisabled();
  await expect(preview.locator(SEL.previewCap)).toHaveCount(0);
  await expect.poll(() => runDetailReadCount(request)).toBe(1);

  await next.click();
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`21–40 of ${SCHEDULE.pagedRunRows}`);
  await expect(preview.locator(SEL.previewRow).first()).toContainText('row-21');

  await next.click();
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`41–45 of ${SCHEDULE.pagedRunRows}`);
  await expect(preview.locator(SEL.previewRow)).toHaveCount(5);
  await expect(preview.locator(SEL.previewRow).last()).toContainText('row-45');
  await expect(next).toBeDisabled();

  // Wait for the network to go quiet first, so a late second fetch cannot
  // slip past an assertion that read the counter too early.
  await page.waitForLoadState('networkidle');
  expect(await runDetailReadCount(request)).toBe(1);

  await page.locator(SEL.netRunRow).first().locator(SEL.rowStretch).click();
  await expect(page.locator(SEL.netRunPreview)).toHaveCount(0);
  const reopened = await expandPagedRun(page);
  await expect(reopened.locator(SEL.resultsSummary)).toHaveText(`1–20 of ${SCHEDULE.pagedRunRows}`);
  await expect.poll(() => runDetailReadCount(request)).toBe(2);
});

test('a preview that fetched less than the run stored says so', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  // The fixture ships the two equal so the uncapped case is testable at
  // all, so the capped one is made here, on the wire.
  const stored = 100;
  await page.route(`**/api/v1/saved/*/runs/${SCHEDULE.pagedRunId}`, async (route) => {
    const response = await route.fetch();
    const body = await response.json();
    body.row_count = stored;
    await route.fulfill({ response, json: body });
  });

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  const preview = await expandPagedRun(page);

  await expect(preview.locator(SEL.previewCap)).toHaveText(
    nameFrom(COPY.previewCapLine, String(stored), String(SCHEDULE.pagedRunRows)),
  );
  await expect(preview.locator(SEL.resultsSummary)).toHaveText(`1–20 of ${SCHEDULE.pagedRunRows}`);
});
