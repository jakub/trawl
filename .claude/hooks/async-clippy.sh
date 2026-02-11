#!/usr/bin/env bash
# PostToolUse hook (async): run clippy after rust file edits
set -euo pipefail

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

# only run for rust files
if [[ "$FILE" != *.rs ]]; then
  exit 0
fi

# run clippy, capture output, only report if there are warnings/errors
OUTPUT=$(cargo clippy --workspace --all-targets --message-format=short -- -D warnings 2>&1) || {
  # clippy found issues — show the last 30 lines (skip the build noise)
  echo "$OUTPUT" | grep -E '(warning|error)' | tail -30
  exit 0
}
