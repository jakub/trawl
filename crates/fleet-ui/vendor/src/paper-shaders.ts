// @paper-design/shaders wrapper consumed by fleet-ui via wasm-bindgen.
//
// Kept minimal on purpose: the package's ShaderMount does all the real
// work (rAF loop, resize/intersection observers, uniform plumbing).
// This wrapper adds exactly what the Rust side needs and nothing more:
//
//   - a handle-based API à la trawl-web-ui's uplot.ts wrapper —
//     createShader(parent, opts) -> ShaderHandle | null with
//     setUniforms / setSpeed / dispose;
//   - the full fragment-shader catalog resolvable BY NAME, so the Rust
//     side passes "meshGradient" as a string and design iteration never
//     requires re-vendoring (shader sources are small strings — the
//     whole catalog fits well inside the 500 KB bundle budget);
//   - hex -> vec4 color conversion (getShaderColorFromString) done
//     wrapper-side, so Rust palettes stay plain `#rrggbb` literals;
//   - graceful degradation: WebGL unavailable => null (callers keep the
//     CSS var(--bg) floor), context lost => canvas hidden so the same
//     floor shows through. No user-visible errors either way.

import {
  ShaderMount,
  getShaderColorFromString,
  ShaderFitOptions,
  type ShaderMountUniforms,
  colorPanelsFragmentShader,
  ditheringFragmentShader,
  dotGridFragmentShader,
  dotOrbitFragmentShader,
  flutedGlassFragmentShader,
  godRaysFragmentShader,
  grainGradientFragmentShader,
  heatmapFragmentShader,
  imageDitheringFragmentShader,
  lensDistortionFragmentShader,
  liquidMetalFragmentShader,
  meshGradientFragmentShader,
  metaballsFragmentShader,
  neuroNoiseFragmentShader,
  paperTextureFragmentShader,
  perlinNoiseFragmentShader,
  pulsingBorderFragmentShader,
  simplexNoiseFragmentShader,
  smokeRingFragmentShader,
  spiralFragmentShader,
  staticMeshGradientFragmentShader,
  staticRadialGradientFragmentShader,
  swirlFragmentShader,
  voronoiFragmentShader,
  warpFragmentShader,
  waterFragmentShader,
  wavesFragmentShader,
} from "@paper-design/shaders";

/** Fragment-shader catalog, keyed by the name the Rust side passes. */
export const shaderCatalog: Record<string, string> = {
  colorPanels: colorPanelsFragmentShader,
  dithering: ditheringFragmentShader,
  dotGrid: dotGridFragmentShader,
  dotOrbit: dotOrbitFragmentShader,
  flutedGlass: flutedGlassFragmentShader,
  godRays: godRaysFragmentShader,
  grainGradient: grainGradientFragmentShader,
  heatmap: heatmapFragmentShader,
  imageDithering: imageDitheringFragmentShader,
  lensDistortion: lensDistortionFragmentShader,
  liquidMetal: liquidMetalFragmentShader,
  meshGradient: meshGradientFragmentShader,
  metaballs: metaballsFragmentShader,
  neuroNoise: neuroNoiseFragmentShader,
  paperTexture: paperTextureFragmentShader,
  perlinNoise: perlinNoiseFragmentShader,
  pulsingBorder: pulsingBorderFragmentShader,
  simplexNoise: simplexNoiseFragmentShader,
  smokeRing: smokeRingFragmentShader,
  spiral: spiralFragmentShader,
  staticMeshGradient: staticMeshGradientFragmentShader,
  staticRadialGradient: staticRadialGradientFragmentShader,
  swirl: swirlFragmentShader,
  voronoi: voronoiFragmentShader,
  warp: warpFragmentShader,
  water: waterFragmentShader,
  waves: wavesFragmentShader,
};

export interface ShaderOpts {
  /** Catalog name ("meshGradient", ...) or raw fragment shader source. */
  shader: string;
  /**
   * CSS color strings ("#2a5c8a", ...) converted wrapper-side into the
   * `u_colors` vec4 array + `u_colorsCount` the multi-color shaders
   * read. Optional for single/no-color shaders.
   */
  colors?: string[];
  /** Extra shader-specific uniforms (u_distortion, u_swirl, ...). */
  uniforms?: ShaderMountUniforms;
  /** Animation speed; 0 stops the rAF loop entirely. Defaults to 0. */
  speed?: number;
}

export interface UniformUpdate {
  colors?: string[];
  uniforms?: ShaderMountUniforms;
}

export interface ShaderHandle {
  /** Push a partial uniform update into the mounted shader in place. */
  setUniforms: (update: UniformUpdate) => void;
  /** Set the animation speed; 0 stops the rAF loop entirely. */
  setSpeed: (speed: number) => void;
  /** Tear down the mount and remove its canvas from the DOM. */
  dispose: () => void;
}

/**
 * Sizing uniforms the package's vertex shader reads. GLSL uniforms
 * default to 0.0, and `u_scale: 0` renders a degenerate frame — the
 * react wrapper normally fills these from its sizing props, so the
 * vanilla path has to provide them itself. "cover" + scale 1 is the
 * full-bleed backdrop treatment.
 */
const sizingUniforms: ShaderMountUniforms = {
  u_fit: ShaderFitOptions.cover,
  u_scale: 1,
  u_rotation: 0,
  u_offsetX: 0,
  u_offsetY: 0,
  u_originX: 0.5,
  u_originY: 0.5,
  u_worldWidth: 0,
  u_worldHeight: 0,
};

function colorUniforms(colors: string[] | undefined): ShaderMountUniforms {
  if (colors === undefined) {
    return {};
  }
  return {
    u_colors: colors.map((c) => [...getShaderColorFromString(c)]),
    u_colorsCount: colors.length,
  };
}

/**
 * Mount a shader into `parent` and return a live handle, or `null` when
 * construction fails. Callers treat null as "keep the CSS fallback"
 * and never retry.
 */
export function createShader(
  parent: HTMLElement,
  opts: ShaderOpts
): ShaderHandle | null {
  const fragment = shaderCatalog[opts.shader] ?? opts.shader;
  const uniforms: ShaderMountUniforms = {
    ...sizingUniforms,
    ...colorUniforms(opts.colors),
    ...opts.uniforms,
  };

  let mount: ShaderMount;
  const reportError = console.error;
  // Upstream reports these two expected failures before returning a null
  // program. Suppress only those diagnostics during synchronous construction.
  console.error = (...args: unknown[]): void => {
    if (
      typeof args[0] === "string" &&
      (args[0].startsWith("An error occurred compiling the shaders: ") ||
        args[0].startsWith("Unable to initialize the shader program: "))
    ) {
      return;
    }
    reportError.apply(console, args);
  };
  try {
    mount = new ShaderMount(
      parent,
      fragment,
      uniforms,
      undefined,
      opts.speed ?? 0,
      0,
      // A blurred full-bleed gradient has nothing to antialias: render
      // at 1x and cap ~2 Mpx (vs the package's 2x / 8.3 Mpx defaults)
      // so a 4K login page doesn't pay retina shader cost.
      1,
      2_000_000
    );
  } catch {
    // build.mjs makes the constructor dispose its partial instance before
    // rethrowing. No handle escapes, and the caller keeps the CSS floor.
    return null;
  } finally {
    console.error = reportError;
  }

  let state: "live" | "dead" | "disposed" = "live";
  // Loss is permanent. Mark the handle dead before stopping animation so
  // later theme or reduced-motion effects cannot restart the mount.
  const onContextLost = (): void => {
    if (state !== "live") return;
    state = "dead";
    try {
      mount.setSpeed(0);
    } finally {
      mount.canvasElement.style.display = "none";
    }
  };
  mount.canvasElement.addEventListener("webglcontextlost", onContextLost);

  return {
    setUniforms(update: UniformUpdate): void {
      if (state !== "live") return;
      mount.setUniforms({
        ...colorUniforms(update.colors),
        ...update.uniforms,
      });
    },
    setSpeed(speed: number): void {
      if (state !== "live") return;
      mount.setSpeed(speed);
    },
    dispose(): void {
      if (state === "disposed") return;
      state = "disposed";
      mount.canvasElement.removeEventListener(
        "webglcontextlost",
        onContextLost
      );
      // Upstream removes the canvas and its JS property. The DOM attribute
      // also enables its shared stylesheet, so remove that marker ourselves.
      try {
        mount.dispose();
      } finally {
        parent.removeAttribute("data-paper-shader");
      }
    },
  };
}
