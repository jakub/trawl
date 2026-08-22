// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

// One stub server, one scenario in flight at a time (harness/server.mjs
// keeps a single mutable "current scenario" — see its `/__ctl/reset`
// contract) — so specs never run concurrently against it.
export default defineConfig({
  testDir: './tests',
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  globalTimeout: 240_000,
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  use: {
    baseURL: 'http://127.0.0.1:8123',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    viewport: { width: 1440, height: 900 },
    locale: 'en-US',
    timezoneId: 'UTC',
    colorScheme: 'light',
    reducedMotion: 'reduce',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'node harness/server.mjs',
    url: 'http://127.0.0.1:8123/__ctl/health',
    reuseExistingServer: !process.env.CI,
    stdout: 'pipe',
  },
});
