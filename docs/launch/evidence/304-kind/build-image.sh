#!/usr/bin/env bash
# Build the trawl server image for the #304 kind reproduction from commit SHA
# of the repository at REPO, and load it into the kind cluster.
#
#   build-image.sh REPO SHA LABEL      # LABEL: before | after
#
# Prints the image ref (trawl-i304:LABEL-<sha12>) on the last line.
#
# It follows the image path of linux-distribution.yml with one difference:
# the release builds x86_64 with cargo-zigbuild for a glibc 2.31 floor. This
# host has no cargo-zigbuild, so the binaries build natively inside the
# release toolchain image, rust:1.98.0-bookworm, whose glibc is the image's
# (the same substitution as docs/launch/evidence/2026-09-28-issue-270-kind-final).
# The SPA builds on the host with trunk, as in the workflow. Everything lands
# under build/ beside this script; the cargo home and target are shared by the
# before and after builds.
set -euo pipefail
REPO=$1
SHA=$(git -C "$REPO" rev-parse --verify "$2^{commit}")
LABEL=$3
HERE="$(cd "$(dirname "$0")" && pwd)"
S=$HERE/build
SRC=$S/src-$LABEL
IMG=trawl-i304:$LABEL-${SHA:0:12}
CLUSTER=trawl-i304

say() { printf '\n## %s\n' "$*"; }
say "export $SHA to $SRC"
rm -rf "$SRC"
mkdir -p "$SRC"
git -C "$REPO" archive --format=tar "$SHA" | tar -x -C "$SRC"

# curl on this host fails github's certificate verification, so seed the
# pinned archive from a local copy when DUCKDB_ZIP names one. distribution.py
# checks its sha256 against duckdb-runtime.json before extracting anything.
if [[ -n ${DUCKDB_ZIP:-} ]]; then
  sum=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["archives"]["x86_64-unknown-linux-gnu"][1])' "$SRC/scripts/release/duckdb-runtime.json")
  for d in "$S/host-target/duckdb-runtime-cache/x86_64-unknown-linux-gnu/$sum" "$S/runtime-$LABEL"; do
    mkdir -p "$d"; cp "$DUCKDB_ZIP" "$d/libduckdb-linux-amd64.zip"
  done
fi

say "SPA"
(cd "$SRC/crates/trawl-web-ui" && env -u NO_COLOR CARGO_TARGET_DIR="$S/host-target" trunk build --release 2>&1 | tail -n 3)
(cd "$SRC" && CARGO_TARGET_DIR="$S/host-target" cargo xtask compress-web 2>&1 | tail -n 3)

say "DuckDB runtime"
python3 "$SRC/scripts/release/distribution.py" prepare --source "$SRC" \
  --target x86_64-unknown-linux-gnu --output "$S/runtime-$LABEL"

say "binaries (rust:1.98.0-bookworm)"
mkdir -p "$S/builder" "$S/cargo-home" "$S/target" "$S/tmp"
cat > "$S/builder/Dockerfile" <<'EOF'
FROM rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
RUN apt-get update && apt-get install -y --no-install-recommends python3 ca-certificates curl && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_TOOLCHAIN=1.98.0
EOF
docker build -q -t trawl-i304-builder:scratch "$S/builder"
# The same package set and flags as build-distribution.sh --image-only.
docker run --rm --init --user "$(id -u):$(id -g)" --workdir "$SRC" \
  --env "HOME=$S/cargo-home" --env "CARGO_HOME=$S/cargo-home" \
  --env "CARGO_TARGET_DIR=$S/target" --env "TMPDIR=$S/tmp" \
  --env "DUCKDB_LIB_DIR=$S/runtime-$LABEL" \
  --env "RUSTFLAGS=-C link-arg=-Wl,-rpath,\$ORIGIN/../lib/trawl" \
  --mount "type=bind,source=$S,target=$S" \
  trawl-i304-builder:scratch \
  cargo build --locked --release --target x86_64-unknown-linux-gnu --no-default-features \
    -p trawl-server -p trawl-admin -p fleet-admin -p trawl-web --bins 2>&1 | tail -n 5

say "stage and build $IMG"
rm -rf "$S/ctx-$LABEL"
mkdir -p "$S/ctx-$LABEL/docker-ctx"
python3 "$SRC/scripts/release/distribution.py" stage --source "$SRC" \
  --binaries "$S/target/x86_64-unknown-linux-gnu/release" --runtime "$S/runtime-$LABEL" \
  --output "$S/ctx-$LABEL/docker-ctx/amd64" --target x86_64-unknown-linux-gnu \
  --source-sha "$SHA" --tooling-sha "$SHA" --image-only
cp "$SRC/Dockerfile" "$S/ctx-$LABEL/"
docker build -q --platform linux/amd64 --label "org.opencontainers.image.revision=$SHA" \
  --tag "$IMG" "$S/ctx-$LABEL"
docker image inspect --format '{{.Id}} {{.Os}}/{{.Architecture}}' "$IMG"

say "load into kind cluster $CLUSTER"
kind load docker-image --name "$CLUSTER" "$IMG"
echo "$IMG"
