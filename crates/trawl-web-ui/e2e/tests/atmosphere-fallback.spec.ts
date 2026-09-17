// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { test, expect } from '../fixtures';
import type { Page } from '@playwright/test';

type Failure = 'none' | 'no-webgl' | 'compile' | 'link' | 'late';

// These probes wrap browser APIs before the app starts. rAF attribution uses
// the shipped snippet's URL, so Leptos work cannot masquerade as shader work.
async function instrument(page: Page, failure: Failure) {
  const errors: string[] = [];
  page.on('console', message => {
    if (message.type() === 'error') errors.push(message.text());
  });
  await page.addInitScript((failure) => {
    const probe = (window as any).__atmosphere = {
      scheduled: 0, frames: 0, pending: new Set<number>(), draws: 0, uniforms: 0,
      faults: 0, lost: 0, contexts: [] as WebGL2RenderingContext[],
      reporter: console.error,
    };
    const request = window.requestAnimationFrame.bind(window);
    const cancel = window.cancelAnimationFrame.bind(window);
    window.requestAnimationFrame = callback => {
      const shader = new Error().stack?.includes('/vendor/paper-shaders.js') ?? false;
      let id: number;
      id = request(time => {
        probe.pending.delete(id);
        if (shader) probe.frames++;
        callback(time);
      });
      if (shader) { probe.scheduled++; probe.pending.add(id); }
      return id;
    };
    window.cancelAnimationFrame = id => { probe.pending.delete(id); cancel(id); };
    const getContext = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function (kind: any, ...args: any[]) {
      if (kind === 'webgl2' && failure === 'no-webgl') {
        probe.faults++;
        return null;
      }
      const context = (getContext as any).call(this, kind, ...args);
      if (kind === 'webgl2' && context && !probe.contexts.includes(context)) {
        probe.contexts.push(context);
        this.addEventListener('webglcontextlost', () => probe.lost++);
      }
      return context;
    } as typeof getContext;
    const gl = WebGL2RenderingContext.prototype;
    const draw = gl.drawArrays;
    gl.drawArrays = function (...args) {
      probe.draws++;
      return draw.apply(this, args);
    };
    const uniform = gl.uniform4fv;
    gl.uniform4fv = function (...args) {
      probe.uniforms++;
      return uniform.apply(this, args);
    };
    const shaderParameter = gl.getShaderParameter;
    gl.getShaderParameter = function (shader, parameter) {
      if (failure === 'compile' && parameter === this.COMPILE_STATUS) {
        probe.faults++;
        // An unrelated diagnostic inside the constructor must still surface.
        console.error('atmosphere-e2e unrelated constructor diagnostic');
        return false;
      }
      return shaderParameter.call(this, shader, parameter);
    };
    const programParameter = gl.getProgramParameter;
    gl.getProgramParameter = function (program, parameter) {
      if (failure === 'link' && parameter === this.LINK_STATUS) {
        probe.faults++;
        return false;
      }
      return programParameter.call(this, program, parameter);
    };
    if (failure === 'late') {
      const setAttribute = Element.prototype.setAttribute;
      Element.prototype.setAttribute = function (name, value) {
        setAttribute.call(this, name, value);
        if (name === 'data-paper-shader' && this.classList.contains('atmosphere')) {
          probe.faults++;
          throw new Error('atmosphere-e2e late constructor failure');
        }
      };
    }
  }, failure);
  return errors;
}

async function floor(page: Page) {
  const host = page.locator('.atmosphere');
  await expect(host).toHaveCount(1);
  const colors = await host.evaluate(el => {
    const reference = document.createElement('div');
    reference.style.background = 'var(--bg)';
    el.append(reference);
    const expected = getComputedStyle(reference).backgroundColor;
    reference.remove();
    return { actual: getComputedStyle(el).backgroundColor, expected };
  });
  expect(colors.actual).toBe(colors.expected);
  expect(colors.actual).not.toBe('rgba(0, 0, 0, 0)');
}

async function quiet(page: Page, label: string) {
  const before = await page.evaluate(() => {
    const p = (window as any).__atmosphere;
    return { scheduled: p.scheduled, frames: p.frames, pending: p.pending.size };
  });
  expect(before.pending, label).toBe(0);
  await page.waitForTimeout(350);
  expect(await page.evaluate(() => {
    const p = (window as any).__atmosphere;
    return { scheduled: p.scheduled, frames: p.frames, pending: p.pending.size };
  }), label).toEqual(before);
}

for (const failure of ['no-webgl', 'compile', 'link', 'late'] as const) {
  test(`${failure} failure leaves a silent CSS floor`, async ({ page }) => {
    const errors = await instrument(page, failure);
    if (failure === 'late') await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.goto('/login');
    await floor(page);
    await expect(page.locator('.atmosphere canvas')).toHaveCount(0);
    await expect(page.locator('.atmosphere')).not.toHaveAttribute('data-paper-shader');
    expect(await page.evaluate(() => (window as any).__atmosphere.faults)).toBeGreaterThan(0);
    if (failure !== 'no-webgl') {
      expect(await page.evaluate(() => (window as any).__atmosphere.contexts.length)).toBe(1);
      await expect.poll(() => page.evaluate(() =>
        (window as any).__atmosphere.contexts.every((gl: WebGL2RenderingContext) => gl.isContextLost()),
      )).toBe(true);
    }
    if (failure === 'late') {
      expect(await page.evaluate(() => (window as any).__atmosphere.scheduled)).toBeGreaterThan(0);
    }
    await quiet(page, 'failed construction must cancel shader work');
    expect(await page.evaluate(() => console.error === (window as any).__atmosphere.reporter)).toBe(true);
    await page.evaluate(() => console.error('atmosphere-e2e unrelated after construction'));
    await expect.poll(() => errors.includes('atmosphere-e2e unrelated after construction')).toBe(true);
    if (failure === 'compile') {
      expect(errors).toContain('atmosphere-e2e unrelated constructor diagnostic');
    }
    expect(errors.filter(message => !message.startsWith('atmosphere-e2e unrelated '))).toEqual([]);
  });
}

test.describe('terminal context loss', () => {
  test.use({ reducedMotion: 'no-preference' });

  test('live shader stays stopped after loss and reactive motion changes', async ({ page }) => {
    const errors = await instrument(page, 'none');
    await page.goto('/login');
    await expect(page.locator('.atmosphere canvas')).toBeVisible();
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.frames)).toBeGreaterThan(2);
    expect(await page.evaluate(() => (window as any).__atmosphere.draws)).toBeGreaterThan(2);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.pending.size)).toBe(0);
    await quiet(page, 'live reduced-motion control stops shader rAF');
    const liveFrames = await page.evaluate(() => (window as any).__atmosphere.frames);
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.frames)).toBeGreaterThan(liveFrames + 2);
    await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      p.host = document.querySelector('.atmosphere');
      p.canvas = p.host.querySelector('canvas');
      const gl = p.canvas.getContext('webgl2');
      const extension = gl.getExtension('WEBGL_lose_context');
      if (!extension) throw new Error('WEBGL_lose_context is required, never skip this test');
      extension.loseContext();
    });
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.lost)).toBe(1);
    await expect(page.locator('.atmosphere canvas')).toBeHidden();
    await floor(page);
    await quiet(page, 'context loss must stop shader rAF');
    const stopped = await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { draws: p.draws, uniforms: p.uniforms };
    });
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.waitForTimeout(100);
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.waitForTimeout(100);
    await quiet(page, 'ATMOSPHERE_NO_RESTART: reactive changes must not restart a dead shader');
    expect(await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { draws: p.draws, uniforms: p.uniforms };
    }), 'ATMOSPHERE_NO_RESTART: dead setters must not issue GL work').toEqual(stopped);
    await expect(page.locator('.atmosphere canvas')).toBeHidden();
    await page.evaluate(() => {
      history.pushState(null, '', '/search');
      dispatchEvent(new PopStateEvent('popstate'));
    });
    await expect(page.locator('.atmosphere')).toHaveCount(0);
    expect(await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { connected: p.host.isConnected, canvas: p.host.querySelector('canvas') !== null,
        marker: p.host.hasAttribute('data-paper-shader'), canvasConnected: p.canvas.isConnected };
    })).toEqual({ connected: false, canvas: false, marker: false, canvasConnected: false });
    await quiet(page, 'disposed shader must have no scheduled work');
    expect(errors).toEqual([]);
  });


  test('shipped handle rejects theme uniforms and speed after loss or disposal', async ({ page }) => {
    const errors = await instrument(page, 'none');
    await page.goto('/search');
    await expect(page.locator('.topbar button.user')).toBeEnabled();
    await expect(page.locator('.atmosphere')).toHaveCount(0);
    // This route loads the same wasm module but mounts no backdrop. Import its
    // unique shipped snippet, then exercise the public handle on a separate
    // host. /login exposes no theme control, so a real theme effect cannot be
    // stimulated there without changing application code or closure lifetimes.
    await page.evaluate(async () => {
      const urls = performance.getEntriesByType('resource')
        .map(entry => entry.name).filter(name => new URL(name).pathname.endsWith('/vendor/paper-shaders.js'));
      if (urls.length !== 1) throw new Error(`Expected one shipped shader snippet, got ${urls.length}`);
      const { createShader } = await import(urls[0]);
      const p = (window as any).__atmosphere;
      p.host = document.createElement('div');
      p.host.className = 'atmosphere';
      document.body.append(p.host);
      p.handle = createShader(p.host, { shader: 'meshGradient', speed: 0.3,
        colors: ['#113355', '#447799', '#aaccee'] });
      if (!p.handle) throw new Error('Real WebGL mount required');
      p.canvas = p.host.querySelector('canvas');
    });
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.frames)).toBeGreaterThan(2);
    expect(await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      const before = p.uniforms;
      p.handle.setUniforms({ colors: ['#552211', '#aa6644', '#ffccaa'] });
      return p.uniforms - before;
    }), 'live color updates must synchronously write GL vec4 uniforms').toBeGreaterThan(0);
    await page.evaluate(() => {
      const gl = (window as any).__atmosphere.canvas.getContext('webgl2');
      const extension = gl.getExtension('WEBGL_lose_context');
      if (!extension) throw new Error('Real context loss is required');
      extension.loseContext();
    });
    await expect.poll(() => page.evaluate(() => (window as any).__atmosphere.lost)).toBe(1);
    await expect(page.locator('.atmosphere canvas')).toBeHidden();
    await floor(page);
    await quiet(page, 'public handle loss stops shader work');
    const stopped = await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { draws: p.draws, uniforms: p.uniforms, scheduled: p.scheduled };
    });
    await page.evaluate(() => {
      const handle = (window as any).__atmosphere.handle;
      handle.setUniforms({ colors: ['#001122', '#334455', '#667788'] });
      handle.setSpeed(0);
      handle.setSpeed(0.3);
    });
    await quiet(page, 'dead public setters must not restart shader work');
    expect(await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { draws: p.draws, uniforms: p.uniforms, scheduled: p.scheduled };
    }), 'dead theme setter must not issue GL work').toEqual(stopped);
    await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      p.handle.dispose();
      p.handle.dispose();
      p.handle.setUniforms({ colors: ['#ffffff'] });
      p.handle.setSpeed(0.3);
    });
    await expect(page.locator('.atmosphere canvas')).toHaveCount(0);
    await expect(page.locator('.atmosphere')).not.toHaveAttribute('data-paper-shader');
    await quiet(page, 'disposed public setters must not restart shader work');
    expect(await page.evaluate(() => {
      const p = (window as any).__atmosphere;
      return { draws: p.draws, uniforms: p.uniforms, scheduled: p.scheduled };
    })).toEqual(stopped);
    expect(errors).toEqual([]);
  });
});
