// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The net drawer's schedule window control (issue #181). What the form
// SHOWS is asserted in the browser; what it SENDS is asserted against
// the stub's recorded PUT bodies, because the whole subject is which
// keys travel: an omitted `window` is query mode, not "unchanged", so a
// spec that only read the rendered form would say nothing about it.

import {
  test,
  expect,
  resetScenario,
  armScheduleRefusal,
  capturedScheduleRequests,
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

  const chip = drawer.locator(SEL.windowSpanChip).filter({ hasText: '1h' });
  await chip.click();
  await expect(chip).toHaveAttribute('aria-pressed', 'true');
  // One value shown two ways: a pressed preset leaves the custom box
  // blank rather than repeating itself.
  await expect(drawer.locator(SEL.windowSpanInput)).toHaveValue('');

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

test('the manual run is offered only where no window owns the coverage', async ({ page, request }) => {
  await resetScenario(request, 'schedule');

  await page.goto(`/jobs/nets?net=${SCHEDULE.windowedNetId}&ntab=query`);
  const windowed = page.locator(SEL.drawerPanel);
  await expect(windowed).toBeVisible();
  await expect(windowed.getByRole('button', { name: 'Search' })).toBeVisible();
  // A windowed schedule advances its own coverage point, so a run out of
  // band would leave a hole it never revisits: the offer is withdrawn.
  await expect(windowed.getByRole('button', { name: COPY.netRunAction })).toHaveCount(0);

  await page.goto(`/jobs/nets?net=${SCHEDULE.plainNetId}&ntab=query`);
  const plain = page.locator(SEL.drawerPanel);
  await expect(plain).toBeVisible();
  await expect(plain.getByRole('button', { name: COPY.netRunAction })).toHaveCount(1);

  // The nets table's row menu reads the same saved state, so the two
  // cannot disagree about whether a run is on offer.
  await page.goto('/jobs/nets');
  const rows = page.locator(SEL.tableRow);
  await expect(rows).toHaveCount(2);
  for (const [name, offered] of [['errors by host', true], [SCHEDULE.windowedNetName, false]] as const) {
    await rows.filter({ hasText: name }).locator(SEL.actionsMenuTrigger).click();
    const items = page.locator(SEL.actionsMenuItem);
    await expect(items.filter({ hasText: COPY.netTriggerAction })).toHaveCount(offered ? 1 : 0);
    await page.keyboard.press('Escape');
  }
});
