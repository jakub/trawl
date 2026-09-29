#!/usr/bin/env bash
# Build the image this proof installed, from commit SHA of the repository
# at REPO, into the scratch directory ~/trawl-270-kind-final-scratch.
#
#   build-image.sh REPO SHA
#
# It follows the image path of linux-distribution.yml with one difference:
# the release builds x86_64 with cargo-zigbuild for a glibc 2.31 floor. This
# host has no cargo-zigbuild, so the binaries build natively inside the
# release toolchain image, rust:1.98.0-bookworm, whose glibc is the image's.
# The SPA builds on the host with trunk, as in the workflow.
#
# The DuckDB archive must already sit in $S/runtime (distribution.py checks
# its sha256 against duckdb-runtime.json before it extracts anything); curl
# from this host's build environment fails on certificate verification.
set -euo pipefail
REPO=$1
SHA=$2
S=$HOME/trawl-270-kind-final-scratch
IMG=trawl-270-kind-final:${SHA:0:12}

say() { printf '\n## %s\n' "$*"; }
say "export $SHA"
rm -rf "$S/src"
mkdir -p "$S/src"
git -C "$REPO" archive --format=tar "$SHA" | tar -x -C "$S/src"
echo "$SHA" > "$S/source-sha"

say "SPA"
(cd "$S/src/crates/trawl-web-ui" && env -u NO_COLOR CARGO_TARGET_DIR="$S/host-target" trunk build --release 2>&1 | tail -n 3)
(cd "$S/src" && CARGO_TARGET_DIR="$S/host-target" cargo xtask compress-web)

say "DuckDB runtime"
python3 "$S/src/scripts/release/distribution.py" prepare --source "$S/src" \
  --target x86_64-unknown-linux-gnu --output "$S/runtime"
cat "$S/runtime/runtime.json"

say "binaries (rust:1.98.0-bookworm)"
mkdir -p "$S/builder" "$S/cargo-home" "$S/target" "$S/tmp"
cat > "$S/builder/Dockerfile" <<'EOF'
FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
RUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates curl && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_TOOLCHAIN=1.98.0
EOF
docker build -q -t trawl-270-final-builder:scratch "$S/builder"
# The same package set and flags as build-distribution.sh --image-only.
docker run --rm --init --user "$(id -u):$(id -g)" --workdir "$S/src" \
  --env "HOME=$S/cargo-home" --env "CARGO_HOME=$S/cargo-home" \
  --env "CARGO_TARGET_DIR=$S/target" --env "TMPDIR=$S/tmp" \
  --env "DUCKDB_LIB_DIR=$S/runtime" \
  --env "RUSTFLAGS=-C link-arg=-Wl,-rpath,\$ORIGIN/../lib/trawl" \
  --mount "type=bind,source=$S,target=$S" \
  trawl-270-final-builder:scratch \
  cargo build --locked --release --target x86_64-unknown-linux-gnu --no-default-features \
    -p trawl-server -p trawl-admin -p fleet-admin -p trawl-web --bins 2>&1 | tail -n 3

say "stage and build $IMG"
rm -rf "$S/ctx"
mkdir -p "$S/ctx/docker-ctx"
python3 "$S/src/scripts/release/distribution.py" stage --source "$S/src" \
  --binaries "$S/target/x86_64-unknown-linux-gnu/release" --runtime "$S/runtime" \
  --output "$S/ctx/docker-ctx/amd64" --target x86_64-unknown-linux-gnu \
  --source-sha "$SHA" --tooling-sha "$SHA" --image-only
cp "$S/src/Dockerfile" "$S/ctx/"
docker build -q --platform linux/amd64 --label "org.opencontainers.image.revision=$SHA" \
  --tag "$IMG" "$S/ctx"
docker image inspect --format '{{.Id}} {{.Os}}/{{.Architecture}}' "$IMG"
