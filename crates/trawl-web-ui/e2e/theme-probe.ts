// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import type { Page } from '@playwright/test';

export interface ProbeOptions {
  key?: string;
  raw?: string | null;
  storage?: 'normal' | 'blocked' | 'read-fails' | 'quota';
  media?: 'normal' | 'unavailable' | 'throws';
  changeDuringRegistration?: boolean;
}

/** Test-only browser API wrappers. No instrumentation is shipped by either app. */
export async function installThemeProbe(page: Page, options: ProbeOptions = {}): Promise<void> {
  await page.addInitScript(options => {
    const key = options.key ?? 'trawl.ui';
    const storage = window.localStorage;
    if (options.raw === null) storage.removeItem(key);
    else if (options.raw !== undefined) storage.setItem(key, options.raw);
    const state = {
      writes: [] as string[], registrations: 0, removals: 0, callbacks: 0,
      active: new Set<EventListenerOrEventListenerObject>(),
    };
    (window as any).__themeProbe = state;
    const write = Storage.prototype.setItem;
    Storage.prototype.setItem = function (name, value) {
      if (name === key) {
        state.writes.push(value);
        if (options.storage === 'quota') throw new DOMException('full', 'QuotaExceededError');
      }
      return write.call(this, name, value);
    };
    const removeItem = Storage.prototype.removeItem;
    Storage.prototype.removeItem = function (name) {
      if (name === key) state.writes.push('__remove__');
      return removeItem.call(this, name);
    };
    const clear = Storage.prototype.clear;
    Storage.prototype.clear = function () {
      state.writes.push('__clear__');
      return clear.call(this);
    };
    if (options.storage === 'blocked') {
      Object.defineProperty(window, 'localStorage', { configurable: true, get() {
        throw new DOMException('blocked', 'SecurityError');
      } });
    } else if (options.storage === 'read-fails') {
      const read = Storage.prototype.getItem;
      Storage.prototype.getItem = function (name) {
        if (name === key) throw new DOMException('blocked read', 'SecurityError');
        return read.call(this, name);
      };
    }
    const matchMedia = window.matchMedia.bind(window);
    window.matchMedia = function (query) {
      if (query !== '(prefers-color-scheme: dark)') return matchMedia(query);
      if (options.media === 'throws') throw new Error('media query unavailable');
      if (options.media === 'unavailable') return null as unknown as MediaQueryList;
      const media = matchMedia(query);
      const add = media.addEventListener.bind(media);
      const remove = media.removeEventListener.bind(media);
      const wrapped = new Map<EventListenerOrEventListenerObject, EventListener>();
      media.addEventListener = ((type: string, callback: EventListenerOrEventListenerObject, opts?: any) => {
        if (type !== 'change') return add(type, callback, opts);
        state.registrations++;
        state.active.add(callback);
        const listener: EventListener = event => {
          state.callbacks++;
          if (typeof callback === 'function') callback.call(media, event);
          else callback.handleEvent(event);
        };
        wrapped.set(callback, listener);
        add(type, listener, opts);
        // Change after registration without dispatching an event. Only the
        // required final matches sample can observe this installation state.
        if (options.changeDuringRegistration) {
          Object.defineProperty(media, 'matches', { configurable: true, value: true });
        }
      }) as typeof media.addEventListener;
      media.removeEventListener = ((type: string, callback: EventListenerOrEventListenerObject, opts?: any) => {
        if (type === 'change' && wrapped.has(callback)) {
          state.removals++;
          state.active.delete(callback);
          remove(type, wrapped.get(callback)!, opts);
          wrapped.delete(callback);
        } else remove(type, callback, opts);
      }) as typeof media.removeEventListener;
      return media;
    };
  }, options);
}

export async function themeProbe(page: Page) {
  return page.evaluate(() => {
    const probe = (window as any).__themeProbe;
    return { writes: probe.writes as string[], registrations: probe.registrations as number,
      removals: probe.removals as number, callbacks: probe.callbacks as number,
      active: probe.active.size as number };
  });
}

export async function settleTheme(page: Page): Promise<void> {
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}
