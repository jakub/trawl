#!/usr/bin/env bash
# Does `claude -p --setting-sources ''` load no optional settings, and does the
# session still answer on the recorder's normal sign-in?
#
# A disposable directory gets .claude/settings.json (the project source) and
# .claude/settings.local.json (the local source). Each holds a SessionStart
# hook that touches its own marker file, and "outputStyle": "Explanatory".
# One `claude -p` runs from that directory twice: the control with
# `--setting-sources project,local`, the probe with `--setting-sources ''`.
#
# Stdout holds only the CLI version and, per run, the exit status, whether each
# marker file exists, the init frame's output_style, and whether the result
# frame carries an answer. No frame is printed. Exit 0 means the control loaded
# both sources and the probe loaded neither and answered.
# Usage: settings-probe.sh > settings-probe.txt   (Claude Code signed in)
set -euo pipefail
D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/cwd/.claude" "$D/out"
for source in project local; do
  case $source in project) file=settings.json ;; local) file=settings.local.json ;; esac
  python3 -I - "$D/cwd/.claude/$file" "$D/out/marker-$source" <<'PY'
import json, shlex, sys
path, marker = sys.argv[1:]
json.dump({"outputStyle": "Explanatory",
           "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "touch " + shlex.quote(marker)}]}]}},
          open(path, "w"))
PY
done

fail=0
run() {
  local name=$1 sources=$2 want_marker=$3 want_style=$4
  rm -f "$D/out/marker-project" "$D/out/marker-local"
  local status=0
  (cd "$D/cwd" && claude -p 'Reply with the single word: pong' --setting-sources "$sources" \
      --output-format stream-json --verbose --no-session-persistence \
      < /dev/null > "$D/out/$name.jsonl" 2> "$D/out/$name.stderr.log") || status=$?
  local project=absent local_=absent
  [ -e "$D/out/marker-project" ] && project=present
  [ -e "$D/out/marker-local" ] && local_=present
  local facts
  facts=$(python3 -I - "$D/out/$name.jsonl" <<'PY'
import json, sys
style, answered = "missing", "no"
for line in open(sys.argv[1]):
    try:
        d = json.loads(line)
    except ValueError:
        continue
    if d.get("type") == "system" and d.get("subtype") == "init":
        style = str(d.get("output_style", "missing"))
    elif d.get("type") == "result":
        if d.get("subtype") == "success" and not d.get("is_error") and str(d.get("result", "")).strip():
            answered = "yes"
print(style, answered)
PY
)
  local style=${facts% *} answered=${facts#* }
  echo "$name: --setting-sources '$sources'"
  echo "  exit status: $status"
  echo "  project hook marker: $project"
  echo "  local hook marker: $local_"
  echo "  init output_style: $style"
  echo "  answered: $answered"
  [ "$status" = 0 ] && [ "$project" = "$want_marker" ] && [ "$local_" = "$want_marker" ] \
    && [ "$style" = "$want_style" ] && [ "$answered" = yes ] || fail=1
}

echo "claude --version: $(claude --version)"
run control project,local present Explanatory
run probe '' absent default
if [ "$fail" = 0 ]; then
  echo "verdict: pass (the control loaded both sources; the probe loaded neither and answered)"
else
  echo "verdict: FAIL"
  exit 1
fi
