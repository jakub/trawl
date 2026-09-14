#!/usr/bin/env bash
# SOURCE TARGET RUNTIME [--cli-only|--image-only]; run from a disposable build checkout.
set -euo pipefail
source_dir="$(cd "$1" && pwd)"
target="$2"
runtime="$(mkdir -p "$3" && cd "$3" && pwd)"
tooling="$(cd "$(dirname "$0")" && pwd)"
python3 "$tooling/distribution.py" prepare --source "$source_dir" --target "$target" --output "$runtime"
export DUCKDB_LIB_DIR="$runtime"
unset DUCKDB_DOWNLOAD_LIB DUCKDB_STATIC
case "$target" in
  *-apple-darwin) export MACOSX_DEPLOYMENT_TARGET=15.0; loader='@executable_path/../lib/trawl'; command=(cargo build); build_target="$target" ;;
  *-unknown-linux-gnu) loader='$ORIGIN/../lib/trawl'; command=(cargo zigbuild); build_target="$target.2.31" ;;
  *) echo 'unsupported distribution target' >&2; exit 1 ;;
esac
# Preserve release build-id flags, but keep the literal loader token intact.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-rpath,$loader"
cd "$source_dir"
case "${4:-}" in
  --cli-only)
    "${command[@]}" --locked --release --target "$build_target" --no-default-features --features trawl-cli/clipboard -p trawl-cli --bin trawl ;;
  --image-only)
    "${command[@]}" --locked --release --target "$build_target" --no-default-features -p trawl-server -p trawl-admin -p fleet-admin -p trawl-web --bins ;;
  "")
    "${command[@]}" --locked --release --target "$build_target" --no-default-features --features trawl-cli/clipboard --workspace --exclude trawl-web-ui ;;
  *) echo 'unsupported distribution build option' >&2; exit 1 ;;
esac
