// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The single controls scattered around the chrome (issue #161,
// ADR-0029): a filter chip's remove, the theme switch, the schedule
// interval presets, the range picker's quick ranges, the service
// drawer's top-field control and a result detail's tag.
//
// Each was a span or a div with a click handler and, in four cases, no
// text at all worth announcing. Two of them are toggles in a set, and a
// set is where a missing name hurts twice: the `.on` class says nothing
// to a screen reader, so `aria-pressed` has to say which one is chosen
// and the assertion is the whole vector rather than the chosen one.

import { test, expect, resetScenario, CORPUS } from '../fixtures';
import { SEL, COPY, nameFrom } from '../selectors';
import { expectFocusRing } from '../a11y';

type Loc = import('@playwright/test').Locator;
type Pg = import('@playwright/test').Page;

const FIELD = CORPUS.columns[1];
const VALUE = CORPUS.hosts[0];

/** Walk the tab order from wherever focus is until `target` has it.
 *
 * For a control with no single stable neighbour to press Tab from, which
 * is the status bar's case: it sits at the end of the document behind a
 * page's worth of controls. Bounded, and it fails naming the number of
 * presses rather than hanging, so "unreachable" and "further away than
 * expected" read differently. */
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

  // Make a filter the honest way rather than hand-writing its encoded
  // payload into the URL: the facet control is proven in facets.spec.ts.
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
  // The glyph is aria-hidden, so the name is the whole sentence and
  // says which filter goes.
  await expect(remove).toHaveAccessibleName(nameFrom(COPY.chipRemoveName, FIELD, VALUE));

  // The editor's last tool is the focusable before the chip strip.
  await page.locator(SEL.editorTool).last().focus();
  await tabUntilFocused(page, remove, 3);
  await expectFocusRing(remove);

  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.filterChip)).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]f=/);
});

test('theme control names its result', async ({ page, request }) => {
  await resetScenario(request, 'health-viewer');
  await page.goto('/settings');
  await expect(page.locator(SEL.healthPage)).toBeVisible();

  const theme = page.locator(SEL.themeControl);
  await expect(theme).toHaveJSProperty('tagName', 'BUTTON');
  await expect(theme).toHaveAttribute('type', 'button');
  // No `title`: it duplicated the name and said less.
  expect(await theme.evaluate((el) => el.hasAttribute('title'))).toBe(false);

  const before = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  const other = before === 'dark' ? 'light' : 'dark';
  // The visible text is the theme in force; the name opens with that
  // same word and then says the theme a press produces. Both halves are
  // asserted: a name that dropped the visible word could not be spoken
  // by someone reading the button (WCAG 2.5.3).
  await expect(theme).toHaveText(before ?? '');
  await expect(theme).toHaveAccessibleName(
    nameFrom(COPY.themeSwitchName, before ?? '', other),
  );

  await page.locator(SEL.topbarUser).focus();
  await tabUntilFocused(page, theme, 20);
  await expectFocusRing(theme);

  await page.keyboard.press('Enter');
  await expect
    .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-theme')))
    .toBe(other);
  // And the name follows the theme rather than freezing at load: both
  // halves swap.
  await expect(theme).toHaveText(other);
  await expect(theme).toHaveAccessibleName(
    nameFrom(COPY.themeSwitchName, other, before ?? ''),
  );
});

test('interval presets expose aria-pressed', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  await expect(page.locator(SEL.drawerPanel)).toBeVisible();

  // The fixture's net has no schedule, so the form is behind its own
  // control.
  await page.getByRole('button', { name: COPY.addScheduleButton }).click();
  const chips = page.locator(SEL.intervalChip);
  await expect(chips.first()).toHaveAttribute('type', 'button');

  // The chosen preset is a pressed state, and the assertion is the
  // whole vector: one true, everything else false.
  const pressed = () => chips.evaluateAll((els) => els.map((el) => el.getAttribute('aria-pressed')));
  const visual = () => chips.evaluateAll((els) => els.map((el) => el.classList.contains('on')));
  const before = await pressed();
  expect(before.filter((v) => v === 'true')).toHaveLength(1);
  // The class is what the eye reads and aria-pressed is what a screen
  // reader reads; they have to agree or one of them is lying.
  expect(await visual()).toEqual(before.map((v) => v === 'true'));

  // Any preset that is not the chosen one, reached from its neighbour:
  // the fixture's net has no schedule, so the form opens on the default
  // and the index of that default is not this test's business.
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
  // The pressed option is the range the trigger is showing, so the two
  // readings of "which range is this" agree.
  await expect(options.nth(before.indexOf('true'))).toHaveText(label);

  // Pick a different one and the pressed state moves with the trigger.
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

  // A command rather than a link, because the navigator can refuse the
  // search it builds, and an anchor has no way to say no.
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
  await page.goto(`/jobs/nets?net=${CORPUS.netId}`);
  const panel = page.locator(SEL.drawerPanel);
  await expect(panel).toBeVisible();

  const rename = page.locator(SEL.netRename);
  await expect(rename).toHaveJSProperty('tagName', 'BUTTON');
  await expect(rename).toHaveAttribute('type', 'button');
  // The visible text is the net's name and the name adds the verb, so
  // the label is inside the name (WCAG 2.5.3).
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
  // The heading the input replaced was the only thing naming it, so the
  // input has to carry the name itself.
  await expect(input).toHaveAccessibleName(
    nameFrom(COPY.netRenameInputName, CORPUS.netName),
  );
  // The trigger removed ITSELF to make room for the input, so the press
  // has to hand focus over: the browser drops it on <body> otherwise
  // and a keyboard user is left outside the editor they just opened.
  await expect(input).toBeFocused();
  await expect(rename).toHaveCount(0);

  // Escape cancels the rename rather than closing the drawer, and
  // focus comes back to the control that opened the editor.
  await page.keyboard.press('Escape');
  await expect(input).toHaveCount(0);
  await expect(page.locator(SEL.netRename)).toBeFocused();
  await expect(page.locator(SEL.netRename)).toHaveText(CORPUS.netName);
  await expect(panel).toBeVisible();
});
