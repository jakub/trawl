#!/usr/bin/env bash
#
# Build the vendored @paper-design/shaders bundle.
#
# Run from the vendor/ directory. Output goes to the same directory,
# alongside the TypeScript source, and is committed to the repo so no
# npm/esbuild toolchain is required to build fleet-ui or any of its
# consumers. CI re-runs this script and diffs the output to catch
# drift. Same house pattern as crates/trawl-web-ui/vendor/build.sh.
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

echo "[vendor] bundling paper-shaders.js"
# Apache-2.0 attribution must ride IN the artifact browsers receive:
# LICENSE/NOTICE never leave the repo checkout, and only a `/*!` banner
# survives minification. The version is read from the package.json pin
# so the banner can never drift from it (asserted by
# tests/atmosphere_vendor_contract.rs).
SHADERS_VERSION="$(node -p "require('./package.json').dependencies['@paper-design/shaders']")"
npx esbuild src/paper-shaders.ts \
  --bundle \
  --format=esm \
  --minify \
  --target=es2022 \
  --banner:js="/*! @paper-design/shaders v${SHADERS_VERSION} | Apache-2.0 | see crates/fleet-ui/vendor/NOTICE */" \
  --outfile=paper-shaders.js \
  --log-level=warning

echo "[vendor] ✓ bundle built:"
ls -lh paper-shaders.js
