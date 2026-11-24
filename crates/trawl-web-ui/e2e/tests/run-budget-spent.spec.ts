// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario, capturedRunRequests, SPENT } from '../fixtures';
import { SEL, COPY } from '../selectors';

function netRow(page: import('@playwright/test').Page, name: string) {
  return page.locator(SEL.tableRow).filter({ hasText: name });
}

test('the nets list disables Run now on a spent net and says why', async ({ page, request }) => {
  await resetScenario(request, 'schedule-spent');
  await page.goto('/jobs/nets');

  const capped = netRow(page, SPENT.cappedNetName);
  await expect(capped.getByRole('button', { name: COPY.netRunNow, exact: true })).toBeEnabled();
  await expect(capped.getByText(/^Next: /)).toBeVisible();
  await expect(capped.getByText(/^spent /)).toHaveCount(0);

  for (const [name, label] of [
    [SPENT.spentNetName, SPENT.spentLabel],
    [SPENT.zeroNetName, SPENT.zeroLabel],
  ] as const) {
    const row = netRow(page, name);
    const runNow = row.getByRole('button', { name: COPY.netRunNow, exact: true });
    await expect(runNow).toBeVisible();
    await expect(runNow).toBeDisabled();
    await expect(runNow).toHaveAccessibleDescription(COPY.netRunNowSpentReason);
    await expect(runNow).toHaveAttribute('title', COPY.netRunNowSpentReason);
    await expect(row.getByText(label, { exact: true })).toBeVisible();
    await expect(row.getByText(/^Next: /)).toHaveCount(0);
    await expect(row.getByText('Pending…')).toHaveCount(0);
    expect(Number(await runNow.evaluate((el) => getComputedStyle(el).opacity))).toBeLessThan(1);
  }

  const deleteX = async (name: string) =>
    (await netRow(page, name).locator('.net-delete').boundingBox())!.x;
  expect(await deleteX(SPENT.spentNetName)).toBe(await deleteX(SPENT.cappedNetName));

  await expect.poll(() => capturedRunRequests(request)).toEqual([]);

  // Positive control: the recorder sees a Run now POST when one is sent.
  await capped.getByRole('button', { name: COPY.netRunNow, exact: true }).click();
  await expect.poll(() => capturedRunRequests(request)).toEqual([SPENT.cappedNetId]);
});

test('the net drawer disables Run now on a spent net and says why', async ({ page, request }) => {
  await resetScenario(request, 'schedule-spent');
  await page.goto(`/jobs/nets?net=${SPENT.spentNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await expect(drawer).toBeVisible();

  const runNow = drawer.getByRole('button', { name: COPY.netRunNow, exact: true });
  await expect(runNow).toBeVisible();
  await expect(runNow).toBeDisabled();
  await expect(runNow).toHaveAccessibleDescription(COPY.netRunNowSpentReason);
  await expect(drawer.getByText(SPENT.spentLabel, { exact: true })).toBeVisible();
  await expect(drawer.getByText(COPY.netRunNowSpentReason, { exact: true })).toBeVisible();

  await page.goto(`/jobs/nets?net=${SPENT.cappedNetId}&ntab=query`);
  await expect(drawer).toBeVisible();
  await expect(drawer.getByRole('button', { name: COPY.netRunNow, exact: true })).toBeEnabled();
  await expect(drawer.getByText(COPY.netRunNowSpentReason)).toHaveCount(0);

  await expect.poll(() => capturedRunRequests(request)).toEqual([]);
});
