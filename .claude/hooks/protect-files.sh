#!/usr/bin/env bash
# PreToolUse hook: block writes to protected files
set -euo pipefail

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')

case "$FILE" in
  *.lock)
    echo "blocked: lock files are managed by cargo, not edited directly" >&2
    exit 2
    ;;
  *.env|*.env.*)
    echo "blocked: env files may contain secrets" >&2
    exit 2
    ;;
  */target/*)
    echo "blocked: target/ is a build artifact directory" >&2
    exit 2
    ;;
esac
