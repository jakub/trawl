#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Evidence tooling for issue #345: run the overlap test at one commit.
#
# Method:
#   1. Export the commit's tree through git's object reads (ls-tree and
#      cat-file), so the run sees the commit and nothing uncommitted.
#   2. Build the trawl-server `repin` test binary there, outside the
#      recorded run.
#   3. Run cleanup_excludes_next_repin_until_owner_release once, with no
#      retries, against the PostgreSQL cluster DATABASE_URL names.
#   4. Print the transcript to stdout: the script's digest, the commit,
#      rustc, the start and end times, the exit status, and the test's
#      output, with the export and home paths replaced by placeholders.
#
# Usage:
#   DATABASE_URL=postgres://... scripts/evidence/issue-345/premise.sh [SHA] \
#     > docs/evidence/issue-345/premise.txt
#     SHA defaults to HEAD. DATABASE_URL names a disposable cluster whose
#     role can create databases; the transcript never prints it.
#
# Environment:
#   PREMISE_WORK_DIR  where the tree is exported (default:
#                     <repo>/.flow-scratch/issue-345-premise). Must not be
#                     on tmpfs.
#   CARGO_TARGET_DIR  the test build (default: <work dir>/target).
#
# Exit status: the test run's, or 2 when the commit has no such test.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"

REV="${1:-HEAD}"
SHA="$(git -C "$ROOT_DIR" rev-parse --verify "$REV^{commit}")"
WORK_DIR="${PREMISE_WORK_DIR:-$ROOT_DIR/.flow-scratch/issue-345-premise}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WORK_DIR/target}"
SUITE=crates/trawl-server/tests/repin.rs
TEST=cleanup_excludes_next_repin_until_owner_release

if [[ -z "${DATABASE_URL:-}" ]]; then
  echo "premise: set DATABASE_URL to a disposable PostgreSQL cluster" >&2
  exit 1
fi
mkdir -p "$WORK_DIR" "$CARGO_TARGET_DIR"
for dir in "$WORK_DIR" "$CARGO_TARGET_DIR"; do
  if [[ "$(stat -f -c %T "$dir")" == tmpfs ]]; then
    echo "premise: $dir is on tmpfs; a test build does not fit there" >&2
    exit 1
  fi
done
suite="$(git -C "$ROOT_DIR" cat-file -p "$SHA:$SUITE" 2>/dev/null || true)"
if ! grep -q "async fn $TEST()" <<<"$suite"; then
  echo "premise: $SHA has no $TEST in $SUITE" >&2
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

# Replace the paths that name this machine with placeholders.
relativize() {
  sed -e "s#$TREE#<tree>#g" -e "s#$ROOT_DIR#<repo>#g" -e "s#$HOME#<home>#g"
}

run() {
  cargo nextest run --locked --cargo-quiet -p trawl-server --test repin \
    -E "test($TEST)" --retries 0 "$@"
}

echo "premise: building the repin suite at ${SHA:0:12}" >&2
if ! (cd "$TREE" && run --no-run) >"$WORK_DIR/build-${SHA:0:12}.log" 2>&1; then
  echo "premise: the repin suite did not build; see $WORK_DIR/build-${SHA:0:12}.log" >&2
  exit 1
fi

LOG="$WORK_DIR/run-${SHA:0:12}.log"
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
set +e
(cd "$TREE" && run) >"$LOG" 2>&1
status=$?
set -e
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "# issue #345: the overlap test at one commit"
echo "script:        scripts/evidence/issue-345/premise.sh"
echo "script sha256: $(sha256sum "$SCRIPT_DIR/premise.sh" | cut -d' ' -f1)"
echo "commit:        $SHA ($(git -C "$ROOT_DIR" log -1 --format=%s "$SHA"))"
echo "tree:          the commit's files, exported from git's objects; nothing uncommitted"
echo "command:       cargo nextest run --locked --cargo-quiet -p trawl-server --test repin -E 'test($TEST)' --retries 0"
echo "database:      DATABASE_URL from the environment (not printed); a disposable PostgreSQL cluster"
echo "rustc:         $(cd "$TREE" && rustc --version)"
echo "kernel:        $(uname -sr)"
echo "started:       $started"
echo "finished:      $finished"
echo "exit:          $status"
echo
relativize <"$LOG"
exit "$status"
