// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

// Same variable harness/server.mjs reads — one owner for the port.
const PORT = Number(process.env.E2E_PORT ?? 8123);

// One stub server, one scenario in flight at a time (harness/server.mjs
// keeps a single mutable "current scenario" — see its `/__ctl/reset`
// contract) — so specs never run concurrently against it.
export default defineConfig({
  testDir: './tests',
  // Scratch output — traces, failure screenshots, `.last-run.json` — lands
  // at the repository root, not beside the suite. `trunk serve` watches
  // crates/trawl-web-ui, so a write anywhere below it wakes a running dev
  // server for a full rebuild, and that rebuild's "applying new
  // distribution" step clears `dist/.stage` under any concurrent `trunk
  // build`, which then dies with "error writing JS loader file to stage
  // dir". Relative paths here resolve against this file's directory.
  outputDir: '../../../e2e-artifacts/test-results',
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 20_000,
  expect: { timeout: 5_000 },
  // Default for local and focused mutation runs. The complete CI suite owns
  // its larger aggregate budget in ci.yml; per-test timeouts stay unchanged.
  // A cut-off suite reports remaining cases as "did not run", never passes.
  globalTimeout: 720_000,
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
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
      // Full Chromium avoids the headless-shell renderer crashes seen
      // during native new-tab tests, including a plain HTML reproduction.
      use: { ...devices['Desktop Chrome'], channel: 'chromium' },
    },
  ],
  webServer: {
    command: 'node harness/server.mjs',
    url: `http://127.0.0.1:${PORT}/__ctl/health`,
    // Never reuse: a server left by another worktree would serve THAT
    // checkout's dist and this run would silently test the wrong SPA.
    // A port collision must fail loudly instead (set E2E_PORT to run
    // suites in parallel).
    reuseExistingServer: false,
    stdout: 'pipe',
  },
});
