#!/usr/bin/env bash
# Prove `trawl trial` (ADR-0045) end to end on a disposable Docker engine.
#
# Usage: test-trial.sh TRAWL EVIDENCE PRIVATE
#
#   TRAWL     the trawl CLI under test.
#   EVIDENCE  a new directory for the log and the screenshots. CI uploads it,
#             so no secret may reach it: scan-trial-secrets.py checks it.
#   PRIVATE   a new directory, mode 0700, never uploaded: the trial's HOME and
#             XDG_STATE_HOME, scratch files, `capture.log`, and `secrets.tsv`,
#             the list of every generated secret value that the scanner reads.
#
# Environment:
#
#   TRIAL_IMAGE    local runs only: pass `--image TRIAL_IMAGE` when a trial is
#                  created. CI leaves it unset and loads the image built from
#                  the release tarball under the default reference, so the
#                  default path is what CI proves.
#   TRIAL_BROWSER  `skip` skips the Playwright sign-in, for a local image
#                  built without the SPA. Refused in CI.
#   TRIAL_E2E_DIR  the directory whose node_modules holds @playwright/test
#                  (default: crates/trawl-web-ui/e2e in this checkout).
#
# The script creates two trials in turn and deletes both. It refuses to start
# when the engine already holds trial resources or a trial port is taken, and
# on exit it removes the fixtures it planted. It prints every token file's
# path but never a secret.
#
# Nothing the script or a command prints goes straight to stdout. Every byte
# is captured in PRIVATE/capture.log first, and `release` passes it on:
# harvest-trial-secrets.py records every secret that exists at that moment
# (and registers its masks with `::add-mask::` when GITHUB_ACTIONS is set),
# scan-trial-secrets.py scans the whole capture for all of them, and only
# then is the new part printed and appended to EVIDENCE/trial.log. When the
# scan finds a value, its result is printed instead, and nothing captured
# afterwards is ever printed. The exit trap follows the same path.
set -euo pipefail

usage() {
  echo "usage: test-trial.sh TRAWL EVIDENCE PRIVATE" >&2
  exit 2
}
[[ $# -eq 3 ]] || usage
[[ -x "$1" ]] || { echo "not an executable: $1" >&2; exit 2; }
for dir in "$2" "$3"; do
  [[ ! -e "$dir" ]] || { echo "refusing: $dir exists; give a new directory" >&2; exit 2; }
done
TRAWL_BIN=$(realpath "$1")
umask 077
mkdir -p "$3" "$2"
PRIVATE=$(cd "$3" && pwd)
EVIDENCE=$(cd "$2" && pwd)
chmod 0755 "$EVIDENCE"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
E2E_DIR=${TRIAL_E2E_DIR:-$repo/crates/trawl-web-ui/e2e}

if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
  [[ -z "${TRIAL_IMAGE:-}" ]] || { echo "refusing TRIAL_IMAGE in CI: the trial must run the default image reference" >&2; exit 2; }
  [[ -z "${TRIAL_BROWSER:-}" ]] || { echo "refusing TRIAL_BROWSER in CI: the browser step always runs" >&2; exit 2; }
fi
[[ -z "${DOCKER_HOST:-}" && -z "${DOCKER_CONTEXT:-}" ]] || {
  echo "refusing: unset DOCKER_HOST and DOCKER_CONTEXT; the script and the trial must use the same local engine" >&2
  exit 2
}

# The trial's HOME has no Docker config, so trawl uses the default context.
# The script's own docker calls use the same empty config: one engine, and
# no stored registry credential is ever offered.
export DOCKER_CONFIG="$PRIVATE/docker-config"
TRIAL_HOME="$PRIVATE/home"
STATE_HOME="$PRIVATE/state"
STATE_DIR="$STATE_HOME/trawl/trial"
WORK="$PRIVATE/work"
SECRETS="$PRIVATE/secrets.tsv"
mkdir -p "$DOCKER_CONFIG" "$TRIAL_HOME/.config/trawl" "$WORK"
: >"$SECRETS"

# The runner's stdout stays on fd 3, for ::add-mask:: lines and released
# output. Every writer appends, so no two overwrite each other.
CAPTURE="$PRIVATE/capture.log"
exec 3>&1
exec >>"$CAPTURE" 2>&1
released=0     # bytes of the capture already printed
withheld=false # a scan found a secret: nothing is printed from then on
harvest=false  # set once this run may have created a trial

# Record the secrets that exist, scan the whole capture for every recorded
# secret, and print what is new. Returns non-zero, and prints no captured
# byte, when a secret cannot be read or a value is found.
release() {
  [[ $withheld == false ]] || return 1
  # DOCKER_HOST is unset: `release` also runs inside a command that sets it
  # for the trial, and the script refused to start with it set.
  if [[ $harvest == true ]] && ! env -u DOCKER_HOST -u DOCKER_CONTEXT \
    python3 "$here/harvest-trial-secrets.py" "$SECRETS" "$STATE_DIR" "${IMAGE:-}" 2>&1 >&3; then
    withhold "the trial's secrets could not be read, so the output cannot be scanned"
    return 1
  fi
  local size
  size=$(stat -c %s "$CAPTURE")
  head -c "$size" "$CAPTURE" >"$PRIVATE/snapshot"
  if [[ -s "$SECRETS" ]] &&
    ! python3 "$here/scan-trial-secrets.py" "$SECRETS" "$PRIVATE/snapshot" >"$PRIVATE/snapshot.scan" 2>&1; then
    cat "$PRIVATE/snapshot.scan" >&3
    withhold "the output holds a secret value"
    return 1
  fi
  tail -c "+$((released + 1))" "$PRIVATE/snapshot" | tee -a "$EVIDENCE/trial.log" >&3
  released=$size
}
withhold() {
  withheld=true
  echo "::error::$1; no captured output is printed from here on, and all of it stays in $CAPTURE" >&3
}

# These literals are checked against the product source by
# `quick_start_queries_are_the_browsers_literals` in
# crates/trawl-cli/src/trial/sample.rs.
DOCUMENTED_QUERY='service=checkout _severity>=error | stats count() as errors by service'
DOCUMENTED_ROW='{"errors":20,"service":"checkout"}'
QUICK_START_QUERIES=(
  "* | head 20"
  "* | stats count() by service"
  "_severity>=error | stats count() as errors by service | sort -errors | head 10"
  "service=web _severity>=warn | timechart span=5m count()"
)
SAMPLE_SERVICES='["api","auth","checkout","tutorial","web","worker"]'
SAMPLE_TOTAL=2000
OPERATOR_PERMS='trawl:export trawl:query trawl:query_cancel trawl:saved_query trawl:schema_read trawl:schema_write trawl:server_manage trawl:stream trawl:validate'
INGEST_PERMS='trawl:ingest'
PROJECT="trawl-trial"
LABEL=sh.trawl.trial.id
PORTS=(15514 18090 25514 28090)

step() {
  release || exit 1
  printf '\n==== %s\n' "$*"
}
fail() {
  echo "::error::$*"
  exit 1
}
note() { printf '  %s\n' "$*"; }
indent() { sed 's/^/   /'; }

# trawl with the trial's HOME and state, and no connection override.
t() {
  env -u TRAWL_URL -u TRAWL_TOKEN -u TRAWL_PROFILE -u TRAWL_INSECURE -u TRAWL_CONFIG \
    -u XDG_CONFIG_HOME HOME="$TRIAL_HOME" XDG_STATE_HOME="$STATE_HOME" "$TRAWL_BIN" "$@"
}

# Run a command with its output in $WORK/NAME.out and .err, then print both.
# `ok` requires exit 0; `refused` requires a non-zero exit.
capture() {
  local name=$1
  shift
  local status=0
  "$@" </dev/null >"$WORK/$name.out" 2>"$WORK/$name.err" || status=$?
  printf -- '-- %s: exit %s\n' "$name" "$status"
  sed 's/^/   out| /' "$WORK/$name.out"
  sed 's/^/   err| /' "$WORK/$name.err"
  release || exit 1
  return "$status"
}
ok() { capture "$@" || fail "$1 exited non-zero"; }
refused() {
  if capture "$@"; then fail "$1 succeeded, and a refusal was expected"; fi
}
says() { # says NAME TEXT: the command's stdout or stderr contains TEXT
  grep -qF -- "$2" "$WORK/$1.out" "$WORK/$1.err" || fail "$1: the output does not contain '$2'"
}

# Every resource in the trial's ownership union: the Compose project label,
# the trial id label, and the claim's reserved name.
listing() {
  {
    for filter in "label=com.docker.compose.project=$PROJECT" "label=$LABEL" "name=^trawl-trial-claim\$"; do
      docker ps -a --filter "$filter" --format "container {{.Names}} id={{.Label \"$LABEL\"}}"
    done
    for filter in "label=com.docker.compose.project=$PROJECT" "label=$LABEL"; do
      docker volume ls --filter "$filter" --format "volume {{.Name}} id={{.Label \"$LABEL\"}}"
      docker network ls --filter "$filter" --format "network {{.Name}} id={{.Label \"$LABEL\"}}"
    done
  } | sort -u
}
show_listing() {
  local current
  current=$(listing)
  echo "-- trial resources on the engine:"
  indent <<<"${current:-(none)}"
}

# The id of the one running-or-stopped container of a trial service.
cid() {
  local ids
  ids=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" \
    --filter "label=com.docker.compose.service=$1" --filter "label=com.docker.compose.oneoff=False")
  [[ $(wc -w <<<"$ids") -eq 1 ]] || fail "expected one $1 container, found: ${ids:-none}"
  echo "$ids"
}

state() { jq -r "$1" "$STATE_DIR/state.json"; }

# `release` records secrets as they appear; after an `up` that exits 0,
# this asserts that every secret of the trial is in the private list.
collect_secrets() {
  python3 "$here/harvest-trial-secrets.py" --require "$SECRETS" "$STATE_DIR" "$IMAGE" 2>&1 >&3 ||
    fail "not every secret of the trial could be read"
  note "the private list holds $(wc -l <"$SECRETS") secret values (values not shown)"
}

scan() { # scan [--classes C,...] PATH...: nothing in the private list may appear
  python3 "$here/scan-trial-secrets.py" "$SECRETS" "$@" || fail "a secret value was found (see above)"
}

# Counts per sample service, as `service count` lines.
sample_counts() {
  t -p trial query '* | stats count() as events by service' -f json |
    jq -r --argjson s "$SAMPLE_SERVICES" 'select(.service as $x | $s | index($x)) | "\(.service) \(.events)"' |
    sort
}
sample_total() { sample_counts | awk '{ total += $2 } END { print total + 0 }'; }

# The hashes of the token files and the recorded prefixes, for a private
# comparison. The result is never printed.
key_fingerprint() {
  {
    sha256sum "$STATE_DIR/operator.token" "$STATE_DIR/ingest.token"
    state '.keys.operator.prefix, .keys.ingest.prefix'
  } | sha256sum | cut -d' ' -f1
}

psql_as_superuser() { # psql_as_superuser DB: SQL on stdin, one row per line
  docker exec -i "$(cid postgres)" psql -U postgres -d "$1" -At -v ON_ERROR_STOP=1
}

# Start `trawl trial up` in its own process group, wait until state.json
# reaches CONDITION (`keys` or `intent`), and SIGKILL the whole group.
up_until_killed() { # up_until_killed NAME CONDITION TIMEOUT-SECONDS [ARGS...]
  local name=$1 condition=$2 timeout=$3 pid status=0
  shift 3
  setsid env -u TRAWL_URL -u TRAWL_TOKEN -u TRAWL_PROFILE -u TRAWL_INSECURE -u TRAWL_CONFIG \
    -u XDG_CONFIG_HOME HOME="$TRIAL_HOME" XDG_STATE_HOME="$STATE_HOME" \
    "$TRAWL_BIN" trial up "$@" </dev/null >"$WORK/$name.out" 2>"$WORK/$name.err" &
  pid=$!
  python3 - "$STATE_DIR/state.json" "$condition" "$pid" "$timeout" <<'PY' || status=$?
import json, os, signal, sys, time

path, condition, pid, timeout = sys.argv[1], sys.argv[2], int(sys.argv[3]), float(sys.argv[4])
for _ in range(500):
    if os.getpgid(pid) == pid:
        break
    time.sleep(0.002)
else:
    sys.exit(f"pid {pid} does not lead its own process group")

def reached(state):
    if condition == "keys":
        return state["keys"]["operator"] is not None and state["keys"]["ingest"] is not None
    return state["samples"]["state"] == "intent"

def running():
    try:
        with open(f"/proc/{pid}/stat") as stat:
            return stat.read().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False

deadline = time.monotonic() + timeout
while time.monotonic() < deadline:
    try:
        with open(path) as f:
            state = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        state = None
    if state is not None and reached(state):
        os.killpg(pid, signal.SIGKILL)
        print(f"state.json reached '{condition}'; sent SIGKILL to the process group of `trawl trial up`")
        sys.exit(0)
    if not running():
        sys.exit(f"`trawl trial up` exited before state.json reached '{condition}': the fault was not injected")
    time.sleep(0.002)
os.killpg(pid, signal.SIGKILL)
sys.exit(f"state.json did not reach '{condition}' within {timeout}s")
PY
  local code=0
  wait "$pid" || code=$?
  sed 's/^/   err| /' "$WORK/$name.err"
  [[ $status -eq 0 ]] || fail "$name: the kill did not happen at the planned point"
  [[ $code -eq 137 ]] || fail "$name: trawl exited $code, not by SIGKILL"
}

# -- background helpers and cleanup ---------------------------------------------

planted=()    # containers and volumes this script created, as "kind id"
background=() # process groups this script started

# Start a command in its own process group, so that stopping it also stops
# any child a wrapper started. The group's id is left in $spawned.
spawn() {
  setsid "$@" </dev/null &
  spawned=$!
  background+=("$spawned")
}
stop_spawned() {
  kill -- "-$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
}
wait_listening() { # wait_listening PORT
  python3 - "$1" <<'PY' || fail "nothing listens on 127.0.0.1:$1"
import socket, sys, time
for _ in range(200):
    try:
        socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1).close()
        sys.exit(0)
    except OSError:
        time.sleep(0.05)
sys.exit(1)
PY
}

cleanup() {
  local status=$?
  set +e
  # Read the secrets while the trial still exists, before anything below
  # can delete it.
  release
  for pid in "${background[@]}"; do kill -- "-$pid" 2>/dev/null; done
  for item in "${planted[@]}"; do
    read -r kind id <<<"$item"
    docker "$kind" rm --force "$id" >/dev/null 2>&1 || docker "$kind" rm "$id" >/dev/null 2>&1
  done
  if [[ -n "$(listing)" && -e "$STATE_DIR/state.json" ]]; then
    echo "-- cleanup: deleting the trial this script left behind"
    t trial down --yes </dev/null
  fi
  printf '\n==== %s\n' "final state of the engine"
  show_listing
  echo "-- listeners on the trial ports:"
  ss -Htln | awk '{ print $4 }' | grep -E ":($(IFS='|'; echo "${PORTS[*]}"))\$" | sed 's/^/   /' || echo "   (none)"
  if [[ $status -eq 0 ]]; then echo "trial proof passed"; else echo "::error::trial proof failed (exit $status)"; fi
  if ! release; then
    [[ $status -ne 0 ]] || status=1
    echo "::error::trial proof failed (exit $status)" >&3
  fi
  exit "$status"
}
trap cleanup EXIT

# -- preconditions -------------------------------------------------------------

step "preconditions"
version=$(t --version | awk '{ print $2 }')
DEFAULT_IMAGE="ghcr.io/jakub/trawl:$version"
note "trawl $version at $TRAWL_BIN"
note "Docker $(docker version --format '{{.Server.Version}}'), $(docker compose version)"
[[ -z "$(listing)" ]] || { show_listing; fail "the engine already holds trial resources; run this on a disposable engine"; }
for port in "${PORTS[@]}"; do
  python3 -c 'import socket, sys; s = socket.socket(); s.bind(("127.0.0.1", int(sys.argv[1])))' "$port" ||
    fail "port $port is taken; the proof needs ${PORTS[*]} free"
done
if [[ -n "${TRIAL_IMAGE:-}" ]]; then
  IMAGE=$TRIAL_IMAGE
  IMAGE_ARGS=(--image "$TRIAL_IMAGE")
  note "local run: trials are created with --image $TRIAL_IMAGE"
else
  IMAGE=$DEFAULT_IMAGE
  IMAGE_ARGS=()
  docker image inspect "$IMAGE" >/dev/null ||
    fail "$IMAGE is not on this engine; load the image built from the release tarball under the default reference first"
fi
IMAGE_ID=$(docker image inspect --format '{{.Id}}' "$IMAGE")
note "trawl image $IMAGE is $IMAGE_ID"
# The engine held no trial resource when the check above ran, so from here
# on every trial on it is this run's, and `release` reads its secrets.
harvest=true
printf '[profiles.unrelated]\nurl = "https://unrelated.invalid:5514"\n' >"$TRIAL_HOME/.config/trawl/config.toml"
config_hash=$(sha256sum <"$TRIAL_HOME/.config/trawl/config.toml")
note "config.toml with one unrelated profile: sha256 ${config_hash%% *}"

# An unlabelled container and volume that share the project's name prefix.
# `down` must leave them alone.
by_volume=$(docker volume create trawl-trial_bystander)
planted+=("volume $by_volume")
by_container=$(docker container create --name trawl-trial-bystander --volume "$by_volume:/data" "$IMAGE" true)
planted+=("container $by_container")
note "planted an unlabelled container trawl-trial-bystander and volume $by_volume"

# -- refusals before anything exists --------------------------------------------

step "preflight: DOCKER_HOST=tcp://example.invalid:2375 is refused before anything is created"
DOCKER_HOST=tcp://example.invalid:2375 refused preflight t trial up "${IMAGE_ARGS[@]}"
says preflight "DOCKER_HOST"
says preflight "unix://"
[[ ! -e "$STATE_HOME" ]] || fail "preflight created $STATE_HOME"
[[ -z "$(listing)" ]] || fail "preflight created Docker resources"
note "no state directory, and no trial resource:"
show_listing

step "a port that is taken is refused, naming the port and its flag; nothing is deleted"
for pair in 15514:--api-port 18090:--web-port; do
  port=${pair%%:*}
  flag=${pair#*:}
  spawn python3 -c 'import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(); time.sleep(600)' "$port"
  holder=$spawned
  wait_listening "$port"
  refused "port-$port" t trial up "${IMAGE_ARGS[@]}"
  says "port-$port" "port $port"
  says "port-$port" "$flag"
  kill -0 "$holder" || fail "the listener on $port is gone"
  stop_spawned "$holder"
  [[ ! -e "$STATE_DIR" ]] || fail "the refused up created $STATE_DIR"
  [[ -z "$(listing)" ]] || fail "the refused up created Docker resources"
done
docker container inspect "$by_container" >/dev/null || fail "the bystander container is gone"
docker volume inspect "$by_volume" >/dev/null || fail "the bystander volume is gone"
note "both refusals left no trial directory and no trial resource; the bystanders remain"

step "a planted trawl-trial project with no trial state is refused"
foreign_id=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
intruder=$(docker container create --name trawl-trial-intruder \
  --label "com.docker.compose.project=$PROJECT" --label "$LABEL=$foreign_id" "$IMAGE" true)
planted+=("container $intruder")
refused orphaned t trial up "${IMAGE_ARGS[@]}"
says orphaned "trawl-trial-intruder"
[[ ! -e "$STATE_DIR" ]] || fail "the refused up created $STATE_DIR"
docker container rm "$intruder" >/dev/null
unset 'planted[-1]'

# -- trial A: the default ports ---------------------------------------------------

step "trial A: kill the first up once both keys are recorded"
up_until_killed up-killed-after-keys keys 600 "${IMAGE_ARGS[@]}"
[[ "$(state '.keys.operator != null and .keys.ingest != null')" == true ]] || fail "the keys are not recorded"
[[ "$(state .phases.services_verified)" == false ]] || fail "the kill landed after the services were verified"
killed_keys=$(key_fingerprint)
note "killed after the key step: services_verified=false, both keys recorded"

step "trial A: the rerun recovers, and prints the summary"
ok up-a t trial up
says up-a "The trial is up."
says up-a "http://127.0.0.1:18090"
says up-a "https://127.0.0.1:15514"
says up-a "$STATE_DIR/operator.token"
says up-a "$STATE_DIR/ingest.token"
says up-a "$DOCUMENTED_QUERY"
[[ "$(key_fingerprint)" == "$killed_keys" ]] || fail "the rerun changed a token file or a key prefix"
note "the rerun kept both keys: token files byte-identical, prefixes unchanged (compared privately)"
[[ "$(state .images.trawl.id)" == "$IMAGE_ID" ]] || fail "the trial runs $(state .images.trawl.id), not $IMAGE_ID"
if [[ -z "${TRIAL_IMAGE:-}" ]]; then
  [[ "$(state .images.trawl.reference)" == "$DEFAULT_IMAGE" && "$(state .images.trawl_overridden)" == false ]] ||
    fail "the trial did not take the default image reference"
  note "no --image: the trial resolved the default $DEFAULT_IMAGE to the image built from the tarball ($IMAGE_ID)"
fi
collect_secrets

step "trial A: a second up resumes without new keys or samples"
before_keys=$(key_fingerprint)
before_samples=$(sample_counts)
[[ $(awk '{ t += $2 } END { print t }' <<<"$before_samples") -eq $SAMPLE_TOTAL ]] || fail "the samples are not $SAMPLE_TOTAL events"
ok up-a-resume t trial up
[[ "$(key_fingerprint)" == "$before_keys" ]] || fail "the resume changed a token file or a key prefix"
[[ "$(sample_counts)" == "$before_samples" ]] || fail "the resume changed the sample counts"
note "token files byte-identical, prefixes unchanged (compared privately); sample counts unchanged:"
indent <<<"$before_samples"

step "trial A: status"
ok status t trial status
says status "http://127.0.0.1:18090"

step "trial A: trawl trial key prints operator.token and nothing else"
t trial key >"$WORK/key.out" 2>"$WORK/key.err" </dev/null || fail "trawl trial key exited non-zero"
cmp -s "$WORK/key.out" "$STATE_DIR/operator.token" || fail "trawl trial key stdout differs from operator.token"
[[ ! -s "$WORK/key.err" ]] || fail "trawl trial key wrote to stderr"
[[ $(wc -l <"$WORK/key.out") -eq 1 ]] || fail "trawl trial key printed more than one line"
rm -f "$WORK/key.out"
note "stdout is byte-identical to operator.token (one line); stderr is empty"

step "trial A: no container has a secret in its environment, command, or labels"
mapfile -t containers < <(docker ps -aq --filter "label=$LABEL=$(state .trial_id)")
docker inspect "${containers[@]}" >"$EVIDENCE/docker-inspect-trial-a.json"
note "docker inspect of ${#containers[@]} containers: $(docker inspect --format '{{.Name}}' "${containers[@]}" | tr '\n' ' ')"
scan "$EVIDENCE/docker-inspect-trial-a.json"
! grep -q TRAWL_WEB_INSECURE_UPSTREAM "$EVIDENCE/docker-inspect-trial-a.json" || fail "a container sets TRAWL_WEB_INSECURE_UPSTREAM"
jq -r '.[] | "\(.Name): env=\(.Config.Env | map(select(startswith("PATH=") | not)) | join(" ")) cmd=\(.Config.Cmd // [] | join(" "))"' \
  "$EVIDENCE/docker-inspect-trial-a.json" | sed 's/^/   /'

step "trial A: host files"
stat -c '%a %U %n' "$STATE_DIR" "$STATE_DIR"/* | sed 's/^/   /'
[[ "$(stat -c %a "$STATE_DIR")" == 700 ]] || fail "the trial directory is not 0700"
for file in operator.token ingest.token; do
  [[ "$(stat -c '%a %U' "$STATE_DIR/$file")" == "600 $(id -un)" ]] || fail "$file is not 0600 and mine"
done
note "the lock file lives outside the trial directory: $(stat -c '%a %n' "$STATE_HOME/trawl/trial.lock")"
# The TLS private key stays in the trawld volume: whoever holds it can
# serve the pinned certificate on the API port while the trial is stopped.
scan --classes password,cookie,key "$STATE_HOME" "$TRIAL_HOME"
note "no database password, superuser password, cookie key, or TLS private key is on the host"
scan --classes token,password,cookie "$STATE_DIR/state.json"
scan "$STATE_DIR/compose.json" "$STATE_DIR/ca.pem"

step "trial A: secret files inside the containers"
check_file() { # check_file SERVICE PATH MODE OWNER
  local found
  found=$(docker exec "$(cid "$1")" stat -c '%a %U' "$2")
  note "$1 $2: $found"
  [[ "$found" == "$3 $4" ]] || fail "$1 $2 is '$found', expected '$3 $4'"
}
check_file postgres /var/lib/postgresql/trial/superuser.password 400 postgres
check_file trawld /var/lib/trawl/trial/secrets/pgpass 400 trawl
check_file trawld /var/lib/trawl/trial/tls/key.pem 400 trawl
check_file trawl-web /var/lib/trawl/trial/web.cookie 400 trawl
docker exec "$(cid trawld)" id | sed 's/^/   trawld runs as /'
docker exec "$(cid trawl-web)" id | sed 's/^/   trawl-web runs as /'

step "trial A: published ports, restart policies, and syslog"
for service in postgres trawld trawl-web; do
  mapping=$(docker port "$(cid "$service")")
  echo "   docker port $service: ${mapping:-(none)}"
  while read -r line; do
    [[ -z "$line" || "$line" == *" -> 127.0.0.1:"* ]] || fail "$service publishes $line beyond 127.0.0.1"
  done <<<"$mapping"
  policy=$(docker inspect --format '{{.HostConfig.RestartPolicy.Name}}' "$(cid "$service")")
  echo "   restart policy $service: ${policy:-(empty)}"
  [[ "$policy" == no || -z "$policy" ]] || fail "$service has restart policy $policy"
done
[[ -z "$(docker port "$(cid postgres)")" ]] || fail "postgres publishes a port"
echo "-- rendered compose.json, ports and restart per service:"
jq -c '.services | to_entries[] | {service: .key, ports: .value.ports, restart: .value.restart}' "$STATE_DIR/compose.json" | sed 's/^/   /'
[[ "$(jq '[.services[] | select(has("restart"))] | length' "$STATE_DIR/compose.json")" == 0 ]] || fail "a service has a restart policy"
[[ "$(jq '[.services[].ports // [] | .[] | select(.host_ip != "127.0.0.1")] | length' "$STATE_DIR/compose.json")" == 0 ]] ||
  fail "a port is published beyond 127.0.0.1"
docker exec "$(cid trawld)" cat /var/lib/trawl/trial/trawld.toml >"$EVIDENCE/trawld.toml"
docker exec "$(cid trawl-web)" cat /var/lib/trawl/trial/web.toml >"$EVIDENCE/web.toml"
echo "-- trawld.toml [syslog]:"
sed -n '/^\[syslog\]/,/^\[/p' "$EVIDENCE/trawld.toml" | sed 's/^/   /'
grep -qx 'enabled = false' <(sed -n '/^\[syslog\]/,/^\[/p' "$EVIDENCE/trawld.toml") || fail "syslog is not disabled"

step "trial A: the rendered project has no insecure setting"
cp "$STATE_DIR/compose.json" "$EVIDENCE/compose.json"
insecure=$(grep -Hin 'insecure' "$EVIDENCE/compose.json" "$EVIDENCE/trawld.toml" "$EVIDENCE/web.toml" |
  grep -v 'web.toml:[0-9]*:allow_insecure_cookies = true$' || true)
grep -Hin 'insecure' "$EVIDENCE/compose.json" "$EVIDENCE/trawld.toml" "$EVIDENCE/web.toml" | sed 's/^/   /' || true
[[ -z "$insecure" ]] || fail "the rendered project holds an insecure setting: $insecure"
note "only allow_insecure_cookies (the plain-HTTP loopback browser leg) matches"
grep -E '^(upstream_url|upstream_ca_path|public_origins) ' "$EVIDENCE/web.toml" | sed 's/^/   /'

step "trial A: the certificate's SANs"
openssl x509 -in "$STATE_DIR/ca.pem" -noout -subject -ext subjectAltName | sed 's/^/   /'
sans=$(openssl x509 -in "$STATE_DIR/ca.pem" -noout -ext subjectAltName | tail -n +2)
for san in DNS:trawld DNS:localhost 'IP Address:127.0.0.1'; do
  grep -qF "$san" <<<"$sans" || fail "the certificate lacks $san"
done

step "trial A: PostgreSQL owners and connections"
psql_as_superuser postgres <<'SQL' | tee "$WORK/owners" | sed 's/^/   /'
SELECT datname || ' owned by ' || pg_get_userbyid(datdba) FROM pg_database WHERE datname IN ('fleet', 'trawl') ORDER BY datname;
SQL
[[ "$(cat "$WORK/owners")" == $'fleet owned by fleet\ntrawl owned by trawl' ]] || fail "the database owners are wrong"
psql_as_superuser postgres <<'SQL' | sort -u | tee "$WORK/sessions" | sed 's/^/   connected: /'
SELECT usename || ' -> ' || datname FROM pg_stat_activity WHERE datname IN ('fleet', 'trawl');
SQL
for session in 'fleet -> fleet' 'trawl -> trawl'; do
  grep -qxF "$session" "$WORK/sessions" || fail "trawld holds no session $session"
done
! grep -qv -e '^fleet -> fleet$' -e '^trawl -> trawl$' "$WORK/sessions" || fail "another role holds a session on fleet or trawl"

step "trial A: the two keys and their roles"
fleet_admin() {
  docker exec -e DATABASE_URL=postgres://fleet@postgres:5432/fleet -e PGPASSFILE=/var/lib/trawl/trial/secrets/pgpass \
    "$(cid trawld)" fleet-admin "$@"
}
for pair in "trial-operator:$OPERATOR_PERMS" "trial-ingest:$INGEST_PERMS"; do
  role=${pair%%:*}
  expected=${pair#*:}
  fleet_admin roles show "$role" 2>&1 | tee "$WORK/role" | sed "s/^/   roles show $role|/"
  found=$(sed -n 's/^ *perms: *//p' "$WORK/role" | tr -d ',' | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' ')
  [[ "${found% }" == "$expected" ]] || fail "$role holds '$found', expected '$expected'"
done
# The prefix column is dropped before anything is printed: the final scan
# searches the evidence for prefixes.
fleet_admin keys list 2>/dev/null | python3 -c '
import sys
rows = []
for line in sys.stdin:
    if "┆" not in line:
        continue
    cells = [c.strip() for c in line.strip().strip("│").split("┆")]
    rows.append(cells[1:])
header, keys = rows[0], rows[1:]
if header[:4] != ["name", "kind", "roles", "active"]:
    sys.exit(f"unexpected keys list header {header}")
for row in [header] + keys:
    print("   " + " | ".join(row))
found = sorted(tuple(r[:4]) for r in keys)
expected = [("trial-ingest", "service", "trial-ingest", "yes"), ("trial-operator", "human", "trial-operator", "yes")]
if found != expected:
    sys.exit(f"the active keys are {found}, expected {expected}")
' || fail "keys list does not show exactly the two trial keys"
psql_as_superuser fleet <<'SQL' | tee "$WORK/expiry" | sed 's/^/   /'
SELECT name || ' ' || kind || ' never expires: ' || (expires_at IS NULL) FROM api_keys WHERE active ORDER BY name;
SQL
[[ "$(cat "$WORK/expiry")" == $'trial-ingest service never expires: true\ntrial-operator human never expires: true' ]] ||
  fail "a key expires, or the active keys are not the two trial keys"

step "trial A: -p trial reaches the trial; config.toml is untouched"
ok schema-fields t -p trial schema fields
ok proof-query t -p trial query '* | head 1' -f json
[[ "$(sha256sum <"$TRIAL_HOME/.config/trawl/config.toml")" == "$config_hash" ]] || fail "config.toml changed"
note "config.toml is byte-identical"

step "trial A: the documented query returns the documented row"
ok documented t -p trial query "$DOCUMENTED_QUERY" -f json
[[ "$(jq -cS . "$WORK/documented.out")" == "$DOCUMENTED_ROW" ]] || fail "the documented query did not return $DOCUMENTED_ROW"
note "exact row: $DOCUMENTED_ROW"

step "trial A: each quick-start example returns rows under last=15m"
for query in "${QUICK_START_QUERIES[@]}"; do
  t -p trial query "last=15m $query" -f json </dev/null >"$WORK/quick-start.out" || fail "'last=15m $query' failed"
  rows=$(grep -c . "$WORK/quick-start.out" || true)
  [[ $rows -ge 1 ]] || fail "'last=15m $query' returned no row"
  note "$rows row(s): last=15m $query"
  cut -c1-160 "$WORK/quick-start.out" | head -n 8 | indent
done

step "trial A: real Chromium signs in with the operator key"
if [[ "${TRIAL_BROWSER:-}" == skip ]]; then
  note "SKIPPED: TRIAL_BROWSER=skip (local image without the SPA)"
else
  # The sign-in runs at localhost, the tutorial's other allowed origin; the
  # page must also load at the address `up` printed.
  printed=$(grep -ohE 'http://127\.0\.0\.1:[0-9]+' "$WORK/up-a.out" "$WORK/up-a.err" | sort -u)
  [[ "$printed" == "http://127.0.0.1:18090" ]] || fail "up printed the browser address '$printed'"
  mkdir -p "$EVIDENCE/browser"
  TRIAL_E2E_DIR="$E2E_DIR" node "$here/trial-browser.mjs" "http://localhost:18090" "$STATE_DIR/operator.token" \
    "$EVIDENCE/browser" "$DOCUMENTED_QUERY" "$DOCUMENTED_ROW" "$printed" || fail "the browser step failed"
fi

step "trial A: a lost token file is revoked by prefix and minted again"
old_operator=$(state .keys.operator.prefix)
rm "$STATE_DIR/operator.token"
ok up-lost-token t trial up
says up-lost-token "revoking 1 earlier trial-operator key"
[[ -s "$STATE_DIR/operator.token" ]] || fail "no new operator token"
[[ "$(state .keys.operator.prefix)" != "$old_operator" ]] || fail "the operator key was not minted again"
printf "SELECT active, revoked_at IS NOT NULL FROM api_keys WHERE prefix = '%s';\n" "$old_operator" |
  psql_as_superuser fleet >"$WORK/revoked"
[[ "$(cat "$WORK/revoked")" == 'f|t' ]] || fail "the old operator key is not revoked"
[[ "$(printf "SELECT count(*) FROM api_keys WHERE active AND name = 'trial-operator';\n" | psql_as_superuser fleet)" == 1 ]] ||
  fail "more than one active operator key"
unset old_operator
note "the old operator key is inactive and revoked, one active operator key remains, and the new prefix differs (compared privately)"

step "trial A: stop keeps everything, and up resumes"
before_keys=$(key_fingerprint)
ok stop t trial stop
docker ps -a --filter "label=$LABEL=$(state .trial_id)" --format '   {{.Names}} {{.State}}'
[[ -z "$(docker ps -q --filter "label=$LABEL=$(state .trial_id)")" ]] || fail "a trial container still runs after stop"
[[ -s "$STATE_DIR/state.json" && -s "$STATE_DIR/operator.token" ]] || fail "stop removed state"
[[ $(docker volume ls -q --filter "label=$LABEL=$(state .trial_id)" | wc -l) -eq 3 ]] || fail "stop removed a volume"

step "trial A: while stopped, another certificate on the API port is refused"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext 'subjectAltName=DNS:trawld,DNS:localhost,IP:127.0.0.1' \
  -keyout "$WORK/impostor.key" -out "$WORK/impostor.pem" 2>/dev/null
cat >"$WORK/impostor.py" <<'PY'
import http.server, ssl, sys
cert, key, port, log = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
class Handler(http.server.BaseHTTPRequestHandler):
    # Log a connection only once a request line arrives over it: a client
    # that refused the certificate never sends one.
    def handle(self):
        try:
            line = self.rfile.readline(65537)
        except OSError:
            return
        if line:
            with open(log, "a") as f:
                f.write("an HTTP request arrived\n")
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server = http.server.HTTPServer(("127.0.0.1", port), Handler)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
PY
: >"$WORK/impostor.log"
spawn python3 "$WORK/impostor.py" "$WORK/impostor.pem" "$WORK/impostor.key" 15514 "$WORK/impostor.log"
impostor=$spawned
wait_listening 15514
openssl x509 -in "$WORK/impostor.pem" -noout -subject -ext subjectAltName | indent
# Controls: a client that trusts the impostor reaches it, and curl pinned to
# the trial certificate names the reason it refuses.
curl --silent --cacert "$WORK/impostor.pem" https://127.0.0.1:15514/ >/dev/null 2>&1 || true
grep -q 'an HTTP request arrived' "$WORK/impostor.log" || fail "the impostor does not serve TLS on 15514"
: >"$WORK/impostor.log"
if curl --silent --show-error --cacert "$STATE_DIR/ca.pem" https://127.0.0.1:15514/ >/dev/null 2>"$WORK/pinned-curl.err"; then
  fail "curl pinned to the trial certificate accepted the impostor"
fi
indent <"$WORK/pinned-curl.err"
refused impostor-query t -p trial query '* | head 1'
[[ ! -s "$WORK/impostor.log" ]] || fail "trawl sent an HTTP request, and so its token, to the impostor"
note "trawl -p trial refused the impostor during the TLS handshake and sent no request, so no token"
stop_spawned "$impostor"

ok up-after-stop t trial up
[[ "$(key_fingerprint)" == "$before_keys" ]] || fail "the resume after stop changed a key"
[[ "$(sample_counts)" == "$before_samples" ]] || fail "the resume after stop changed the samples"
note "resumed with the same keys (compared privately) and the same sample counts"

step "trial A: a trawl-trial resource with a foreign id makes up, stop, and down refuse"
intruder=$(docker container create --name trawl-trial-intruder \
  --label "com.docker.compose.project=$PROJECT" --label "$LABEL=$foreign_id" "$IMAGE" true)
planted+=("container $intruder")
before=$(listing)
refused foreign-up t trial up
refused foreign-stop t trial stop
refused foreign-down t trial down --yes
for name in foreign-up foreign-stop foreign-down; do says "$name" "trawl-trial-intruder"; done
[[ "$(listing)" == "$before" ]] || fail "a refused verb changed the engine"
docker container rm "$intruder" >/dev/null
unset 'planted[-1]'
note "all three refused and changed nothing"

step "trial A: a sample result the trial cannot account for is never posted over"
expected=$(jq -Rn '[inputs | split(" ") | {key: .[0], value: (.[1] | tonumber)}] | from_entries' <<<"$before_samples")
jq --argjson expected "$expected" \
  '.samples = {state: "intent", seed: .samples.seed, anchor: .samples.last, expected: $expected}' \
  "$STATE_DIR/state.json" >"$STATE_DIR/state.json.new"
chmod 0600 "$STATE_DIR/state.json.new"
mv "$STATE_DIR/state.json.new" "$STATE_DIR/state.json"
note "state.json now records the samples as an unverified intent with the expected counts"
printf 'Authorization: Bearer %s\n' "$(<"$STATE_DIR/ingest.token")" >"$WORK/ingest.header"
printf '{"service":"checkout","level":"error","message":"one event the trial did not post"}\n' |
  curl --fail --silent --show-error --cacert "$STATE_DIR/ca.pem" -H @"$WORK/ingest.header" \
    -H 'Content-Type: application/x-ndjson' --data-binary @- https://127.0.0.1:15514/api/v1/ingest >/dev/null
rm "$WORK/ingest.header"
for _ in $(seq 60); do
  [[ "$(sample_total)" -eq $((SAMPLE_TOTAL + 1)) ]] && break
  sleep 1
done
extra=$(sample_counts)
[[ "$(awk '{ t += $2 } END { print t }' <<<"$extra")" -eq $((SAMPLE_TOTAL + 1)) ]] || fail "the extra event did not land"
refused up-unaccounted t trial up
says up-unaccounted "never posts the samples twice"
says up-unaccounted "trawl trial down --yes"
says up-unaccounted "--no-sample-data"
[[ "$(sample_counts)" == "$extra" ]] || fail "the refused up changed the sample counts"
[[ "$(state .samples.state)" == intent ]] || fail "the refused up rewrote the samples state"
note "refused with recovery text, and posted nothing: $((SAMPLE_TOTAL + 1)) events before and after"

step "trial A: down without a terminal and without --yes deletes nothing"
before=$(listing)
refused down-no-tty t trial down
says down-no-tty "--yes"
[[ "$(listing)" == "$before" && -d "$STATE_DIR" ]] || fail "down without --yes changed something"
note "the inventory and the trial directory are unchanged"

step "trial A: down --yes deletes every labelled resource and the trial directory"
ok down-a t trial down --yes
[[ -z "$(listing)" ]] || { show_listing; fail "labelled resources remain after down"; }
[[ ! -e "$STATE_DIR" ]] || fail "the trial directory remains after down"
show_listing
docker container inspect --format '   unlabelled container {{.Name}} survives' "$by_container" || fail "down deleted the bystander container"
docker volume inspect --format '   unlabelled volume {{.Name}} survives' "$by_volume" || fail "down deleted the bystander volume"
note "the lock file stays outside: $(ls "$STATE_HOME/trawl")"
ok down-again t trial down
note "a second down exits 0"
[[ "$(sha256sum <"$TRIAL_HOME/.config/trawl/config.toml")" == "$config_hash" ]] || fail "config.toml changed"
note "config.toml is byte-identical across up and down"

# -- trial B: port overrides, no samples, and a kill during seeding -----------------

step "trial B: --api-port 25514 --web-port 28090 --no-sample-data"
ok up-b t trial up --api-port 25514 --web-port 28090 --no-sample-data "${IMAGE_ARGS[@]}"
says up-b "http://127.0.0.1:28090"
says up-b "https://127.0.0.1:25514"
collect_secrets
scan --classes password,cookie,key "$STATE_HOME" "$TRIAL_HOME"
note "no database password, superuser password, cookie key, or TLS private key is on the host"
docker exec "$(cid trawl-web)" cat /var/lib/trawl/trial/web.toml >"$EVIDENCE/web-b.toml"
grep -F 'public_origins' "$EVIDENCE/web-b.toml" | sed 's/^/   /'
grep -qxF 'public_origins = ["http://localhost:28090", "http://127.0.0.1:28090"]' "$EVIDENCE/web-b.toml" ||
  fail "the origins do not follow --web-port"
for service in trawld trawl-web; do echo "   docker port $service: $(docker port "$(cid "$service")")"; done
[[ "$(docker port "$(cid trawld)")" == "5514/tcp -> 127.0.0.1:25514" ]] || fail "trawld is not on 127.0.0.1:25514"
[[ "$(docker port "$(cid trawl-web)")" == "8090/tcp -> 127.0.0.1:28090" ]] || fail "trawl-web is not on 127.0.0.1:28090"
[[ "$(state .samples.state)" == skipped ]] || fail "--no-sample-data did not record skipped"
[[ "$(sample_total)" -eq 0 ]] || fail "--no-sample-data seeded events"
ok proof-query-b t -p trial query '* | stats count() as events by service' -f json
note "no sample service holds an event; -p trial follows the port"

step "trial B: kill up while it seeds"
up_until_killed up-killed-seeding intent 600
for _ in 1 2 3 4 5; do
  settled=$(sample_counts)
  sleep 3
  [[ "$(sample_counts)" == "$settled" ]] && break
done
note "after the kill, the sample services hold $(awk '{ t += $2 } END { print t + 0 }' <<<"$settled") events"
if capture up-after-seeding-kill t trial up; then
  [[ "$(state .samples.state)" == complete ]] || fail "the rerun exited 0 without complete samples"
  [[ "$(sample_total)" -eq $SAMPLE_TOTAL ]] || fail "the rerun recorded complete samples that are not $SAMPLE_TOTAL events"
  ok documented-b t -p trial query "$DOCUMENTED_QUERY" -f json
  [[ "$(jq -cS . "$WORK/documented-b.out")" == "$DOCUMENTED_ROW" ]] || fail "the documented row is wrong after recovery"
  note "outcome: the killed post had landed; the rerun verified the exact counts and recorded them"
else
  says up-after-seeding-kill "never posts the samples twice"
  says up-after-seeding-kill "trawl trial down --yes"
  [[ "$(sample_counts)" == "$settled" ]] || fail "the refused rerun changed the sample counts"
  [[ "$(state .samples.state)" == intent ]] || fail "the refused rerun rewrote the samples state"
  note "outcome: the rerun refused with recovery text and posted nothing"
fi

step "trial B: down"
ok down-b t trial down --yes
[[ -z "$(listing)" && ! -e "$STATE_DIR" ]] || fail "trial B left resources or its directory"
docker container rm "$by_container" >/dev/null
docker volume rm "$by_volume" >/dev/null
planted=()

step "scan every captured log and artifact for secret values"
scan "$EVIDENCE" "$WORK"/*.out "$WORK"/*.err
