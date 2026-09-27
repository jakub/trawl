// The `navigator` uPlot sees, injected by build.sh (`--inject`).
//
// uPlot 1.6.32 reads `navigator` for one thing only. While its module
// loads, it runs `new Intl.NumberFormat(navigator.language)`, and it
// formats axis ticks and legend values with the result. A browser can
// report a language that is not a BCP 47 tag: Chromium reported
// `en-US@posix`, the POSIX locale spelling. Intl throws a RangeError on
// that tag, and the exception stopped the whole SPA, the sign-in form
// included, because the wasm glue imports this bundle at startup.
//
// This module passes a valid tag through unchanged. For an invalid tag
// it gives `undefined`, so Intl uses the browser's default locale. The
// chart tooltip's `toLocaleString()` in uplot.ts uses the same default.
// Check the uPlot source for other `navigator` reads before an upgrade:
// this object has no other members.
//
// It reads the real object as `globalThis.navigator`, because esbuild's
// inject replaces only the bare global identifier.

function formatterLanguage(tag: string): string | undefined {
  try {
    Intl.getCanonicalLocales(tag);
    return tag;
  } catch {
    return undefined;
  }
}

const shim = { language: formatterLanguage(globalThis.navigator.language) };

export { shim as navigator };
