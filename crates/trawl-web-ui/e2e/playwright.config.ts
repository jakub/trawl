// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { defineConfig, devices } from '@playwright/test';

// No `webServer`: every worker starts its own harness/server.mjs on
// E2E_PORT + its parallelIndex (the worker-scoped `stubOrigin` fixture in
// fixtures.ts), and `baseURL` comes from that fixture, not from here. A
// stub server keeps one mutable "current scenario" (see its
// `/__ctl/reset` contract), so it must never serve two tests at once; a
// server per worker gives that at any worker count.
//
// Local runs use four workers, so N servers listen on E2E_PORT ..
// E2E_PORT + 3; the full suite then fits well inside a ten-minute window
// (measured 9.4 min on one worker, 3.4 min on four, five clean runs in a
// row). CI keeps one worker per job: it splits the suite across shard jobs
// instead, each on its own runner. `--workers=N` overrides either. A port
// already in use fails the worker loudly rather than reusing a server
// that may be serving another worktree's dist.
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
  workers: process.env.CI ? 1 : 4,
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
});
