#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run the #235 scenarios in order and regenerate README.md.
#
#   CARGO_TARGET_DIR=... visual-evidence/issue-235/run-all.sh [first-scenario]
#
# Starts with setup unless a first scenario is given, stops expanding at the
# first 500 or content failure in a..f (issue #235 item 6c), and always tears
# the disposable infrastructure down. Scenario a must run right after setup:
# its content check needs the seeded window to still be inside last=15m.
set -euo pipefail
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
root="$(CDPATH= cd -- "$here/../.." && pwd -P)"
harness="$here/harness.mjs"
trap 'node "$harness" teardown' EXIT

first="${1:-}"
if [[ -z "$first" ]]; then
    # The binaries under test are this checkout's committed crate sources.
    if [[ -n "$(git -C "$root" status --porcelain -- 'crates/*/src' 'crates/*/Cargo.toml' Cargo.toml Cargo.lock)" ]]; then
        echo "uncommitted crate source changes: refusing to pin a SHA" >&2
        exit 1
    fi
    (cd "$root" && cargo build --release --locked -p trawl-server -p fleet-admin)
    mkdir -p "$here/results"
    rm -f "$here"/results/[a-g].json "$here"/results/[a-g]-http_failure.jsonl "$here"/results/restart-probe.json
    git -C "$root" rev-parse HEAD > "$here/results/commit.txt"
    node "$harness" setup
    node "$harness" restart-probe
    first=a
fi
for s in a b c d e f g; do
    [[ "$s" < "$first" ]] && continue
    node "$harness" scenario "$s"
    if [[ "$s" != g ]] && node -e '
        const r = require(process.argv[1]);
        process.exit(r.fiveXX.some(x => x.status === 500) || r.contentFailures ? 0 : 1);
    ' "$here/results/$s.json"; then
        echo "scenario $s reproduced a failure: stopping" >&2
        break
    fi
done
node "$harness" report > "$here/README.md"
