// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect, resetScenario } from '../fixtures';
import { SEL } from '../selectors';

type Page = import('@playwright/test').Page;
type Command = { label: string; path: string };

async function open(page: Page, chord?: string) {
  await expect(page.locator(SEL.paletteTrigger)).toBeVisible();
  await expect(page.locator(SEL.paletteTrigger)).toBeEnabled();
  if (chord) await page.keyboard.press(chord);
  else await page.locator(SEL.paletteTrigger).click();
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
  await expect(page.locator(SEL.paletteInput)).toBeFocused();
  await expect(page.locator(SEL.paletteTrigger)).toHaveAttribute('aria-expanded', 'true');
}

async function closed(page: Page) {
  await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);
  await expect(page.locator(SEL.paletteTrigger)).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator(SEL.paletteTrigger)).toBeFocused();
}

async function chromeCommands(page: Page): Promise<Command[]> {
  // The sidebar is the whole route inventory now: the palette's options
  // are its links, in its order, deduped by path.
  await expect(page.locator(SEL.paletteRailLink).first()).toBeVisible();
  const rail = await page.locator(SEL.paletteRailLink).evaluateAll((links) =>
    links.map((link) => ({ label: link.getAttribute('title')!, path: link.getAttribute('href')! })),
  );
  expect(rail.length).toBeGreaterThan(1);
  return rail.filter((command, index, all) =>
    all.findIndex((candidate) => candidate.path === command.path) === index,
  );
}

async function renderedCommands(page: Page): Promise<Command[]> {
  const options = page.locator(SEL.paletteOption);
  const result: Command[] = [];
  for (const option of await options.all()) {
    result.push({
      label: (await option.locator(SEL.paletteLabel).innerText()).trim(),
      path: (await option.locator(SEL.palettePath).innerText()).trim(),
    });
    expect(await option.getAttribute('href')).toBe(result.at(-1)!.path);
    await expect(option).toHaveAttribute('tabindex', '-1');
  }
  return result;
}

async function selected(page: Page, index: number) {
  const options = page.locator(SEL.paletteOption);
  const id = await options.nth(index).getAttribute('id');
  expect(id).toBeTruthy();
  await expect(page.locator(SEL.paletteInput)).toHaveAttribute('aria-activedescendant', id!);
  await expect(options.nth(index)).toHaveAttribute('aria-selected', 'true');
  expect(await options.evaluateAll((nodes) =>
    nodes.filter((node) => node.getAttribute('aria-selected') === 'true').length,
  )).toBe(1);
  await expect(page.locator(SEL.paletteInput)).toBeFocused();
}

for (const chord of ['Control+k', 'Meta+k']) {
  for (const origin of ['page', 'editor']) {
    test(`${chord} opens from ${origin} and Escape restores trigger`, async ({ page }) => {
      await page.goto('/search');
      await expect(page.locator(SEL.cmContent)).toBeVisible();
      if (origin === 'editor') await page.locator(SEL.cmContent).click();
      else await page.locator(SEL.runButton).focus();
      await open(page, chord);
      await expect(page.locator(SEL.paletteInput)).toHaveAttribute('aria-expanded', 'true');
      const listId = await page.locator(SEL.paletteList).getAttribute('id');
      expect(listId).toBeTruthy();
      await expect(page.locator(SEL.paletteInput)).toHaveAttribute('aria-controls', listId!);
      await page.keyboard.press('Escape');
      await closed(page);
    });
  }
}

test('trigger accessible name includes its visible Go to label', async ({ page }) => {
  await page.goto('/search');
  const trigger = page.locator(SEL.paletteTrigger);
  await expect(trigger).toContainText('Go to…');
  await expect(trigger).toHaveAccessibleName('Go to… Command palette');
  await page.getByRole('button', { name: 'Go to… Command palette', exact: true }).click();
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
});

for (const kind of ['composing', 'consumed'] as const) {
  test(`${kind} Escape leaves the palette open until an ordinary Escape`, async ({ page }) => {
    await page.goto('/search');
    await open(page);
    const input = page.locator(SEL.paletteInput);
    const prevented = await input.evaluate((node, kind) => {
      if (kind === 'consumed') {
        node.addEventListener('keydown', (event) => event.preventDefault(), { once: true });
      }
      const event = new KeyboardEvent('keydown', {
        key: 'Escape', bubbles: true, cancelable: true, isComposing: kind === 'composing',
      });
      node.dispatchEvent(event);
      return event.defaultPrevented;
    }, kind);
    await expect(page.locator(SEL.paletteDialog)).toBeVisible();
    await expect(input).toBeFocused();
    expect(prevented).toBe(kind === 'consumed');
    await page.keyboard.press('Escape');
    await closed(page);
  });
}

for (const route of ['/search/schema', '/settings/health']) {
  test(`trigger opens deduplicated mode-first inventory at ${route}`, async ({ page, request }) => {
    if (route.startsWith('/settings')) await resetScenario(request, 'health-viewer');
    await page.goto(route);
    await expect(page.locator(SEL.paletteTrigger)).toHaveAttribute('aria-haspopup', 'dialog');
    const expected = await chromeCommands(page);
    await open(page);
    expect(await renderedCommands(page)).toEqual(expected);
    const current = page.locator(SEL.paletteOption).filter({ has: page.locator(SEL.paletteCurrent) });
    await expect(current).toHaveCount(1);
    await expect(current).toHaveAttribute('href', route);
    await expect(page.locator(SEL.paletteKbd)).toHaveText('Ctrl+K');
    await expect(page.locator(SEL.paletteTrigger)).toHaveAttribute('aria-keyshortcuts', 'Control+K');
    if (route.startsWith('/settings')) {
      // Operations contributes Health and nothing else: with the mode
      // tabs gone there is no bare /settings destination, and Schema is
      // listed once, under Search (ADR-0032).
      expect(expected.some((command) => command.path === '/settings/health')).toBe(true);
      expect(expected.filter((command) => command.path === '/settings')).toHaveLength(0);
      expect(expected.filter((command) => command.path === '/search/schema')).toHaveLength(1);
    }
  });
}

test('filter matches every token across label and path, announces count and empty, and resets on reopen', async ({ page }) => {
  await page.goto('/search');
  const inventory = await chromeCommands(page);
  await open(page);
  const input = page.locator(SEL.paletteInput);
  const status = page.locator(SEL.paletteStatus);
  await expect(status).toHaveAttribute('aria-live', 'polite');
  for (const query of ['  sChEmA  ', '/search', '  HiSt /SEARCH  ']) {
    await input.fill(query);
    const tokens = query.trim().toLowerCase().split(/\s+/);
    const expected = inventory.filter((command) => tokens.every((token) =>
      command.label.toLowerCase().includes(token) || command.path.toLowerCase().includes(token),
    ));
    expect(expected.length).toBeGreaterThan(0);
    await expect(page.locator(SEL.paletteOption)).toHaveCount(expected.length);
    expect(await renderedCommands(page)).toEqual(expected);
    await expect(status).toContainText(String(expected.length));
    await selected(page, 0);
  }
  await input.fill('history no-such-command-168');
  await expect(page.locator(SEL.paletteOption)).toHaveCount(0);
  await expect(page.locator(SEL.paletteEmpty)).toBeVisible();
  await expect(status).toContainText(/no matching pages/i);
  expect(await input.getAttribute('aria-activedescendant')).toBeFalsy();
  const url = page.url();
  const historyLength = await page.evaluate(() => history.length);
  await page.keyboard.press('Enter');
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
  expect(page.url()).toBe(url);
  expect(await page.evaluate(() => history.length)).toBe(historyLength);
  await page.keyboard.press('Escape');
  await open(page);
  await expect(input).toHaveValue('');
  await expect(page.locator(SEL.paletteOption)).toHaveCount(inventory.length);
  await selected(page, 0);
});

test('arrows wrap active descendant, hover preserves selection, Home and End move caret, and Tab stays inside', async ({ page }) => {
  await page.goto('/search');
  await open(page);
  const count = await page.locator(SEL.paletteOption).count();
  await selected(page, 0);
  await page.keyboard.press('ArrowUp');
  await selected(page, count - 1);
  await page.keyboard.press('ArrowDown');
  await selected(page, 0);
  await page.keyboard.press('ArrowDown');
  await selected(page, 1);
  await page.locator(SEL.paletteOption).last().hover();
  await selected(page, 1);
  const input = page.locator(SEL.paletteInput);
  await input.fill('/search');
  await page.keyboard.press('End');
  expect(await input.evaluate((node: HTMLInputElement) => node.selectionStart)).toBe(7);
  await selected(page, 0);
  await page.keyboard.press('Home');
  expect(await input.evaluate((node: HTMLInputElement) => node.selectionStart)).toBe(0);
  await selected(page, 0);
  for (const key of ['Tab', 'Tab', 'Shift+Tab', 'Shift+Tab']) {
    await page.keyboard.press(key);
    expect(await page.locator(SEL.paletteDialog).evaluate((node) => node.contains(document.activeElement))).toBe(true);
  }
});

test('Enter activates one router anchor and adds exactly one history entry', async ({ page }) => {
  await page.goto('/search');
  await open(page);
  await page.locator(SEL.paletteInput).fill('history');
  await expect(page.locator(SEL.paletteOption)).toHaveCount(1);
  const before = await page.evaluate(() => history.length);
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/\/search\/history$/);
  await closed(page);
  expect(await page.evaluate(() => history.length)).toBe(before + 1);
  await page.goBack();
  await expect(page).toHaveURL(/\/search$/);
});

test('ordinary pointer activation navigates and restores trigger', async ({ page }) => {
  await page.goto('/search');
  await open(page);
  await page.locator(SEL.paletteInput).fill('history');
  const before = await page.evaluate(() => history.length);
  await page.locator(SEL.paletteOption).click();
  await expect(page).toHaveURL(/\/search\/history$/);
  await closed(page);
  expect(await page.evaluate(() => history.length)).toBe(before + 1);
});

for (const gesture of ['control-click', 'middle-click'] as const) {
  test(`${gesture} opens the native anchor in a new tab and leaves the palette open`, async ({ page, context }) => {
    await page.goto('/search');
    await open(page);
    await page.locator(SEL.paletteInput).fill('history');
    const newPage = context.waitForEvent('page');
    if (gesture === 'control-click') await page.locator(SEL.paletteOption).click({ modifiers: ['Control'] });
    else await page.locator(SEL.paletteOption).click({ button: 'middle' });
    const tab = await newPage;
    try {
      await expect(tab).toHaveURL(/\/search\/history$/);
      await expect(page).toHaveURL(/\/search$/);
      await expect(page.locator(SEL.paletteDialog)).toBeVisible();
      const input = page.locator(SEL.paletteInput);
      await expect(input).toHaveValue('history');
      await expect.soft(input, 'modified activation must keep combobox focus').toBeFocused();
      for (const key of ['Tab', 'Tab', 'Shift+Tab', 'Shift+Tab']) {
        await page.keyboard.press(key);
        expect.soft(
          await page.locator(SEL.paletteDialog).evaluate((node) => node.contains(document.activeElement)),
          `${gesture} followed by ${key} must keep focus inside the dialog`,
        ).toBe(true);
      }
      await expect(input).toBeFocused();
    } finally {
      await tab.close();
    }
  });
}

test('close button and scrim mousedown restore trigger', async ({ page }) => {
  await page.goto('/search');
  await open(page);
  await page.locator(SEL.paletteClose).click();
  await closed(page);
  await open(page);
  await expect(page.locator(SEL.paletteScrim)).toBeVisible();
  const outside = await page.locator(SEL.paletteDialog).evaluate((node) => {
    const box = node.getBoundingClientRect();
    return { x: Math.max(2, box.left / 2), y: window.innerHeight - 2 };
  });
  await page.mouse.move(outside.x, outside.y);
  await page.mouse.down();
  await closed(page);
  await page.mouse.up();
});

test('chord while open toggles closed', async ({ page }) => {
  await page.goto('/search');
  await open(page, 'Control+k');
  await page.keyboard.press('Control+k');
  await closed(page);
});

test('overlay interaction: export modal blocks the chord and one Escape closes only the modal', async ({ page }) => {
  await page.goto('/search');
  await page.locator(SEL.exportAction).click();
  await expect(page.locator(SEL.modalPanel)).toBeVisible();
  for (const chord of ['Control+k', 'Meta+k']) {
    await page.keyboard.press(chord);
    await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);
    await expect(page.locator(SEL.modalPanel)).toBeVisible();
  }
  await page.keyboard.press('Escape');
  await expect(page.locator(SEL.modalPanel)).toHaveCount(0);
  await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);
  await expect(page.locator(SEL.exportAction)).toBeFocused();
  await open(page, 'Control+k');
});

test('ignored chords retain browser defaults and composing Enter does not activate', async ({ page }) => {
  await page.goto('/search');
  await expect(page.locator(SEL.paletteTrigger)).toBeVisible();
  const cases = [
    { repeat: true }, { isComposing: true }, { altKey: true },
    { shiftKey: true }, { metaKey: true }, { prevented: true },
  ];
  for (const extra of cases) {
    const prevented = await page.locator(SEL.runButton).evaluate((node, extra) => {
      const event = new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, bubbles: true, cancelable: true, ...extra });
      if ('prevented' in extra) event.preventDefault();
      node.dispatchEvent(event);
      return event.defaultPrevented;
    }, extra);
    expect(prevented).toBe('prevented' in extra);
    await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);
  }
  await open(page);
  await page.locator(SEL.paletteInput).fill('history');
  await page.locator(SEL.paletteInput).dispatchEvent('keydown', {
    key: 'Enter', isComposing: true, bubbles: true, cancelable: true,
  });
  await expect(page.locator(SEL.paletteDialog)).toBeVisible();
  await expect(page).toHaveURL(/\/search$/);
});

test('small viewport contains the panel and scrolls the selected option into view', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 320 });
  await page.goto('/search');
  await open(page, 'Control+k');
  const bounds = await page.locator(SEL.paletteDialog).boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x).toBeGreaterThanOrEqual(0);
  expect(bounds!.y).toBeGreaterThanOrEqual(0);
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(390);
  expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(320);
  await page.keyboard.press('ArrowUp');
  const options = page.locator(SEL.paletteOption);
  await selected(page, await options.count() - 1);
  // Chromium rounds the scroll offset while layout retains fractional
  // pixels. Allow one CSS pixel at the list edge, not a clipped row.
  await expect(options.last()).toBeInViewport({ ratio: 0.99 });
  const clippedPixels = await options.last().evaluate((node, listSelector) => {
    const row = node.getBoundingClientRect();
    const list = node.closest(listSelector)!;
    const top = list.getBoundingClientRect().top + list.clientTop;
    const bottom = top + list.clientHeight;
    return Math.max(top - row.top, row.bottom - bottom, 0);
  }, SEL.paletteList);
  expect(clippedPixels).toBeLessThanOrEqual(1);
});

// Only the UA changes. Actual Chromium keyboard events still pass through
// Shell's listener and the DOM target classifier used by the application.
test.describe('macOS editable target carve-out', () => {
  test.use({ userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36' });

  for (const target of ['checkbox', 'readonly', 'readonly-textarea', 'text', 'textarea', 'contenteditable'] as const) {
    test(`Ctrl+K ${target === 'checkbox' || target === 'readonly' || target === 'readonly-textarea' ? 'opens' : 'preserves editing'} on ${target}`, async ({ page }) => {
      await page.goto('/search');
      await expect(page.locator(SEL.paletteTrigger)).toBeEnabled();
      await expect(page.locator(SEL.paletteKbd)).toHaveText('⌘K');
      await page.evaluate((target) => {
        const node = document.createElement(target === 'textarea' || target === 'readonly-textarea' ? 'textarea' : target === 'contenteditable' ? 'div' : 'input');
        node.setAttribute('aria-label', 'Palette target fixture');
        if (node instanceof HTMLInputElement) {
          node.type = target === 'checkbox' ? 'checkbox' : 'text';
          node.readOnly = target === 'readonly';
          if (node.type === 'text') node.value = 'keep this text';
        }
        if (node instanceof HTMLTextAreaElement) {
          node.value = 'keep this text';
          node.readOnly = target === 'readonly-textarea';
        }
        if (target === 'contenteditable') {
          node.contentEditable = 'true';
          node.textContent = 'keep this text';
        }
        document.body.append(node);
        node.focus();
      }, target);
      const fixture = page.getByLabel('Palette target fixture', { exact: true });
      await expect(fixture).toBeFocused();
      await page.keyboard.press('Control+k');
      if (target === 'checkbox' || target === 'readonly' || target === 'readonly-textarea') {
        await expect(page.locator(SEL.paletteDialog)).toBeVisible();
        await expect(page.locator(SEL.paletteInput)).toBeFocused();
        await page.keyboard.press('Escape');
        await closed(page);
        await fixture.focus();
      } else {
        await expect(page.locator(SEL.paletteDialog)).toHaveCount(0);
        await expect(fixture).toBeFocused();
      }
      await open(page, 'Meta+k');
      await page.keyboard.press('Escape');
      await closed(page);
    });
  }
});
