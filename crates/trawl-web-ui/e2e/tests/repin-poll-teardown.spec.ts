// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A leaked Interval is network-silent — its body is
// `poll_once.try_run(())`, which no-ops once the owner is disposed —
// so only the browser's own timer table can see it (`trackIntervals`).
// A flat status-hit count would stay flat under a leaked timer too (the
// in-flight guard sees a read still parked), so it is never a stand-in.
//
// The e2e identity holds no `schema_write`, so the mount-time status
// probe is the ONLY way a poll can start here.

import {
  test,
  expect,
  intervalCount,
  scriptRepinStatus,
  toastCount,
  trackIntervals,
  trackToasts,
} from '../fixtures';
import { SEL, TIMING } from '../selectors';

type Ctl = import('@playwright/test').APIRequestContext;
type Pg = import('@playwright/test').Page;

const FIELD_A = 'duration';
const FIELD_B = 'status';

const QUIET_MS = 500;

type RepinState = {
  field: string | null;
  hits: number;
  held: boolean;
  aborted: boolean;
  pendingAtRelease: boolean | null;
};

async function repinState(request: Ctl): Promise<RepinState> {
  const state = await (await request.get('/__ctl/state')).json();
  return state.repin as RepinState;
}

async function waitForRepinState(
  request: Ctl,
  pred: (s: RepinState) => boolean,
  what: string,
  timeoutMs = 5_000,
): Promise<RepinState> {
  const deadline = Date.now() + timeoutMs;
  let last: RepinState | null = null;
  for (;;) {
    last = await repinState(request);
    if (pred(last)) return last;
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}; last state was ${JSON.stringify(last)}`);
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

async function waitForHeldStatusRead(request: Ctl, n: number): Promise<RepinState> {
  return waitForRepinState(
    request,
    (s) => s.hits === n && s.held,
    `status read ${n} to be parked open by the stub`,
  );
}

/** Read 1 is the mount probe, read 2 the immediate `poll_once` the
 * adoption starts. The stub parks read 2, so the in-flight guard makes
 * every later tick a no-op and the hit count stays at 2. */
async function expectPollStarted(page: Pg, request: Ctl) {
  await expect(page.locator(SEL.fieldCase)).toBeVisible();
  await expect(
    page.locator(SEL.fieldCaseJob),
    'the drawer never adopted the running job, so there was no poll to tear down',
  ).toBeVisible();
  await waitForHeldStatusRead(request, 2);
  expect(
    await intervalCount(page, TIMING.repinPollMs),
    'expected exactly one live repin poll interval while the case file is open',
  ).toBe(1);
}

async function openCaseFile(page: Pg, request: Ctl, url: string) {
  await page.goto(url);
  await expectPollStarted(page, request);
}

async function expectPollStopped(page: Pg, request: Ctl, baselineHits: number) {
  expect(
    await intervalCount(page, TIMING.repinPollMs),
    'timer leak: the repin poll Interval outlived the drawer that owns it ' +
      '(on_cleanup did not drop the StoredValue)',
  ).toBe(0);

  await page.waitForTimeout(TIMING.repinPollMs + 1_000);

  expect(
    (await repinState(request)).hits,
    'late read: a status read reached the server after the drawer was gone',
  ).toBe(baselineHits);
  expect(
    await intervalCount(page, TIMING.repinPollMs),
    'timer leak: a repin poll Interval reappeared after the drawer was gone',
  ).toBe(0);

  const released = await request.post('/__ctl/repin/release');
  const body = await released.json();
  expect(
    { status: released.status(), ...body },
    'the parked status read was not still open at teardown — a 409 here means it ' +
      'was aborted or already released, and this test proved nothing about the latch',
  ).toMatchObject({ status: 200, ok: true, pending: true, aborted: false });

  await page.waitForTimeout(QUIET_MS);

  expect(
    await toastCount(page),
    'latch regression: a finished repin raised a toast for a drawer nobody is looking at',
  ).toBe(0);
  expect(
    (await repinState(request)).hits,
    'late read: answering the parked read restarted the poll',
  ).toBe(baselineHits);
}

test('closing the case file stops its repin poll', async ({ page, request }) => {
  await trackIntervals(page);
  await trackToasts(page);
  await scriptRepinStatus(request, FIELD_A);

  await openCaseFile(page, request, `/search/schema?field=${FIELD_A}`);

  await page.locator(SEL.drawerClose).click();
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);
  expect(
    await page.evaluate(() => location.search),
    'closing the case file left `field=` in the URL, so the drawer could remount',
  ).not.toContain('field=');

  await expectPollStopped(page, request, 2);
});

test('Escape back to the service context stops the repin poll', async ({ page, request }) => {
  await trackIntervals(page);
  await trackToasts(page);
  await scriptRepinStatus(request, FIELD_A);

  // With `svc=` present `on_escape` takes the BACK branch rather than the
  // close one, so this is a different teardown path than the X above.
  await openCaseFile(page, request, `/search/schema?field=${FIELD_A}&svc=nginx&stab=fields`);

  await page.keyboard.press('Escape');
  expect(await page.evaluate(() => location.search)).toBe('?svc=nginx&stab=fields');
  // The services fixture is empty, so the service drawer this returns to
  // cannot mount: nothing is left on screen for the count below to see.
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);

  await expectPollStopped(page, request, 2);
});

test('browser Back out of a case file stops its repin poll', async ({ page, request }) => {
  await trackIntervals(page);
  await trackToasts(page);
  await scriptRepinStatus(request, FIELD_A);

  await page.goto('/search/schema');
  await page.evaluate(() => {
    (window as any).__e2e_marker = true;
  });

  // Planted with pushState (which the router does not observe) and then
  // REACHED with a real Forward, so what the router sees is the popstate
  // a shared deep link delivers.
  await page.evaluate((field) => {
    history.pushState({}, '', `/search/schema?field=${field}`);
  }, FIELD_A);
  await page.goBack();
  await page.goForward();
  await expectPollStarted(page, request);

  await page.goBack();
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);

  await expectPollStopped(page, request, 2);
  expect(
    await page.evaluate(() => (window as any).__e2e_marker === true),
    'Back left the SPA — the router did a full load, so this proved nothing about unmount',
  ).toBe(true);
});

test('navigating from one case file to another stops the first one\'s poll', async ({
  page,
  request,
}) => {
  await trackIntervals(page);
  await trackToasts(page);
  await scriptRepinStatus(request, FIELD_A);

  await page.goto('/search/schema');
  await page.evaluate(() => {
    (window as any).__e2e_marker = true;
  });

  await page.evaluate(
    ([a, b]) => {
      history.pushState({}, '', `/search/schema?field=${a}`);
      history.pushState({}, '', `/search/schema?field=${b}`);
    },
    [FIELD_A, FIELD_B],
  );
  await page.goBack();
  await expectPollStarted(page, request);

  // B's mount probe is status read 3: the succeeded job, still stamped
  // with A's field, which B must not adopt.
  await page.goForward();
  await expect(page.locator(SEL.drawerTitle)).toContainText(FIELD_B);
  await expect(page.locator(SEL.drawerTitle)).not.toContainText(FIELD_A);
  await expect(page.locator(SEL.fieldCase)).toBeVisible();
  await waitForRepinState(
    request,
    (s) => s.hits === 3,
    "the second case file's own mount probe",
  );
  await expect(
    page.locator(SEL.fieldCaseJob),
    'the second case file adopted a job belonging to another field',
  ).toHaveCount(0);

  await expectPollStopped(page, request, 3);
  expect(
    await page.evaluate(() => (window as any).__e2e_marker === true),
    'Forward left the SPA — the router did a full load, so this proved nothing about unmount',
  ).toBe(true);
});
