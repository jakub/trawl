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
# Touched inside the container by the config-FIFO writer this script holds open,
# the moment that writer's open() returns. See hold_config.
readonly CFG_HELD=/tmp/cfg-held
# Where that writer's own stderr goes, so a refused open is one readable line
# instead of a 60s timeout with nothing to read. See hold_config.
readonly CFG_WRITER_ERR=/tmp/cfg-writer-err

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

# trawld parked at the FIFO sits right after `trawl_crashdump::init()` returned
# and before anything else: the monitor is up, the daemon is sealed, the fatal
# signal handler is attached, and no config has been read. That is the exact
# moment both /proc status files mean what this script claims they mean, and
# hold_config is what waits for it. No step may infer it from the mere existence
# of two trawld processes.
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

note() { printf '  %s\n' "$*"; }
phase() { printf '\n\n## %s\n\n' "$*"; }
run() { printf '\n$ %s\n' "$*"; "$@"; }

# ----------------------------------------------------------- failure capture --

# A failure takes its own evidence with it: cleanup removes every container on
# the way out, and what CI keeps is $LOG_DIR. So a post-mortem only gets what
# was written to disk before die() returned. The case this exists for is step 7
# on the CI docker daemon, where trawld never opened the config FIFO, the
# containers were removed by the trap, and the uploaded artifact said nothing
# about the run at all.
#
# Every reader below is best-effort and returns success. A diagnostic that
# fails the script it is diagnosing would replace one unexplained failure with
# another, so each reader keeps whatever it printed, error text included.

readonly DIAG_TIMEOUT_SECS=20

# Runs INSIDE a container. Prints the pid of every trawld process. It reads
# comm rather than status or environ because comm stays readable to uid 1000
# whatever the exec did to dumpability, and a hung process is exactly when the
# other two are least likely to answer.
# shellcheck disable=SC2016  # the container's shell expands these, not ours
readonly FIND_TRAWLD_COMM='
for d in /proc/[0-9]*; do
  if [ "$(cat "$d/comm" 2>/dev/null)" = trawld ]; then echo "${d#/proc/}"; fi
done
exit 0
'

# Which uid the readers run as, set per container by capture_state.
#
# It matters. trawld exec's with a file capability, which makes the exec
# secureexec, which clears dumpable, which hands the daemon's own /proc files
# to root: uid 1000 gets "Permission denied" for wchan, syscall and fd, and
# those are the three that tell a hang from an exit. A root `docker exec` is
# not an escalation the container performed, so the runtime allows it even
# under no_new_privileges, and it reads them. On the container this failure is
# about it prints wchan=wait_for_partner and syscall=257, which names the FIFO
# open by number. /proc/<pid>/stack stays denied: that one wants CAP_SYS_ADMIN
# in the initial user namespace, which no container here has.
DIAG_EXEC_USER=""

# diag <container> <outfile> <sh-command>. Runs one reader in the container and
# puts everything it printed into <outfile>, stderr included, then records the
# exit status when it is not 0. An unreadable /proc file is itself the finding.
diag() {
  local rc=0
  if [ -n "$DIAG_EXEC_USER" ]; then
    timeout "$DIAG_TIMEOUT_SECS" docker exec -u "$DIAG_EXEC_USER" "$1" sh -c "$3" >"$2" 2>&1 || rc=$?
  else
    timeout "$DIAG_TIMEOUT_SECS" docker exec "$1" sh -c "$3" >"$2" 2>&1 || rc=$?
  fi
  [ "$rc" -eq 0 ] || printf '[reader exited %s]\n' "$rc" >>"$2"
  return 0
}

# capture_state <container> <label>. Writes the container's logs, its state as
# the daemon sees it, /tmp, and one set of /proc files per trawld process, all
# under $LOG_DIR/<label>-*.
#
# wchan, syscall and stack are what tell a hang from an exit: a process parked
# in the kernel names the function it is parked in, and one that is gone leaves
# no files at all next to a non-zero exit code in <label>-state.txt.
capture_state() {
  local c="$1" label="$2" pids pid rc=0
  [ -n "$c" ] || return 0

  timeout "$DIAG_TIMEOUT_SECS" docker logs "$c" >"$LOG_DIR/$label-container.log" 2>&1 || rc=$?
  # Same colouring as logs(), stripped the same way, in place.
  sed -i -e 's/\x1b\[[0-9;]*m//g' "$LOG_DIR/$label-container.log" 2>/dev/null || true
  [ "$rc" -eq 0 ] || printf '[docker logs exited %s]\n' "$rc" >>"$LOG_DIR/$label-container.log"

  timeout "$DIAG_TIMEOUT_SECS" docker inspect \
    --format '{{.State.Status}} {{.State.ExitCode}} {{.State.Pid}}' "$c" \
    >"$LOG_DIR/$label-state.txt" 2>&1 || true

  # Root if the runtime will give it, the container's own uid otherwise: a
  # partial set of readable files beats none.
  DIAG_EXEC_USER=""
  if timeout "$DIAG_TIMEOUT_SECS" docker exec -u 0 "$c" true >/dev/null 2>&1; then
    DIAG_EXEC_USER=0
  fi

  diag "$c" "$LOG_DIR/$label-tmp.txt" 'ls -l /tmp'

  pids="$(timeout "$DIAG_TIMEOUT_SECS" docker exec "$c" sh -c "$FIND_TRAWLD_COMM" 2>/dev/null | tr -d '\r')" || pids=""
  for pid in $pids; do
    diag "$c" "$LOG_DIR/$label-pid$pid-status.txt" "cat /proc/$pid/status"
    diag "$c" "$LOG_DIR/$label-pid$pid-wchan.txt" "cat /proc/$pid/wchan; echo"
    diag "$c" "$LOG_DIR/$label-pid$pid-syscall.txt" "cat /proc/$pid/syscall"
    diag "$c" "$LOG_DIR/$label-pid$pid-stack.txt" "cat /proc/$pid/stack"
    diag "$c" "$LOG_DIR/$label-pid$pid-fd.txt" "ls -l /proc/$pid/fd"
  done
  return 0
}

# Every container this run started, labelled by its short name. Called from
# die, so the capture happens while the containers still exist.
capture_all_state() {
  local c
  ((${#CONTAINERS[@]})) || return 0
  printf '\n  saving container diagnostics to %s\n' "$LOG_DIR" >&2
  for c in "${CONTAINERS[@]}"; do
    capture_state "$c" "fail-${c#"$PREFIX-"}"
  done
  return 0
}

die() { printf '\nFAIL: %s\n' "$*" >&2; capture_all_state; exit 1; }

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
# INT and TERM get their own handlers rather than sharing one with EXIT. A
# signal delivered between two commands that succeeded enters the trap with $?
# still 0, so a single `trap cleanup EXIT INT TERM` would tear the containers
# down and exit 0: an interrupted proof reported as a passing one, which is the
# worst answer this script can give. These record the conventional 128+signal
# status and hand it to cleanup through `exit`, so cleanup's `$?` is non-zero.
# Checked by extracting cleanup and the traps into a stub and sending it
# SIGTERM between two `true`s: the stub exits 143.
on_signal() { # on_signal <name> <status>
  trap - INT TERM
  printf '\n\n!! %s received. Removing containers and exiting %s.\n' "$1" "$2"
  exit "$2"
}
trap 'on_signal SIGINT 130' INT
trap 'on_signal SIGTERM 143' TERM
trap cleanup EXIT

# Step 7 runs trawld as the container's own init process, so nothing inside can
# create its config FIFO first. One host-side FIFO is made here and copied into
# each of those containers before they start. It lives in its own temp dir and
# not in $LOG_DIR, which CI uploads wholesale: a FIFO in an artifact upload is
# an open() that never returns.
SEED_DIR="$(mktemp -d)"
mkfifo "$SEED_DIR/cfg"
# Its own mode and owner never matter: start_as_init plants it through a tar
# stream that states both.

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
  # Planted through a tar stream rather than `docker cp <path>`, which would
  # carry the seed file's own uid into the container. That matters because
  # /tmp is mode 1777 and the kernel's fs.protected_fifos (1 by default under
  # systemd, and a global sysctl every container inherits) refuses an O_CREAT
  # open of a FIFO in a sticky world-writable directory unless the FIFO is
  # owned by the directory's owner or by the caller. hold_config's writer uses
  # shell `>`, which is O_CREAT, and it runs as the container's uid 1000; the
  # script itself runs as whoever CI gave it, uid 1001 on the ARC runners. So
  # a copied-in FIFO owned by 1001 is refused, the writer's open never returns
  # a descriptor, and the step reports a config read trawld had in fact
  # reached. Owner 0 is the one choice that holds for every pairing, because
  # the directory owner is root in every one of these images.
  #
  # The alternative, an opener without O_CREAT (`exec 7<>$CFG_FIFO`), is not
  # used: a read-write open of a FIFO never blocks, so it would return before
  # trawld opened the read end and destroy the only thing hold_config proves.
  printf '$ tar <fifo> | docker cp - %s:%s && docker start %s\n' \
    "$CONTAINER" "${CFG_FIFO%/*}" "$CONTAINER"
  tar -C "$SEED_DIR" -cf - --owner=0 --group=0 --numeric-owner --mode=0666 cfg |
    docker cp - "$CONTAINER:${CFG_FIFO%/*}" >/dev/null
  docker start "$CONTAINER" >/dev/null
}

# poll <secs> <description> <predicate...>
# Polls. A bare sleep is never a synchronisation primitive here, and every wait
# is bounded. Returns 1 on timeout, for the one caller that has more to say
# about a timeout than the description does.
poll() {
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
  return 1
}

# wait_until <secs> <description> <predicate...>. poll, but a timeout is fatal.
wait_until() {
  poll "$@" || die "timed out after ${1}s waiting for: $2"
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
fifo_ready() { docker exec "$1" test -p "$CFG_FIFO" >/dev/null 2>&1; }
config_held() { docker exec "$1" test -e "$CFG_HELD" >/dev/null 2>&1; }
# What the config-FIFO writer's own open() said, if anything. Empty when the
# open blocked rather than failed, which is the other way hold_config times
# out; best-effort, because a diagnostic must not fail the failure path.
writer_error() {
  local text
  text="$(timeout "$DIAG_TIMEOUT_SECS" docker exec "$1" cat "$CFG_WRITER_ERR" 2>&1 | tr '\n' ' ')" || true
  printf '%s' "${text:-<empty: the open blocked rather than failed>}"
}
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

# hold_config <container>. Parks trawld at its config read, proves it got there,
# and keeps it there.
#
# Opening a FIFO for writing returns only once something has opened the read
# end. trawld's read end is `Config::from_file`, which main() reaches only after
# `trawl_crashdump::init()` has RETURNED: monitor spawned, client connected,
# monitor declared this process's ptracer, daemon sealed, fatal signal handler
# attached. A writer whose open returned has therefore watched the whole install
# finish. The writer then holds the descriptor and writes nothing, and a FIFO
# with a live writer and no data reads as "not yet" rather than EOF, so trawld
# stays parked for as long as the container lives.
#
# Counting two trawld processes proves none of that, which is why no step uses
# it that way. The child exists from the moment it is forked, which is before
# the parent connects to it, before PR_SET_PTRACER, before the seal and before
# the handler exists: a status capture taken there can read a daemon that has
# not sealed yet, and a SIGSEGV sent there can find the default disposition and
# kill trawld with no dump at all.
hold_config() {
  # The writer must never be what CREATES the path. In the shell-fronted steps
  # the container mkfifo's it at startup, and an ordinary file opened into
  # existence here first would make that mkfifo fail.
  wait_until "$PROC_WAIT_SECS" "the config FIFO exists in $1" fifo_ready "$1"
  # stderr is redirected in its own `exec` first, so the redirection that can
  # actually fail reports into the file rather than into a detached `docker
  # exec`'s discarded output. `docker exec -d` returns 0 either way, so this
  # file is the only place a refused open is written down.
  docker exec -d "$1" \
    sh -c "exec 2>$CFG_WRITER_ERR; exec 7>$CFG_FIFO; : >$CFG_HELD; sleep 86400" ||
    die "cannot start a config-FIFO writer in $1"
  poll "$EXEC_WAIT_SECS" "trawld in $1 is parked at its config read (init() returned)" \
    config_held "$1" ||
    die "trawld in $1 never reached its config read within ${EXEC_WAIT_SECS}s (init() did not finish?); config-FIFO writer stderr: $(writer_error "$1")"
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

phase "step 2: the runner's kernel, and its own masks as facts"

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
  echo "docker server:                 $docker_version"
  echo "runner CapBnd (informational): $runner_bnd (CAP_SYS_PTRACE=$(cap_bit "$runner_bnd"))"
  echo "ptrace_scope:                  $scope"
} | tee "$LOG_DIR/02-runner.txt"

# The runner's own bounding set is recorded here and asserted nowhere. This
# process is not the docker daemon: on the k8s runners the daemon is a separate
# privileged container, which is the only reason `docker build` works there at
# all, and what that daemon can hand a container it starts is unrelated to the
# mask this shell happens to carry. Reading /proc/self/status to predict a grant
# measures the wrong process, and it fails a job that would have passed.
#
# The honest measurement is a container started with --cap-add SYS_PTRACE
# reading its OWN /proc/self/status. Step 3 does exactly that, so step 3 is the
# gate. ptrace_scope stays a real input, because that sysctl IS this kernel's
# and the containers share it.

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

# This is the premise of the whole exercise, and the gate for the run: --cap-add
# puts the capability in the BOUNDING set only. An ordinary binary run by uid
# 1000 holds none of it. Everything the monitor ends up with therefore came from
# the file capability, and a bounding set without the bit means every later step
# would be testing a shape the daemon refused to build.
[ "$(cap_bit "$base_bnd")" = 1 ] ||
  die "--cap-add SYS_PTRACE did not reach this container's bounding set, so the docker daemon could not grant SYS_PTRACE (container CapBnd=$base_bnd; the runner's own CapBnd=$runner_bnd is informational, the container's is what was measured); this job needs a daemon that can grant it"
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

# The handshake first, then the pids: hold_config is what makes the two status
# captures and the SIGSEGV below land after the install, and resolve_pids is
# what insists there are exactly two processes to read.
hold_config "$crash_c"
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
[[ "$threads" =~ ^[0-9]+$ ]] || die "unparsable thread count: $threads"
[[ "$regions" =~ ^[0-9]+$ ]] || die "unparsable region count: $regions"
case "$scope" in
  3)
    # yama 3 refuses every attach, so the capability buys nothing here and even
    # this monitor writes a header and no content. Steps 6b and 7a expect the
    # same thing at scope 3, and so must this one.
    [ "$threads" -eq 0 ] ||
      die "at scope 3 the monitor captured $threads threads, which yama should have refused"
    [ "$regions" -eq 0 ] ||
      die "at scope 3 the monitor captured $regions memory regions, which yama should have refused"
    ;;
  *)
    # Below scope 3 the effective capability licenses the attach on its own, so
    # the dump has real content. An empty dump is the signature of a denied
    # attach, which is what this proves is not happening.
    [ "$threads" -gt 0 ] ||
      die "the minidump captured $threads threads; a denied ptrace attach writes exactly this"
    [ "$regions" -gt 0 ] ||
      die "the minidump captured $regions memory regions"
    ;;
esac

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
    # Scope 0 and 1 never ask for the capability, so dropping it changes none of
    # the inputs the classifier reads here: the credentials match, both sides
    # hold an empty permitted set, the daemon is dumpable, and PR_SET_PTRACER
    # succeeded (checked at scope 1, not required at scope 0). All of them are
    # readable in this container shape, so ready is the only verdict the
    # classifier can reach, and this step is the capability-free control that
    # says so. Accepting denied too would pass a probe that stopped reading an
    # input, or a classifier that started demanding the capability at these
    # scopes; requiring the exact class also makes indeterminate the failure it
    # is, since nothing here is unreadable.
    assert_class "$misc_line" 'readiness="ready"'
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

hold_config "$misc_crash_c"
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
# So the FIFO is planted from outside instead, into a created but not yet
# started container. Same park at the config read, same proof that init()
# finished, nothing between the runtime and trawld. start_as_init says why it
# is planted as root.

start_as_init chart-shape-crash \
  --user 1000 --cap-add SYS_PTRACE --security-opt no-new-privileges \
  -e TRAWL_CRASH_DUMP_DIR="$DUMP_DIR" -e RUST_LOG=trawld=info \
  --entrypoint /usr/bin/trawld "$IMAGE" --config "$CFG_FIFO" --no-monitor
chart_c="$CONTAINER"

# Recorded unconditionally, because the shape docker actually built is only
# knowable while the container exists, and this is the step whose failure mode
# is "trawld never reached its config read on that daemon". The planted FIFO
# goes in the same file: whether uid 1000 inside can open it is the first
# thing to doubt when the read end never opens, and under fs.protected_fifos
# ownership decides that as much as mode does. Owner 0 is what start_as_init
# plants, so anything else in this listing is the answer.
docker inspect \
  --format '{{.HostConfig.SecurityOpt}} {{.HostConfig.CapAdd}} {{.Config.User}} {{.Config.Entrypoint}} {{.Config.Cmd}}' \
  "$chart_c" >"$LOG_DIR/07-chart-shape-container.txt" 2>&1 || true
printf 'seed fifo:\n' >>"$LOG_DIR/07-chart-shape-container.txt"
timeout "$DIAG_TIMEOUT_SECS" docker exec "$chart_c" ls -ln "$CFG_FIFO" \
  >>"$LOG_DIR/07-chart-shape-container.txt" 2>&1 || true

hold_config "$chart_c"
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
  echo "runner CapBnd:    $runner_bnd (informational)"
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
