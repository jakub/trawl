#!/usr/bin/env bash
# PreToolUse hook: block dangerous bash commands
set -euo pipefail

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# literal strings (grep -F, no regex interpretation)
declare -a LITERAL_BLOCKED=(
  '--no-verify'
  'cargo publish'
  'rm -rf'
  'reset --hard'
  'clean -fd'
  'branch -D'
)

# regex patterns (grep -E)
declare -a REGEX_BLOCKED=(
  'push.+--force'
  'push.+-f[[:space:]]'
  'checkout[[:space:]]+\.'
  'restore[[:space:]]+\.'
)

for pattern in "${LITERAL_BLOCKED[@]}"; do
  if echo "$CMD" | grep -qF -- "$pattern"; then
    echo "blocked: matched dangerous pattern '$pattern'" >&2
    exit 2
  fi
done

for pattern in "${REGEX_BLOCKED[@]}"; do
  if echo "$CMD" | grep -qE -- "$pattern"; then
    echo "blocked: matched dangerous pattern '$pattern'" >&2
    exit 2
  fi
done
