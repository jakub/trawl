// SPDX-License-Identifier: MPL-2.0
import { test, expect, resetScenario, capturedSavedRequests, CORPUS } from '../fixtures';
import type { Page } from '@playwright/test';
import { SEL, COPY } from '../selectors';

const INVALID = 'Name must not be blank or contain control or invisible formatting characters.';
const DUPLICATE = 'A net with this name already exists. Choose another name.';
const generic = (status: number) => `Could not save the net (server returned ${status}).`;
const HOSTILE = '<img src=x onerror="window.__netNamePwned=1">';

// Fixed copy keys on the pair of status and code, whatever the message
// says (ADR-0066). Any other pair, and trawld's old `bad_request` with its
// old text, is generic copy that quotes nothing the server wrote.
const REFUSALS: Array<[string, number, string, string, (status: number) => string]> = [
  ['invalid name', 400, 'invalid_name', 'name must not be blank or contain control or invisible formatting characters', () => INVALID],
  ['duplicate name', 409, 'duplicate_name', 'a saved query with this name already exists', () => DUPLICATE],
  ['invalid name, reworded', 400, 'invalid_name', 'that name will not do (reworded by a newer trawld)', () => INVALID],
  ['duplicate name, hostile message', 409, 'duplicate_name', HOSTILE, () => DUPLICATE],
  ['invalid_name at 409', 409, 'invalid_name', HOSTILE, generic],
  ['duplicate_name at 400', 400, 'duplicate_name', 'a saved query with this name already exists', generic],
  ['old bad_request blank-name text', 400, 'bad_request', 'name must not be blank or contain control or invisible formatting characters', generic],
  ['old bad_request duplicate text', 409, 'bad_request', 'a saved query with this name already exists', generic],
];

/** The n-th error toast says `copy`. Error toasts persist, so each
 * refusal adds one at the end of the stack. */
async function expectRefusal(page: Page, title: string, copy: string, label: string) {
  const toasts = page.locator(SEL.toastAny);
  const index = REFUSALS.findIndex(([name]) => name === label);
  await expect(toasts, label).toHaveCount(index + 1);
  await expect(toasts.nth(index).locator('.title'), label).toHaveText(title);
  await expect(toasts.nth(index).locator('.detail'), label).toHaveText(copy);
}

/** The hostile message was never rendered as markup or as text. */
async function expectNothingHostile(page: Page) {
  await expect(page.locator('img[src="x"]')).toHaveCount(0);
  expect(await page.evaluate(() => (window as unknown as { __netNamePwned?: number }).__netNamePwned)).toBeUndefined();
  await expect(page.locator('body')).not.toContainText('onerror');
  await expect(page.locator('body')).not.toContainText('saved query with this name');
  await expect(page.locator('body')).not.toContainText('reworded by a newer trawld');
}

test('readable net names preserve punctuation and key refusal copy on status and code', async ({ page, request }) => {
  await resetScenario(request, 'saved-success');
  await page.goto('/search');
  await page.locator(SEL.editorTool).filter({ hasText: COPY.saveAsNetTool }).click();
  const dialog = page.getByRole('dialog', { name: 'Save query as Net', exact: true });
  const input = dialog.getByLabel('Name', { exact: true });
  const save = dialog.getByRole('button', { name: 'Save as Net', exact: true });
  await input.fill('   ');
  await expect(input).toHaveAttribute('aria-invalid', 'true');
  await expect(dialog.locator('#netNameError')).toHaveText('Enter a name.');
  await expect(save).toBeDisabled();
  expect(await capturedSavedRequests(request)).toHaveLength(0);
  await input.fill('name\tbad');
  await expect(save).toBeDisabled();
  await expect(dialog.locator('#netNameError')).toHaveText('Name must not contain control or invisible formatting characters.');
  for (const unsafe of ['\u200b', 'name\u202e', '👩\u200d💻']) {
    await input.fill(unsafe);
    await expect(save).toBeDisabled();
    await expect(input).toHaveAttribute('aria-invalid', 'true');
    await expect(dialog.locator('#netNameError')).toHaveText('Name must not contain control or invisible formatting characters.');
  }
  const name = '雪  / "Audit" \\ reports';
  await input.fill(`  ${name}  `);
  await expect(save).toBeEnabled();
  let refusals = 0;
  const replies = [...REFUSALS];
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    expect(route.request().postDataJSON().name).toBe(name);
    refusals++;
    const [, status, code, message] = replies.shift()!;
    await route.fulfill({ status, json: { error: { code, message } } });
  });
  for (const [label, status, , , copy] of REFUSALS) {
    await save.click();
    await expectRefusal(page, "Couldn't save", copy(status), label);
    // The draft survives every refusal, and the dialog stays open on it.
    await expect(dialog).toBeVisible();
    await expect(input).toHaveValue(`  ${name}  `);
  }
  expect(refusals).toBe(REFUSALS.length);
  await expectNothingHostile(page);
  await page.unroute('**/api/v1/saved');
  await save.click();
  await expect(dialog).toHaveCount(0);
  const calls = await capturedSavedRequests(request);
  expect(calls).toHaveLength(1);
  expect(calls[0].name).toBe(name);
});

test('readable net rename validates blanks, keys refusal copy on status and code, and preserves the draft', async ({ page, request }) => {
  await resetScenario(request, 'corpus');
  await page.goto(`/jobs/nets?net=${CORPUS.netId}&ntab=query`);
  await page.locator(SEL.netRename).click();
  const input = page.locator(SEL.netRenameInput);
  await input.fill('  ');
  await input.press('Enter');
  await expect(input).toBeVisible();
  await expect(input).toHaveAttribute('aria-invalid', 'true');
  await expect(page.locator('#netRenameError')).toHaveText('Enter a name.');
  const name = '雪  / "Rename" \\ reports';
  let writes = 0;
  const replies = [...REFUSALS];
  await page.route(`**/api/v1/saved/${CORPUS.netId}`, async route => {
    if (route.request().method() !== 'PUT') return route.continue();
    writes++;
    expect(route.request().postDataJSON().name).toBe(name);
    const [, status, code, message] = replies.shift()!;
    await route.fulfill({ status, json: { error: { code, message } } });
  });
  await input.fill(`  ${name}  `);
  for (const [label, status, , , copy] of REFUSALS) {
    await input.press('Enter');
    await expectRefusal(page, 'Rename failed', copy(status), label);
    await expect(input).toBeVisible();
    await expect(input).toHaveValue(`  ${name}  `);
  }
  expect(writes).toBe(REFUSALS.length);
  await expectNothingHostile(page);
});
