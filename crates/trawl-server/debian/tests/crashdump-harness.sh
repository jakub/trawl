#!/usr/bin/env bash
#
# crashdump-harness.sh — end-to-end proof that the .deb's crash-dump drop-in
# works on a real systemd host.
#
# The container shares the host kernel, so /proc/sys/kernel/yama/ptrace_scope
# is the HOST's setting. The harness reads it first, never writes a value below
# it, needs --allow-host-sysctl to raise it, and restores it from an EXIT/INT/
# TERM trap — including under --keep.
#
# Packaging copies current Git-tracked files into the container's /tmp.
# Stage new source files first, including when using --allow-dirty.
#
#   * cargo-deb is pinned to 3.8.0. Updating it requires a full run because its
#     generated maintainer-script behavior is part of what this tests.
#
# WHAT THIS RUNS AS. The node container is --privileged, which is root on the
# host kernel in every way that matters, and inside it this harness runs the
# package's own maintainer scripts.
# Point it only at a tree and a .deb you trust.

set -Eeuo pipefail


readonly RUST_IMAGE="rust:1.98-trixie@sha256:620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959"
readonly DEBIAN_IMAGE="debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1"
readonly POSTGRES_IMAGE="postgres:18@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280"
readonly CARGO_DEB_VERSION="3.8.0"
readonly CARGO_ZIGBUILD_VERSION="0.23.4"

readonly BUILDER_IMAGE="trawl-crashdump-builder:cargo-deb-${CARGO_DEB_VERSION}"
readonly NODE_IMAGE="trawl-crashdump-node:trixie"
readonly NET="trawl-crashdump-net"
readonly PG="trawl-crashdump-pg"
readonly NODE="trawl-crashdump-node"
readonly CARGO_VOLUME="trawl-crashdump-cargo-registry"

readonly FLEET_DSN="postgres://fleet:fleetpw@${PG}:5432/fleet"
readonly TRAWL_DSN="postgres://trawl:trawlpw@${PG}:5432/trawl"

readonly YAMA="/proc/sys/kernel/yama/ptrace_scope"
readonly CORES="/var/lib/trawl/cores"
# The certificate trawld generates on its first start, which the packaged
# [web] upstream_ca_path pins.
readonly WEB_UPSTREAM_CA="/var/lib/trawl/tls/cert.pem"

readonly EXIT_SYSCTL_RESTORE_FAILED=3
readonly EXIT_PARTIAL=4
readonly PTRACE_CAP_BIT=19

# docker cp cannot write into a tmpfs mount, and the node container runs with
# --tmpfs /tmp, so the .deb is staged under /root instead.
readonly PACKAGES_IN_NODE="/root/trawl-packages"

# How long to wait for a dump to appear after a fault. RestartSec=5s, so this
# also has to cover the restart that follows.
readonly DUMP_WAIT_SECS=45
readonly ACTIVE_WAIT_SECS=60
readonly VERDICT_WAIT_SECS=60
readonly NEG_SETTLE_SECS=15

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)
readonly repo_root
. "$repo_root/crates/trawl-server/debian/tests/harness-cleanup.sh"
readonly DOCS_PAGE="$repo_root/docs/src/content/docs/reference/crash-dumps.md"

# The enable command from docs/.../crash-dumps.md, reproduced verbatim. The
# harness diffs this against the doc before running it, so the two cannot drift.
readonly ENABLE_CMD='sudo install -D -m 0644 \
  /usr/share/doc/trawl-server/examples/crashdump.conf \
  /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld'

# The disable command from the same page, also verbatim, also diffed before use.
readonly DISABLE_CMD='sudo rm /etc/systemd/system/trawld.service.d/crashdump.conf
sudo systemctl daemon-reload && sudo systemctl restart trawld'


PACKAGES=""
DEB=""
SCOPES_ARG="1,2"
ALLOW_HOST_SYSCTL=0
RUN_NEGATIVE=1
KEEP=0
OUT=""
ALLOW_DIRTY=0
ARGV=("$@")

usage() {
  cat <<'EOF'
crashdump-harness.sh [--packages DIRECTORY] [--scopes 1,2] [--allow-host-sysctl]
                     [--no-negative] [--keep] [--out FILE] [--allow-dirty]

  --packages DIRECTORY  use the three distribution .debs in DIRECTORY
  --scopes LIST         yama ptrace_scope values to exercise (default 1,2)
  --allow-host-sysctl   permit RAISING the host's kernel.yama.ptrace_scope
  --no-negative         skip phase E (the capability-removed control)
  --keep                leave containers running after the run
  --out FILE            tee the transcript to FILE
  --allow-dirty         run against a dirty working tree
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --packages) PACKAGES="${2:?--packages needs a directory}"; shift 2 ;;
    --scopes) SCOPES_ARG="${2:?--scopes needs a list}"; shift 2 ;;
    --allow-host-sysctl) ALLOW_HOST_SYSCTL=1; shift ;;
    --no-negative) RUN_NEGATIVE=0; shift ;;
    --keep) KEEP=1; shift ;;
    --out) OUT="${2:?--out needs a path}"; shift 2 ;;
    --allow-dirty) ALLOW_DIRTY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

IFS=',' read -r -a SCOPES <<< "$SCOPES_ARG"
for s in "${SCOPES[@]}"; do
  case "$s" in
    0|1|2) ;;
    3) echo "refusing scope 3: yama locks that value until reboot" >&2; exit 2 ;;
    *) echo "invalid scope '$s' (expected 0, 1 or 2)" >&2; exit 2 ;;
  esac
done
# Ascending, so the run only ever raises the scope. The single lowering is the
# trap's restore back to the value the host started with.
mapfile -t SCOPES < <(printf '%s\n' "${SCOPES[@]}" | sort -n -u)


TEE_FD_SAVED=0
if [[ -n "$OUT" ]]; then
  mkdir -p "$(dirname "$OUT")"
  exec 3>&1
  exec > >(tee "$OUT") 2>&1
  TEE_FD_SAVED=1
fi

finish_transcript() {
  if [[ "$TEE_FD_SAVED" == 1 ]]; then
    exec 1>&3 2>&3
    # Give tee a moment to drain the pipe before the shell exits.
    sleep 0.3
  fi
}


# Container names, the network and the cargo volume are fixed strings, so two
# concurrent runs would share them and the second one's teardown would tear down
# the first one's containers mid-crash. Take the lock BEFORE installing the
# cleanup trap, so a refused run exits without running any teardown at all.
#
# The lock lives in the per-user runtime directory, which systemd creates 0700
# and owns. /tmp is not usable for this: it is world-writable.
runtime_dir="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
if [[ ! -d "$runtime_dir" ]]; then
  echo "no runtime directory at $runtime_dir; refusing to run without a lock on a directory only this user can write" >&2
  echo "(set XDG_RUNTIME_DIR to one, or run from a session that has /run/user/\$(id -u))" >&2
  exit 2
fi
readonly LOCKFILE="$runtime_dir/trawl-crashdump-harness.lock"
command -v flock >/dev/null 2>&1 \
  || { echo "flock is not installed; refusing to run without the concurrency lock" >&2; exit 2; }
exec 9>>"$LOCKFILE"
if ! flock -n 9; then
  echo "another crashdump-harness.sh run holds $LOCKFILE; refusing to start" >&2
  echo "(the docker names this uses are fixed, so a second run would tear down the first)" >&2
  exit 2
fi


# Transcripts get committed as PR evidence, so the worktree's absolute path
# stays out of them. $REPO is substituted into everything this prints; the
# commands themselves still run against the real path.
rel() { printf '%s' "${1//$repo_root/\$REPO}"; }

phase() { printf '\n\n## phase %s\n\n' "$*"; }
note()  { printf '  %s\n' "${*//$repo_root/\$REPO}"; }
run()   { printf '\n$ %s\n' "${*//$repo_root/\$REPO}"; "$@"; }
runq()  { printf '\n$ %s\n' "${*//$repo_root/\$REPO}"; "$@" >/dev/null; }
# Run a script inside the node container, echoing it line by line first: these
# are multi-line, and %q would render them as one backslash-mangled blob.
nsh() {
  printf '\n[%s]\n' "$NODE"
  printf '%s\n' "$1" | sed 's/^/$ /'
  docker exec "$NODE" bash -c "$1"
}
nshq()  { docker exec "$NODE" bash -c "$1"; }
die()   { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }


ORIG_SCOPE=""
STUB_CREATED=""
# Docker objects THIS run created, recorded as IDs from the create commands that
# actually succeeded. Names are the wrong handle: they are fixed strings.
readonly RUN_LABEL="trawl-crashdump-harness-run=$$-$(date +%s)"
CREATED_CONTAINERS=()
CREATED_NETWORK=""
# Set to 1 BEFORE the write, never after: a write that lands and then fails to
# report, or a signal delivered mid-write, must still reach the restore path.
SCOPE_MODIFIED=0
# The value this run last ATTEMPTED to write, recorded before the write for the
# same reason.
SCOPE_EXPECTED=""

set_host_scope() {
  local value="$1"
  if docker inspect -f '{{.State.Running}}' "$NODE" 2>/dev/null | grep -qx true; then
    docker exec "$NODE" bash -c "echo $value > $YAMA"
  else
    # The node container is gone (cleanup path). A throwaway privileged
    # container reaches the same kernel knob.
    docker run --rm --privileged "$NODE_IMAGE" bash -c "echo $value > $YAMA"
  fi
}

# The host file and the container's view are the same kernel knob. Teardown
# reads the host directly, because by then the container may already be gone.
host_scope() { cat "$YAMA" 2>/dev/null || true; }

restore_host_scope() { # 0 restored or nothing to do, 1 needs a human
  local cur
  if [[ "$SCOPE_MODIFIED" != 1 || -z "$ORIG_SCOPE" ]]; then
    note "host ptrace_scope was never modified"
    return 0
  fi

  cur=$(host_scope)
  if [[ "$cur" == "$ORIG_SCOPE" ]]; then
    note "host ptrace_scope is already $ORIG_SCOPE; nothing to restore"
    return 0
  fi
  if [[ -n "$SCOPE_EXPECTED" && "$cur" != "$SCOPE_EXPECTED" ]]; then
    printf '\n!! NOT RESTORING %s: it reads %s, this run last tried to set %s.\n' \
      "$YAMA" "$cur" "$SCOPE_EXPECTED"
    printf '!! Something else changed it. Decide by hand; it was %s before this run.\n' "$ORIG_SCOPE"
    return 1
  fi

  note "restoring host ptrace_scope to $ORIG_SCOPE"
  if ! set_host_scope "$ORIG_SCOPE"; then
    printf '\n!! COULD NOT WRITE %s — set it to %s by hand !!\n' "$YAMA" "$ORIG_SCOPE"
    return 1
  fi

  cur=$(host_scope)
  if [[ "$cur" != "$ORIG_SCOPE" ]]; then
    printf '\n!! RESTORE DID NOT TAKE: %s reads %s, wanted %s. Set it by hand !!\n' \
      "$YAMA" "$cur" "$ORIG_SCOPE"
    return 1
  fi
  return 0
}

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  phase "down: teardown"

  local restore_failed=0
  restore_host_scope || restore_failed=1

  if [[ -n "$STUB_CREATED" && -f "$STUB_CREATED" ]]; then
    note "removing the stub SPA this run created: $(rel "$STUB_CREATED")"
    rm -f "$STUB_CREATED"
    rmdir "$(dirname "$STUB_CREATED")" 2>/dev/null || true
  fi

  if [[ "$KEEP" == 1 ]]; then
    note "--keep: leaving $NODE, $PG and network $NET in place"
  elif (( ${#CREATED_CONTAINERS[@]} == 0 )) && [[ -z "$CREATED_NETWORK" ]]; then
    note "this run created no containers or networks; nothing to remove"
  else
    local id
    for id in ${CREATED_CONTAINERS[@]+"${CREATED_CONTAINERS[@]}"}; do
      if [[ "$(docker inspect -f '{{index .Config.Labels "trawl-crashdump-harness-run"}}' "$id" 2>/dev/null)" == "${RUN_LABEL#*=}" ]]; then
        # Postgres declares an anonymous data volume. Remove it with its owned
        # disposable container; named registry caches are unaffected by -v.
        runq docker rm -fv "$id" 2>/dev/null || true
      else
        note "not removing container $id: it is gone, or it is no longer the one this run created"
      fi
    done
    if [[ -n "$CREATED_NETWORK" ]]; then
      if [[ "$(docker network inspect -f '{{index .Labels "trawl-crashdump-harness-run"}}' "$CREATED_NETWORK" 2>/dev/null)" == "${RUN_LABEL#*=}" ]]; then
        runq docker network rm "$CREATED_NETWORK" 2>/dev/null || true
      else
        note "not removing network $CREATED_NETWORK: it is gone, or it is no longer the one this run created"
      fi
    fi
  fi

  printf '\n$ cat %s   # host, after restore\n' "$YAMA"
  cat "$YAMA" 2>/dev/null || echo "(yama not present)"

  if (( restore_failed )); then
    rc="$EXIT_SYSCTL_RESTORE_FAILED"
    printf '\nexit status: %s (host ptrace_scope needs manual attention)\n' "$rc"
  else
    printf '\nexit status: %s\n' "$rc"
  fi
  finish_transcript
  exit "$rc"
}
# INT and TERM get their own handlers, not a shared one with EXIT. A signal
# delivered after a command that succeeded enters the trap with $? still 0, so a
# single `trap cleanup EXIT INT TERM` tears the stack down and then reports
# "exit status: 0".
on_signal() { # on_signal <name> <status>
  trap - INT TERM
  printf '\n\n!! %s received. Tearing down and exiting %s.\n' "$1" "$2"
  exit "$2"
}
trap 'on_signal SIGINT 130' INT
trap 'on_signal SIGTERM 143' TERM
trap cleanup EXIT


wait_for() { # wait_for <secs> <description> <bash test command>
  local secs="$1" what="$2" cmd="$3" i
  for ((i = 1; i <= secs; i++)); do
    if eval "$cmd" >/dev/null 2>&1; then
      note "$what after ${i}s"
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_active() {
  wait_for "$ACTIVE_WAIT_SECS" "trawld is-active=active" \
    "[[ \$(docker exec $NODE systemctl is-active trawld 2>/dev/null) == active ]]" && return 0
  docker exec "$NODE" systemctl status trawld --no-pager -l | head -40 || true
  docker exec "$NODE" journalctl -u trawld --no-pager -n 40 || true
  die "trawld did not reach active"
}

main_pid() { docker exec "$NODE" systemctl show trawld -p MainPID --value | tr -d '\r'; }

# The daemon and its monitor are both /usr/bin/trawld, so the monitor is found
# by parentage plus the one environment key that distinguishes it. Only that key
# is ever printed: the daemon's environment carries the postgres DSNs.
find_monitor() {
  docker exec "$NODE" bash -c '
    main=$1
    for d in /proc/[0-9]*; do
      [[ -r "$d/status" ]] || continue
      ppid=$(awk "/^PPid:/{print \$2}" "$d/status" 2>/dev/null) || continue
      [[ "$ppid" == "$main" ]] || continue
      if tr "\0" "\n" < "$d/environ" 2>/dev/null | grep -qx "TRAWL_CRASHDUMP_MONITOR=1"; then
        echo "${d#/proc/}"
        exit 0
      fi
    done
    exit 1
  ' bash "$1"
}

cap_bit() { # cap_bit <pid> <field>
  docker exec "$NODE" bash -c '
    v=$(awk "/^$2:/{print \$2}" "/proc/$1/status")
    echo $(( (0x$v >> '"$PTRACE_CAP_BIT"') & 1 ))
  ' bash "$1" "$2" | tr -d ' \r'
}

caps_report() { # caps_report <pid> <label>
  docker exec "$NODE" bash -c '
    pid=$1; label=$2; bit=$3
    eff=$(awk "/^CapEff:/{print \$2}" "/proc/$pid/status")
    amb=$(awk "/^CapAmb:/{print \$2}" "/proc/$pid/status")
    nnp=$(awk "/^NoNewPrivs:/{print \$2}" "/proc/$pid/status")
    printf "  %-8s pid=%-6s CapEff=0x%s bit%d=%d  CapAmb=0x%s bit%d=%d  NoNewPrivs=%s\n" \
      "$label" "$pid" "$eff" "$bit" "$(( (0x$eff >> bit) & 1 ))" \
      "$amb" "$bit" "$(( (0x$amb >> bit) & 1 ))" "$nnp"
  ' bash "$1" "$2" "$PTRACE_CAP_BIT"
}

dump_count() { nshq "ls -1 $CORES/*.dmp 2>/dev/null | wc -l" | tr -d ' \r'; }
newest_dump() { nshq "ls -1t $CORES/*.dmp 2>/dev/null | head -1" | tr -d '\r'; }
# One path per line, empty when there are none. Comparing path sets is exact,
# where "the newest file" is a guess that happens to be right most of the time.
dump_paths() { nshq "ls -1 $CORES/*.dmp 2>/dev/null | sort" || true; }

nrestarts() { docker exec "$NODE" systemctl show trawld -p NRestarts --value | tr -d ' \r'; }

# A minidump that could not ptrace its target still parses: minidump-writer
# treats a failed PTRACE_ATTACH as a soft error and drops the thread. So the
# useful question is not "is there a file" but "did it capture any threads".
# MDMP header: magic(4) version(4) stream_count(4) directory_rva(4); each
# directory entry is type(4) size(4) rva(4); ThreadListStream is 3, and its
# payload starts with a u32 count. MemoryListStream is 5, same shape.
install_dump_reader() {
  docker exec -i "$NODE" bash -c 'cat > /root/mdmp-summary && chmod +x /root/mdmp-summary' <<'READER'
#!/usr/bin/env bash
set -euo pipefail
u32() { od -An -tu4 -j "$2" -N4 "$1" | tr -d ' '; }
f="$1"
magic=$(dd if="$f" bs=1 count=4 2>/dev/null)
streams=$(u32 "$f" 8); rva=$(u32 "$f" 12)
threads=-1; mem=-1
for ((i = 0; i < streams; i++)); do
  off=$((rva + 12 * i))
  t=$(u32 "$f" "$off"); d=$(u32 "$f" $((off + 8)))
  [[ "$t" == 3 ]] && threads=$(u32 "$f" "$d")
  [[ "$t" == 5 ]] && mem=$(u32 "$f" "$d")
done
printf 'path=%s magic=%s bytes=%s owner=%s mode=%s streams=%s threads=%s memory_regions=%s\n' \
  "$f" "$magic" "$(stat -c %s "$f")" "$(stat -c %U:%G "$f")" "$(stat -c %a "$f")" \
  "$streams" "$threads" "$mem"
READER
}

dump_summary() { nshq "/root/mdmp-summary '$1'"; }
dump_field() { dump_summary "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2}'; }

# Force a real fault: gdb parks the main thread's program counter at address 0
# and detaches, so the next instruction fetch is a genuine SIGSEGV rather than a
# signal the kernel delivered on our behalf.
force_fault() {
  local pid="$1"
  printf '\n$ docker exec %s gdb -q -n -batch -p %s -ex '\''set $pc = 0'\'' -ex detach\n' "$NODE" "$pid"
  if docker exec "$NODE" gdb -q -n -batch -p "$pid" -ex 'set $pc = 0' -ex detach 2>&1 | tail -3; then
    return 0
  fi
  note "gdb did not complete; falling back to kill -SEGV (a delivered signal, not a faulting instruction)"
  run docker exec "$NODE" kill -SEGV "$pid"
}

journal_crash_lines() {
  nsh "journalctl -u trawld --no-pager --since '-3min' | grep -iE 'crashdump|minidump|FATAL signal|Main process exited|Scheduled restart' | tail -12"
}

# The sed is not cosmetic. trawld colours its output whether or not anything is
# watching, journald stores those bytes, and the escapes land between the field
# name and its value (`ESC[3mevent_type ESC[0m ESC[2m= ESC[0m"crash_dump"`), so
# a grep for `event_type="crash_dump"` finds nothing in a log that plainly
# contains it. ci/crashdump-image.sh strips them the same way.
verdict_line() { # verdict_line <daemon pid>
  docker exec "$NODE" journalctl _SYSTEMD_UNIT=trawld.service _PID="$1" \
    --no-pager -o cat --since '-10min' 2>/dev/null \
    | sed -e 's/\x1b\[[0-9;]*m//g' \
    | grep -F 'event_type="crash_dump"' | tail -1
}

# Wait until the daemon has published that verdict, and only then read anyone's
# capability masks. `systemctl is-active` and the monitor's existence both come
# BEFORE the parent has dropped CAP_SYS_PTRACE (ADR-0023 ruling 4).
wait_verdict() { # wait_verdict <daemon pid>
  if wait_for "$VERDICT_WAIT_SECS" "trawld logged its crash-dump verdict" \
    "[[ -n \$(verdict_line $1) ]]"; then
    printf '\n%s\n' "$(verdict_line "$1")"
    return 0
  fi
  docker exec "$NODE" journalctl -u trawld --no-pager -n 40 || true
  die "trawld pid $1 never logged a crash-dump verdict"
}


phase "1 header"

if [[ "$ALLOW_DIRTY" == 0 ]]; then
  if [[ -n "$(git -C "$repo_root" status --porcelain)" ]]; then
    git -C "$repo_root" status --short
    die "working tree is dirty; re-run with --allow-dirty if that is intended"
  fi
  note "working tree is clean"
else
  note "--allow-dirty: not checking the working tree"
  git -C "$repo_root" status --short || true
fi

run git -C "$repo_root" rev-parse HEAD
printf '\nargv: %s %s\n' "$(basename "${BASH_SOURCE[0]}")" "${ARGV[*]}"
run date -u
run uname -r
run docker --version

note "file paths below are shown as \$REPO/..., where \$REPO is the worktree this ran from"

printf '\npinned images:\n'
note "rust     $RUST_IMAGE"
note "debian   $DEBIAN_IMAGE"
note "postgres $POSTGRES_IMAGE"
note "cargo-deb $CARGO_DEB_VERSION (see the header comment for why this is pinned)"

if [[ ! -e "$YAMA" ]]; then
  ORIG_SCOPE=""
  note "yama is not present on this kernel; every scope case will be skipped"
else
  ORIG_SCOPE=$(cat "$YAMA")
  printf '\n$ cat %s   # host, before anything runs\n%s\n' "$YAMA" "$ORIG_SCOPE"
fi


phase "2 build"

if [[ -n "$PACKAGES" ]]; then
  [[ -d "$PACKAGES" ]] || die "--packages $PACKAGES does not exist"
  PACKAGES=$(cd "$PACKAGES" && pwd)
  note "skipped: using --packages $PACKAGES"
else
  target_dir="$repo_root/target/deb-harness"
  case "$(uname -m)" in
    x86_64) build_target=x86_64-unknown-linux-gnu ;;
    aarch64) build_target=aarch64-unknown-linux-gnu ;;
    *) die "unsupported distribution build host: $(uname -m)" ;;
  esac
  PACKAGES="$target_dir/$build_target/debian"

  printf '\n$ docker build %s   # FROM %s\n' "$BUILDER_IMAGE" "$RUST_IMAGE"
  docker build -t "$BUILDER_IMAGE" -q - <<EOF | sed 's/^/  /'
FROM $RUST_IMAGE
RUN apt-get update && apt-get install -y --no-install-recommends python3 curl dpkg-dev \
 && rm -rf /var/lib/apt/lists/*
# Only amd64 uses Zig; arm64 uses the native GNU compiler in the Rust image.
RUN if [ "\$(uname -m)" = x86_64 ]; then \
      curl --proto '=https' --tlsv1.2 -fLsS https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz -o /tmp/zig.tar.xz \
      && echo '70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00  /tmp/zig.tar.xz' | sha256sum --check --strict \
      && mkdir /opt/zig && tar -xf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig \
      && ln -s /opt/zig/zig /usr/local/bin/zig && rm /tmp/zig.tar.xz; fi
RUN cargo install cargo-zigbuild --version $CARGO_ZIGBUILD_VERSION --locked
RUN rustup component add clippy rustfmt rust-analyzer
RUN cargo install cargo-deb --version $CARGO_DEB_VERSION --locked \\
 && chmod -R a+rwX "\$CARGO_HOME" "\$RUSTUP_HOME"
EOF

  runq docker volume create "$CARGO_VOLUME"
  # A fresh named volume is root-owned; the build runs as the invoking uid.
  docker run --rm --user 0:0 -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    "$BUILDER_IMAGE" chown -R "$(id -u):$(id -g)" /usr/local/cargo/registry
  note "cargo registry cache: docker volume $CARGO_VOLUME at /usr/local/cargo/registry"
  note "CARGO_TARGET_DIR: $target_dir (inside the worktree; /tmp here is a 16G tmpfs)"

  # rust-embed scans this directory at macro-expansion time, so trawl-web will
  # not compile without it.
  dist="$repo_root/crates/trawl-web-ui/dist"
  if [[ -f "$dist/index.html" ]]; then
    note "SPA dist present at $dist (not stubbed)"
  else
    mkdir -p "$dist"
    # Marked before the write, like SCOPE_MODIFIED.
    STUB_CREATED="$dist/index.html"
    printf '%s\n' '<!doctype html><title>trawl</title><p>crashdump-harness stub SPA</p>' > "$dist/index.html"
    note "STUBBED the SPA: wrote a one-line placeholder to $dist/index.html"
    note "the embedded web UI in this .deb is a stub, not a real build"
    note "this run created it, so teardown removes it again"
  fi

  mkdir -p "$target_dir"
  # Preserve linked-worktree provenance without permitting Git metadata writes.
  common_git=$(git -C "$repo_root" rev-parse --path-format=absolute --git-common-dir)
  builder=(docker run --rm --user "$(id -u):$(id -g)"
    -v "$repo_root:$repo_root" -w "$repo_root"
    -v "$common_git:$common_git:ro"
    -v "$CARGO_VOLUME:/usr/local/cargo/registry"
    -e "CARGO_TARGET_DIR=$target_dir" -e "XDG_CACHE_HOME=$target_dir/cache" "$BUILDER_IMAGE")
  run "${builder[@]}" bash scripts/release/build-distribution.sh \
    "$repo_root" "$build_target" "$target_dir/runtime"

  # The target directory is reused across runs, so a version bump leaves the
  # previous release's .deb sitting beside the new one.
  if compgen -G "$PACKAGES/*.deb" >/dev/null; then
    note "clearing .deb files left in $PACKAGES by an earlier run"
    rm -f "$PACKAGES"/*.deb
  fi

  # cargo-deb variants temporarily modify manifests. Keep those writes in a
  # disposable source snapshot even if the builder is killed without cleanup.
  # The --rm container owns /tmp; interrupted snapshots cannot accumulate in target.
  packager=(docker run --rm --user "$(id -u):$(id -g)"
    -v "$repo_root:$repo_root:ro" -v "$target_dir:$target_dir" -w "$repo_root"
    -v "$common_git:$common_git:ro"
    -v "$CARGO_VOLUME:/usr/local/cargo/registry"
    -e "CARGO_TARGET_DIR=$target_dir" "$BUILDER_IMAGE")
  run "${packager[@]}" python3 crates/trawl-server/debian/tests/package-snapshot.py \
    --source "$repo_root" --work-dir /tmp \
    --binaries "$target_dir/$build_target/release" --runtime "$target_dir/runtime" \
    --output "$PACKAGES" --target "$build_target"

fi

mapfile -t deb_candidates < <(find "$PACKAGES" -maxdepth 1 -name '*.deb' | sort)
[[ ${#deb_candidates[@]} == 3 ]] || die "expected exactly three distribution .debs in $PACKAGES"
declare -A package_paths=()
package_version=""
package_arch=""
for candidate in "${deb_candidates[@]}"; do
  name=$(dpkg-deb -f "$candidate" Package)
  version=$(dpkg-deb -f "$candidate" Version)
  arch=$(dpkg-deb -f "$candidate" Architecture)
  case "$name" in
    trawl-cli|trawl-server|trawl-runtime) ;;
    *) die "unexpected distribution package $name" ;;
  esac
  [[ -z "${package_paths[$name]:-}" ]] || die "duplicate distribution package $name"
  package_paths[$name]="$candidate"
  package_version="${package_version:-$version}"
  package_arch="${package_arch:-$arch}"
  [[ "$version" == "$package_version" && "$arch" == "$package_arch" ]] \
    || die "distribution packages have different versions or architectures"
  if [[ "$name" != trawl-runtime ]]; then
    depends=$(dpkg-deb -f "$candidate" Depends)
    [[ ", $depends," == *", trawl-runtime (= $version),"* ]] \
      || die "$name does not require the exact matching trawl-runtime"
  fi
  note "installing $(basename "$candidate")"
  sha256sum "$candidate" | sed "s|$repo_root|\$REPO|"
  run dpkg-deb -f "$candidate" Package Version Architecture Depends
done
DEB="${package_paths[trawl-server]}"


phase "3 up"

# A stopped container with a prior harness label is debris from an interrupted
# run or --keep. Clear it rather than reusing it: a container left
# over from an earlier build would quietly test the wrong .deb.
preflight=()
for stale in "$NODE" "$PG"; do
  removed=$(remove_stopped_harness_container "$stale") || die "container preflight refused $stale"
  [[ -z "$removed" ]] || preflight+=("$removed")
done
if docker network inspect "$NET" >/dev/null 2>&1; then
  docker network rm "$NET" >/dev/null 2>&1 || true
  preflight+=("network $NET")
fi
if (( ${#preflight[@]} )); then
  note "removed leftovers from an earlier run: ${preflight[*]}"
fi

printf '\n$ docker network create --label %s %s\n' "$RUN_LABEL" "$NET"
CREATED_NETWORK=$(docker network create --label "$RUN_LABEL" "$NET")
note "network id ${CREATED_NETWORK:0:12}"

printf '\n$ docker run -d --name %s --network %s --label %s %s\n' "$PG" "$NET" "$RUN_LABEL" "$POSTGRES_IMAGE"
pg_id=$(docker run -d --name "$PG" --network "$NET" --label "$RUN_LABEL" \
  -e POSTGRES_PASSWORD=harness -e POSTGRES_USER=postgres -e POSTGRES_DB=postgres \
  "$POSTGRES_IMAGE")
CREATED_CONTAINERS+=("$pg_id")
note "container id ${pg_id:0:12}"
wait_for 60 "postgres accepting connections" \
  "docker exec $PG pg_isready -U postgres" || die "postgres never came up"

run docker exec "$PG" psql -U postgres -v ON_ERROR_STOP=1 \
  -c "CREATE ROLE fleet LOGIN PASSWORD 'fleetpw';" \
  -c "CREATE DATABASE fleet OWNER fleet;" \
  -c "CREATE ROLE trawl LOGIN PASSWORD 'trawlpw';" \
  -c "CREATE DATABASE trawl OWNER trawl;"

printf '\n$ docker build %s   # FROM %s\n' "$NODE_IMAGE" "$DEBIAN_IMAGE"
docker build -t "$NODE_IMAGE" -q - <<EOF | sed 's/^/  /'
FROM $DEBIAN_IMAGE
ENV DEBIAN_FRONTEND=noninteractive container=docker
RUN apt-get update \\
 && apt-get install -y --no-install-recommends \\
      systemd systemd-sysv dbus sudo gdb procps ca-certificates
RUN systemctl mask \\
      systemd-udevd.service systemd-udev-trigger.service \\
      systemd-networkd.service systemd-resolved.service \\
      systemd-firstboot.service systemd-modules-load.service \\
      getty@tty1.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
EOF

printf '\n$ docker run -d --name %s --privileged --cgroupns=private --tmpfs /run --tmpfs /tmp --network %s --label %s %s /sbin/init\n' \
  "$NODE" "$NET" "$RUN_LABEL" "$NODE_IMAGE"
node_id=$(docker run -d --name "$NODE" --privileged --cgroupns=private \
  --tmpfs /run --tmpfs /tmp --network "$NET" --label "$RUN_LABEL" \
  "$NODE_IMAGE" /sbin/init)
CREATED_CONTAINERS+=("$node_id")
note "container id ${node_id:0:12}"
wait_for 60 "systemd up in $NODE" \
  "docker exec $NODE systemctl is-system-running --wait 2>/dev/null | grep -qE 'running|degraded'" \
  || die "systemd never finished booting in $NODE"

run docker exec "$NODE" systemctl is-system-running
nsh "dpkg-query -W -f='  \${Package} \${Version}\n' systemd gdb sudo procps libc6"
run docker exec "$PG" postgres --version
install_dump_reader


phase "4 install"

run docker exec "$NODE" mkdir -p "$PACKAGES_IN_NODE"
for name in trawl-runtime trawl-cli trawl-server; do
  run docker cp "${package_paths[$name]}" "$NODE:$PACKAGES_IN_NODE/$name.deb"
done

# `set -o pipefail` inside the container: a bash -c does not inherit it, so
# without this the pipeline reports tail's status and a failed configure would
# read as a clean install.
nsh "set -o pipefail
DEBIAN_FRONTEND=noninteractive apt-get install -y $PACKAGES_IN_NODE/*.deb 2>&1 | tail -8"

for name in trawl-runtime trawl-cli trawl-server; do
  install_status=$(nshq "dpkg-query -W -f='\${Status}' $name")
  printf '\n$ dpkg-query -W -f=%s %s\n%s\n' "'\${Status}'" "$name" "$install_status"
  [[ "$install_status" == "install ok installed" ]] \
    || die "$name is '$install_status', not 'install ok installed' — the package did not configure"
done
run docker exec "$NODE" dpkg-query -S /usr/lib/trawl/libduckdb.so
run docker exec "$NODE" trawl --version

printf '\n$ dpkg-deb -c %s | grep examples/crashdump.conf\n' "$(basename "$DEB")"
dpkg-deb -c "$DEB" | grep 'examples/crashdump.conf' \
  || die "the .deb does not ship usr/share/doc/trawl-server/examples/crashdump.conf"

nsh "cat >> /etc/default/trawld <<'EOF'
FLEET_DATABASE_URL=$FLEET_DSN
TRAWL_DATABASE_URL=$TRAWL_DSN
EOF
chown root:trawl /etc/default/trawld && chmod 0640 /etc/default/trawld
grep -c '^[A-Z_]*DATABASE_URL=' /etc/default/trawld"

printf '\n$ docker exec -e DATABASE_URL=... %s /usr/bin/fleet-admin migrate\n' "$NODE"
docker exec -e "DATABASE_URL=$FLEET_DSN" "$NODE" /usr/bin/fleet-admin migrate

run docker exec "$NODE" systemctl restart trawld
wait_active
run docker exec "$NODE" systemctl show trawld -p MainPID -p ActiveState -p SubState


phase "5 A inert"

run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment

printf '\n$ test -e /etc/systemd/system/trawld.service.d\n'
if nshq "test -e /etc/systemd/system/trawld.service.d"; then
  die "a drop-in directory already exists; the package must ship capture off"
fi
note "absent — the package installs no drop-in"

pid=$(main_pid)
caps_report "$pid" "trawld"
[[ "$(cap_bit "$pid" CapEff)" == 0 ]] || die "the inert daemon already holds CAP_SYS_PTRACE in CapEff"
[[ "$(cap_bit "$pid" CapAmb)" == 0 ]] || die "the inert daemon already holds CAP_SYS_PTRACE in CapAmb"
note "CAP_SYS_PTRACE (bit $PTRACE_CAP_BIT) is clear in both CapEff and CapAmb"

# %F as well as the mode: stat follows symlinks, so a link pointing at some
# other 0700 trawl-owned directory would satisfy owner and mode alone.
run docker exec "$NODE" stat -c '%F %U %G %a' "$CORES"
[[ "$(nshq "stat -c '%F %U %G %a' $CORES")" == "directory trawl trawl 700" ]] \
  || die "$CORES is not a plain directory owned trawl:trawl mode 0700"

# The tmpfiles entry is `d=`. Whether this systemd honours the suffix is a
# question about the running version, not about the file we shipped.
nsh "rmdir $CORES && : > $CORES && stat -c 'planted: %F' $CORES
systemd-tmpfiles --create trawl.conf
stat -c 'after tmpfiles: %F %U:%G %a' $CORES"
[[ "$(nshq "stat -c '%F %U %G %a' $CORES")" == "directory trawl trawl 700" ]] \
  || die "systemd-tmpfiles left a non-directory at $CORES; this systemd does not honour the 'd=' type suffix"
note "systemd $(nshq "systemctl --version | head -1 | awk '{print \$2}'") replaced the planted file with the directory, so 'd=' is honoured here"

run docker exec "$NODE" systemctl show trawl-web -p InaccessiblePaths
web_masked=$(nshq "systemctl show trawl-web -p InaccessiblePaths --value")
[[ "$web_masked" == *"$CORES"* ]] \
  || die "trawl-web.service does not mask $CORES (InaccessiblePaths='$web_masked')"

# The start below is only evidence about the mask once the pinned certificate
# exists. Without the file trawl-web starts anyway and waits for it.
printf '\n$ grep -Fx %s /etc/trawl/trawld.toml\n' "'upstream_ca_path = \"$WEB_UPSTREAM_CA\"'"
nshq "grep -Fx 'upstream_ca_path = \"$WEB_UPSTREAM_CA\"' /etc/trawl/trawld.toml" \
  || die "the installed trawld.toml does not pin $WEB_UPSTREAM_CA for trawl-web"
wait_for "$ACTIVE_WAIT_SECS" "trawld wrote $WEB_UPSTREAM_CA" \
  "docker exec $NODE test -s $WEB_UPSTREAM_CA" \
  || { docker exec "$NODE" journalctl -u trawld --no-pager -n 40 || true
       die "trawld never wrote $WEB_UPSTREAM_CA, the certificate trawl-web pins"; }

run docker exec "$NODE" systemctl restart trawl-web
wait_for "$ACTIVE_WAIT_SECS" "trawl-web is-active=active" \
  "[[ \$(docker exec $NODE systemctl is-active trawl-web 2>/dev/null) == active ]]" \
  || { docker exec "$NODE" journalctl -u trawl-web --no-pager -n 20 || true
       die "trawl-web did not start; the mask may be hiding web.cookie or $WEB_UPSTREAM_CA"; }
# Type=simple reports active on exec. trawl-web exits when the cookie or the
# pinned certificate read fails, except a certificate that is not there: then
# it logs upstream_ca_pending and keeps running.
sleep 3
[[ "$(nshq "systemctl is-active trawl-web")" == "active" ]] \
  || { docker exec "$NODE" journalctl -u trawl-web --no-pager -n 20 || true
       die "trawl-web exited after starting; check whether the mask hid /var/lib/trawl/web.cookie or $WEB_UPSTREAM_CA"; }
web_invocation=$(nshq "systemctl show trawl-web -p InvocationID --value" | tr -d ' \r')
[[ -n "$web_invocation" ]] || die "systemd reports no invocation id for the running trawl-web"
web_journal=$(nshq "journalctl _SYSTEMD_INVOCATION_ID=$web_invocation --no-pager -o cat")
# Positive control: trawl-web logs "loaded config" only after resolving its
# pin, so without that line the missing upstream_ca_pending proves nothing.
grep -q "loaded config" <<<"$web_journal" \
  || die "the journal for trawl-web invocation $web_invocation has no 'loaded config' line, so the pin check cannot see this run"
if grep -q upstream_ca_pending <<<"$web_journal"; then
  docker exec "$NODE" journalctl -u trawl-web --no-pager -n 20 || true
  die "trawl-web started without $WEB_UPSTREAM_CA, which trawld had written; the mask may be hiding it"
fi

nsh "ps -o pid,user,group,args -p \$(systemctl show trawld -p MainPID --value) -p \$(systemctl show trawl-web -p MainPID --value)
id trawl-web
stat -c '%n %U:%G %a' /var/lib/trawl /var/lib/trawl/web.cookie /var/lib/trawl/cores /etc/trawl/trawld.toml"
web_user=$(nshq "ps -o user= -p \$(systemctl show trawl-web -p MainPID --value)" | tr -d ' \r')
[[ "$web_user" == "trawl-web" ]] \
  || die "trawl-web runs as '$web_user', not trawl-web; same-uid access defeats every dump protection here"
[[ "$(nshq "stat -c '%U:%G %a' /var/lib/trawl/cores")" == "trawl:trawl 700" ]] \
  || die "the dump directory is not trawl:trawl 0700, so the uid split alone would be carrying it"
note "the proxy runs as trawl-web; the dump directory is trawl:trawl 0700, denying that uid by mode as well"

# No dumps exist yet, so an empty directory on both sides would prove nothing.
# Plant a sentinel in the real directory AFTER the proxy started: the mask is a
# mount established at unit start, and nothing underneath it shows through.
web_pid=$(docker exec "$NODE" systemctl show trawl-web -p MainPID --value | tr -d ' \r')
nsh "install -o trawl -g trawl -m 0600 /dev/null $CORES/sentinel-not-a-dump
printf 'real directory:      '; ls -A $CORES | tr '\n' ' '; echo
printf 'trawl-web sees:      '; ls -A /proc/$web_pid/root$CORES | tr '\n' ' '; echo '(nothing)'
stat -c 'web.cookie in that namespace: %n %U %a' /proc/$web_pid/root/var/lib/trawl/web.cookie"
[[ -n "$(nshq "ls -A $CORES")" ]] || die "the sentinel was not created; the check below would prove nothing"
[[ -z "$(nshq "ls -A /proc/$web_pid/root$CORES")" ]] \
  || die "trawl-web can see the contents of $CORES despite InaccessiblePaths"
note "the sentinel is invisible inside trawl-web's mount namespace, web.cookie is not"
nshq "rm -f $CORES/sentinel-not-a-dump"


phase "6 B enable"

docs_block=$(awk '
  /^### Enable capture on Debian$/ { found = 1; next }
  found && /^   ```bash$/ { inblock = 1; next }
  inblock && /^   ```$/ { exit }
  inblock { sub(/^   /, ""); print }
' "$DOCS_PAGE")
if [[ "$docs_block" != "$ENABLE_CMD" ]]; then
  printf '\n--- docs %s ---\n%s\n--- harness ---\n%s\n' "$(rel "$DOCS_PAGE")" "$docs_block" "$ENABLE_CMD"
  die "the enable command in the docs no longer matches the one this harness runs"
fi
note "enable command matches $DOCS_PAGE"

printf '\n$ docker exec -i %s bash -s   # the docs enable command, verbatim:\n%s\n' "$NODE" "$ENABLE_CMD"
docker exec -i "$NODE" bash -s <<< "$ENABLE_CMD"

wait_active
run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment


# Sets SCOPE_READY (1 usable, 0 skip) and SCOPE_SKIP_REASON, rather than an exit
# status: a function called as an `if` condition runs with errexit suppressed
# for its whole body.
SCOPE_READY=0
SCOPE_SKIP_REASON=""

ensure_scope() { # ensure_scope <wanted>
  local want="$1" cur readback
  SCOPE_READY=0
  SCOPE_SKIP_REASON=""

  if [[ -z "$ORIG_SCOPE" ]]; then
    SCOPE_SKIP_REASON="no $YAMA on this kernel"
    return 0
  fi

  cur=$(nshq "cat $YAMA")
  printf '\n$ docker exec %s cat %s   # shared kernel: this IS the host value\n%s\n' "$NODE" "$YAMA" "$cur"

  if (( want < ORIG_SCOPE )); then
    SCOPE_SKIP_REASON="the host runs ptrace_scope=$ORIG_SCOPE and this harness never lowers it"
    return 0
  fi
  if (( want > cur )); then
    if (( ALLOW_HOST_SYSCTL == 0 )); then
      SCOPE_SKIP_REASON="requires --allow-host-sysctl"
      return 0
    fi
    printf '\n$ docker exec %s bash -c '\''echo %s > %s'\''\n' "$NODE" "$want" "$YAMA"
    SCOPE_MODIFIED=1
    SCOPE_EXPECTED="$want"
    set_host_scope "$want"
    readback=$(nshq "cat $YAMA")
    [[ "$readback" == "$want" ]] \
      || die "asked the kernel for ptrace_scope=$want, it reads $readback"
    note "raised $cur -> $want, confirmed by readback (restored to $ORIG_SCOPE by the exit trap)"
  fi

  readback=$(nshq "cat $YAMA")
  [[ "$readback" == "$want" ]] \
    || die "scope case $want would run against ptrace_scope=$readback"

  SCOPE_READY=1
  return 0
}

scope_case() { # scope_case <scope>
  local scope="$1" pid mon before after restarts_before restarts_after newest threads

  run docker exec "$NODE" systemctl restart trawld
  wait_active
  printf '\n$ docker exec %s cat %s\n%s\n' "$NODE" "$YAMA" "$(nshq "cat $YAMA")"

  pid=$(main_pid)
  wait_for 20 "monitor process found" "find_monitor $pid" || die "no crash-dump monitor under pid $pid"
  mon=$(find_monitor "$pid")
  printf '\n$ tr "\\0" "\\n" < /proc/%s/environ | grep TRAWL_CRASHDUMP_MONITOR\n' "$mon"
  nshq "tr '\0' '\n' < /proc/$mon/environ | grep '^TRAWL_CRASHDUMP_MONITOR='"
  wait_verdict "$pid"
  printf '\ncapabilities (bit %s = CAP_SYS_PTRACE):\n' "$PTRACE_CAP_BIT"
  caps_report "$pid" "daemon"
  caps_report "$mon" "monitor"
  [[ "$(cap_bit "$pid" CapEff)" == 0 ]] || die "scope $scope: the daemon still holds CAP_SYS_PTRACE after startup"
  [[ "$(cap_bit "$mon" CapEff)" == 1 ]] || die "scope $scope: the monitor did not inherit CAP_SYS_PTRACE"

  before=$(dump_count)
  restarts_before=$(docker exec "$NODE" systemctl show trawld -p NRestarts --value | tr -d '\r')
  note "dumps before the fault: $before   NRestarts=$restarts_before"

  force_fault "$pid"

  wait_for "$DUMP_WAIT_SECS" "a new dump appeared" "(( \$(docker exec $NODE bash -c 'ls -1 $CORES/*.dmp 2>/dev/null | wc -l') > $before ))" \
    || { journal_crash_lines; die "scope $scope: no new dump within ${DUMP_WAIT_SECS}s"; }

  after=$(dump_count)
  newest=$(newest_dump)
  printf '\n$ /root/mdmp-summary %s\n' "$newest"
  dump_summary "$newest"

  [[ "$(dump_field "$newest" magic)" == "MDMP" ]] || die "scope $scope: $newest is not a minidump"
  [[ "$(dump_field "$newest" owner)" == "trawl:trawl" ]] || die "scope $scope: $newest is not owned by trawl:trawl"
  [[ "$(dump_field "$newest" mode)" == "600" ]] || die "scope $scope: $newest is not mode 600"
  (( $(dump_field "$newest" bytes) > 0 )) || die "scope $scope: $newest is empty"
  threads=$(dump_field "$newest" threads)
  (( threads > 0 )) || die "scope $scope: $newest captured $threads threads — the monitor could not ptrace"
  note "scope $scope: dumps $before -> $after, $threads threads captured"

  journal_crash_lines
  wait_active
  restarts_after=$(docker exec "$NODE" systemctl show trawld -p NRestarts --value | tr -d '\r')
  note "NRestarts $restarts_before -> $restarts_after (Restart=on-failure brought trawld back)"
  (( restarts_after > restarts_before )) || die "scope $scope: systemd did not restart trawld"
}

POSITIVE_REQUESTED=${#SCOPES[@]}
POSITIVE_RUN=0
NOT_RUN=()

for scope in "${SCOPES[@]}"; do
  phase "C/D fault at yama ptrace_scope=$scope"
  ensure_scope "$scope"
  if (( SCOPE_READY )); then
    scope_case "$scope"
    POSITIVE_RUN=$((POSITIVE_RUN + 1))
  else
    note "skipped: $SCOPE_SKIP_REASON"
    NOT_RUN+=("crash case at ptrace_scope=$scope ($SCOPE_SKIP_REASON)")
  fi
done


phase "E negative (capability removed)"

NEGATIVE_READY=0
if [[ "$RUN_NEGATIVE" == 0 ]]; then
  note "skipped: --no-negative"
else
  ensure_scope 2
  if (( SCOPE_READY )); then
    NEGATIVE_READY=1
  else
    note "skipped: $SCOPE_SKIP_REASON"
    NOT_RUN+=("negative control at ptrace_scope=2 ($SCOPE_SKIP_REASON)")
  fi
fi

if (( NEGATIVE_READY )); then
  cat <<'EOF'

  Control: the same drop-in with AmbientCapabilities removed. The two
  Environment lines stay, so trawld still starts its monitor and still logs
  "trawl-crashdump: enabled". The assertion is that NO dump created by this
  crash carries thread data. A zero-thread file is a pass, and is what
  actually happens.
EOF

  nsh "cat > /etc/systemd/system/trawld.service.d/crashdump.conf <<'EOF'
[Service]
Environment=TRAWL_CRASH_DUMP_DIR=$CORES
Environment=TRAWL_CRASH_DUMP_RETAIN=10
EOF
cat /etc/systemd/system/trawld.service.d/crashdump.conf"

  run docker exec "$NODE" systemctl daemon-reload
  run docker exec "$NODE" systemctl restart trawld
  wait_active
  run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment

  neg_pid=$(main_pid)
  wait_for 20 "monitor process found" "find_monitor $neg_pid" || die "no monitor under pid $neg_pid"
  neg_mon=$(find_monitor "$neg_pid")
  wait_verdict "$neg_pid"
  caps_report "$neg_pid" "daemon"
  caps_report "$neg_mon" "monitor"
  [[ "$(cap_bit "$neg_pid" CapEff)" == 0 ]] || die "the control still grants the daemon CAP_SYS_PTRACE"
  [[ "$(cap_bit "$neg_mon" CapEff)" == 0 ]] || die "the control still grants the monitor CAP_SYS_PTRACE"

  mapfile -t neg_before_paths < <(dump_paths)
  neg_restarts_before=$(nrestarts)
  note "dumps before the fault: ${#neg_before_paths[@]}   NRestarts=$neg_restarts_before"

  force_fault "$neg_pid"

  # Judge only once the crash has fully played out.
  # The dump, if there is one, is written before the process dies:
  # the crash handler blocks on the monitor's ack.
  wait_for "$DUMP_WAIT_SECS" "faulted pid $neg_pid is gone" \
    "! docker exec $NODE test -d /proc/$neg_pid" \
    || die "pid $neg_pid never exited after the fault"
  wait_for "$DUMP_WAIT_SECS" "systemd restarted the unit" \
    "(( \$(docker exec $NODE systemctl show trawld -p NRestarts --value | tr -d ' \r') > $neg_restarts_before ))" \
    || die "systemd did not restart trawld after the negative-control fault"
  note "settling ${NEG_SETTLE_SECS}s before judging (a working capture lands within a second of the fault)"
  sleep "$NEG_SETTLE_SECS"

  mapfile -t neg_after_paths < <(dump_paths)
  neg_new_paths=()
  for p in "${neg_after_paths[@]}"; do
    seen=0
    for q in "${neg_before_paths[@]}"; do
      [[ "$p" == "$q" ]] && { seen=1; break; }
    done
    (( seen )) || neg_new_paths+=("$p")
  done

  if (( ${#neg_new_paths[@]} == 0 )); then
    note "outcome: no new dump at all"
  else
    note "${#neg_new_paths[@]} new dump(s) from this crash; every one must be empty of thread data"
    for p in "${neg_new_paths[@]}"; do
      printf '\n$ /root/mdmp-summary %s\n' "$p"
      dump_summary "$p"
      neg_threads=$(dump_field "$p" threads)
      neg_mem=$(dump_field "$p" memory_regions)
      (( neg_threads == 0 )) \
        || die "negative control: $p captured $neg_threads threads without CAP_SYS_PTRACE — the capability line is not load-bearing, or the drop-in did not take effect"
      (( neg_mem == 0 )) \
        || die "negative control: $p captured $neg_mem memory regions without CAP_SYS_PTRACE"
    done
    note "outcome: ${#neg_new_paths[@]} file(s) appeared, all with 0 threads and 0 memory regions"
  fi
  journal_crash_lines

  note "restoring the shipped drop-in"
  nsh "install -D -m 0644 /usr/share/doc/trawl-server/examples/crashdump.conf /etc/systemd/system/trawld.service.d/crashdump.conf && systemctl daemon-reload && systemctl restart trawld"
  wait_active
  run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities
fi


phase "F attack direction (trawl-web against the dumps)"

if (( POSITIVE_RUN == 0 )); then
  note "skipped: no crash case ran, so there are no dumps to try to reach"
else

docs_disable=$(awk '
  /^### Disable capture and delete dumps$/ { found = 1; next }
  found && /^   ```bash$/ { inblock = 1; next }
  inblock && /^   ```$/ { exit }
  inblock { sub(/^   /, ""); print }
' "$DOCS_PAGE")
if [[ "$docs_disable" != "$DISABLE_CMD" ]]; then
  printf '\n--- docs %s ---\n%s\n--- harness ---\n%s\n' "$(rel "$DOCS_PAGE")" "$docs_disable" "$DISABLE_CMD"
  die "the disable command in the docs no longer matches the one this harness runs"
fi
note "disable command matches $DOCS_PAGE"

printf '\n$ docker exec -i %s bash -s   # the docs disable command, verbatim:\n%s\n' "$NODE" "$DISABLE_CMD"
docker exec -i "$NODE" bash -s <<< "$DISABLE_CMD"
wait_active

run docker exec "$NODE" systemctl show trawld -p AmbientCapabilities -p Environment
attack_pid=$(main_pid)
caps_report "$attack_pid" "daemon"
[[ "$(cap_bit "$attack_pid" CapEff)" == 0 ]] \
  || die "trawld still holds CAP_SYS_PTRACE after the documented disable procedure"

attack_dump=$(newest_dump)
[[ -n "$attack_dump" ]] || die "no dumps left on disk; there would be nothing to try to steal"
note "capture is off, trawld is cap-less, and $(basename "$attack_dump") is still on disk"

# runuser initgroups, so this shell carries the trawl group exactly as the
# service does. That is the strongest form of the question: even holding the
# group, can this uid reach a dump?
# stdout goes to /dev/null and never to the transcript. Two of these probes cat
# a minidump, and the transcript gets committed as PR evidence.
probe() { # probe <label> <command...>; expects a permission denial
  local label="$1"; shift
  printf '\n$ runuser -u trawl-web -- %s\n' "$*"
  local err rc=0
  err=$(docker exec "$NODE" runuser -u trawl-web -- "$@" 2>&1 >/dev/null) || rc=$?
  err=$(printf '%s' "$err" | head -1 | cut -c1-200)
  printf '%s\nexit=%s\n' "${err:-(no stderr)}" "$rc"
  (( rc != 0 )) \
    || die "$label: unexpectedly readable as trawl-web (content withheld from this transcript deliberately)"
  grep -qiE 'permission denied|operation not permitted' <<< "$err" \
    || die "$label failed with something other than a permission error: $err"
  note "$label denied"
}

nsh "id trawl-web"
probe "listing the dump directory directly" ls -l "$CORES"
probe "reading a dump directly" cat "$attack_dump"
probe "listing the dump directory through /proc/$attack_pid/root" ls -l "/proc/$attack_pid/root$CORES"
probe "reading a dump through /proc/$attack_pid/root" cat "/proc/$attack_pid/root$attack_dump"

printf '\n$ runuser -u trawl -- ls -l %s   # control: the owner can still read them\n' "$CORES"
docker exec "$NODE" runuser -u trawl -- ls -l "$CORES"
note "the trawl user reads its own dumps; the denials above are the uid split and the 0700 mode, not missing files"
fi

phase "result"

note "crash cases executed: $POSITIVE_RUN of $POSITIVE_REQUESTED requested (${SCOPES[*]})"
if (( ${#NOT_RUN[@]} == 0 )); then
  note "every assertion passed"
else
  note "result: partial"
  for missing in "${NOT_RUN[@]}"; do
    note "  not run: $missing"
  done
  note "what ran passed, but this run does not cover what it was asked to cover"
  exit "$EXIT_PARTIAL"
fi
