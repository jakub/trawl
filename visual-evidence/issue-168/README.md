# Command palette evidence

Captured on 2026-09-09 UTC with the committed Chromium Playwright dependency,
against the disposable E2E server and SPA built from
`51164dad6876769d0529f878a7878fbd8d9d3a17`. The source implementation is
`9d1bd246283604294b225bff8408027966bacb94`. These captures contain fixture data.

| Capture | Setup |
| --- | --- |
| [Search](chromium-search.png) | `/search`, default fixture, 1440 by 900, light theme, trigger opened |
| [Settings](chromium-settings.png) | `/settings/health`, `health-viewer` fixture, 1440 by 900, trigger opened |
| [Small viewport](chromium-small.png) | `/search`, 390 by 420, Ctrl+K opened, ArrowUp selected the last option |

The Search inventory is Search, Jobs, Settings, History, and Schema. The
Settings inventory is Search, Jobs, Settings, and Health. Jobs points to
`/jobs/nets`. Health already has its own `/settings/health` route at this base.
The Settings mode wins the exact-path deduplication for `/settings`, so the
Sources, Schema, Users & API, and Retention rail placeholders do not become
separate palette entries. This change creates no new destinations for them.

The behavioral proof is committed in
[`command-palette.spec.ts`](../../crates/trawl-web-ui/e2e/tests/command-palette.spec.ts).
Its 26 tests cover the keyboard and pointer contracts, one history entry per
normal activation, focus restoration, overlay exclusion, viewport bounds,
and the macOS editable-control regression. They also cover the visible trigger
label and composing or consumed Escape. After integration with PR #173, the
full Chromium suite passed 167 tests at `54ec6b11d5c8dd72315da9012b28960879440677`.

From the repository root, rebuild and run the focused evidence with:

```sh
(cd crates/trawl-web-ui && env -u NO_COLOR trunk build)
(cd crates/trawl-web-ui/e2e && npm ci && E2E_PORT=8168 npx playwright test tests/command-palette.spec.ts)
cargo nextest run -p fleet-ui command_palette
cargo nextest run -p fleet-ui component_class_contract
cargo nextest run -p trawl-web-ui --test e2e_selector_contract
```

The Mac UA cases run in Chromium. They prove the application's UA-dependent
keyboard logic, including that Ctrl+K opens on a checkbox and read-only
controls while preserving text editing on writable controls. They do not
prove Safari browser-chrome delivery.

## Browser delivery residual

The one-time manual headed Firefox Ctrl+K and Safari/macOS Meta+K checks were
not performed. Safari/macOS is unavailable on this Linux host. No screenshot
here is a Firefox or Safari capture. Browser-chrome shortcut preemption
remains unverified under the residual allowed by ADR-0031; the manual capture
acceptance criterion is not claimed as passed.

## Consumer API

`Shell` callers pass no new props. Direct `TopBar` callers must provide
`on_open_palette`, `palette_open`, `palette_available`, and `palette_trigger`.
Coastwatch receives this change with its next Trawl revision update and must
account for these required props if it mounts `TopBar` directly.
