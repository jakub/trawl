#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #336 (ADR-0063, plan ruling R14): calibrate
# the live driver's chunk size.
#
# Method:
#   1. Export the commit's tree through git's object reads (ls-tree and
#      cat-file), so the run sees the commit and nothing uncommitted.
#   2. Build crates/trawl-server/tests/live_chunk_calibration.rs in
#      release, outside the timed run.
#   3. Wait while the 1-minute load average is over the core count: up
#      to three waits of 60 s, then run whatever the load is.
#   4. Run the ignored harness. It drives the real live driver over an
#      in-process bus at 1, 4, 16, 64 and 256 events a job, over mostly
#      rejected events and over long subjects, and applies R14's rule. It
#      fails when the rule rejects the commit's LIVE_CHUNK_EVENTS.
#   5. Print the transcript to stdout: the host, the load average at the
#      start and end, rustc, the commit, and the harness's table.
#
# Usage:
#   scripts/evidence/issue-336/live-chunk.sh [SHA] > docs/evidence/issue-336/live-chunk.txt
#     SHA defaults to HEAD. The run takes about 4 minutes after the build.
#
# Environment:
#   LIVE_CHUNK_WORK_DIR  where the tree is exported (default:
#                        <repo>/.flow-scratch/issue-336-live-chunk). Must
#                        not be on tmpfs.
#   CARGO_TARGET_DIR     the release build (default: <work dir>/target).
#
# Exit status: the harness's, non-zero when the rule rejects the commit's
# LIVE_CHUNK_EVENTS, or 2 when the commit has no harness.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
WORK_DIR="${LIVE_CHUNK_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-336-live-chunk}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
HARNESS=crates/trawl-server/tests/live_chunk_calibration.rs

mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "live-chunk: $dir is on tmpfs; a release build does not fit there" >&2
    exit 1
  fi
done
if ! git -C "$ROOT_DIR" cat-file -e "$SHA:$HARNESS" 2>/dev/null; then
  echo "live-chunk: $SHA has no $HARNESS" >&2
  exit 2
fi

TREE="$WORK_DIR/tree-${SHA:0:12}"
rm -rf "$TREE"
mkdir -p "$TREE"
# The commit's files through git's object reads, each stamped with the
# commit's time, so a second export of one commit rebuilds nothing.
python3 -I - "$ROOT_DIR" "$SHA" "$TREE" <<'EXPORT'
import os, subprocess, sys

root, sha, tree = sys.argv[1:]
when = int(subprocess.run(["git", "-C", root, "log", "-1", "--format=%ct", sha],
                          capture_output=True, text=True, check=True).stdout)
listing = subprocess.run(["git", "-C", root, "ls-tree", "-r", "-z", sha],
                         capture_output=True, check=True).stdout.split(b"\0")
entries = []
for line in filter(None, listing):
    meta, path = line.split(b"\t", 1)
    mode, kind, oid = meta.split()
    if kind == b"blob":
        entries.append((mode, oid, os.fsdecode(path)))
batch = subprocess.Popen(["git", "-C", root, "cat-file", "--batch"],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE)
for mode, oid, path in entries:
    batch.stdin.write(oid + b"\n")
    batch.stdin.flush()
    header = batch.stdout.readline().split()
    data = batch.stdout.read(int(header[2]))
    batch.stdout.read(1)
    target = os.path.join(tree, path)
    os.makedirs(os.path.dirname(target), exist_ok=True)
    if mode == b"120000":
        os.symlink(os.fsdecode(data), target)
        continue
    with open(target, "wb") as out:
        out.write(data)
    os.chmod(target, 0o755 if mode == b"100755" else 0o644)
    os.utime(target, (when, when))
batch.stdin.close()
batch.wait()
EXPORT

# One constant's definition in the exported tree.
constant() {
  grep -m1 -E "^(pub )?const $2: " "$TREE/$1" | sed -E 's/^(pub )?const //; s/;$//'
}

echo "live-chunk: building the harness at ${SHA:0:12}" >&2
binary="$(
  cd "$TREE" &&
    cargo test --release --locked -p trawl-server --test live_chunk_calibration --no-run \
      --message-format=json 2>/dev/null |
    python3 -I -c '
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("executable") \
            and message["target"]["name"] == "live_chunk_calibration":
        print(message["executable"])
'
)"
if [[ -z "$binary" ]]; then
  echo "live-chunk: the harness did not build" >&2
  exit 1
fi

cores="$(nproc)"
waits=0
load_start="$(cut -d' ' -f1-3 /proc/loadavg)"
while awk -v now="${load_start%% *}" -v cores="$cores" 'BEGIN { exit !(now > cores) }' && ((waits < 3)); do
  echo "live-chunk: load $load_start over $cores cores, waiting 60 s" >&2
  sleep 60
  waits=$((waits + 1))
  load_start="$(cut -d' ' -f1-3 /proc/loadavg)"
done

REPORT="$WORK_DIR/report-${SHA:0:12}.txt"
rm -f "$REPORT"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
set +e
(cd "$TREE" && LIVE_CHUNK_REPORT="$REPORT" "$binary" --ignored --exact live_chunk_calibration \
  --nocapture --test-threads=1) >"$WORK_DIR/run-${SHA:0:12}.log" 2>&1
status=$?
set -e
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
load_end="$(cut -d' ' -f1-3 /proc/loadavg)"

echo "# issue #336: live chunk calibration (ADR-0063, plan ruling R14)"
echo "script:        scripts/evidence/issue-336/live-chunk.sh"
echo "script sha256: $(sha256sum "$SCRIPT_DIR/live-chunk.sh" | cut -d' ' -f1)"
echo "harness:       $HARNESS, #[ignore]d, built with cargo test --release --locked"
echo "commit:        $SHA ($(git -C "$ROOT_DIR" log -1 --format=%s "$SHA"))"
echo "tree:          the commit's files, exported from git's objects; nothing uncommitted"
echo "driver:        $(constant crates/trawl-core/src/stream.rs LIVE_CHUNK_EVENTS); $(constant crates/trawl-server/src/live.rs LIVE_CHUNK_STRING_BYTES); $(constant crates/trawl-server/src/live.rs LIVE_JOB_FRAME_BYTES)"
echo "rustc:         $(cd "$TREE" && rustc --version)"
echo "cpu:           $(sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1), $cores threads"
echo "kernel:        $(uname -sr)"
echo "started:       $started"
echo "finished:      $finished"
echo "load average:  $load_start at the start (1, 5 and 15 minutes), after $waits wait(s) of 60 s; $load_end at the end"
echo "               other build and test runs share this machine"
echo "exit:          $status"
echo
if [[ -s "$REPORT" ]]; then
  cat "$REPORT"
else
  echo "the harness wrote no report; its output:"
  cat "$WORK_DIR/run-${SHA:0:12}.log"
fi
exit "$status"
