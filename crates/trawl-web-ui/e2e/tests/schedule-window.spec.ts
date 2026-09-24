// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The net drawer's schedule window control (issue #181), and Run now
// (issue #236). What the form
// SHOWS is asserted in the browser; what it SENDS is asserted against
// the stub's recorded PUT bodies, because the whole subject is which
// keys travel: an omitted `window` is query mode, not "unchanged", so a
// spec that only read the rendered form would say nothing about it.

import {
  test,
  expect,
  resetScenario,
  armScheduleRefusal,
  armRunRefusal,
  capturedScheduleRequests,
  capturedRunRequests,
  trackToasts,
  toastCount,
  SCHEDULE,
} from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';

/** The window strip's option, by its visible label. */
function windowOption(scope: import('@playwright/test').Locator, label: string) {
  return scope.locator(SEL.windowOption).filter({ hasText: label });
}

/** Open one net's drawer on the Query tab, with its schedule form up. */
async function openScheduleForm(page: import('@playwright/test').Page, netId: number) {
  await page.goto(`/jobs/nets?net=${netId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await expect(drawer).toBeVisible();
  const add = drawer.getByRole('button', { name: COPY.addScheduleButton, exact: true });
  // A net with no schedule keeps the form behind this control; a net
  // with one opens straight onto it.
  if (await add.count()) await add.click();
  await expect(drawer.getByRole('button', { name: 'Save schedule', exact: true })).toBeVisible();
  return drawer;
}

test('the windowed net opens on its saved window and lag', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const drawer = await openScheduleForm(page, SCHEDULE.windowedNetId);

  // The whole vector, not just the pressed one: a strip that pressed two
  // options would still satisfy a single positive assertion.
  await expect(drawer.locator(SEL.windowOption)).toHaveText([
    COPY.windowOptionQuery,
    COPY.windowOptionSinceLast,
    COPY.windowOptionFixed,
  ]);
  const pressed = await drawer
    .locator(SEL.windowOption)
    .evaluateAll((els) => els.map((el) => el.getAttribute('aria-pressed')));
  expect(pressed).toEqual(['false', 'true', 'false']);

  await expect(drawer.locator(SEL.windowLagInput)).toHaveValue(SCHEDULE.lag);
  await expect(drawer.locator(SEL.windowLagHint)).toHaveText(COPY.windowLagHintText);
  await expect(drawer.getByText(COPY.windowSinceLastHint)).toBeVisible();
  // Tiling has no span, so the span field is not merely empty: it is
  // absent, which is what says the two modes are not one field.
  await expect(drawer.locator(SEL.windowSpanInput)).toHaveCount(0);
});

test('query text drops the window and the lag from the request', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const drawer = await openScheduleForm(page, SCHEDULE.windowedNetId);

  await windowOption(drawer, COPY.windowOptionQuery).click();
  await expect(windowOption(drawer, COPY.windowOptionQuery)).toHaveAttribute('aria-pressed', 'true');
  // Query mode has no lag to allow for, so the box goes rather than
  // sitting there holding a value the request will not carry.
  await expect(drawer.locator(SEL.windowLagInput)).toHaveCount(0);
  await expect(drawer.getByText(COPY.windowRemovalHint)).toBeVisible();
  await expect(drawer.getByText(COPY.windowRescheduleHint)).toBeVisible();

  await drawer.getByRole('button', { name: 'Save schedule', exact: true }).click();

  await expect.poll(async () => (await capturedScheduleRequests(request)).length).toBe(1);
  const [put] = await capturedScheduleRequests(request);
  expect(put.savedId).toBe(SCHEDULE.windowedNetId);
  // ABSENT, not null: `skip_serializing_if` on the request type means a
  // null would be a different wire fact, and the server reads a missing
  // `window` as query mode.
  expect(Object.keys(put.body)).not.toContain('window');
  expect(Object.keys(put.body)).not.toContain('lag');
  expect(put.body.interval).toBe('1h');
});

test('a fixed span travels with no lag beside it', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const drawer = await openScheduleForm(page, SCHEDULE.windowedNetId);

  await windowOption(drawer, COPY.windowOptionFixed).click();
  await expect(drawer.getByText(COPY.windowFixedHint)).toBeVisible();
  // The saved lag survives the mode change, so blanking it is a
  // deliberate edit rather than the form having forgotten.
  await expect(drawer.locator(SEL.windowLagInput)).toHaveValue(SCHEDULE.lag);
  await drawer.locator(SEL.windowLagInput).fill('');

  // One span is one value in the server's grammar, so it is typed into
  // one box rather than chosen from a second preset strip.
  await drawer.locator(SEL.windowSpanInput).fill('1h');
  await expect(drawer.locator(SEL.windowSpanInput)).toHaveValue('1h');

  await drawer.getByRole('button', { name: 'Save schedule', exact: true }).click();

  await expect.poll(async () => (await capturedScheduleRequests(request)).length).toBe(1);
  const [put] = await capturedScheduleRequests(request);
  expect(put.body.window).toBe('1h');
  expect(Object.keys(put.body)).not.toContain('lag');
});

test('a refused save stays in the form with the draft intact', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await trackToasts(page);
  // The PLAIN net, whose saved text carries `last=1h`: that clause is
  // the half the server's refusal names, so this is the real conflict
  // rather than a canned message pinned to an unrelated query.
  const drawer = await openScheduleForm(page, SCHEDULE.plainNetId);

  await windowOption(drawer, COPY.windowOptionSinceLast).click();
  await drawer.locator(SEL.windowLagInput).fill('90s');

  await armScheduleRefusal(request, 'refuse');
  await drawer.getByRole('button', { name: 'Save schedule', exact: true }).click();

  // The server's own sentence, character for character. The form has no
  // copy of the window/query rule and must not invent one.
  await expect(drawer.locator(SEL.scheduleSaveError)).toHaveText(
    'schedule window "since_last" conflicts with the saved query\'s last= time clause; remove one side',
  );
  await expect(drawer.locator(SEL.scheduleSaveError)).toHaveAttribute('role', 'alert');
  // A refusal the operator has to act on does not fly past as a toast.
  expect(await toastCount(page)).toBe(0);
  // And the draft is still there to fix: a form that reset itself would
  // make the operator retype what the server just rejected.
  await expect(windowOption(drawer, COPY.windowOptionSinceLast)).toHaveAttribute('aria-pressed', 'true');
  await expect(drawer.locator(SEL.windowLagInput)).toHaveValue('90s');

  // A 5xx with no envelope has nothing to quote, so the inline error
  // falls back to the client's own status text rather than going blank.
  await armScheduleRefusal(request, 'fail');
  await drawer.getByRole('button', { name: 'Save schedule', exact: true }).click();
  await expect(drawer.locator(SEL.scheduleSaveError)).toHaveText(
    nameFrom(COPY.apiStatusText, '500'),
  );
  expect(await toastCount(page)).toBe(0);
  expect(await capturedScheduleRequests(request)).toHaveLength(2);
});

// ---- Run now ---------------------------------------------------------------
// A manual run is the schedule's next window fired early, so it is on
// offer wherever a schedule is saved, in every mode and paused or not,
// and nowhere else (ADR-0018 as amended on 2026-09-23).

/** Every net in the `schedule` scenario, and whether it has a schedule. */
const RUN_NOW_CASES = [
  { id: SCHEDULE.plainNetId, name: 'errors by host', offered: false },
  { id: SCHEDULE.windowedNetId, name: SCHEDULE.windowedNetName, offered: true },
  { id: SCHEDULE.fixedNetId, name: 'fixed error window', offered: true },
  { id: SCHEDULE.queryModeNetId, name: 'paused error sweep', offered: true },
] as const;

test('Run now is offered on every scheduled net and on no other', async ({ page, request }) => {
  await resetScenario(request, 'schedule');

  for (const { id, offered } of RUN_NOW_CASES) {
    await page.goto(`/jobs/nets?net=${id}&ntab=query`);
    const drawer = page.locator(SEL.drawerPanel);
    await expect(drawer).toBeVisible();
    await expect(drawer.getByRole('button', { name: 'Search' })).toBeVisible();
    await expect(drawer.getByRole('button', { name: COPY.netRunNow, exact: true })).toHaveCount(
      offered ? 1 : 0,
    );
  }

  // The nets table's direct actions read the same saved state through the
  // same predicate, so the two cannot disagree about the offer.
  await page.goto('/jobs/nets');
  const rows = page.locator(SEL.tableRow);
  await expect(rows).toHaveCount(RUN_NOW_CASES.length);
  for (const { name, offered } of RUN_NOW_CASES) {
    const action = rows.filter({ hasText: name }).getByRole('button', { name: COPY.netRunNow, exact: true });
    await expect(action).toHaveCount(offered ? 1 : 0);
  }
  // Looking sends nothing.
  expect(await capturedRunRequests(request)).toEqual([]);
});

test('Run now in the drawer states the claimed window and the run shows as Manual', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const drawer = page.locator(SEL.drawerPanel);
  await drawer.getByRole('button', { name: COPY.netRunNow, exact: true }).click();

  // The bounds are the server's claim, from the net's watermark to the
  // claim instant less its lag, printed in UTC. The browser computes none.
  const toast = page.locator(SEL.toastSuccess);
  await expect(toast).toHaveCount(1);
  await expect(toast.locator('.title')).toHaveText(nameFrom(COPY.runStartedToast, '09:55', '11:15'));
  const link = page.locator(SEL.toastLink);
  await expect(link).toHaveText(COPY.runViewLink);
  await expect(link).toHaveAttribute('href', `/jobs/runs?run=504&net=${SCHEDULE.windowedNetId}`);
  expect(await capturedRunRequests(request)).toEqual([SCHEDULE.windowedNetId]);

  // The run leads the net's history, and it alone is marked Manual.
  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=runs`);
  const history = page.locator(SEL.netRunRow);
  await expect(history).toHaveCount(4);
  await expect(history.first()).toContainText(COPY.runOriginManual);
  await expect(history.filter({ hasText: COPY.runOriginManual })).toHaveCount(1);
});

test('Run now in a row is disabled while its request is in flight', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  // Hold the request so the in-flight state can be observed, then let it
  // through to the stub.
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route(`**/api/v1/saved/${SCHEDULE.fixedNetId}/run`, async (route) => {
    await held;
    await route.continue();
  });

  await page.goto('/jobs/nets');
  const action = page
    .locator(SEL.tableRow)
    .filter({ hasText: 'fixed error window' })
    .getByRole('button', { name: COPY.netRunNow, exact: true });
  await action.click();
  await expect(action).toBeDisabled();

  release();
  await expect(action).toBeEnabled();
  // A fixed span reads the span ending now.
  await expect(page.locator(SEL.toastSuccess).locator('.title')).toHaveText(
    nameFrom(COPY.runStartedToast, '11:00', '11:15'),
  );
  expect(await capturedRunRequests(request)).toEqual([SCHEDULE.fixedNetId]);
});

test('Run now of a query-mode schedule claims no window', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto('/jobs/nets');
  await page
    .locator(SEL.tableRow)
    .filter({ hasText: 'paused error sweep' })
    .getByRole('button', { name: COPY.netRunNow, exact: true })
    .click();
  // Paused, and still fired: the schedule's enabled flag gates the
  // scheduler, not a person.
  await expect(page.locator(SEL.toastSuccess).locator('.title')).toHaveText(COPY.runStartedPlain);
  expect(await capturedRunRequests(request)).toEqual([SCHEDULE.queryModeNetId]);
});

test('a refused Run now shows the server\'s message', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  await page.goto('/jobs/nets');
  await armRunRefusal(request);
  await page
    .locator(SEL.tableRow)
    .filter({ hasText: SCHEDULE.windowedNetName })
    .getByRole('button', { name: COPY.netRunNow, exact: true })
    .click();

  // The server's own sentence, which names where coverage stands, and
  // never the client's bare status text.
  const toast = page.locator(SEL.toastError);
  await expect(toast.locator('.title')).toHaveText(COPY.runNotStarted);
  await expect(toast.locator('.detail')).toHaveText(
    'nothing new to read: coverage already reaches 2026-09-01T11:15:00.000000Z, and a run now would end at 2026-09-01T11:14:30.000000Z',
  );
  await expect(toast).not.toContainText(nameFrom(COPY.apiStatusText, '409'));
  await expect(page.locator(SEL.toastSuccess)).toHaveCount(0);
});

test('the schedule form says what Run now reads in each windowed mode', async ({ page, request }) => {
  await resetScenario(request, 'schedule');
  const drawer = await openScheduleForm(page, SCHEDULE.windowedNetId);

  await expect(drawer.getByText(COPY.runNowSinceLastLine)).toBeVisible();
  await expect(drawer.getByText(COPY.runNowFixedLine)).toHaveCount(0);

  await windowOption(drawer, COPY.windowOptionFixed).click();
  await expect(drawer.getByText(COPY.runNowFixedLine)).toBeVisible();
  await expect(drawer.getByText(COPY.runNowSinceLastLine)).toHaveCount(0);

  // Query mode reads the saved text whenever it runs: nothing to say.
  await windowOption(drawer, COPY.windowOptionQuery).click();
  await expect(drawer.getByText(COPY.runNowFixedLine)).toHaveCount(0);
  await expect(drawer.getByText(COPY.runNowSinceLastLine)).toHaveCount(0);
});
