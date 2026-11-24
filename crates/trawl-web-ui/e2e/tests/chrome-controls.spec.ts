// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The single controls scattered around the chrome (ADR-0029).

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

type Loc = import('@playwright/test').Locator;
type Pg = import('@playwright/test').Page;

const FIELD = CORPUS.columns[1];
const VALUE = CORPUS.hosts[0];

async function tabUntilFocused(page: Pg, target: Loc, max: number): Promise<number> {
  for (let i = 1; i <= max; i += 1) {
    await page.keyboard.press('Tab');
    if (await target.evaluate((el) => el === document.activeElement)) return i;
  }
  throw new Error(`the target was not in the first ${max} tab stops`);
}

test('chip remove is named by its filter', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);

  await page
    .locator(SEL.facetGroup)
    .filter({
      has: page.getByRole('button', {
        name: nameFrom(COPY.facetGroupName, FIELD, String(CORPUS.hosts.length)),
      }),
    })
    .getByRole('button', { name: nameFrom(COPY.facetIncludeName, FIELD, VALUE) })
    .click();
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);

  const remove = page.locator(SEL.chipRemove);
  await expect(remove).toHaveJSProperty('tagName', 'BUTTON');
  await expect(remove).toHaveAttribute('type', 'button');
  await expect(remove).toHaveAccessibleName(nameFrom(COPY.chipRemoveName, FIELD, VALUE));

  // The editor's last tool is the focusable before the chip strip.
  await page.locator(SEL.editorTool).last().focus();
  await tabUntilFocused(page, remove, 3);
  await expectFocusRing(remove);

  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});

test('theme choices are named native radios and the footer control is absent', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page.locator(SEL.healthPage)).toBeVisible();

  await expect(page.locator('.statusbar button')).toHaveCount(0);
  await expect(page.locator(SEL.statusLabel)).toBeVisible();
  await page.locator(SEL.topbarUser).focus();
  await page.keyboard.press('Enter');
  const group = page.getByRole('group', { name: 'Theme', exact: true });
  await expect(group.getByRole('menuitemradio')).toHaveCount(3);
  for (const name of ['Light', 'Dark', 'System']) {
    const radio = group.getByRole('menuitemradio', { name, exact: true });
    await expect(radio).toHaveJSProperty('tagName', 'BUTTON');
    await expect(radio).toHaveAttribute('type', 'button');
    await expect(radio).toHaveAccessibleName(name);
    await expect(radio).toHaveAttribute('aria-checked', String(name === 'System'));
  }
  const selected = group.getByRole('menuitemradio', { name: 'System', exact: true });
  await expect(selected).toBeFocused();
  await expectFocusRing(selected);
  await expect(selected.locator('.menu-choice-mark')).toHaveText('✓');
  await page.keyboard.press('Home');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('Space');
  await expect(page.locator(SEL.userMenu)).toHaveCount(0);
  await expect(page.locator(SEL.topbarUser)).toBeFocused();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
});

test('interval presets expose aria-pressed', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  await expect(page.locator(SEL.drawerPanel)).toBeVisible();

  await page.getByRole('button', { name: COPY.addScheduleButton }).click();
  const chips = page.locator(SEL.intervalChip);
  await expect(chips.first()).toHaveAttribute('type', 'button');

  const pressed = () => chips.evaluateAll((els) => els.map((el) => el.getAttribute('aria-pressed')));
  const visual = () => chips.evaluateAll((els) => els.map((el) => el.classList.contains('on')));
  const before = await pressed();
  expect(before.filter((v) => v === 'true')).toHaveLength(1);
  expect(await visual()).toEqual(before.map((v) => v === 'true'));

  const chosen = before.indexOf('true');
  const next = chosen === 0 ? 1 : 0;
  const target = chips.nth(next);
  if (next === 0) {
    await chips.nth(1).focus();
    await page.keyboard.press('Shift+Tab');
  } else {
    await chips.nth(next - 1).focus();
    await page.keyboard.press('Tab');
  }
  await expectFocusRing(target);
  await page.keyboard.press('Enter');

  const after = await pressed();
  expect(after.filter((v) => v === 'true')).toHaveLength(1);
  expect(after.indexOf('true')).toBe(next);
  expect(await visual()).toEqual(after.map((v) => v === 'true'));
});

test('quick range exposes aria-pressed', async ({ page }) => {
  await page.goto('/search');
  const trigger = page.locator(SEL.dateRangeTrigger);
  const label = await trigger.innerText();

  await trigger.click();
  const options = page.locator(SEL.quickRangeOption);
  await expect(options.first()).toHaveAttribute('type', 'button');
  const pressed = () =>
    options.evaluateAll((els) => els.map((el) => el.getAttribute('aria-pressed')));
  const before = await pressed();
  expect(before.filter((v) => v === 'true')).toHaveLength(1);
  await expect(options.nth(before.indexOf('true'))).toHaveText(label);

  const next = before.indexOf('true') === 0 ? 1 : 0;
  const nextLabel = await options.nth(next).innerText();
  await options.nth(next).click();
  await expect(trigger).toContainText(nextLabel);

  await trigger.click();
  const after = await pressed();
  expect(after.filter((v) => v === 'true')).toHaveLength(1);
  expect(after.indexOf('true')).toBe(next);
});

test('top field uses the field in Search', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search/schema?svc=${CORPUS.service}&stab=overview`);

  const field = page.locator(SEL.serviceTopField).first();
  await expect(field).toHaveJSProperty('tagName', 'BUTTON');
  await expect(field).toHaveAttribute('type', 'button');
  await expect(field).toHaveAccessibleName(CORPUS.topCardinalityField);

  await page.locator(SEL.drawerTab).last().focus();
  await tabUntilFocused(page, field, 6);
  await expectFocusRing(field);

  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(
    new RegExp(`/search\\?q=${CORPUS.topCardinalityField}%3D\\*`),
  );
});

test('detail tag adds a filter', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/search?q=service%3D${CORPUS.service}&page=0`);

  const caret = page.locator(SEL.resultsExpandControl).first();
  await caret.click();
  await expect(page.locator(SEL.resultsDetailCell)).toHaveCount(1);

  const value = CORPUS.firstHost;
  // Scoped to the detail cell: the facet rail carries a button with the
  // very same name, and the point here is the tag inside the row.
  const tag = page
    .locator(SEL.resultsDetailCell)
    .getByRole('button', { name: nameFrom(COPY.resultsTagName, FIELD, value) });
  await expect(tag).toHaveJSProperty('tagName', 'BUTTON');
  await expect(tag).toHaveAttribute('type', 'button');

  // The detail row follows its own row in the DOM, so the caret is the
  // stop before its tags.
  await caret.focus();
  await tabUntilFocused(page, tag, 3);
  await expectFocusRing(tag);

  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.filterChip)).toHaveCount(1);
  await expect(page.locator(SEL.chipRemove)).toHaveAccessibleName(
    nameFrom(COPY.chipRemoveName, FIELD, value),
  );
});

test('net rename opens the editor', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  // Below 1100px the net panel is still the overlay drawer that
  // captures focus.
  await page.setViewportSize({ width: 1024, height: 900 });
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  const panel = page.locator(SEL.drawerPanel);
  await expect(panel).toBeVisible();

  const rename = page.locator(SEL.netRename);
  await expect(rename).toHaveJSProperty('tagName', 'BUTTON');
  await expect(rename).toHaveAttribute('type', 'button');
  await expect(rename).toHaveText(CORPUS.netName);
  await expect(rename).toHaveAccessibleName(nameFrom(COPY.netRenameName, CORPUS.netName));

  // The drawer captures focus when it opens (fleet-ui
  // FocusPolicy::Capture) and this is the first control inside it, so
  // the capture lands here. That alone would also be true of a control
  // outside the tab order, so step out backwards and come back: the
  // Tab that returns is the proof.
  await expect(rename).toBeFocused();
  await page.keyboard.press('Shift+Tab');
  await expect(rename).not.toBeFocused();
  await page.keyboard.press('Tab');
  await expectFocusRing(rename);

  await page.keyboard.press('Enter');
  const input = page.locator(SEL.netRenameInput);
  await expect(input).toHaveValue(CORPUS.netName);
  await expect(input).toHaveAccessibleName(
    nameFrom(COPY.netRenameInputName, CORPUS.netName),
  );
  await expect(input).toBeFocused();
  await expect(rename).toHaveCount(0);

  await page.keyboard.press('Escape');
  await expect(input).toHaveCount(0);
  await expect(page.locator(SEL.netRename)).toBeFocused();
  await expect(page.locator(SEL.netRename)).toHaveText(CORPUS.netName);
  await expect(panel).toBeVisible();
});
