#!/usr/bin/env bash
# Stop hook: lint the workspace once the turn is done.
#
# This used to be a PostToolUse hook keyed on *.rs, which meant a full
# `--workspace --all-targets` lint after every single file write — six edits in a
# row bought six identical workspace lints and a thrashed build cache. At Stop it
# runs once per turn, which is the granularity the feedback is useful at anyway:
# clippy on a half-finished multi-file change mostly reports errors you were
# about to fix in the next edit.
#
# Findings go back to the model via asyncRewake (see settings.json), so they get
# addressed in the same turn rather than surfacing to the human as noise.
set -uo pipefail

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

# Nothing to lint if the tree has no Rust changes since HEAD.
git diff --quiet HEAD -- '*.rs' 2>/dev/null && git diff --cached --quiet HEAD -- '*.rs' 2>/dev/null && exit 0

OUTPUT=$(cargo clippy --workspace --all-targets --message-format=short -- -D warnings 2>&1) && exit 0

echo "clippy found issues:"
echo "$OUTPUT" | grep -E '(warning|error)' | tail -30
exit 0
