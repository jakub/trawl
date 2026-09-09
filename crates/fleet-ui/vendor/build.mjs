// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import { readFile } from "node:fs/promises";
import { build } from "esbuild";

const { dependencies } = JSON.parse(await readFile("package.json", "utf8"));
const version = dependencies["@paper-design/shaders"];

// These edits apply only to the pinned upstream module. Fail the build if an
// upgrade changes any target, so constructor cleanup cannot silently fall off.
function replaceOnce(source, before, after) {
  const count = source.split(before).length - 1;
  if (count !== 1) {
    throw new Error(`Expected one shader-mount.js patch target, found ${count}: ${before}`);
  }
  return source.replace(before, () => after);
}

await build({
  entryPoints: ["src/paper-shaders.ts"],
  bundle: true,
  format: "esm",
  minify: true,
  target: "es2022",
  banner: {
    js: `/*! @paper-design/shaders v${version} | Apache-2.0 | see crates/fleet-ui/vendor/NOTICE */`,
  },
  outfile: "paper-shaders.js",
  logLevel: "warning",
  plugins: [{
    name: "transactional-shader-mount",
    setup(builder) {
      let patchedModules = 0;
      builder.onEnd(() => {
        if (patchedModules !== 1) {
          throw new Error(`Expected to patch one shader-mount.js module, found ${patchedModules}`);
        }
      });
      builder.onLoad({ filter: /\/node_modules\/@paper-design\/shaders\/dist\/shader-mount\.js$/ }, async ({ path }) => {
        patchedModules += 1;
        let source = await readFile(path, "utf8");
        const patch = (before, after) => {
          source = replaceOnce(source, before, after);
        };

        // A failed `new` never gives the wrapper its partial instance. Catch
        // inside the constructor, after its parent and document are known.
        patch(
          "    this.ownerDocument = parentElement.ownerDocument;",
          "    this.ownerDocument = parentElement.ownerDocument;\n    try {",
        );
        patch(
          '    this.ownerDocument.addEventListener("visibilitychange", this.handleDocumentVisibilityChange);\n  }',
          `    this.ownerDocument.addEventListener("visibilitychange", this.handleDocumentVisibilityChange);
    } catch (error) {
      try { this.dispose(); } catch {}
      try {
        this.gl?.getExtension("WEBGL_lose_context")?.loseContext();
      } catch {}
      this.parentElement.removeAttribute("data-paper-shader");
      throw error;
    }
  }`,
        );
        patch(
          "    if (!program) return;",
          '    if (!program) throw new Error("Paper Shaders: shader program initialization failed");',
        );

        // Keep upstream disposal as the only resource inventory. A failed GL
        // cleanup must not skip the observers, listeners, or partial canvas.
        patch(
          "  dispose = () => {\n    this.hasBeenDisposed = true;",
          `  dispose = () => {
    const cleanup = (release) => {
      try { release(); } catch {}
    };
    this.hasBeenDisposed = true;`,
        );
        patch(
          "    this.hasBeenDisposed = true;\n    if (this.rafId !== null) {\n      cancelAnimationFrame(this.rafId);",
          "    this.hasBeenDisposed = true;\n    if (this.rafId !== null) {\n      cleanup(() => cancelAnimationFrame(this.rafId));",
        );
        patch(
          "    if (this.gl && this.program) {",
          "    cleanup(() => {\n    if (this.gl && this.program) {",
        );
        patch(
          "      this.gl.getError();\n    }\n    if (this.resizeObserver) {",
          "      this.gl.getError();\n    }\n    });\n    if (this.resizeObserver) {",
        );
        for (const statement of [
          "this.resizeObserver.disconnect()",
          "this.intersectionObserver.disconnect()",
          'visualViewport?.removeEventListener("resize", this.handleVisualViewportChange)',
          'this.ownerDocument.removeEventListener("visibilitychange", this.handleDocumentVisibilityChange)',
        ]) {
          patch(`${statement};`, `cleanup(() => ${statement});`);
        }
        patch("    this.canvasElement.remove();", "    cleanup(() => this.canvasElement?.remove());");
        return { contents: source, loader: "js" };
      });
    },
  }],
});
