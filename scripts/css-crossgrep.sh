#!/usr/bin/env bash
# css-crossgrep — issue #28 AC4 guard (recreates PR #29's slice-A
# criterion-2 check as a script).
#
# Asserts the fleet-ui.css / main.css split is a MOVE, not a copy:
#   1. no top-level selector is defined in both stylesheets;
#   2. no custom property (--foo) is defined in both;
#   3. every selector fleet-ui components emit (the moved families) is
#      defined in fleet-ui.css and absent from main.css.
#
# Run from the repo root: scripts/css-crossgrep.sh
set -euo pipefail

FLEET="crates/fleet-ui/styles/fleet-ui.css"
APP="crates/trawl-web-ui/styles/main.css"

fail=0

# ── 1. selector dual-definition ────────────────────────────────────
# Top-level selectors: rule-opening lines at column 0. Comma groups are
# split so `.a, .b {` contributes both. Approximation: continuation
# lines of multi-line selector groups don't open a brace and are
# skipped — good enough for the flat style both files use.
selectors() {
  grep -E '^[a-zA-Z.#:*[][^{]*\{' "$1" \
    | sed -E 's/\{.*$//' \
    | tr ',' '\n' \
    | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' \
    | grep -v '^$' \
    | sort -u
}

dups=$(comm -12 <(selectors "$FLEET") <(selectors "$APP") || true)
if [[ -n "$dups" ]]; then
  echo "FAIL: selectors defined in BOTH $FLEET and $APP:" >&2
  echo "$dups" >&2
  fail=1
fi

# ── 2. custom-property dual-definition ─────────────────────────────
props() {
  grep -oE '^[[:space:]]*--[a-zA-Z0-9-]+[[:space:]]*:' "$1" \
    | sed -E 's/[[:space:]]//g; s/:$//' \
    | sort -u
}

pdups=$(comm -12 <(props "$FLEET") <(props "$APP") || true)
if [[ -n "$pdups" ]]; then
  echo "FAIL: custom properties defined in BOTH $FLEET and $APP:" >&2
  echo "$pdups" >&2
  fail=1
fi

# ── 3. fleet-emitted selectors live fleet-side only ────────────────
# The class families fleet-ui components render. main.css may still
# OVERRIDE them with more specific app selectors (.dr-pop .tabs,
# .login-card .error) — those are different selector strings and pass
# check 1; what must not remain is the base definition itself.
moved=(
  ".btn-sm" ".btn-xs"
  ".modal .m-hd .ic" ".modal .m-field" ".reason-input"
  ".tabs" ".tabs .t" ".tabs .t.active" ".tabs .t .c" ".tabs .sp"
  ".sd-scrim" ".sd-drawer" ".sd-hd" ".sd-ttl" ".sd-actions" ".sd-x"
  ".sd-tabs" ".sd-tabs .tb" ".sd-tabs .tb.on" ".sd-tabs .sp"
  ".sd-tabs .meta" ".sd-body"
)
for sel in "${moved[@]}"; do
  if ! selectors "$FLEET" | grep -qxF "$sel"; then
    echo "FAIL: moved selector '$sel' missing from $FLEET" >&2
    fail=1
  fi
  if selectors "$APP" | grep -qxF "$sel"; then
    echo "FAIL: moved selector '$sel' still defined in $APP" >&2
    fail=1
  fi
done

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
echo "css-crossgrep: OK — no dual definitions; moved selectors live only in fleet-ui.css"
