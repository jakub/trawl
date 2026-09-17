// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Classic and synchronous: set appearance before styles or Wasm run.
// Keep parsing/resolution aligned with theme/preference-fixtures.json.
(() => {
  let preference = 'system';
  try {
    const key = document.currentScript.dataset.storageKey;
    const stored = JSON.parse(localStorage.getItem(key));
    if (stored !== null && typeof stored === 'object' && !Array.isArray(stored)
        && ['light', 'dark', 'system'].includes(stored.theme)) {
      preference = stored.theme;
    }
  } catch (_) {
    // Read failures and malformed storage keep System without repairing storage.
  }
  let dark = preference === 'dark';
  if (preference === 'system') {
    try {
      dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
    } catch (_) {
      dark = false;
    }
  }
  document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
})();
