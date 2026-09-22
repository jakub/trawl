# Theme evidence

The ordinary suite runs `tests/theme-preference.spec.ts` against the existing
API harness. It executes the built bootstrap against Fleet's shared fixture
table and checks preference persistence, live media changes, and menu behavior.
The harness's storage policy and request contract are unchanged.

`playwright.theme.config.ts` runs this directory against three separate
processes of the `trawl-web` `theme_assets` example. The first serves embedded
Trawl assets, the second serves a Trawl disk distribution, and the third serves
the Fleet workbench disk distribution. Every process uses Trawl's existing asset
handlers and security-header layers. The tests visit login and the standalone
workbench, so they need no backend or database.

Build both HTML entry points with Trunk, then build the example in release mode
after the candidate Trawl distribution is in `crates/trawl-web-ui/dist`:

```sh
cargo build --release --locked -p trawl-web --example theme_assets
```

Set these environment variables to absolute paths before running
`npm run test:theme` from `crates/trawl-web-ui/e2e`:

- `THEME_ASSET_SERVER`: the built `theme_assets` example binary.
- `THEME_TRAWL_DIST`: the candidate Trawl distribution directory.
- `THEME_WORKBENCH_DIST`: the candidate Fleet workbench distribution directory.
- `THEME_ASSET_MANIFEST`: optional absolute path to the asset check's
  `theme-assets.json`; defaults to `e2e-artifacts/theme-builds/theme-assets.json`
  at the repository root.
- `THEME_E2E_PORT`: optional first port, default 8136. The runner also owns the
  next two ports and refuses to reuse existing servers.

Run the asset identity check before the browser tests, with Node and Trunk installed:

```sh
node crates/trawl-web-ui/e2e/scripts/verify-theme-assets.mjs \
  crates/trawl-web-ui/dist crates/fleet-ui/dist e2e-artifacts/theme-builds
```

It records both emitted HTML files and bootstrap bytes, checks the namespace and
blocking order, and runs two temporary JavaScript-only Trunk builds to prove
that changing bootstrap content changes its hashed URL. It does not modify the
candidate source or either distribution.
The browser tests compare served identity-encoded bytes and URLs with this
manifest. Embedded and disk Trawl serving must match the same Trawl entry;
the workbench must match its own entry.

The production browser tests hold Wasm while observing computed background and
`color-scheme`, then release it and compare the installed appearance. They also
exercise changed handoff inputs, disabled JavaScript, and failed bootstrap
loading. The workbench tests dispose its sole preference owner and verify a
subsequent OS change cannot invoke the removed callback. All browser API probes
come from test init scripts; the shipped bootstrap and runtime expose no test
globals.
The workbench also has Clear/Restore demo identity controls. These extend the
planned install/dispose probe to exercise actual identity loss while the menu
is open, without an outside click hiding the menu first. They change only the
demo's existing user signal and add no production API or second installation.

CI retains the emitted assets, HTTP headers, computed appearance values,
screenshots, traces, and HTML report in `theme-production-evidence`. The ordinary
suite retains its selected-System screenshots in the `theme-menu-captures-N`
artifact of the shard that ran them.
These expiring CI artifacts are an evidence handoff; acceptance requires the
selected-System captures to be committed or published privately with durable
retention when the final evidence is assembled.
