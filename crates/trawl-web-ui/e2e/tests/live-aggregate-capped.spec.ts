// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The live cap notice (ADR-0001).
//
// One stream at a time: `POST /__ctl/stream/frame` broadcasts to every
// open SSE response, so every frame waits for `sse.open` to settle at 1.

import { readFileSync } from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

const CAPPED = JSON.parse(
  readFileSync(`${__dirname}/../harness/wire/live-snapshot-capped.json`, 'utf8'),
);
const UNCAPPED = { columns: CAPPED.columns, rows: CAPPED.rows };

const STATS = '/search?q='
  + encodeURIComponent('* | stats count() by service')
  + '&mode=live';

const GROUPS_TEXT = `New groups past 10,000 are not counted. ${COPY.liveCapAdvice}`;

async function sseOpen(request: Ctl): Promise<number> {
  return (await (await request.get('/__ctl/state')).json()).sse.open;
}

async function frame(request: Ctl, payload: unknown) {
  const sent = await request.post('/__ctl/stream/frame', {
    data: { data: JSON.stringify(payload) },
  });
  expect(sent.ok(), `frame: HTTP ${sent.status()}`).toBe(true);
  expect((await sent.json()).sent, 'exactly one stream is listening').toBe(1);
}

const notice = (page: Pg) => page.locator(SEL.liveCapNotice);
const results = (page: Pg) => page.locator(SEL.resultsPane);

async function haul(page: Pg, query: string): Promise<void> {
  await page.locator(SEL.cmContent).click();
  await page.keyboard.press('Control+a');
  await page.keyboard.insertText(query);
  await page.keyboard.press('Control+Enter');
}

test.describe('live aggregate cap notice', () => {
  test('a capped snapshot shows the notice above rows that stay, on both tabs', async ({ page, request }, testInfo) => {
    await resetScenario(request, 'stream-chart');
    await page.goto(STATS);
    await expect.poll(() => sseOpen(request)).toBe(1);

    await frame(request, UNCAPPED);
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(page.locator('.live-cap-live')).toHaveAttribute('role', 'status');
    await expect(notice(page)).toHaveCount(0);

    await frame(request, CAPPED);
    await expect(notice(page)).toBeVisible();
    await expect(notice(page).locator(SEL.liveCapNoticeTitle)).toHaveText(COPY.liveCapTitle);
    await expect(notice(page).locator(SEL.liveCapNoticeText)).toHaveText(GROUPS_TEXT);
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(results(page).getByText('postgres', { exact: true })).toBeVisible();
    await expect(page.getByRole('alert')).toHaveCount(0);
    const noticeBox = (await notice(page).boundingBox())!;
    const resultsBox = (await results(page).boundingBox())!;
    expect(noticeBox.y + noticeBox.height).toBeLessThanOrEqual(resultsBox.y + 1);

    await page.screenshot({ path: testInfo.outputPath('live-aggregate-capped.png') });

    await frame(request, { ...CAPPED, rows: [['nginx', '42']] });
    await expect(results(page).getByText('42', { exact: true })).toBeVisible();
    await expect(notice(page)).toBeVisible();

    await page.getByRole('tab', { name: 'Visualization', exact: true }).click();
    await expect(notice(page)).toBeVisible();
    await expect(notice(page).locator(SEL.liveCapNoticeText)).toHaveText(GROUPS_TEXT);
  });

  test('a disconnect keeps the notice; an uncapped snapshot or a new stream clears it', async ({ page, request }) => {
    await resetScenario(request, 'stream-chart');
    await page.goto(STATS);
    await expect.poll(() => sseOpen(request)).toBe(1);
    await frame(request, CAPPED);
    await expect(notice(page)).toBeVisible();

    await request.post('/__ctl/stream/drop');
    await expect(page.getByText('Live stream disconnected.', { exact: false })).toBeVisible();
    await expect(notice(page)).toBeVisible();
    await expect.poll(() => sseOpen(request)).toBe(1);

    await frame(request, UNCAPPED);
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(notice(page)).toHaveCount(0);

    await frame(request, CAPPED);
    await expect(notice(page)).toBeVisible();
    await haul(page, '* | stats count() by host');
    await expect(page).toHaveURL(/mode=live/);
    await expect(notice(page)).toHaveCount(0);
  });
});
