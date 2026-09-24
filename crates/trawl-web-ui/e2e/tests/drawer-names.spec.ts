// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Each drawer's dialog is named by the bare name of the thing it shows
// (issue #242, ADR-0028). fleet-ui's Drawer used to name its dialog from
// the whole title slot, so the rename button, the run's status badge and
// the field case's Back button all leaked into the name: "Rename errors
// by host", "errors by host success", "Back to nginx duration". Every
// assertion here is `exact: true`, because a substring match passes with
// or without that bug.

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL } from '../selectors';
import type { Page } from '@playwright/test';

const RENAMED = 'errors by service';

function dialogNamed(page: Page, name: string) {
  return page.getByRole('dialog', { name, exact: true });
}

test('the net drawer is named after the net while viewing', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  await expect(page.locator(SEL.netRename)).toBeVisible();
  await expect(dialogNamed(page, CORPUS.netName)).toBeVisible();
});

test('the net drawer keeps the saved name while renaming and follows a landed rename', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // The stub does not persist a rename. The PUT and every later list
  // read share one committed name, so the list the drawer refetches
  // after the PUT says what the PUT said.
  let committed: string | null = null;
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    if (committed !== null) {
      body.queries.find((q: any) => q.id === CORPUS.netId).name = committed;
    }
    await route.fulfill({ response, json: body });
  });
  await page.route(new RegExp(`/api/v1/saved/${CORPUS.netId}$`), async route => {
    if (route.request().method() !== 'PUT') return route.continue();
    const sent = route.request().postDataJSON();
    committed = sent.name;
    await route.fulfill({
      json: {
        id: CORPUS.netId,
        name: sent.name,
        query: sent.query,
        created_at: '2026-09-01T10:00:00Z',
        updated_at: '2026-09-01T10:05:00Z',
      },
    });
  });

  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  await expect(dialogNamed(page, CORPUS.netName)).toBeVisible();

  // The controls keep their own names; only the dialog's name changed.
  await expect(page.locator(SEL.netRename)).toHaveAccessibleName(`Rename ${CORPUS.netName}`);
  await page.locator(SEL.netRename).click();
  const input = page.locator(SEL.netRenameInput);
  await expect(input).toBeVisible();
  await expect(input).toHaveAccessibleName(`New name for ${CORPUS.netName}`);
  // The edit buffer is not the name: typing a new one leaves the dialog
  // named after the saved net until the rename lands.
  await input.fill(RENAMED);
  await expect(dialogNamed(page, CORPUS.netName)).toBeVisible();

  await input.press('Enter');
  await expect(page.locator(SEL.netRename)).toHaveText(RENAMED);
  expect(committed).toBe(RENAMED);
  await expect(dialogNamed(page, RENAMED)).toBeVisible();
});

test('a net deleted under an open drawer names it Deleted net', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  let deleted = false;
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'GET') return route.continue();
    const response = await route.fetch();
    const body = await response.json();
    if (deleted) body.queries = body.queries.filter((q: any) => q.id !== CORPUS.netId);
    await route.fulfill({ response, json: body });
  });
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  await expect(dialogNamed(page, CORPUS.netName)).toBeVisible();

  deleted = true;
  const drawer = page.locator(SEL.drawerPanel);
  await expect(drawer.getByText('This net no longer exists.', { exact: false })).toBeVisible({ timeout: 8000 });
  await expect(dialogNamed(page, 'Deleted net')).toBeVisible();
});

test('the run drawer is named after the run and its net, not its status', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/runs?run=${CORPUS.runWithResult}&net=${CORPUS.netId}`);
  // The badge is in the title slot first, so the name is read with it
  // there: the badge must be beside the name, not inside it.
  await expect(page.locator(`${SEL.runDetail} ${SEL.drawerTitle} .bdg`)).toBeVisible();
  await expect(dialogNamed(page, `Run ${CORPUS.runWithResult}, ${CORPUS.netName}`)).toBeVisible();
});

test('a run whose net name is unknown is named by its id alone', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // No listed run carries this id, so the page has no net name for it,
  // while the run read itself answers for any id.
  const runId = 9999;
  await page.goto(`/jobs/runs?run=${runId}&net=${CORPUS.netId}`);
  const detail = page.locator(SEL.runDetail);
  await expect(detail.locator(`${SEL.drawerTitle} .bdg`)).toBeVisible();
  await expect(detail.locator(`${SEL.drawerTitle} .name`)).toHaveText(`Run ${runId}`);
  await expect(dialogNamed(page, `Run ${runId}`)).toBeVisible();
});

test('a field case opened from a service is named after the field alone', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?field=duration&svc=${CORPUS.service}&stab=fields`);
  await expect(page.getByRole('button', { name: `Back to ${CORPUS.service}`, exact: true })).toBeVisible();
  await expect(dialogNamed(page, 'duration')).toBeVisible();
});

test('the service drawer is named after the service', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?svc=${CORPUS.service}`);
  await expect(page.locator(`${SEL.drawerTitle} .sub`)).toBeVisible();
  await expect(dialogNamed(page, CORPUS.service)).toBeVisible();
});

test('the event inspector is named after the event', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);
  await page.locator(SEL.viewControl).click();
  await page.locator(SEL.viewPanel).getByRole('button', { name: 'Inspector', exact: true }).click();
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.viewPanel)).toHaveCount(0);

  await page.locator(SEL.resultsExpandControl).nth(1).click();
  await expect(page.locator(`${SEL.inspector} ${SEL.drawerTitle} .sub`)).toBeVisible();
  await expect(dialogNamed(page, 'Event 2')).toBeVisible();
});
