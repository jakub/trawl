#!/usr/bin/env bash
# SOURCE RUNTIME [--cli-only|--image-only]; outputs use SOURCE/target/bookworm.
set -euo pipefail
source_dir="$(cd "$1" && pwd)"
runtime="$(mkdir -p "$2" && cd "$2" && pwd)"
tooling="$(cd "$(dirname "$0")" && pwd)"
build_home="$source_dir/target/bookworm-home"
target_dir="$source_dir/target/bookworm"
mkdir -p "$build_home/.cargo" "$target_dir"
state="$(mktemp -d "$target_dir/container.XXXXXX")"
trap 'rm -rf "$state"' EXIT

# Build tools come from the pinned image. Do not mount a host Cargo home, which
# can contain credentials or host executables requiring a newer glibc.
docker build --file "$tooling/Dockerfile.build-arm64" --iidfile "$state/image" "$tooling"
image="$(cat "$state/image")"
container=(docker run --rm --user "$(id -u):$(id -g)"
  --workdir "$source_dir"
  --env "HOME=$build_home" --env "CARGO_HOME=$build_home/.cargo"
  --env "CARGO_TARGET_DIR=$target_dir" --env "TMPDIR=$state"
  --env CARGO_TERM_COLOR --env RUSTFLAGS --env CARGO_PROFILE_RELEASE_STRIP
  --mount "type=bind,source=$source_dir,target=$source_dir"
  --mount "type=bind,source=$tooling,target=$tooling,readonly"
  --mount "type=bind,source=$runtime,target=$runtime")

# A linked worktree's .git file points outside its checkout. Preserve that
# identity without granting the build write access to the common Git metadata.
common_git="$(git -C "$source_dir" rev-parse --path-format=absolute --git-common-dir)"
container+=(--mount "type=bind,source=$common_git,target=$common_git,readonly")

"${container[@]}" "$image" bash -c '
  set -euo pipefail
  python3 "$1/check-arm64-linker.py" "$CARGO_TARGET_DIR"
  exec bash "$1/build-distribution.sh" "$2" aarch64-unknown-linux-gnu "$3" "$4"
' build-arm64 "$tooling" "$source_dir" "$runtime" "${3:-}"
