# WebGL shader backdrop (Atmosphere)

status: accepted (2026-08-07)

> **amendment (2026-08-07):** both intended consumers have since
> adopted the backdrop — coastwatch on its login + auth-boot surfaces
> (coastwatch#311) and trawl-web-ui on `/login`. trawl-web-ui now
> enables the `atmosphere` feature dep-level, so the CI negative gate
> described below (snippet absent from the trawl-web-ui dist) is
> flipped: `trunk-build` asserts **exactly one** emitted snippet in
> both dists. the feature stays default-off in fleet-ui; the
> feature-off compile shape is still guarded by the bare
> `cargo check -p fleet-ui` wasm gate.

> **amendment (2026-09-08, slice F prep, #100):** "silent and permanent"
> is refined into a terminal contract the code actually keeps. Shader
> creation is transactional: WebGL absence, compile failure and link
> failure all take one cleanup path (stop scheduled work, remove the
> partial canvas and parent marker, suppress only the vendored library's
> expected shader diagnostics) and return null — the compile-failure leg
> no longer falls back by accident via a TypeError that `console.error`s.
> A lost context flips the wrapper handle into a terminal `dead` state:
> speed AND uniform calls become no-ops, so the live reduced-motion and
> theme effects cannot restart a dead mount. `dispose()` also removes
> `data-paper-shader` from the parent (upstream only deletes its JS
> property). Rust observes none of this — the only product outcome of
> every failure is the CSS floor, unchanged.

jakub/coastwatch#308 asks fleet-ui for a decorative animated backdrop —
a slow-drifting mesh gradient behind the login card, themed with the
Mira Blue ramp (ADR-0007) — rendered with `@paper-design/shaders`, a
zero-dependency WebGL2 library. The backdrop is pure decoration: it may
never block content, capture input, appear in the accessibility tree,
or produce a user-visible error. This ADR records how it is vendored,
themed, degraded, and consumed.

Admission is ADR-0002 territory and clear-cut: a full-viewport shader
backdrop is generic by nature (no app types, copy, routes, or layout
constants — one `theme` prop), so it lands in fleet-ui on first
extraction even though coastwatch's login page is today's only intended
consumer. The component is `Atmosphere`; every tunable lives in
`atmosphere::palette`, the single iteration site.

## Decisions

### Vendor the package as one committed ESM bundle

`@paper-design/shaders` arrives the same way codemirror and uPlot did
in trawl-web-ui: an esbuild-bundled, minified, committed
`vendor/paper-shaders.js` produced by `vendor/build.sh`, consumed via
`#[wasm_bindgen(module = "/vendor/paper-shaders.js")]`. No npm step in
any consumer's build; CI's existing `vendor-drift` job rebuilds the
bundle and diffs the committed bytes.

Two properties force the one-bundle shape:

- **wasm-bindgen's module constraint.** `module = "…"` loads a single
  browser ES module with no resolver behind it — the artifact must be
  self-contained (no relative imports, no `require()`), which is
  exactly what `esbuild --bundle --format=esm` emits and what
  `tests/atmosphere_vendor_contract.rs` pins.
- **Full catalog by name.** The wrapper (`vendor/src/paper-shaders.ts`)
  exports the package's entire fragment-shader catalog keyed by name,
  so the Rust side selects `"meshGradient"` as a string and design
  iteration (trying `grainGradient`, `waves`, …) is a one-line palette
  change, never a re-vendoring. Shader sources are small strings — the
  whole catalog fits in a 142 KB bundle against the issue's 500 KB cap.

The wrapper adds only what Rust needs: `createShader(parent, opts) →
handle | null`, `setUniforms` / `setSpeed` / `dispose`, hex→vec4 color
conversion wrapper-side, and canvas sizing uniforms the vanilla (non-
React) path must supply itself.

### Palettes live in Rust, mirrored from the stylesheet

The obvious alternative — read the live theme via `getComputedStyle`
so the mesh always matches the tokens — is rejected twice over:

- the Mira Blue tokens are `oklch()` / `color-mix()` expressions, which
  the vendored `getShaderColorFromString` cannot parse;
- resolve-at-mount plumbing would freeze the first theme anyway, and a
  reactive re-resolve pipeline for a decorative layer is machinery out
  of proportion to its value.

So `atmosphere::palette` owns per-theme `[&str; 5]` hex stops. The two
`--accent` literals are the only literal-hex blue tokens in
`fleet-ui.css`, and they are **machine-pinned**: `tests/
atmosphere_palette_parity.rs` extracts each theme's `--accent` from the
stylesheet and asserts the palette carries it. The surrounding base and
wash stops are hand-approximated from the oklch bg/panel ramp —
documented, deliberately not machine-pinned (an oklch→sRGB conversion
dependency for a decorative approximation is not worth its weight). The
accepted consequence: a future retheme that moves `--bg` without
touching `--accent` shifts the mesh's match quietly; the mitigation is
the single knobs site, where re-tuning is a one-file edit.

### Degradation is silent, and a lost context is permanent

The `.atmosphere` CSS layer (`position: fixed; inset: 0; z-index: -1;
pointer-events: none`) paints `background: var(--bg)` — the
theme-reactive floor that shows before the shader mounts and whenever
it can't. The contract, each leg guarded:

- **WebGL2 unavailable** → `createShader` returns `null` (removing the
  stray canvas ShaderMount prepends before probing); the Rust side
  latches `failed` so theme flips never retry a dead capability. The
  floor is all that paints. No console errors, no user-visible sign.
- **`prefers-reduced-motion: reduce`** → speed 0, honoured live via a
  media-query signal; the package stops its rAF loop entirely at
  speed 0, so the static frame costs nothing per frame.
- **Context loss** → the wrapper hides the dead canvas so the CSS floor
  shows through, and that state is **permanent by design**: there is no
  `webglcontextrestored` remount. For a decorative backdrop, recovery
  machinery outweighs the pixels; this is a recorded consequence, not a
  bug to file.
- **Hidden tab or off-screen canvas** → the vendored `ShaderMount`
  drops its effective speed to 0 while `document.hidden` is true or its
  `IntersectionObserver` reports the canvas out of the viewport, which
  stops the rAF loop; it resumes at the set speed when either clears.
  This is the package's behaviour, not the wrapper's, and the wrapper
  adds no second pause.
- **Software-rasterized WebGL2** renders slowly rather than degrading —
  accepted for a login-page layer. Measured under headless Chromium's
  SwiftShader at 1440×900 (#241): the live shader holds the page's rAF
  rate near 34 fps against 60 at speed 0, and a screenshot takes about
  0.2–1.1 s. A 30 s screenshot timeout reported by the 0.9.0 smoke test
  did not reproduce on the bundled or system Chromium, the persistent
  profile it used, 2× device scale, full-page capture, or the deployed
  instance, so no throttle, settle, or renderer probe was added.

Theme flips re-color the mounted mesh in place via `setUniforms`
(never a remount), which is what makes the backdrop feel continuous
across the toggle.

### The bundle rides along, behind a default-off feature

`module = "/vendor/paper-shaders.js"` is **path-shaped**, and
wasm-bindgen reads a leading `/`, `./` or `../` as a **local JS
snippet**: the file is resolved at **compile time** against the crate
root (`crates/fleet-ui/vendor/paper-shaders.js`), inlined into the wasm
custom section, and re-emitted under `dist/snippets/fleet-ui-<hash>/`,
which the generated shim imports by relative path. Verified on the
workbench build — `dist/shell_demo-<hash>.js` line 1 is `import {
createShader } from './snippets/fleet-ui-<hash>/vendor/paper-shaders.js'`.

So `Atmosphere` costs a consumer no *build wiring*: **no `copy-file`
directive, no dist-root URL, no cross-repo path bookkeeping** — unlike
`fleet-ui.css`, which really is a runtime asset Trunk must copy. A
missing or renamed bundle is a **build failure**, not a silent runtime
404.

But "rides along" cuts both ways, and this is the sharp edge of a
compile-time snippet: emission keys off **linking** the extern block,
not off calling it. An unconditional `extern` in fleet-ui therefore
plants all 142 KB in the dist of *every* consumer of the crate and gets
it `modulepreload`ed from their `index.html`, even one whose generated
glue contains zero references to it. Measured on trawl-web-ui, which
does not mount the backdrop: `snippets/fleet-ui-<hash>/vendor/
paper-shaders.js` (144,920 bytes) present, preload emitted, `grep -c
paper-shaders` on the glue = 0. That is a pure wire-size regression on
the SPA whose budgets `cargo xtask compress-web` enforces one step
later in the same CI job.

So the extern block and the component over it sit behind a **default-off
`atmosphere` cargo feature**. Only a consumer that mounts the backdrop
enables it — `features = ["atmosphere"]` on the dep, or
`data-cargo-features="atmosphere"` on Trunk's `rel="rust"` link, which
is how the workbench opts in. `atmosphere::palette` stays unconditional:
pure native data with no snippet attached, and the parity tests name it
under default features. A cargo feature rather than a `cfg` because the
decision belongs to the *consumer*, not to fleet-ui's build.

Both legs are gated in CI. `wasm-check` compiles fleet-ui for wasm32
twice, feature off and on, so the non-mounting shape can't rot. The
`trunk-build` job asserts the emitted
`dist/snippets/*/vendor/paper-shaders.js` on the fleet-ui workbench
(the only wasm-target build of this code, and where the path must hold)
and asserts its **absence** from the trawl-web-ui dist — feature
unification is silent, so the negative needs a gate of its own. That
step first purges `target/wasm-bindgen/release/snippets`: wasm-bindgen
appends to its output dir and never prunes it, so an orphan from an
earlier build reads as a leak. `tests/atmosphere_vendor_contract.rs`
carries the path↔filename agreement onto native builds, where the
wasm32 compiler never looks.

The workbench (`src/bin/shell_demo.rs`, served by `trunk serve`) is
also the evidence venue: the xtask design-cards pages are static HTML
and structurally cannot host WebGL, so ACs about mount/unmount, live
re-color, reduced motion, and degradation are observed on the demo's
`/login` route.

### `.login-shell` loses its background

`.login-shell` declared `background: var(--bg)` on a normal-flow opaque
block — which paints *above* the `z-index: -1` canvas and fully
occluded the backdrop on exactly the page it targets. The declaration
is removed. Today this is zero visual delta: `body` declares
`background: var(--bg)` and `html` declares no background, so the
body's background propagates to the viewport canvas and paints the same
pixels. Both invariants are pinned (`css_chrome_parity`:
`.login-shell` declares no background; `atmosphere_palette_parity`: no
`html`-selector background rule). coastwatch consumes `fleet-ui.css`
wholesale and absorbs this change on its next `TRAWL_REV` bump — which
is precisely the change its login page wants.

### Attribution rides in the bundle

`@paper-design/shaders` is Apache-2.0. `vendor/LICENSE` and
`vendor/NOTICE` cover the repo checkout, but the minified bundle is the
only vendored byte a browser ever receives, so `build.sh` stamps an
esbuild `--banner:js` comment — `/*! @paper-design/shaders v0.0.79 |
Apache-2.0 | see crates/fleet-ui/vendor/NOTICE */` — the `/*!` form
minifiers preserve. The version is read from the `package.json` pin at
build time, and the contract test asserts banner == pin.

## Consequences

- Design iteration is a `palette.rs` edit (stops, shader name, speed,
  texture knobs) — no JS, no re-vendoring, no consumer changes. The
  shipped look is an explicit placeholder.
- The palette mirror drifts by construction outside the two pinned
  anchors; a retheme owes the mesh a manual glance.
- Consumers owe no build wiring for the JS — the snippet travels with
  the crate — but they do owe one feature flag, because linking the
  extern block is what emits the bundle. The cost of the gate is that
  `Atmosphere` is not reachable from a plain `fleet-ui` dep and
  forgetting the flag is a *compile* error (a missing name), which is
  the failure direction worth having.
- A lost WebGL context downgrades to the static floor for the session.
  Permanent, silent, by design.
- trawl-web-ui mounting and coastwatch wiring are deliberately out of
  scope here (coastwatch companion PR on the next `TRAWL_REV` bump).
