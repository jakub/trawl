#!/usr/bin/env bash
#
# crashdump-image.sh proves, against the built container image, that crash-dump
# capture has the privilege it needs and reports it honestly.
#
# Four things the image can get wrong that no unit test can see:
#
#   1. the `cap_sys_ptrace+p` file capability has to survive the image build
#      (an overlay that drops security.capability xattrs leaves an image that
#      looks right and captures nothing),
#   2. exec'ing trawld has to turn that permitted-only bit into an EFFECTIVE
#      one in the monitor while the daemon itself gives it back
#      (ADR-0023 ruling 4),
#   3. that bit has to survive the exec with no_new_privs set, because the
#      chart deploys exactly that shape: SYS_PTRACE added and
#      allowPrivilegeEscalation left false (step 7),
#   4. the readiness verdict trawld logs has to match what the kernel would
#      really allow at this host's yama ptrace_scope.
#
# usage: crashdump-image.sh <image>
#
# env:
#   LOG_DIR   where logs and /proc captures land (default ./crashdump-ci-logs)
#
# The host's kernel.yama.ptrace_scope is READ, never written. This runs on
# shared runners, where the sysctl is somebody else's kernel; scope 2 evidence
# comes from the transcript in chart/trawl/tests/, taken by hand on a
# workstation. The scope decides which assertions apply here, and it is re-read
# at the end so a run that raced an operator changing it fails instead of
# reporting a verdict against a value that had already stopped holding.
#
# Every path this script asks the container to write is under /tmp, because the
# containers run as uid 1000 and nothing else in the image is writable by it.

set -euo pipefail

# ----------------------------------------------------------------- constants --

# CAP_SYS_PTRACE. The masks in /proc/<pid>/status are hex bitmaps, so the
# question "does this process have it" is one shift and one mask.
readonly PTRACE_CAP_BIT=19

readonly YAMA=/proc/sys/kernel/yama/ptrace_scope
readonly DUMP_DIR=/tmp/cores
readonly CFG_FIFO=/tmp/cfg

# How long to wait for each observable. Generous: a debug-profile trawld is a
# few hundred MB and the monitor walks its whole address space.
readonly PROC_WAIT_SECS=60
readonly LOG_WAIT_SECS=120
readonly EXEC_WAIT_SECS=60
# The disabled run has to prove a NEGATIVE (no monitor). This is how long it
# watches for one to appear before feeding the config; the config feed is what
# turns the window into proof, because the writer's open can only return once
# trawld reached the config read, which is after init() has run to completion.
readonly NO_MONITOR_WATCH_SECS=5

# trawld parked at an unopened FIFO sits right after `trawl_crashdump::init()`
# and before anything else: the monitor is up, the daemon is sealed, and no
# config has been read. That is the exact moment both /proc status files mean
# what this script claims they mean.
#
# `exec` is deliberately absent. Without it the container's pid 1 is the shell,
# so trawld is an ordinary process: pid 1 in a pid namespace ignores signals
# whose disposition is SIG_DFL, which is exactly what crash-handler restores
# before it re-raises, and a `kill -SEGV` proof against pid 1 would depend on
# that. The trailing echo also stops dash from exec-optimising the tail call
# back into pid 1. Step 7 does run trawld as pid 1, because the shape it proves
# has no room for a shell, and it says there what that costs.
readonly FIFO_CMD='mkfifo '"$CFG_FIFO"' || exit 1
/usr/bin/trawld --config '"$CFG_FIFO"' --no-monitor
echo "trawld exit=$?"'

# Runs INSIDE a container. Prints one `pid ppid role` line per trawld process.
#
# `role` is monitor only when /proc/<pid>/environ could be read AND carries the
# monitor key. An enabled trawld exec's with a file capability that grants a new
# permitted bit, which makes the exec secureexec, which clears dumpable, which
# makes environ unreadable to anything without CAP_SYS_PTRACE. So environ is
# corroboration; parentage is the identification that always works.
#
# shellcheck disable=SC2016  # every $ in here is the container's shell, not ours
readonly FIND_TRAWLD='
for d in /proc/[0-9]*; do
  [ -r "$d/status" ] || continue
  name=$(head -n 1 "$d/status" | cut -f 2)
  [ "$name" = trawld ] || continue
  ppid=$(grep -m 1 "^PPid:" "$d/status" | cut -f 2)
  role=unknown
  if tr "\000" "\n" < "$d/environ" 2>/dev/null | grep -q -x "TRAWL_CRASHDUMP_MONITOR=1"; then
    role=monitor
  fi
  echo "${d#/proc/} $ppid $role"
done
'

# ------------------------------------------------------------------- plumbing --

IMAGE="${1:-}"
[ -n "$IMAGE" ] || { echo "usage: $0 <image>" >&2; exit 2; }

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="${LOG_DIR:-$PWD/crashdump-ci-logs}"
mkdir -p "$LOG_DIR"
LOG_DIR="$(cd -- "$LOG_DIR" && pwd)"

# The smallest config that gets trawld past `Config::from_file` and into
# tracing. Postgres is unreachable from these containers and trawld exits over
# it shortly afterwards, which costs nothing: the crash-dump verdict is logged
# before any store is touched, and every assertion below is on the log.
cat >"$LOG_DIR/trawld.toml" <<'TOML'
[server]

[data]
path = "/tmp/crashdump-ci-data"

[ingest]
enabled = false
internal_telemetry = false
TOML

PREFIX="trawl-cdci-$$"
CONTAINERS=()
CONTAINER=""

die() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }
phase() { printf '\n\n## %s\n\n' "$*"; }
run() { printf '\n$ %s\n' "$*"; "$@"; }

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if ((${#CONTAINERS[@]})); then
    printf '\n  removing containers: %s\n' "${CONTAINERS[*]}"
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
  fi
  if [ -n "${SEED_DIR:-}" ]; then
    rm -rf "$SEED_DIR"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# Step 7 runs trawld as the container's own init process, so nothing inside can
# create its config FIFO first. One host-side FIFO is made here and copied into
# each of those containers before they start. It lives in its own temp dir and
# not in $LOG_DIR, which CI uploads wholesale: a FIFO in an artifact upload is
# an open() that never returns.
SEED_DIR="$(mktemp -d)"
mkfifo "$SEED_DIR/cfg"
# Mode survives the copy, and the containers run as uid 1000 while the copy is
# made by whoever runs this script, so both ends need the other's bits.
chmod 666 "$SEED_DIR/cfg"

# start <short-name> <docker run args...>
# Detached, tracked for cleanup, and left un-removed so `docker logs` still
# works after the process inside dies.
start() {
  CONTAINER="${PREFIX}-$1"
  shift
  CONTAINERS+=("$CONTAINER")
  printf '\n$ docker run -d --name %s %s\n' "$CONTAINER" "$*"
  docker run -d --name "$CONTAINER" "$@" >/dev/null
}

# start_as_init <short-name> <docker create args...>
# Same tracking as start(), but trawld is the container's init process and the
# config FIFO is planted into the created container before it runs. Step 7
# explains why the shell has to go.
start_as_init() {
  CONTAINER="${PREFIX}-$1"
  shift
  CONTAINERS+=("$CONTAINER")
  printf '\n$ docker create --name %s %s\n' "$CONTAINER" "$*"
  docker create --name "$CONTAINER" "$@" >/dev/null
  printf '$ docker cp <fifo> %s:%s && docker start %s\n' "$CONTAINER" "$CFG_FIFO" "$CONTAINER"
  docker cp "$SEED_DIR/cfg" "$CONTAINER:$CFG_FIFO" >/dev/null
  docker start "$CONTAINER" >/dev/null
}

# wait_until <secs> <description> <predicate...>
# Polls. A bare sleep is never a synchronisation primitive here, and every wait
# is bounded.
wait_until() {
  local secs="$1" what="$2"
  shift 2
  local i
  for ((i = 0; i < secs; i++)); do
    if "$@"; then
      note "$what (after ${i}s)"
      return 0
    fi
    sleep 1
  done
  die "timed out after ${secs}s waiting for: $what"
}

# ------------------------------------------------------------- small readers --

# cap_bit <hexmask>. Prints 1 when CAP_SYS_PTRACE is set in that mask, else 0.
cap_bit() {
  [[ "$1" =~ ^[0-9a-fA-F]+$ ]] || die "not a capability mask: '$1'"
  printf '%s' "$(( 0x$1 >> PTRACE_CAP_BIT & 1 ))"
}

# field <status-file> <name>. Prints the value of one /proc/<pid>/status field.
field() {
  local value
  value="$(awk -v k="$2:" '$1 == k { print $2; exit }' "$1")"
  [ -n "$value" ] || die "no $2 field in $1"
  printf '%s' "$value"
}

# Container output with the tracing subscriber's colouring stripped. trawld
# colours its stdout whether or not anything is watching, and the escapes land
# between the field name and its value: the raw bytes are
# `ESC[3mevent_type ESC[0m ESC[2m= ESC[0m"crash_dump"`, so a grep for
# `event_type="crash_dump"` finds nothing in a log that plainly contains it.
logs() { docker logs "$1" 2>&1 | sed -e 's/\x1b\[[0-9;]*m//g'; }
log_has() { logs "$1" | grep -qF "$2"; }
crash_dump_line() { logs "$1" | grep -F 'event_type="crash_dump"' | head -n 1; }

trawld_procs() { docker exec "$1" sh -c "$FIND_TRAWLD" 2>/dev/null | tr -d '\r'; }
trawld_count() { trawld_procs "$1" | grep -c . || true; }
have_procs() { [ "$(trawld_count "$1")" -ge "$2" ]; }

# resolve_pids <container>. Sets PARENT_PID, MONITOR_PID and MONITOR_BY.
resolve_pids() {
  local procs count p1 pp1 p2 pp2 claimed
  procs="$(trawld_procs "$1")"
  count="$(printf '%s\n' "$procs" | grep -c . || true)"
  [ "$count" -eq 2 ] || die "expected 2 trawld processes in $1, saw ${count}: ${procs}"
  read -r p1 pp1 _ <<<"$(printf '%s\n' "$procs" | sed -n 1p)"
  read -r p2 pp2 _ <<<"$(printf '%s\n' "$procs" | sed -n 2p)"
  if [ "$pp2" = "$p1" ]; then
    PARENT_PID="$p1"; MONITOR_PID="$p2"
  elif [ "$pp1" = "$p2" ]; then
    PARENT_PID="$p2"; MONITOR_PID="$p1"
  else
    die "the two trawld processes are not parent and child: ${procs}"
  fi
  MONITOR_BY=parentage
  claimed="$(printf '%s\n' "$procs" | awk '$3 == "monitor" { print $1 }')"
  if [ -n "$claimed" ]; then
    [ "$claimed" = "$MONITOR_PID" ] ||
      die "TRAWL_CRASHDUMP_MONITOR is set on pid ${claimed}, but ${MONITOR_PID} is the child"
    MONITOR_BY=environ
  fi
  note "parent pid=${PARENT_PID} monitor pid=${MONITOR_PID} (identified by ${MONITOR_BY})"
}

# capture <container> <pid> <label>. Saves a status file into $LOG_DIR and
# prints its path.
capture() {
  local out="$LOG_DIR/$3.status"
  docker exec "$1" cat "/proc/$2/status" >"$out" || die "cannot read /proc/$2/status in $1"
  printf '%s' "$out"
}

# feed_config <container>. Unblocks trawld's config read.
#
# Opening a FIFO for writing blocks until a reader opens it, so a feed that
# returns is also proof that trawld reached the config read, and therefore that
# init() finished. A feed that never returns is a wedged init(), which is why it
# is bounded rather than trusted.
feed_config() {
  timeout "$EXEC_WAIT_SECS" docker exec -i "$1" sh -c "cat > $CFG_FIFO" <"$LOG_DIR/trawld.toml" ||
    die "trawld in $1 never opened $CFG_FIFO (init() did not finish?)"
  note "config fed"
}

# assert_class <line> <literal...>. The verdict has to be one of them.
assert_class() {
  local line="$1" want
  shift
  for want in "$@"; do
    case "$line" in *"$want"*) note "verdict: $want"; return 0 ;; esac
  done
  die "crash-dump verdict is none of [$*] in: $line"
}

# refute <line> <literal> <why>
refute() {
  case "$1" in *"$2"*) die "$3: $1" ;; esac
  note "absent as required: $2"
}

# mdmp_field <container> <name>. Reads threads= and memory_regions= off the
# monitor's own line, which is the only place those counts exist without
# parsing a dump this script is not allowed to keep.
mdmp_field() {
  local line rest
  line="$(logs "$1" | grep -F 'wrote minidump' | tail -n 1)"
  [ -n "$line" ] || die "no 'wrote minidump' line in $1"
  rest="${line##*"$2"=}"
  printf '%s' "${rest%% *}"
}

# ------------------------------------------------------- step 1: the stamp --

phase "step 1: the file capability the Dockerfile stamps is on the image"

docker image inspect "$IMAGE" >/dev/null 2>&1 ||
  die "image '$IMAGE' is not in the local docker store; build it before running this"

expected="$(sed -n 's/^# trawld-file-capability: //p' "$REPO_ROOT/Dockerfile")"
[ -n "$expected" ] ||
  die "Dockerfile carries no '# trawld-file-capability:' marker; this script has nothing to check against"
[ "$(printf '%s\n' "$expected" | wc -l)" -eq 1 ] ||
  die "Dockerfile carries more than one '# trawld-file-capability:' marker"
readback="/usr/bin/trawld $expected"
note "marker: $expected"

# The marker is only worth something if the build itself asserts the same
# string. A bare `getcap` on a file with no capability prints nothing and exits
# 0, so the Dockerfile compares the whole line, and so does this.
grep -qF "= \"$readback\" ]" "$REPO_ROOT/Dockerfile" ||
  die "the Dockerfile's getcap read-back does not compare against '$readback'"
note "the Dockerfile RUN asserts the same line"

printf '\n$ docker run --rm --entrypoint getcap %s /usr/bin/trawld\n' "$IMAGE"
actual="$(docker run --rm --entrypoint getcap "$IMAGE" /usr/bin/trawld | tr -d '\r')"
printf '%s\n' "$actual" >"$LOG_DIR/01-getcap.txt"
[ "$actual" = "$readback" ] ||
  die "getcap in the image says '${actual}', expected '${readback}' (did the build drop the xattr?)"
note "getcap agrees: $actual"

# ------------------------------------------------- step 2: the runner itself --

phase "step 2: what the runner's kernel and bounding set allow"

runner_bnd="$(field /proc/self/status CapBnd)"
if [ -r "$YAMA" ]; then
  scope="$(tr -d ' \r' <"$YAMA")"
else
  # No yama LSM built in is classic scope-0 semantics, which is what the crate
  # reports too (probe::ptrace_scope).
  scope=0
fi
[[ "$scope" =~ ^[0-3]$ ]] || die "unreadable yama ptrace_scope: '$scope'"
docker_version="$(docker version --format '{{.Server.Version}}')"

{
  echo "docker server:  $docker_version"
  echo "runner CapBnd:  $runner_bnd (CAP_SYS_PTRACE=$(cap_bit "$runner_bnd"))"
  echo "ptrace_scope:   $scope"
} | tee "$LOG_DIR/02-runner.txt"

# Nested containers cannot be granted a capability the runner does not hold, so
# an absent bit is a runner problem to fix, never a reason to skip the proof.
[ "$(cap_bit "$runner_bnd")" = 1 ] ||
  die "the runner's own bounding set lacks CAP_SYS_PTRACE, so --cap-add SYS_PTRACE cannot grant it; this job needs a runner that has it"

# -------------------------------------------------- step 3: the baseline sh --

phase "step 3: an ordinary executable in the enabled container shape"

printf '\n$ docker run --rm --user 1000 --cap-add SYS_PTRACE --entrypoint sh %s -c "cat /proc/self/status"\n' "$IMAGE"
docker run --rm --user 1000 --cap-add SYS_PTRACE --entrypoint sh "$IMAGE" \
  -c 'cat /proc/self/status' >"$LOG_DIR/03-baseline-sh.status"

base_bnd="$(field "$LOG_DIR/03-baseline-sh.status" CapBnd)"
base_prm="$(field "$LOG_DIR/03-baseline-sh.status" CapPrm)"
base_eff="$(field "$LOG_DIR/03-baseline-sh.status" CapEff)"
base_nnp="$(field "$LOG_DIR/03-baseline-sh.status" NoNewPrivs)"
note "CapBnd=$base_bnd CapPrm=$base_prm CapEff=$base_eff NoNewPrivs=$base_nnp"

# This is the premise of the whole exercise: --cap-add puts the capability in
# the BOUNDING set only. An ordinary binary run by uid 1000 holds none of it.
# Everything the monitor ends up with therefore came from the file capability.
[ "$(cap_bit "$base_bnd")" = 1 ] ||
  die "--cap-add SYS_PTRACE did not reach the container's bounding set"
[ "$(cap_bit "$base_prm")" = 0 ] ||
  die "an ordinary executable already holds CAP_SYS_PTRACE permitted; the file capability proves nothing here"
[ "$(cap_bit "$base_eff")" = 0 ] ||
  die "an ordinary executable already holds CAP_SYS_PTRACE effective; the file capability proves nothing here"
# The enabled shape leaves no_new_privs off, which is docker's default. It has
# to be off HERE because this container reaches trawld through a shell, and a
# shell carries no permitted capability of its own. Under no_new_privs the file
# capability would then be a gain, and the kernel takes a gain straight back.
# Step 7 runs the shape where no_new_privs is on and the runtime execs trawld.
[ "$base_nnp" = 0 ] || die "the enabled container shape sets no_new_privs=$base_nnp"

# ------------------------- step 4: the capability transition, and a real dump --

phase "step 4: enabled, the monitor gains the capability and the daemon loses it"

start enabled-crash \
  --user 1000 --cap-add SYS_PTRACE \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint sh "$IMAGE" -c "$FIFO_CMD"
crash_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$crash_c" 2
resolve_pids "$crash_c"
parent_status="$(capture "$crash_c" "$PARENT_PID" 04-enabled-parent)"
monitor_status="$(capture "$crash_c" "$MONITOR_PID" 04-enabled-monitor)"

mon_eff="$(field "$monitor_status" CapEff)"
mon_prm="$(field "$monitor_status" CapPrm)"
par_eff="$(field "$parent_status" CapEff)"
par_prm="$(field "$parent_status" CapPrm)"
par_nnp="$(field "$parent_status" NoNewPrivs)"
note "monitor CapEff=$mon_eff CapPrm=$mon_prm"
note "daemon  CapEff=$par_eff CapPrm=$par_prm NoNewPrivs=$par_nnp"

[ "$(cap_bit "$mon_eff")" = 1 ] ||
  die "the monitor does not hold CAP_SYS_PTRACE effective (CapEff=$mon_eff); the file capability did not take"
[ "$(cap_bit "$par_eff")" = 0 ] ||
  die "the daemon still holds CAP_SYS_PTRACE effective (CapEff=$par_eff); the seal did not take"
[ "$(cap_bit "$par_prm")" = 0 ] ||
  die "the daemon still holds CAP_SYS_PTRACE permitted (CapPrm=$par_prm); it could raise it again"
[ "$par_nnp" = 1 ] ||
  die "the daemon's no_new_privs is $par_nnp; it could exec its way back to the file capability"

run docker exec "$crash_c" sh -c "kill -SEGV $PARENT_PID"
wait_until "$LOG_WAIT_SECS" "the monitor wrote a minidump" log_has "$crash_c" 'wrote minidump'
logs "$crash_c" >"$LOG_DIR/04-enabled-crash.log"

threads="$(mdmp_field "$crash_c" threads)"
regions="$(mdmp_field "$crash_c" memory_regions)"
note "threads=$threads memory_regions=$regions"
# With the capability effective in the monitor, yama has nothing to say at any
# scope below 3: the attach succeeds and the dump has real content. A dump with
# no threads is the signature of a denied attach, which is what this proves is
# not happening.
[[ "$threads" =~ ^[0-9]+$ ]] && [ "$threads" -gt 0 ] ||
  die "the minidump captured $threads threads; a denied ptrace attach writes exactly this"
[[ "$regions" =~ ^[0-9]+$ ]] && [ "$regions" -gt 0 ] ||
  die "the minidump captured $regions memory regions"

# ------------------------------------------------ step 5: the enabled verdict --

phase "step 5: enabled, what trawld logs about itself"

start enabled-verdict \
  --user 1000 --cap-add SYS_PTRACE \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint sh "$IMAGE" -c "$FIFO_CMD"
verdict_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$verdict_c" 2
feed_config "$verdict_c"
wait_until "$LOG_WAIT_SECS" "trawld logged its crash-dump verdict" log_has "$verdict_c" 'event_type="crash_dump"'
logs "$verdict_c" >"$LOG_DIR/05-enabled-verdict.log"

enabled_line="$(crash_dump_line "$verdict_c")"
printf '%s\n' "$enabled_line"
if [ "$scope" = 3 ]; then
  # yama 3 refuses every attach, capability or not, and the crate says so
  # before it says anything about capabilities.
  assert_class "$enabled_line" 'readiness="denied"'
else
  assert_class "$enabled_line" 'readiness="ready"'
fi
refute "$enabled_line" 'readiness="failed"' "arming the handler failed"
# The seal is a claim about the daemon, so the daemon reads it back and reports
# what it found. These three are that read-back.
for f in 'self_cap_eff_ptrace=false' 'self_cap_prm_ptrace=false' 'self_no_new_privs=true'; do
  case "$enabled_line" in
    *"$f"*) note "reported: $f" ;;
    *) die "the verdict does not report $f: $enabled_line" ;;
  esac
done

# ------------------------------------------ step 6: the misconfigured verdict --

phase "step 6: misconfigured, no capability at all, and trawld still starts"

start misconfigured-verdict \
  --user 1000 --cap-drop ALL --security-opt no-new-privileges \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint sh "$IMAGE" -c "$FIFO_CMD"
misc_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$misc_c" 2
feed_config "$misc_c"
wait_until "$LOG_WAIT_SECS" "trawld logged its crash-dump verdict" log_has "$misc_c" 'event_type="crash_dump"'
logs "$misc_c" >"$LOG_DIR/06-misconfigured-verdict.log"

misc_line="$(crash_dump_line "$misc_c")"
printf '%s\n' "$misc_line"
case "$scope" in
  2 | 3)
    # Nothing but the capability can license the attach at scope 2, and nothing
    # can at scope 3.
    assert_class "$misc_line" 'readiness="denied"'
    ;;
  *)
    # At scope 0 and 1 the answer turns on PR_SET_PTRACER, dumpable and the
    # credential match rather than on the capability, so both verdicts are
    # legitimate. What is never legitimate is a shrug: every input the scope 0/1
    # branch reads is readable in this container shape.
    assert_class "$misc_line" 'readiness="ready"' 'readiness="denied"'
    refute "$misc_line" 'readiness="indeterminate"' "every probe input is readable in this shape, so a no-verdict is a bug"
    ;;
esac
refute "$misc_line" 'readiness="failed"' "arming the handler failed"
# ADR-0023 ruling 6 retired the old unconditional "crash-dump capture enabled"
# line: an operator who greps for it must find nothing rather than a claim the
# kernel would not honour.
refute "$misc_line" enabled "the verdict still uses the retired word 'enabled'"

phase "step 6b: misconfigured, what a crash actually produces"

start misconfigured-crash \
  --user 1000 --cap-drop ALL --security-opt no-new-privileges \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint sh "$IMAGE" -c "$FIFO_CMD"
misc_crash_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$misc_crash_c" 2
resolve_pids "$misc_crash_c"
capture "$misc_crash_c" "$PARENT_PID" 06-misconfigured-parent >/dev/null
capture "$misc_crash_c" "$MONITOR_PID" 06-misconfigured-monitor >/dev/null
run docker exec "$misc_crash_c" sh -c "kill -SEGV $PARENT_PID"
wait_until "$LOG_WAIT_SECS" "the monitor wrote a minidump" log_has "$misc_crash_c" 'wrote minidump'
logs "$misc_crash_c" >"$LOG_DIR/06-misconfigured-crash.log"

misc_threads="$(mdmp_field "$misc_crash_c" threads)"
misc_regions="$(mdmp_field "$misc_crash_c" memory_regions)"
note "threads=$misc_threads memory_regions=$misc_regions"
[[ "$misc_threads" =~ ^[0-9]+$ ]] || die "unparsable thread count: $misc_threads"
case "$scope" in
  2 | 3)
    # The dump is still written, still has a valid header, and is still empty.
    # That is the whole reason the readiness verdict exists.
    [ "$misc_threads" -eq 0 ] ||
      die "at scope $scope a monitor with no capability captured $misc_threads threads"
    [ "$misc_regions" -eq 0 ] ||
      die "at scope $scope a monitor with no capability captured $misc_regions memory regions"
    ;;
  *)
    # At scope 0 and 1 the declared tracer is enough, so the dump has content
    # even though nothing here holds CAP_SYS_PTRACE.
    [ "$misc_threads" -gt 0 ] ||
      die "at scope $scope the PR_SET_PTRACER path should still capture threads, got $misc_threads"
    ;;
esac

# --------------------------------------------- step 7: the chart's own shape --

phase "step 7: the chart shape, no_new_privs on, still captures"

# The chart adds SYS_PTRACE and leaves allowPrivilegeEscalation false, so the
# pod runs with no_new_privs set. That reads like it should be fatal, since
# under no_new_privs an exec may not GAIN a permitted capability: the kernel
# intersects the new permitted set back down to what the caller already held.
#
# The gain is what the rule is about, not the flag. The container runtime puts
# SYS_PTRACE in the init process's permitted set, so when that process execs
# /usr/bin/trawld the file capability grants a bit it already had. No gain,
# nothing taken back. The monitor exec repeats the same story one level down.
#
# Which is why trawld has to BE the init process here, exec'd by the runtime,
# exactly as it is in the pod. Every other step parks trawld behind a shell
# that mkfifo's its config, and a shell holds no permitted capability of its
# own, because its own exec emptied the set. trawld's file capability is then a
# gain, and the kernel takes it back. That shell does not exist in the pod, and
# leaving it in would turn this step into a proof of the opposite.
#
# So the FIFO is planted from outside instead, with docker cp into a created
# but not yet started container. Same park at the config read, same proof that
# init() finished, nothing between the runtime and trawld.

start_as_init chart-shape-crash \
  --user 1000 --cap-add SYS_PTRACE --security-opt no-new-privileges \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint /usr/bin/trawld "$IMAGE" --config "$CFG_FIFO" --no-monitor
chart_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$chart_c" 2
resolve_pids "$chart_c"
chart_parent_status="$(capture "$chart_c" "$PARENT_PID" 07-chart-shape-parent)"
chart_monitor_status="$(capture "$chart_c" "$MONITOR_PID" 07-chart-shape-monitor)"

chart_mon_eff="$(field "$chart_monitor_status" CapEff)"
chart_mon_prm="$(field "$chart_monitor_status" CapPrm)"
chart_mon_nnp="$(field "$chart_monitor_status" NoNewPrivs)"
chart_par_eff="$(field "$chart_parent_status" CapEff)"
chart_par_prm="$(field "$chart_parent_status" CapPrm)"
chart_par_nnp="$(field "$chart_parent_status" NoNewPrivs)"
note "monitor CapEff=$chart_mon_eff CapPrm=$chart_mon_prm NoNewPrivs=$chart_mon_nnp"
note "daemon  CapEff=$chart_par_eff CapPrm=$chart_par_prm NoNewPrivs=$chart_par_nnp"

# This one assertion is what the chart's securityContext rests on.
[ "$(cap_bit "$chart_mon_eff")" = 1 ] ||
  die "the monitor holds no CAP_SYS_PTRACE effective under no_new_privs (CapEff=$chart_mon_eff); the shape the chart deploys captures nothing"
[ "$chart_mon_nnp" = 1 ] ||
  die "the monitor's no_new_privs is $chart_mon_nnp, so this run did not test the chart's shape at all"
[ "$(cap_bit "$chart_par_eff")" = 0 ] ||
  die "the daemon still holds CAP_SYS_PTRACE effective (CapEff=$chart_par_eff); the seal did not take"
[ "$(cap_bit "$chart_par_prm")" = 0 ] ||
  die "the daemon still holds CAP_SYS_PTRACE permitted (CapPrm=$chart_par_prm); it could raise it again"
[ "$chart_par_nnp" = 1 ] ||
  die "the daemon's no_new_privs is $chart_par_nnp"

# trawld is pid 1 in this container, and the kernel discards a signal sent to
# pid 1 of a namespace when its disposition is SIG_DFL. The crash handler is
# installed, so SIGSEGV still reaches it and the monitor still dumps; what gets
# discarded is the re-raise the handler does afterwards, which leaves the
# container running. Nothing here reads an exit status, only the dump.
run docker exec "$chart_c" sh -c "kill -SEGV $PARENT_PID"
wait_until "$LOG_WAIT_SECS" "the monitor wrote a minidump" log_has "$chart_c" 'wrote minidump'
logs "$chart_c" >"$LOG_DIR/07-chart-shape-crash.log"

chart_threads="$(mdmp_field "$chart_c" threads)"
chart_regions="$(mdmp_field "$chart_c" memory_regions)"
note "threads=$chart_threads memory_regions=$chart_regions"
[[ "$chart_threads" =~ ^[0-9]+$ ]] || die "unparsable thread count: $chart_threads"
[[ "$chart_regions" =~ ^[0-9]+$ ]] || die "unparsable region count: $chart_regions"
case "$scope" in
  3)
    # yama 3 refuses every attach, so even a capable monitor writes an empty
    # dump. The verdict below is what has to say so.
    [ "$chart_threads" -eq 0 ] ||
      die "at scope 3 the monitor captured $chart_threads threads, which yama should have refused"
    ;;
  *)
    [ "$chart_threads" -gt 0 ] ||
      die "the minidump captured $chart_threads threads; a denied ptrace attach writes exactly this"
    [ "$chart_regions" -gt 0 ] ||
      die "the minidump captured $chart_regions memory regions"
    ;;
esac

phase "step 7b: the chart shape, what trawld logs about itself"

start_as_init chart-shape-verdict \
  --user 1000 --cap-add SYS_PTRACE --security-opt no-new-privileges \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint /usr/bin/trawld "$IMAGE" --config "$CFG_FIFO" --no-monitor
chart_verdict_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld and its monitor are up" have_procs "$chart_verdict_c" 2
feed_config "$chart_verdict_c"
wait_until "$LOG_WAIT_SECS" "trawld logged its crash-dump verdict" log_has "$chart_verdict_c" 'event_type="crash_dump"'
logs "$chart_verdict_c" >"$LOG_DIR/07-chart-shape-verdict.log"

chart_line="$(crash_dump_line "$chart_verdict_c")"
printf '%s\n' "$chart_line"
if [ "$scope" = 3 ]; then
  assert_class "$chart_line" 'readiness="denied"'
else
  assert_class "$chart_line" 'readiness="ready"'
fi
refute "$chart_line" 'readiness="failed"' "arming the handler failed"
# The first of these is the /proc capture above as trawld itself sees it; the
# rest are the seal read-back, unchanged by the shape.
for f in 'monitor_cap_eff_ptrace=true' 'self_cap_eff_ptrace=false' 'self_cap_prm_ptrace=false' 'self_no_new_privs=true'; do
  case "$chart_line" in
    *"$f"*) note "reported: $f" ;;
    *) die "the verdict does not report $f: $chart_line" ;;
  esac
done

# --------------------------------------------------- step 8: capture disabled --

phase "step 8: disabled, the stamped binary still runs and spawns nothing"

start disabled \
  --user 1000 --cap-drop ALL --security-opt no-new-privileges \
  -e RUST_LOG=trawld=info \
  --entrypoint sh "$IMAGE" -c "$FIFO_CMD"
off_c="$CONTAINER"

wait_until "$PROC_WAIT_SECS" "trawld is up" have_procs "$off_c" 1
for ((i = 0; i < NO_MONITOR_WATCH_SECS; i++)); do
  n="$(trawld_count "$off_c")"
  [ "$n" -eq 1 ] || die "a monitor process appeared with no dump directory configured (${n} trawld processes)"
  sleep 1
done
note "still exactly one trawld process after ${NO_MONITOR_WATCH_SECS}s"
# The feed returning is what makes that window meaningful: trawld can only have
# been parked at the config open, which is past init(), for the whole of it.
feed_config "$off_c"
wait_until "$LOG_WAIT_SECS" "trawld got past config load" log_has "$off_c" 'configuration loaded'
logs "$off_c" >"$LOG_DIR/08-disabled.log"

if log_has "$off_c" 'event_type="crash_dump"'; then
  die "capture is off, but trawld logged a crash-dump verdict: $(crash_dump_line "$off_c")"
fi
note "no crash-dump verdict logged, as expected"

# ------------------------------------------------------------------ summary --

phase "summary"

if [ -r "$YAMA" ]; then
  scope_now="$(tr -d ' \r' <"$YAMA")"
else
  scope_now=0
fi
[ "$scope_now" = "$scope" ] ||
  die "kernel.yama.ptrace_scope changed under this run ($scope -> $scope_now); the verdicts above were asserted against a value that no longer holds"

{
  echo "image:            $IMAGE"
  echo "docker server:    $docker_version"
  echo "ptrace_scope:     $scope (unchanged)"
  echo "file capability:  $actual"
  echo "baseline sh:      CapBnd=$base_bnd CapPrm=$base_prm CapEff=$base_eff NoNewPrivs=$base_nnp"
  echo "monitor:          CapEff=$mon_eff CapPrm=$mon_prm"
  echo "daemon:           CapEff=$par_eff CapPrm=$par_prm NoNewPrivs=$par_nnp"
  echo "enabled crash:    threads=$threads memory_regions=$regions"
  echo "enabled verdict:  $enabled_line"
  echo "misconf verdict:  $misc_line"
  echo "misconf crash:    threads=$misc_threads memory_regions=$misc_regions"
  echo "chart monitor:    CapEff=$chart_mon_eff CapPrm=$chart_mon_prm NoNewPrivs=$chart_mon_nnp"
  echo "chart daemon:     CapEff=$chart_par_eff CapPrm=$chart_par_prm NoNewPrivs=$chart_par_nnp"
  echo "chart crash:      threads=$chart_threads memory_regions=$chart_regions"
  echo "chart verdict:    $chart_line"
  echo "disabled:         one trawld process, no crash-dump verdict"
  echo "logs:             $LOG_DIR"
} | tee "$LOG_DIR/summary.txt"

printf '\nPASS\n'
