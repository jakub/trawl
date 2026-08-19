#!/usr/bin/env bash
# PreToolUse hook: block dangerous bash commands
set -euo pipefail

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

# What belongs here: operations that destroy work no reflog can return, or that
# publish irreversibly. `reset --hard` and `branch -D` were dropped precisely
# because the reflog does return them, and blocking them broke the ordinary
# squash-merge workflow (a squashed branch is no ancestor of main, so `-d`
# refuses it) with no safety earned.

# literal strings (grep -F, no regex interpretation)
declare -a LITERAL_BLOCKED=(
  'cargo publish'   # irreversible: crates.io has no unpublish
  'rm -rf'
  'clean -fd'       # deletes untracked files; nothing recovers them
)

# regex patterns (grep -E — no PCRE lookaround available here)
declare -a REGEX_BLOCKED=(
  # Bare --force overwrites whatever the remote holds. --force-with-lease is
  # allowed: it refuses when the remote moved under you, which is the only
  # thing the bare form gets wrong. `[^-]|$` is how ERE spells "not followed
  # by -with-lease" without a negative lookahead.
  'push.+--force([^-]|$)'
  'push.+-f[[:space:]]'
  # Anchored to a git command so that PROSE about hook bypasses — a PR comment
  # or commit body containing the flag name — is not itself blocked. Writing
  # about a rule is not breaking it.
  'git[[:space:]].*--no-verify'
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
