// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The live cap notice (ADR-0001): what Search says above a live
// aggregate whose snapshot reports `capped`.
//
// The stream's aggregation dropped input past a cap, so its rows leave
// something out. The rows are still the live answer and stay on screen;
// the notice sits above them, names each cap with the limit the frame
// carried, and stays for as long as the snapshots keep reporting it. A
// disconnect says nothing about the aggregation, so it does not clear
// the notice. A snapshot with no `capped`, or a new stream, does.
//
// One stream at a time, as in live-chart.spec.ts: `POST
// /__ctl/stream/frame` broadcasts to every open SSE response, so every
// frame waits for `sse.open` to settle at 1.

import { readFileSync } from 'node:fs';
import { test, expect, resetScenario } from '../fixtures';
import { SEL, COPY } from '../selectors';

type Pg = import('@playwright/test').Page;
type Ctl = import('@playwright/test').APIRequestContext;

/// The frame trawld sends once `stats … by` drops a new group.
/// e2e_wire_fixture_contract.rs ties it to trawl-core's own report.
const CAPPED = JSON.parse(
  readFileSync(`${__dirname}/../harness/wire/live-snapshot-capped.json`, 'utf8'),
);
/// The same rows with nothing dropped: trawld omits the field.
const UNCAPPED = { columns: CAPPED.columns, rows: CAPPED.rows };

const STATS = '/search?q='
  + encodeURIComponent('* | stats count() by service')
  + '&mode=live';

/// What the notice says for the fixture's one cap, built from the limit
/// the frame carried.
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

/// Replace the editor's text and run it, the way a person does.
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

    // Nothing dropped: the rows, no notice, and the status region waiting.
    await frame(request, UNCAPPED);
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(page.locator('.live-cap-live')).toHaveAttribute('role', 'status');
    await expect(notice(page)).toHaveCount(0);

    // The cap is reported: the notice names it with the frame's limit.
    await frame(request, CAPPED);
    await expect(notice(page)).toBeVisible();
    await expect(notice(page).locator(SEL.liveCapNoticeTitle)).toHaveText(COPY.liveCapTitle);
    await expect(notice(page).locator(SEL.liveCapNoticeText)).toHaveText(GROUPS_TEXT);
    // A partial answer is still an answer: the rows stay, and nothing
    // reads it as a failure.
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(results(page).getByText('postgres', { exact: true })).toBeVisible();
    await expect(page.getByRole('alert')).toHaveCount(0);
    // Above the results, not inside them.
    const noticeBox = (await notice(page).boundingBox())!;
    const resultsBox = (await results(page).boundingBox())!;
    expect(noticeBox.y + noticeBox.height).toBeLessThanOrEqual(resultsBox.y + 1);

    await page.screenshot({ path: testInfo.outputPath('live-aggregate-capped.png') });

    // Every later snapshot repeats the report, and the notice stays.
    await frame(request, { ...CAPPED, rows: [['nginx', '42']] });
    await expect(results(page).getByText('42', { exact: true })).toBeVisible();
    await expect(notice(page)).toBeVisible();

    // The Visualization tab draws the same partial rows, under the same notice.
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

    // The connection drops and the browser reconnects. The failure copy
    // is the connection's; the cap report is the aggregation's, and only
    // a snapshot can change it.
    await request.post('/__ctl/stream/drop');
    await expect(page.getByText('Live stream disconnected.', { exact: false })).toBeVisible();
    await expect(notice(page)).toBeVisible();
    await expect.poll(() => sseOpen(request)).toBe(1);

    // The reconnected stream's first snapshot dropped nothing.
    await frame(request, UNCAPPED);
    await expect(results(page).getByText('nginx', { exact: true })).toBeVisible();
    await expect(notice(page)).toHaveCount(0);

    // Capped again, then a new query: a new stream starts with nothing dropped.
    await frame(request, CAPPED);
    await expect(notice(page)).toBeVisible();
    await haul(page, '* | stats count() by host');
    await expect(page).toHaveURL(/mode=live/);
    await expect(notice(page)).toHaveCount(0);
  });
});
