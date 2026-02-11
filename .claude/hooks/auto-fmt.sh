#!/usr/bin/env bash
# PostToolUse hook: auto-format rust files after Edit/Write
set -euo pipefail

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

# only act on .rs files that exist
if [[ "$FILE" == *.rs && -f "$FILE" ]]; then
  cargo fmt -- "$FILE" 2>/dev/null || true
fi
