// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// One stretched control per list row (issue #161, ADR-0029).
//
// Five tables used to act on a whole-row `on:click`: schema services,
// nets, runs, history and the results table. A keyboard could not reach
// any of them, and every nested control had to `stop_propagation` its
// way out of the row. Each row now carries exactly ONE control, a link
// where the row is a place and a button where it is a command, and that
// control's `::after` covers the row so the pointer still activates from
// anywhere on it.
//
// The proofs are deliberately three-sided, because two of them can pass
// for the wrong reason:
//
//   1. The ELEMENT: tag, `type="button"` or `href`, and an accessible
//      name that is words rather than a glyph.
//   2. The KEYBOARD: the control is reached by pressing Tab from a
//      known predecessor (never `.focus()` on the control itself, which
//      would prove the tab order nothing), shows the fleet-ui ring
//      (`:focus-visible` plus a non-`none` box-shadow), and Enter and
//      Space each act exactly ONCE. On a link, Space must do nothing —
//      that is the platform behaviour a div never had.
//   3. The COUNT: a click listener on the row itself, so "activates
//      once" is a number rather than an end state. A toggle that fired
//      twice looks identical to one that never fired.

import { test, expect, resetScenario, CORPUS, capturedQueryCount } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

type Pg = import('@playwright/test').Page;

/** Count clicks that reach `selector`, so an activation is a number. */
async function countClicksOn(page: Pg, selector: string): Promise<void> {
  await page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (!el) throw new Error(`${sel} is not mounted`);
    (window as any).__e2eRowClicks = 0;
    el.addEventListener('click', () => {
      (window as any).__e2eRowClicks += 1;
    });
  }, selector);
}

function rowClicks(page: Pg): Promise<number> {
  return page.evaluate(() => (window as any).__e2eRowClicks as number);
}

test('schema row: pointer on the query cell opens the drawer once', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const row = page.locator(SEL.tableRow).first();
  const link = row.locator(SEL.rowStretch);
  await expect(link).toHaveJSProperty('tagName', 'A');
  await expect(link).toHaveAccessibleName(CORPUS.service);
  // The drawer is a place, and this one replaces its history entry.
  await expect(link).toHaveJSProperty('replace', true);

  await countClicksOn(page, `${SEL.tableRow}`);

  // The LAST data cell, as far from the control as the row goes. The
  // stretched pseudo-element is what the pointer actually lands on.
  const cell = row.locator('.num').last();
  const point = await cell.evaluate((el) => {
    const box = el.getBoundingClientRect();
    const at = { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    const under = document.elementFromPoint(at.x, at.y);
    return { ...at, hit: under?.closest('a')?.className ?? null };
  });
  expect(point.hit, 'the pointer over a data cell must land on the row control').toContain(
    'row-stretch',
  );

  // `page.mouse`, not `cell.click()`: Playwright refuses to click an
  // element another element covers, and being covered by the row's own
  // control is the whole design. This is the press a user makes.
  await page.mouse.click(point.x, point.y);

  await expect(page).toHaveURL(new RegExp(`svc=${CORPUS.service}`));
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(1);
  expect(await rowClicks(page)).toBe(1);
});

test('nets row: ActionsMenu opens without the drawer', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/jobs/nets');

  const trigger = page.locator(SEL.actionsMenuTrigger);
  await expect(trigger).toHaveCount(1);
  await countClicksOn(page, SEL.tableRow);

  await trigger.click();

  await expect(page.locator(SEL.actionsMenuItem).first()).toBeVisible();
  // The trigger sits above the stretched anchor, so the press never
  // reached it: no drawer, no `?net=` and no navigation at all.
  await expect(page).toHaveURL(/\/jobs\/nets$/);
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);
  // fleet-ui's trigger stops the press at itself (ADR-0028), so the row
  // never sees it. That zero only means something beside a press the row
  // DOES see, which is the next three lines.
  expect(await rowClicks(page)).toBe(0);

  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.actionsMenuItem)).toHaveCount(0);
  const cell = page.locator(SEL.tableRow).first().locator('.mono').last();
  const point = await cell.evaluate((el) => {
    const box = el.getBoundingClientRect();
    return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
  });
  await page.mouse.click(point.x, point.y);
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(1);
  expect(await rowClicks(page)).toBe(1);
});

test('history row: Save as Net opens the modal without a rerun', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/history');

  const row = page.locator(SEL.tableRow).first();
  await expect(row.locator(SEL.rowStretch)).toContainText(CORPUS.history.query);
  const save = row.locator(SEL.historySaveAsNet);
  await expect(save).toHaveJSProperty('tagName', 'BUTTON');
  await expect(save).toHaveAttribute('type', 'button');

  const before = await capturedQueryCount(request);
  await save.click();

  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  // A rerun would leave this page for /search, which would post a
  // query. Neither happened.
  await expect(page).toHaveURL(/\/search\/history$/);
  expect(await capturedQueryCount(request)).toBe(before);
});

test('results row: Space expands once and leaves the detail open', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);

  const carets = page.locator(SEL.resultsExpandControl);
  await expect(carets).toHaveCount(CORPUS.rowCount);
  const caret = carets.first();
  await expect(caret).toHaveJSProperty('tagName', 'BUTTON');
  await expect(caret).toHaveAttribute('type', 'button');
  await expect(caret).toHaveAccessibleName(nameFrom(COPY.resultsExpandName, '1'));
  await expect(caret).toHaveAttribute('aria-expanded', 'false');

  // Reached by Tab from the last column header's sort control, which is
  // the focusable immediately before the first row.
  await page.locator(SEL.resultsSortControl).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(caret);

  await countClicksOn(page, `${SEL.resultsRow}:nth-of-type(1)`);

  // Enter opens exactly one detail row, Enter again closes it: one
  // activation per press, not two.
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(1);
  expect(await rowClicks(page)).toBe(1);
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(0);
  expect(await rowClicks(page)).toBe(2);

  // And Space does the same thing once, leaving the detail up.
  await page.keyboard.press(' ');
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(1);
  await expect(caret).toHaveAttribute('aria-expanded', 'true');
  expect(await rowClicks(page)).toBe(3);
});

test('service field row: degraded badge opens the case without expanding', async ({
  page,
  request,
}) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?svc=${CORPUS.service}&stab=fields`);

  const row = page
    .locator(SEL.serviceFieldRow)
    .filter({ has: page.locator('.c-name', { hasText: CORPUS.degradedField }) });
  await expect(row).toHaveCount(1);
  const toggle = row.locator(SEL.rowStretch);
  const badge = row.locator(SEL.serviceDegradedBadge);
  await expect(badge).toHaveJSProperty('tagName', 'BUTTON');
  await expect(badge).toHaveAttribute('type', 'button');

  // Counting on the ROW's own control rather than on the row: the case
  // file swaps this pane out, so the end state cannot answer "did the
  // row expand too?" — the number can.
  await page.evaluate(
    ({ rowSel, field }) => {
      const toggleEl = [...document.querySelectorAll(rowSel)]
        .find((el) => el.querySelector('.c-name')?.textContent === field)
        ?.querySelector('.row-stretch');
      if (!toggleEl) throw new Error(`no row control for ${field}`);
      (window as any).__e2eRowClicks = 0;
      toggleEl.addEventListener('click', () => {
        (window as any).__e2eRowClicks += 1;
      });
    },
    { rowSel: SEL.serviceFieldRow, field: CORPUS.degradedField },
  );

  // Tab from the row's own control, which precedes the badge.
  await toggle.focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(badge);
  await page.keyboard.press('Enter');

  await expect(page.locator(SEL.fieldCase)).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`field=${CORPUS.degradedField}`));
  expect(await rowClicks(page)).toBe(0);
});

test('schema link: Space does not navigate', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const link = page.locator(SEL.tableRow).first().locator(SEL.rowStretch);
  // The last header control is the focusable immediately before the
  // first row's link.
  await page.locator(SEL.tableSortControl).last().focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(link);

  await page.keyboard.press(' ');
  await expect(page).toHaveURL(/\/search\/schema$/);
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);

  // Enter is the half that does navigate, so the Space proof is about
  // Space and not about a dead control.
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(new RegExp(`svc=${CORPUS.service}`));
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(1);
});

test('nets link: modified click leaves the page in place', async ({ page, context, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/jobs/nets');

  const link = page.locator(SEL.tableRow).first().locator(SEL.rowStretch);
  await expect(link).toHaveJSProperty('tagName', 'A');
  await expect(link).toHaveAccessibleName(CORPUS.netName);
  await expect(link).toHaveJSProperty('replace', true);

  // A Ctrl-click is the browser's, not the router's: whatever the new
  // tab does, THIS page stays where it was.
  const opened: import('@playwright/test').Page[] = [];
  context.on('page', (p) => opened.push(p));
  await page.click(`${SEL.tableRow} ${SEL.rowStretch}`, { modifiers: ['Control'] });

  await expect(page).toHaveURL(/\/jobs\/nets$/);
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(0);
  for (const p of opened) await p.close();
});

test('schema drawer: Back skips the replace', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // Two real entries, so Back has somewhere to go that is not the
  // drawer's own URL.
  await page.goto('/search/history');
  await page.goto('/search/schema');

  await page.locator(SEL.tableRow).first().locator(SEL.rowStretch).click();
  await expect(page).toHaveURL(new RegExp(`svc=${CORPUS.service}`));

  // `prop:replace` overwrote the schema entry, so one Back lands on the
  // page before it rather than on the drawer-less schema page.
  await page.goBack();
  await expect(page).toHaveURL(/\/search\/history$/);
});

test('runs row selects in place; the receipt opens the net and Back returns to runs', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/jobs/runs');

  const link = page.locator(SEL.tableRow).first().locator(SEL.rowStretch);
  await expect(link).toHaveJSProperty('tagName', 'A');
  await expect(link).toHaveAccessibleName(CORPUS.netName);
  // Selecting a run REPLACES: reading down a list is one history entry,
  // not one per row glanced at.
  await expect(link).toHaveJSProperty('replace', true);

  await link.click();
  await expect(page).toHaveURL(/run=\d+&net=\d+/);

  // The receipt's Open net is the one control here that PUSHES.
  await page.locator(SEL.runOpenNet).click();
  await expect(page).toHaveURL(new RegExp(`net=${CORPUS.netId}&ntab=runs`));

  await page.goBack();
  await expect(page).toHaveURL(/\/jobs\/runs\?run=/);
});

test('history row: over-bound query is refused, not navigated', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/history');

  // The second entry is one byte over the link bound (`fixtures.ts`
  // pins its length), so the navigator refuses it rather than rerunning
  // it into an unreadable URL.
  const rerun = page.locator(SEL.tableRow).nth(1).locator(SEL.rowStretch);
  await expect(rerun).toHaveJSProperty('tagName', 'BUTTON');
  await expect(rerun).toContainText(CORPUS.history.overBoundPrefix);

  const before = await capturedQueryCount(request);
  await rerun.click();

  await expect(page.locator(SEL.toastError)).toContainText(COPY.linkTooLongToast);
  await expect(page).toHaveURL(/\/search\/history$/);
  expect(await capturedQueryCount(request)).toBe(before);
});

test('schema quick actions: Tab reveals and Enter searches', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const row = page.locator(SEL.tableRow).first();
  const search = row.locator(SEL.schemaQuickAction).first();
  await expect(search).toHaveJSProperty('tagName', 'BUTTON');
  await expect(search).toHaveAttribute('type', 'button');
  await expect(search).toHaveAccessibleName(nameFrom(COPY.schemaSearchName, CORPUS.service));

  // Hidden at rest, revealed by the row's own :focus-within — which is
  // what makes Tab able to reach it at all.
  await expect(row.locator('.row-act')).toHaveCSS('opacity', '0');
  await row.locator(SEL.rowStretch).focus();
  await page.keyboard.press('Tab');
  await expectFocusRing(search);
  // Retried, not read once: the reveal is a 100ms opacity transition, so
  // an immediate read catches it part way there.
  await expect(row.locator('.row-act')).toHaveCSS('opacity', '1');

  await page.keyboard.press('Enter');
  // A command, not a place: it goes through the navigator, which builds
  // the service's own search.
  await expect(page).toHaveURL(/\/search\?q=service%3D/);
});

test('net run row: Enter expands once and Space collapses', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // The run's result read, counted by URL. A row that toggled on both
  // the button AND an ancestor would open and close in one press and
  // leave no end state to assert; the number sees it.
  const resultPath = `/api/v1/saved/${CORPUS.netId}/runs/${CORPUS.runWithResult}`;
  const resultReads: string[] = [];
  page.on('request', (r) => {
    if (new URL(r.url()).pathname === resultPath) resultReads.push(r.url());
  });
  await page.goto(`/jobs/nets?net=${CORPUS.netId}&ntab=runs`);

  const row = page.locator(SEL.netRunRow).first();
  const control = row.locator(SEL.rowStretch);
  await expect(control).toHaveJSProperty('tagName', 'BUTTON');
  await expect(control).toHaveAttribute('type', 'button');
  await expect(control).toHaveAttribute('aria-expanded', 'false');

  // The local scroll region follows Runs, then the first row control.
  await page.locator(SEL.drawerTab).nth(1).focus();
  await page.keyboard.press('Tab');
  await expect(page.getByRole('region', { name: 'Net runs', exact: true })).toBeFocused();
  await page.keyboard.press('Tab');
  await expectFocusRing(control);

  await countClicksOn(page, SEL.netRunRow);

  await page.keyboard.press('Enter');
  await expect(control).toHaveAttribute('aria-expanded', 'true');
  await expect(page.locator(SEL.netRunPreview)).toHaveCount(1);
  expect(await rowClicks(page)).toBe(1);
  await expect.poll(() => resultReads.length).toBe(1);

  // Space is the other half of what a button answers to, and it
  // collapses rather than opening a second copy.
  await page.keyboard.press(' ');
  await expect(control).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator(SEL.netRunPreview)).toHaveCount(0);
  expect(await rowClicks(page)).toBe(2);
  expect(resultReads).toHaveLength(1);

  // And the pointer, landing on the far cell rather than the control:
  // the stretched `::after` is what it hits, so the row toggles once.
  const cell = row.locator('.mono').last();
  const point = await cell.evaluate((el) => {
    const box = el.getBoundingClientRect();
    const at = { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    const under = document.elementFromPoint(at.x, at.y);
    return { ...at, hit: under?.closest('button')?.className ?? null };
  });
  expect(point.hit, 'the pointer over a run cell must land on the row control').toContain(
    'row-stretch',
  );
  await page.mouse.click(point.x, point.y);
  await expect(control).toHaveAttribute('aria-expanded', 'true');
  expect(await rowClicks(page)).toBe(3);
});

test('schema quick actions: Live Tail opens the tail tab', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto('/search/schema');

  const row = page.locator(SEL.tableRow).first();
  const tail = row.locator(SEL.schemaQuickAction).nth(1);
  await expect(tail).toHaveJSProperty('tagName', 'BUTTON');
  await expect(tail).toHaveAttribute('type', 'button');
  // The glyph is an icon, so the name is the whole sentence — and it
  // names the service, since every row carries the same pair.
  await expect(tail).toHaveAccessibleName(nameFrom(COPY.schemaTailName, CORPUS.service));

  // Two stops past the row's own control: Search first, then this one.
  await row.locator(SEL.rowStretch).focus();
  await page.keyboard.press('Tab');
  await page.keyboard.press('Tab');
  await expectFocusRing(tail);

  await page.keyboard.press('Enter');
  // A command through the navigator, and what it builds is the drawer
  // opened on the tail tab rather than on the default one.
  await expect(page).toHaveURL(new RegExp(`svc=${CORPUS.service}&stab=tail`));
  await expect(page.locator(SEL.drawerPanel)).toHaveCount(1);
  await expect(page.locator(SEL.drawerTab).nth(2)).toHaveAttribute('aria-selected', 'true');
});
