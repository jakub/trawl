// SPDX-License-Identifier: MPL-2.0
import { test, expect, resetScenario, capturedSavedRequests, CORPUS } from '../fixtures';
import { SEL, COPY } from '../selectors';

test('readable net names preserve punctuation and show bounded refusals', async ({ page, request }) => {
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
  await page.route('**/api/v1/saved', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    expect(route.request().postDataJSON().name).toBe(name);
    refusals++;
    await route.fulfill({ status: refusals === 1 ? 400 : 409, json: { error: { code: 'bad_request', message: refusals === 1
      ? 'name must not be blank or contain control or invisible formatting characters'
      : 'a saved query with this name already exists' } } });
  });
  await save.click();
  await expect(page.getByText('Name must not be blank or contain control or invisible formatting characters.', { exact: true })).toBeVisible();
  await expect(input).toHaveValue(`  ${name}  `);
  await save.click();
  await expect(page.getByText('A net with this name already exists. Choose another name.', { exact: true })).toBeVisible();
  await expect(input).toHaveValue(`  ${name}  `);
  await page.unroute('**/api/v1/saved');
  await save.click();
  await expect(dialog).toHaveCount(0);
  const calls = await capturedSavedRequests(request);
  expect(calls).toHaveLength(1);
  expect(calls[0].name).toBe(name);
});

test('readable net rename validates blanks and preserves the draft on refusal', async ({ page, request }) => {
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
  await page.route(`**/api/v1/saved/${CORPUS.netId}`, async route => {
    if (route.request().method() !== 'PUT') return route.continue();
    writes++;
    expect(route.request().postDataJSON().name).toBe(name);
    await route.fulfill({ status: 409, json: { error: { code: 'bad_request', message: 'a saved query with this name already exists' } } });
  });
  await input.fill(`  ${name}  `);
  await input.press('Enter');
  await expect(page.getByText('A net with this name already exists. Choose another name.', { exact: true })).toBeVisible();
  await expect(input).toHaveValue(`  ${name}  `);
  expect(writes).toBe(1);
});
