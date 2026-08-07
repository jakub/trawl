# jakub/coastwatch#308 evidence — Atmosphere WebGL shader backdrop (ADR-0012)

Captured against the fleet-ui workbench (`trunk serve` on :8082, the
shell_demo `/login` route) and trawl-web-ui (`trunk serve` on :8083),
driven via Playwright (Chromium). Native/build gates ran on the same
tree (`feat/issue-308-shader-atmosphere`) — the lint and nextest
transcripts below were captured at `a9ef621d`, the last code-bearing
commit on the branch, so re-run them if a later commit touches code.

## AC-3 — one canvas, clean unmount, clean console

- `login-light.png` — `/login` with the mesh-gradient backdrop under
  the login card, light theme. DOM probe: exactly 1 `canvas`, inside
  the single `div.atmosphere[aria-hidden=true]`.
- Three SPA round trips `/login → / → /login` (leptos-router popstate,
  no reloads): canvas count 1 → 0 → 1 on every trip — `on_cleanup`
  disposes the mount, nothing accumulates.
- `console-roundtrips.log` — full transcript for the session: 0 errors.
  The warnings are trunk-dev's preload-integrity notice, the EXPECTED
  `CONTEXT_LOST_WEBGL` from `dispose()` releasing the context on route
  exit, and NVIDIA driver perf chatter (ReadPixels stalls from the
  screenshot captures).

## AC-4 — theme flip re-colors the SAME canvas, no remount

- `login-dark.png` — after clicking the workbench theme toggle while
  `/login` stayed mounted. Node-identity probe across the flip:
  `before === after` true, `isConnected` true, still exactly 1 canvas.
  The mesh re-colored in place via `setUniforms`.

## AC-5 — prefers-reduced-motion: static frame, no rAF

- `login-reduced-motion.png` — `emulateMedia({reducedMotion:
  'reduce'})`, fresh mount. A static mesh frame paints; instrumented
  `requestAnimationFrame` counted **0** callbacks scheduled over
  1500 ms — the vendored package stops its loop entirely at speed 0.

## AC-6 — WebGL2 unavailable: silent var(--bg) floor

- `login-no-webgl2.png` — `getContext('webgl2'|'webgl')` stubbed to
  `null` via init script before any page script ran. No canvas mounts;
  `.atmosphere`'s computed background equals body's (`oklch(0.145 0 0)`
  dark floor — flat `var(--bg)`); console shows zero errors (only the
  trunk-dev preload notice).

## AC-7 — bundle size

- `vendor-drift-transcript.txt` — `du -h` reports **144K** on disk
  (142K file) against the issue's 500 KB cap, and a fresh
  `vendor/build.sh` run leaves `git status` clean (drift gate green,
  Apache-2.0 banner riding in the artifact).

## AC-2 / AC-8 — builds and native suite

- `trawl-trunk-build-transcript.txt` — trawl-web-ui `trunk build`
  succeeds with no npm step anywhere.
- `nextest-transcript.txt` — `cargo nextest run --workspace`:
  **2623 tests run: 2623 passed, 0 skipped** (includes the 154
  fleet-ui native tests: palette parity, vendor contract, chrome
  parity, class contracts).
- `nextest-no-defaults-transcript.txt` — the second pre-push suite,
  `--no-default-features` on its own postgres cluster:
  **1269 tests run: 1269 passed, 0 skipped**. This is the config CI's
  `test` job runs, and the one the atmosphere slice ships in by
  default — the `atmosphere` feature is off unless a consumer opts in.
- `lint-transcript.txt` — `cargo fmt --check` clean, and
  `cargo clippy --workspace --all-targets` emits zero warnings both
  with default features (`-D warnings`) and `--no-default-features`.
- `cargo check -p fleet-ui --target wasm32-unknown-unknown` clean both
  with and without `--features atmosphere`; fleet-ui `trunk build`
  emits the vendored bundle as a wasm-bindgen snippet at
  `dist/snippets/fleet-ui-<hash>/vendor/paper-shaders.js`, which the
  generated shim imports on its first line.
- `snippet-scoping-transcript.txt` — the snippet is scoped to consumers
  that opt in. wasm-bindgen emits a local snippet for anything that
  *links* the extern block, so the default-off `atmosphere` feature is
  what keeps it out of a non-mounting consumer: trawl-web-ui's release
  dist contains **no** `paper-shaders.js` and **no** modulepreload for
  it (grep = 0), while the workbench dist carries all 144,920 bytes and
  imports them on line 1. CI asserts both directions.

## .login-shell zero-delta check (trawl-web-ui)

- `trawl-login-light.png` / `trawl-login-dark.png` — trawl-web-ui's
  real `/login` after the `.login-shell` background removal:
  `.login-shell` computes `rgba(0,0,0,0)` and body's `var(--bg)` paints
  the viewport in both themes — visually identical to before the
  change (no backdrop is mounted there in this slice).
