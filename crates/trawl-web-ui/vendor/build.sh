#!/usr/bin/env bash
#
# Build the vendored CodeMirror + uPlot bundles.
#
# Run from the vendor/ directory. Outputs go to the same directory,
# alongside the TypeScript sources, and are committed to the repo so no
# npm/esbuild toolchain is required to build trawl-web-ui itself. CI
# re-runs this script and diffs the output to catch drift.
#
# Prerequisites: node 20+ (project uses 25.x locally) and npm.

set -euo pipefail

cd "$(dirname "$0")"

echo "[vendor] installing deps ($(npm --version))"
if [ -f package-lock.json ]; then
    npm ci --no-audit --no-fund
else
    # First-run bootstrap; generates package-lock.json which MUST be committed.
    npm install --no-audit --no-fund
fi

echo "[vendor] bundling codemirror.js"
npx esbuild src/codemirror.ts \
  --bundle \
  --format=esm \
  --minify \
  --target=es2022 \
  --outfile=codemirror.js \
  --log-level=warning

echo "[vendor] bundling uplot.js"
npx esbuild src/uplot.ts \
  --bundle \
  --format=esm \
  --minify \
  --target=es2022 \
  --outfile=uplot.js \
  --log-level=warning

# uPlot ships a CSS file that has to be loaded for the chart to render.
# Copy it into place so Trunk can pick it up via a copy-file directive.
cp node_modules/uplot/dist/uPlot.min.css uplot.css

echo "[vendor] ✓ bundles built:"
ls -lh codemirror.js uplot.js uplot.css
