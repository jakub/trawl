#!/usr/bin/env python3
"""Prove the sender recipes in the docs against a disposable Debian 13 VM.

This script produces the committed transcript for issue #198. It builds the
three distribution .debs from the checked-out commit, boots a throwaway
qemu/KVM guest from a pinned Debian cloud image, and installs PostgreSQL,
trawld, Vector, nginx, Docker and UFW in it. It then runs the proof blocks
from the two documentation pages exactly as they are printed.

Usage:
  run.py (--build | --packages DIR) [--image-cache DIR] [--keep] [--allow-dirty]
         > transcript.txt

  --build            build the .debs from HEAD in the pinned builder container
  --packages DIR     use the three distribution .debs already in DIR
  --image-cache DIR  where the cloud image and the Vector .deb are cached
                     (default ~/.cache/trawl-evidence)
  --keep             leave the VM running and the run directory in place
  --allow-dirty      run against a dirty working tree

stdout is the transcript: each command as a `$ ` line (guest commands carry a
`debian@trawl-proof$` or `root@trawl-proof#` prompt), then its output, then a
PASS or FAIL line for each assertion. stderr carries progress only. The exit
status is 0 when every assertion passed, 1 when any failed, and 2 when the
run could not continue.

Host boundary. The host's Docker daemon runs only the build container. qemu
runs as the invoking user with user-mode networking and one loopback port
forward for SSH. A loopback HTTP server serves the cloud-init seed and stops
once the guest is up. The run directory, the VM disk and the build output all
live under $REPO/target, because /tmp on the author's host is a small tmpfs.
No host service is reconfigured, and no saved trawl CLI profile on the host
is read or used.

Secrets. The PostgreSQL passwords and both API keys are generated inside the
guest and stay in root-only files there. No guest script carries a secret as
a literal, and no secret passes through this process: the final token check
pipes each key from the guest straight into grep's standard input.
"""

# Assertion and ownership model
# =============================
#
# Absence and delta checks. Each one names what makes it non-vacuous. Every
# absence goes through expect_absent or once_query, so a query that never
# read successfully fails instead of passing on zero rows.
#
# - history, nginx pre-start line absent: `service=nginx "<seed>-nginx-pre"
#   last=1d`, read once, and only after the control of the same shape
#   (`service=nginx "<seed>-nginx-post" last=1d _ingested>=...`) found the
#   post-start line. A wrongly shipped pre-start line keeps service=nginx.
# - history, Docker pre-start line absent: `host=<seed> "<seed> docker-pre"
#   last=1d`. Its control is the identical shape for "docker-post". A wrongly
#   shipped line keeps the container-derived host.
# - negatives (401, invalid_env, env_not_allowed), marker absent: an
#   env-free harness query, `service=<marker> "<marker>" last=1d
#   _ingested>=<T0>`, polled for the full window. It has no env, host or
#   _producer predicate, so an event accepted under the edited env still
#   matches. Its control runs the same query shape for the marker sent after
#   the restore, and must find it. The doc's journald-check runs once for the
#   transcript only: its `env=prod` would hide an event accepted as Prod or
#   staging. Two more checks close the input side: the marker's unit wrote
#   its line to the local journal, and Vector was still active when the
#   window ended.
# - invalid_env and env_not_allowed, rejection counter rose: the counter is
#   labelled by reason only, so a delta cannot be tied to the marker's batch.
#   Every journald event Vector ships in that window (sshd and sudo lines
#   from this harness among them) carries the edited env. The delta proves
#   that trawld rejected Vector's traffic for that reason. The env-free
#   absence proves the marker was not stored. Both reads of /metrics must
#   succeed, so a failed read cannot pose as 0.
# - invalid_env and env_not_allowed, Vector logs no sink error: the journal
#   read must succeed. The 401 case, which runs first, is the control: the
#   same pipeline must have found the 401 there, or these checks fail.
# - first start, Vector dropped no events: the window is Vector's whole
#   first run, from VECTOR_START to a timestamp taken just before its first
#   restart (the 401 case's), so a backfill batch dropped late still counts.
#   The read runs after that restart and must succeed and hold at least one
#   Vector line. Its control is the 401 case, which reads its own window
#   through the same drop_extraction() and must count more than zero
#   dropped events there (Vector 0.57 logs "Events dropped
#   intentional=false count=N" for a non-retriable 401). The verdict, with
#   both window bounds, is written only then; if the 401 case never runs or
#   Vector is never restarted, it is written as FAIL.
# - the key in /etc/default/vector equals the minted key: the minted file
#   must be non-empty, so two empty files cannot compare equal.
# - token absence: each key's grep pipe is first proven to find that key in
#   a control file. grep must exit 1; exit 2 (an error) fails.
#
# Build identity. trawld and trawl print `<version> (<short sha>[*] <date>,
# ...)` for --version, where `*` marks a build from a dirty tree. Right after
# the install, both must name HEAD's short SHA with no `*`. The package
# version alone cannot tell a stale --packages deb from this commit's build.
# The trawl check fails the run and stops it before any recipe, because the
# CLI runs every proof query. Under --allow-dirty the `*` is noted, not
# asserted.
#
# Preconditions. One run per checkout at a time, from a clean checkout
# (--allow-dirty runs are not evidence). No SPA build is needed. The
# guarantees below hold against this harness only: nothing here guards
# against another program writing to target/issue-198-deb, the cargo
# registry volume or the builder image tag.
#
# Host state this run owns. Nothing outside this list is written or removed.
#
# - $REPO/target (created if missing, never removed) and
#   $REPO/target/issue-198-vm.XXXXXX (mkdtemp): the run directory with the
#   VM disk, keys, seed, logs, build cidfiles, the stub SPA in spa/, the
#   token checks' control files, the transcript copy and debs/, the only
#   package set that is hashed, copied into the guest and installed.
#   --build packages straight into it. --packages DIR copies DIR's .debs
#   into it first. No .deb outside it is ever deleted. It is removed at exit
#   unless --keep.
# - target/issue-198-deb: a shared cargo target and runtime cache. It is
#   used only under an exclusive flock on target/issue-198-deb/build.lock,
#   held from the builder image through packaging. The lock file is never
#   unlinked.
# - crates/trawl-web-ui/dist: never written into, and nothing under it is
#   ever deleted. trawl-web embeds that fixed path at compile time, so the
#   builder container gets $RUN/spa (a one-line index.html) bind-mounted
#   read-only over it; the host's dist/ is not touched. It is the mount
#   point, so the run creates the empty directory if it is missing, as
#   trawl-web's build.rs would, and leaves it in place.
# - Docker: three `docker run --rm` containers, chown (hands the cargo
#   volume to the invoking uid), build and package. Each goes through
#   build_container(): named with the run directory's unique basename and
#   its step, its id written to a cidfile in the run directory. A step that
#   ends normally is removed by --rm. A step that fails, or is cut short by
#   a signal, is removed by the id in its cidfile before the build lock is
#   released, so none is left writing to the shared caches. The containers
#   run the image id this run's `docker build` printed, not the tag. The
#   builder mounts the checkout read-write; cargo writes to
#   CARGO_TARGET_DIR. The builder image tag and the cargo registry volume
#   are shared by every checkout, so this checkout's build lock does not
#   serialize them across checkouts; cargo's own registry lock does.
# - --image-cache: created if missing. Downloads go to a mkstemp .part
#   file, then an atomic rename; the .part is removed on failure. The image
#   extracts into a mkdtemp staging directory. If another run publishes
#   disk.raw first, this run deletes its own staging copy. The cloud image
#   is checked against its sha512 on the host. The Vector .deb is checked
#   against its sha256 on the host and again in the guest.
# - loopback ports: the seed server binds port 0 itself. qemu must bind the
#   SSH forward port itself, so the port is chosen by bind-and-release. If
#   qemu reports that the forward could not be set up, the run tries a new
#   port, up to 3 times. SSH pins a host key generated for this run, so a
#   foreign listener can never be taken for the guest.
# - qemu: a child process of this run, stopped at exit unless --keep.

from __future__ import annotations

import argparse
import atexit
import fcntl
import functools
import http.server
import json
import os
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

# ------------------------------------------------------------------ paths --

EVIDENCE_DIR = Path(__file__).resolve().parent
REPO = EVIDENCE_DIR.parents[2]
VECTOR_PAGE = REPO / "docs/src/content/docs/getting-started/vector-integration.md"
INGESTION_PAGE = REPO / "docs/src/content/docs/operate/ingestion.md"
VECTOR_CONFIG_DIR = REPO / "config/vector/debian"
VECTOR_DROP_INS = ("base.toml", "nginx.toml", "docker.toml")

# ------------------------------------------------------------------- pins --

# The build container and tools match crates/trawl-server/debian/tests/
# crashdump-harness.sh, so the .debs come from the same packaging path.
RUST_IMAGE = (
    "rust:1.98-trixie@sha256:"
    "620dbcd124499c59e2406d3741574b5c5838cf9eb9656f0c3a03948f79b02959"
)
CARGO_DEB_VERSION = "3.8.0"
CARGO_ZIGBUILD_VERSION = "0.23.4"
ZIG_URL = "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz"
ZIG_SHA256 = "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
BUILDER_IMAGE = f"trawl-evidence-builder:cargo-deb-{CARGO_DEB_VERSION}"
BUILD_CONTAINER_PREFIX = "trawl-evidence-198-build"
CARGO_VOLUME = "trawl-evidence-cargo-registry"
BUILD_TARGET = "x86_64-unknown-linux-gnu"
BUILD_DIR = REPO / "target/issue-198-deb"
BUILD_LOCK = BUILD_DIR / "build.lock"
SPA_DIST = REPO / "crates/trawl-web-ui/dist"
SPA_STUB_TEXT = "<!doctype html><title>trawl</title><p>issue-198 evidence stub SPA</p>\n"

# Debian 13 genericcloud, one dated serial. The sha512 is the one published in
# that serial's SHA512SUMS on cloud.debian.org.
IMAGE_SERIAL = "20260914-2601"
IMAGE_TARBALL = f"debian-13-genericcloud-amd64-{IMAGE_SERIAL}.tar.xz"
IMAGE_URL = f"https://cloud.debian.org/images/cloud/trixie/{IMAGE_SERIAL}/{IMAGE_TARBALL}"
IMAGE_SHA512 = (
    "ba03aae045d06ee3ccd8fe6d4fdac58f7fbb25e635927f23055223ad8be75c21"
    "850fef84b8146066ec1f24ffab65f4fc86d55344845e72e73d9fd569b83f26a0"
)

# Vector is evidence setup, not a documented install path: the guide installs
# from Vector's apt repository, which serves whatever is newest. The run pins
# the GitHub release artifact and its published SHA256SUMS entry instead.
VECTOR_VERSION = "0.57.0"
VECTOR_DEB = f"vector_{VECTOR_VERSION}-1_amd64.deb"
VECTOR_URL = (
    f"https://github.com/vectordotdev/vector/releases/download/v{VECTOR_VERSION}/{VECTOR_DEB}"
)
VECTOR_SHA256 = "ee24ecf72292ce965b2fefc5439efa9281c75a982c4a90a2295e30561311e88e"

# ------------------------------------------------------------------ guest --

GUEST_HOSTNAME = "trawl-proof"
GUEST_USER = "debian"
GUEST_DISK_SIZE = "20G"
GUEST_MEMORY_MB = 4096
GUEST_CPUS = 4
GUEST_EVIDENCE = "/root/evidence"  # root-only: DSNs and API keys
GUEST_PACKAGES = f"/home/{GUEST_USER}/packages"
GUEST_CONFIG = f"/home/{GUEST_USER}/vector-config"
TRAWLD_URL = "https://localhost:5514"
TRAWLD_CA = "/var/lib/trawl/tls/cert.pem"
PROFILE = "prod"
TRAWL_ENV = "prod"


@dataclass(frozen=True)
class Peer:
    """A network namespace in the guest, reached over a veth pair."""

    netns: str
    guest_ip: str
    peer_ip: str
    role: str


# The UFW packet must come from another machine; the syslog frames from a
# distinct address the listener maps. Each gets its own namespace.
# The device takes 192.0.2.1, the address proof:syslog-config names, so that
# block runs byte-for-byte. The UFW peer's addresses go through proof:ufw-vars.
PEER_UFW = Peer("peer-ufw", "10.198.1.1", "10.198.1.2", "remote host for the UFW packet")
PEER_DEVICE = Peer("peer-dev", "192.0.2.254", "192.0.2.1", "SIMULATED DEVICE (syslog sender)")
DOCKER_WAIT_SECS = 30  # "Wait about 30 seconds" in the Docker recipe
BACKFILL_WAIT_SECS = 90

POLL_TRIES = 12
POLL_SECS = 5
QEMU_PORT_TRIES = 3
SSH_WAIT_SECS = 300
CLOUD_INIT_WAIT_SECS = 900

# ------------------------------------------------------- proof block list --

# D4: each executable doc block carries `<!-- proof:NAME -->` on the line
# immediately before its fence. The inventory is fixed; a missing, duplicate,
# or unknown marker fails the run.
BLOCK_INVENTORY: dict[Path, tuple[str, ...]] = {
    VECTOR_PAGE: (
        "key-write", "vector-start", "vars",
        "journald-send", "journald-check",
        "nginx-send", "nginx-confirm", "nginx-check",
        "docker-send", "docker-check", "docker-cleanup",
        "ufw-vars", "ufw-rule", "ufw-send", "ufw-check",
        "history-finder",
    ),
    INGESTION_PAGE: ("syslog-config", "syslog-vars", "syslog-firewall-allow", "syslog-check"),
}

EXIT_FAIL = 1
EXIT_FATAL = 2


class HarnessError(Exception):
    """The run cannot continue."""


class BlockInventoryError(HarnessError):
    """The docs' proof markers do not match the fixed inventory."""


# --------------------------------------------------------------- output --


def progress(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", file=sys.stderr, flush=True)


class Transcript:
    """stdout, mirrored to a copy the token check can scan."""

    def __init__(self) -> None:
        self.copy = None
        self.buffer: list[str] = []
        self.failures: list[str] = []
        self.passes = 0
        self.redactions: list[tuple[str, str]] = []

    def attach_copy(self, path: Path) -> None:
        self.copy = open(path, "w", encoding="utf-8")
        for text in self.buffer:
            self.copy.write(text)
        self.buffer.clear()
        self.copy.flush()

    def redact(self, path: Path, label: str) -> None:
        # Longest first, so the run directory wins over the repo it sits in.
        self.redactions.append((str(path), label))
        self.redactions.sort(key=lambda pair: len(pair[0]), reverse=True)

    def scrub(self, text: str) -> str:
        for raw, label in self.redactions:
            text = text.replace(raw, label)
        return text

    def out(self, text: str) -> None:
        text = self.scrub(text)
        sys.stdout.write(text)
        sys.stdout.flush()
        if self.copy is None:
            self.buffer.append(text)
        else:
            self.copy.write(text)
            self.copy.flush()

    def phase(self, name: str) -> None:
        self.out(f"\n{'=' * 78}\n== {name}\n{'=' * 78}\n")
        progress(f"phase: {name}")

    def note(self, text: str) -> None:
        self.out(f"# {text}\n")

    def command(self, prompt: str, script: str) -> None:
        lines = script.rstrip("\n").split("\n")
        self.out(f"\n{prompt} {lines[0]}\n")
        for line in lines[1:]:
            self.out(f"> {line}\n")

    def output(self, text: str) -> None:
        if text:
            self.out(text if text.endswith("\n") else text + "\n")

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        suffix = f": {detail}" if detail else ""
        self.out(f"{'PASS' if ok else 'FAIL'} {name}{suffix}\n")
        if ok:
            self.passes += 1
        else:
            self.failures.append(name)
            progress(f"FAIL {name}{suffix}")
        return ok

    def require(self, name: str, ok: bool, detail: str = "") -> None:
        if not self.check(name, ok, detail):
            raise HarnessError(name)


T = Transcript()


# ------------------------------------------------------------- commands --


def host(argv: list[str], *, shown: str | None = None, check: bool = True,
         echo: bool = True, stream: bool = False, tail: int = 20,
         timeout: float | None = None) -> subprocess.CompletedProcess:
    """Run a host command. `stream` sends output to stderr and keeps a tail."""
    if echo:
        T.command("$", shown or shlex.join(argv))
    if stream:
        proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                stdin=subprocess.DEVNULL, text=True, errors="replace")
        kept: list[str] = []
        assert proc.stdout is not None
        for line in proc.stdout:
            sys.stderr.write(line)
            kept.append(line)
            del kept[:-tail]
        code = proc.wait()
        text = "".join(kept)
        if echo:
            T.output(f"[last {len(kept)} lines]\n{text}" if kept else "")
        result = subprocess.CompletedProcess(argv, code, text, "")
    else:
        result = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                stdin=subprocess.DEVNULL, text=True, errors="replace",
                                timeout=timeout)
        if echo:
            T.output(result.stdout)
    if check and result.returncode != 0:
        if echo:
            T.out(f"[exit {result.returncode}]\n")
        raise HarnessError(f"host command failed ({result.returncode}): {shown or shlex.join(argv)}")
    return result


def die(msg: str) -> None:
    raise HarnessError(msg)


# ------------------------------------------------------------ doc blocks --

MARKER_RE = re.compile(r"^(?P<indent>[ \t]*)<!-- proof:(?P<name>[a-z0-9-]+) -->[ \t]*$")
LOOSE_MARKER_RE = re.compile(r"<!--\s*proof\s*:")
FENCE_OPEN_RE = re.compile(r"^(?P<indent>[ \t]*)(?P<fence>`{3,}|~{3,})(?P<info>[^`]*)$")


@dataclass(frozen=True)
class Block:
    name: str
    page: Path
    line: int  # 1-based line of the marker
    lang: str
    text: str

    @property
    def is_vars(self) -> bool:
        return self.name == "vars" or self.name.endswith("-vars")

    def where(self) -> str:
        return f"{self.page.relative_to(REPO)}:{self.line}"


def parse_blocks(page: Path) -> list[Block]:
    """Every marked block on one page, in order. Raises on a malformed marker."""
    lines = page.read_text(encoding="utf-8").split("\n")
    found: list[Block] = []
    for i, line in enumerate(lines):
        m = MARKER_RE.match(line)
        if not m:
            if LOOSE_MARKER_RE.search(line):
                raise BlockInventoryError(
                    f"{page.relative_to(REPO)}:{i + 1}: malformed proof marker {line.strip()!r}")
            continue
        name, indent = m["name"], m["indent"]
        opener = FENCE_OPEN_RE.match(lines[i + 1]) if i + 1 < len(lines) else None
        if opener is None or opener["indent"] != indent:
            raise BlockInventoryError(
                f"{page.relative_to(REPO)}:{i + 1}: proof:{name} is not immediately "
                "followed by a code fence at the same indent")
        fence = opener["fence"]
        close = re.compile(rf"^{re.escape(indent)}{re.escape(fence[0])}{{{len(fence)},}}[ \t]*$")
        body: list[str] = []
        for j in range(i + 2, len(lines)):
            if close.match(lines[j]):
                break
            raw = lines[j]
            if raw.startswith(indent):
                body.append(raw[len(indent):])
            elif raw.strip() == "":
                body.append("")
            else:
                raise BlockInventoryError(
                    f"{page.relative_to(REPO)}:{j + 1}: proof:{name} body leaves its indent")
        else:
            raise BlockInventoryError(
                f"{page.relative_to(REPO)}:{i + 1}: proof:{name} fence is never closed")
        found.append(Block(name, page, i + 1, opener["info"].strip(), "\n".join(body) + "\n"))
    return found


def load_blocks() -> dict[str, Block]:
    """The fixed inventory, read from this checkout. Fails closed."""
    problems: list[str] = []
    blocks: dict[str, Block] = {}
    for page, expected in BLOCK_INVENTORY.items():
        seen: dict[str, list[Block]] = {}
        for block in parse_blocks(page):
            seen.setdefault(block.name, []).append(block)
        rel = page.relative_to(REPO)
        for name in expected:
            hits = seen.get(name, [])
            if not hits:
                problems.append(f"{rel}: proof:{name} is missing")
            elif len(hits) > 1:
                where = ", ".join(str(b.line) for b in hits)
                problems.append(f"{rel}: proof:{name} appears {len(hits)} times (lines {where})")
            else:
                blocks[name] = hits[0]
        for name in sorted(set(seen) - set(expected)):
            problems.append(f"{rel}: proof:{name} is not in the inventory")
    if problems:
        raise BlockInventoryError("; ".join(problems))
    return blocks


def substitute(block: Block, values: dict[str, str]) -> str:
    """Site values in a *-vars block. No other block may be edited."""
    if not block.is_vars:
        raise HarnessError(f"proof:{block.name} is not a vars block; it must run byte-for-byte")
    text = block.text
    for old, new in values.items():
        if old not in text:
            raise HarnessError(f"proof:{block.name} has no {old!r} to substitute")
        text = text.replace(old, new)
    return text


# ----------------------------------------------------------------- guest --


class Guest:
    """SSH into the VM over its loopback port forward, with a pinned host key."""

    def __init__(self, port: int, key: Path, known_hosts: Path) -> None:
        self.port = port
        self.key = key
        self.known_hosts = known_hosts

    def ssh_base(self) -> list[str]:
        # -F /dev/null: nothing from the invoking user's ssh config or agent.
        return [
            "ssh", "-F", "/dev/null", "-i", str(self.key), "-p", str(self.port),
            "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
            "-o", f"UserKnownHostsFile={self.known_hosts}",
            "-o", "GlobalKnownHostsFile=/dev/null",
            "-o", "StrictHostKeyChecking=yes", "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=5", "-o", "LogLevel=ERROR",
            "-o", "ServerAliveInterval=15",
            f"{GUEST_USER}@127.0.0.1",
        ]

    def argv(self, script: str, root: bool) -> list[str]:
        shell = ["bash", "-euo", "pipefail", "-c", script]
        remote = (["sudo"] if root else []) + shell
        return self.ssh_base() + ["--", shlex.join(remote)]

    def run(self, script: str, *, root: bool = False, check: bool = True, echo: bool = True,
            timeout: float = 900, merge: bool = True) -> subprocess.CompletedProcess:
        """Run `script` with bash -euo pipefail in the guest; stdin is /dev/null."""
        if echo:
            T.command(f"root@{GUEST_HOSTNAME}#" if root else f"{GUEST_USER}@{GUEST_HOSTNAME}$",
                      script)
        result = subprocess.run(
            self.argv(script, root), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT if merge else subprocess.PIPE,
            text=True, errors="replace", timeout=timeout)
        if echo:
            T.output(result.stdout)
            if not merge:
                T.output(result.stderr)
        if check and result.returncode != 0:
            if echo:
                T.out(f"[exit {result.returncode}]\n")
            raise HarnessError(f"guest script exited {result.returncode}: {script.splitlines()[0]}")
        return result

    def copy_in(self, sources: list[Path], dest: str) -> None:
        argv = ["scp", "-q", "-F", "/dev/null", "-i", str(self.key), "-P", str(self.port),
                "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
                "-o", f"UserKnownHostsFile={self.known_hosts}",
                "-o", "GlobalKnownHostsFile=/dev/null",
                "-o", "StrictHostKeyChecking=yes", "-o", "BatchMode=yes",
                *[str(s) for s in sources], f"{GUEST_USER}@127.0.0.1:{dest}/"]
        shown = "scp " + " ".join(shlex.quote(str(s)) for s in sources) + f" {GUEST_HOSTNAME}:{dest}/"
        host(argv, shown=shown)

    def reachable(self) -> bool:
        probe = subprocess.run(self.ssh_base() + ["--", "true"], stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                               timeout=30)
        return probe.returncode == 0


class ProofShell:
    """Runs doc blocks as the operator would, carrying shell variables between them.

    Each block runs in its own `bash -euo pipefail` over SSH. Variables a block
    defines (MARKER, T0, HOST, ...) are saved to a 0600 state file and sourced
    before the next block, as if the reader kept one terminal open.
    """

    STATE = f"/home/{GUEST_USER}/.proof-state"

    def __init__(self, guest: Guest) -> None:
        self.guest = guest

    def reset(self) -> None:
        self.guest.run(f"rm -f {self.STATE}", echo=False)

    def wrap(self, text: str) -> str:
        if not text.endswith("\n"):
            text += "\n"
        return (
            f"__baseline=$(bash --norc --noprofile -c 'compgen -v' | sort)\n"
            f"if [ -f {self.STATE} ]; then . {self.STATE}; fi\n"
            f"{text}"
            f"(umask 077; for __v in $(comm -13 <(printf '%s\\n' \"$__baseline\") "
            f"<(compgen -v | sort)); do case $__v in __*|BASH_*|FUNCNAME|PIPESTATUS) ;; "
            f"*) declare -p \"$__v\" ;; esac; done > {self.STATE}.new) && mv {self.STATE}.new {self.STATE}\n"
        )

    def run_text(self, text: str, *, netns: str | None = None, check: bool = True,
                 echo: bool = True, label: str = "script") -> subprocess.CompletedProcess:
        """Run operator text with the saved variables; `netns` runs it on a peer."""
        if echo:
            T.command(f"{GUEST_USER}@{netns or GUEST_HOSTNAME}$", text)
        wrapped = self.wrap(text)
        if netns is None:
            result = self.guest.run(wrapped, check=False, echo=False)
        else:
            inner = shlex.join(["bash", "-euo", "pipefail", "-c", wrapped])
            result = self.guest.run(f"ip netns exec {netns} runuser -u {GUEST_USER} -- {inner}",
                                    root=True, check=False, echo=False)
        if echo:
            T.output(result.stdout)
        if check and result.returncode != 0:
            if echo:
                T.out(f"[exit {result.returncode}]\n")
            raise HarnessError(f"{label} exited {result.returncode}")
        return result

    def run_block(self, block: Block, text: str | None = None, *, netns: str | None = None,
                  check: bool = True, echo: bool = True) -> subprocess.CompletedProcess:
        if echo:
            how = "site values substituted" if text is not None else "verbatim"
            where = f", in network namespace {netns}" if netns else ""
            T.note(f"proof:{block.name} from {block.where()} ({how}{where})")
        return self.run_text(block.text if text is None else text, netns=netns, check=check,
                             echo=echo, label=f"proof:{block.name}")

    def var(self, name: str) -> str:
        result = self.guest.run(f". {self.STATE}; printf '%s' \"${{{name}:?}}\"", echo=False)
        return result.stdout


def json_rows(text: str) -> tuple[list[dict], list[str]]:
    """NDJSON rows from `trawl query` on a pipe, plus any line that was not a row."""
    rows: list[dict] = []
    other: list[str] = []
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            other.append(line)
            continue
        if isinstance(value, dict):
            rows.append(value)
        else:
            other.append(line)
    return rows, other


def expect_fields(label: str, rows: list[dict], expected: dict[str, str],
                  contains: dict[str, str] | None = None) -> bool:
    """One event shape: PASS only when a single row carries every `expected`
    value and every `contains` substring at once. A union across rows could
    assemble the shape from several events, so it proves nothing. Other rows
    may exist (retries can duplicate the event). Each row's matched and
    missed fields go into the transcript as diagnostics."""
    contains = contains or {}
    shape = ", ".join([f"{k}={v}" for k, v in expected.items()]
                      + [f"{k} contains {v!r}" for k, v in contains.items()])
    T.note(f"{label}: expected event: {shape}")
    matching = 0
    for n, row in enumerate(rows, 1):
        missed = [f"{k}={v} (got {str(row.get(k))[:80]!r})"
                  for k, v in expected.items() if str(row.get(k)) != v]
        missed += [f"{k} contains {v!r} (got {str(row.get(k))[:80]!r})"
                   for k, v in contains.items() if v not in str(row.get(k))]
        if missed:
            T.note(f"{label}: row {n} matches {len(expected) + len(contains) - len(missed)} "
                   f"of {len(expected) + len(contains)}; misses {'; '.join(missed)}")
        else:
            matching += 1
            T.note(f"{label}: row {n} matches every field")
    return T.check(f"{label}: one row carries the whole expected event", matching > 0,
                   f"{matching} of {len(rows)} row(s) match every field")


def poll_block(shell: ProofShell, block: Block, label: str, until=None,
               diagnose: bool = True) -> Polled:
    """Rerun a check block until it yields rows (and `until(rows)`), or give up.

    When a positive check gives up, the sink errors Vector logged meanwhile
    go into the transcript, so a miss names its cause.
    """
    T.note(f"proof:{block.name} from {block.where()} (verbatim; rerun up to {POLL_TRIES} "
           f"times, {POLL_SECS}s apart, as the guide says to rerun a check)")
    T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$", block.text)
    started = time.time()
    polled = _poll(lambda: shell.run_text(block.text, check=False, echo=False), label, until)
    if not polled.rows and diagnose:
        window = int(time.time() - started) + 60
        T.note(f"{label}: diagnostics, Vector's sink errors in the last {window}s")
        shell.guest.run(f"journalctl -u vector --since '-{window}s' -o cat "
                        "| sed -e 's/\\x1b\\[[0-9;]*m//g' | grep -E ' ERROR sink' "
                        "| grep -v 'has been suppressed' | cut -c1-300 | tail -6 || true",
                        root=True, check=False)
    return polled


def poll_query(guest: Guest, query: str, label: str, until=None) -> Polled:
    """A harness query, not a doc block, rerun until it yields rows."""
    shown = f"trawl -p {PROFILE} query {shlex.quote(query)}"
    T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$", shown)
    return _poll(lambda: guest.run(shown, check=False, echo=False, merge=False), label, until)


def marker_query(marker: str, t0: str) -> str:
    """The marker's journald event in any env: identity by unit and phrase only.

    No env, host or _producer predicate, so an event that trawld wrongly
    accepted under an edited TRAWL_ENV still matches.
    """
    return (f'service={marker} "{marker}" last=1d _ingested>="{t0}" '
            "| head 20 | table _time, _ingested, _producer, env, service, host, message")


def once_query(guest: Guest, query: str) -> list[dict]:
    shown = f"trawl -p {PROFILE} query {shlex.quote(query)}"
    result = guest.run(shown, check=False, merge=False)
    if result.returncode != 0:
        raise HarnessError(f"query failed: {query}")
    return json_rows(result.stdout)[0]


@dataclass(frozen=True)
class Polled:
    """What a poll saw. `reads` counts attempts whose query exited 0, so an
    empty result can be told apart from a query that never ran."""

    rows: list[dict]
    attempts: int
    reads: int
    last_read: int  # attempt number of the last successful query; 0 if none


def expect_absent(name: str, polled: Polled) -> bool:
    """Absence holds only when the final attempt read successfully: a failed
    query proves nothing, and an early empty read says nothing about the end
    of the window."""
    if polled.rows:
        return T.check(name, False, f"{len(polled.rows)} row(s)")
    if polled.last_read != polled.attempts:
        return T.check(name, False, f"the final query (attempt {polled.attempts}) did not "
                                    f"succeed; {polled.reads} of {polled.attempts} did")
    return T.check(name, True, f"0 rows; {polled.reads} of {polled.attempts} queries "
                               f"succeeded, the last on attempt {polled.last_read}")


def _poll(attempt, label: str, until) -> Polled:
    last = ""
    last_code = 0
    reads = last_read = 0
    for n in range(1, POLL_TRIES + 1):
        result = attempt()
        last, last_code = result.stdout, result.returncode
        rows, _ = json_rows(last)
        if result.returncode == 0:
            reads, last_read = reads + 1, n
            if rows and (until is None or until(rows)):
                T.output(last)
                T.note(f"{label}: rows on attempt {n} of {POLL_TRIES}")
                return Polled(rows, n, reads, last_read)
            progress(f"{label}: attempt {n}/{POLL_TRIES}: no matching rows")
        else:
            progress(f"{label}: attempt {n}/{POLL_TRIES}: query exited {result.returncode}")
        if n < POLL_TRIES:
            time.sleep(POLL_SECS)
    T.output(last or "(no output)")
    if last_code != 0:
        T.out(f"[exit {last_code}]\n")
    T.note(f"{label}: no matching rows after {POLL_TRIES} attempts, {POLL_SECS}s apart; "
           f"{reads} of {POLL_TRIES} queries succeeded")
    return Polled([], POLL_TRIES, reads, last_read)


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
DROP_COUNT_RE = re.compile(r"Events dropped intentional=\w+ count=(\d+)")


def drop_extraction(journal: str) -> tuple[int, list[str]]:
    """From `journalctl -u vector -o cat` text: the non-empty line count, and
    every line naming 'Events dropped' once colour codes are stripped. That
    includes Vector's "Internal log [Events dropped] is being suppressed"
    notices, which also mean events were dropped."""
    lines = [ANSI_RE.sub("", line) for line in journal.splitlines() if line.strip()]
    return len(lines), [line for line in lines if "Events dropped" in line]


def drop_count(drops: list[str]) -> int:
    """The events Vector reports dropped in `drops`, from each line's count=N.
    Suppression notices carry no count and add nothing."""
    return sum(int(m.group(1)) for line in drops if (m := DROP_COUNT_RE.search(line)))


def utc_prefix(value: object) -> str:
    """'2026-09-29 05:14:55.1+00' or '2026-09-29T05:14:55Z' -> '2026-09-29T05:14:55'."""
    return str(value).replace(" ", "T")[:19]


# ------------------------------------------------------------- lifecycle --


class Run:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.cache = Path(args.image_cache).expanduser().resolve()
        self.run_dir: Path | None = None
        self.qemu: subprocess.Popen | None = None
        self.httpd: http.server.ThreadingHTTPServer | None = None
        # The first Vector run's drop read: taken by restart_vector() over
        # [VECTOR_START, the first restart], and held until the 401 case has
        # proven the same extraction; see settle_first_start().
        self.first_start: tuple[int, list[str], str] | None = None
        self.first_start_settled = False
        # Set only while a build container may be running; see build_container().
        self.build_cidfile: Path | None = None
        self.guest: Guest | None = None
        self.shell: ProofShell | None = None
        self.packages: dict[str, Path] = {}
        self.package_version = ""
        self.short_sha = ""
        self.head = ""
        self.dirty = False
        self.seed_requests: list[str] = []
        self.state: dict[str, str] = {}
        self._blocks: dict[str, Block] | None = None
        self._block_error: BlockInventoryError | None = None
        self._cleaned = False

    # -- cleanup ----------------------------------------------------------

    def cleanup(self) -> None:
        if self._cleaned:
            return
        self._cleaned = True
        self.stop_httpd()
        self.remove_build_container()
        if self.args.keep:
            if self.qemu is not None and self.qemu.poll() is None:
                progress(f"--keep: VM left running, qemu pid {self.qemu.pid}")
                if self.guest is not None:
                    progress("ssh: " + shlex.join(self.guest.ssh_base()))
            if self.run_dir is not None:
                progress(f"--keep: run directory left at {self.run_dir}")
            return
        if self.qemu is not None and self.qemu.poll() is None:
            self.qemu.terminate()
            try:
                self.qemu.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.qemu.kill()
                self.qemu.wait()
            progress("stopped qemu")
        if self.run_dir is not None and self.run_dir.exists():
            shutil.rmtree(self.run_dir, ignore_errors=True)
            progress("removed the run directory")

    def remove_build_container(self) -> None:
        """Remove the build container this run created, and no other.

        Docker writes the container id to the cidfile only once it has created
        the container, so an empty or missing file means there is nothing of
        ours to remove.
        """
        cidfile, self.build_cidfile = self.build_cidfile, None
        if cidfile is None or not cidfile.exists():
            return
        cid = cidfile.read_text().strip()
        if cid:
            subprocess.run(["docker", "rm", "-f", cid], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
            progress("removed the build container this run created")

    def stop_httpd(self) -> None:
        if self.httpd is not None:
            self.httpd.shutdown()
            self.httpd.server_close()
            self.httpd = None
            progress("stopped the seed HTTP server")

    # -- blocks -----------------------------------------------------------

    def blocks(self) -> dict[str, Block]:
        if self._blocks is None and self._block_error is None:
            try:
                self._blocks = load_blocks()
            except BlockInventoryError as err:
                self._block_error = err
        if self._block_error is not None:
            raise self._block_error
        assert self._blocks is not None
        return self._blocks

    # -- phase 1 ----------------------------------------------------------

    def phase_header(self) -> None:
        T.phase("1 header")
        status = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain"],
                                stdout=subprocess.PIPE, text=True, check=True).stdout
        self.dirty = bool(status.strip())
        if self.dirty and not self.args.allow_dirty:
            sys.stderr.write(status)
            die("working tree is dirty; commit first, or pass --allow-dirty for a trial run")
        if self.dirty:
            T.note("--allow-dirty: the working tree is dirty; this run is not evidence")
            host(["git", "-C", str(REPO), "status", "--short"], shown="git status --short")
        else:
            T.note("working tree is clean")
        self.head = host(["git", "-C", str(REPO), "rev-parse", "HEAD"],
                         shown="git rev-parse HEAD").stdout.strip()
        self.short_sha = subprocess.run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"],
                                        stdout=subprocess.PIPE, text=True,
                                        check=True).stdout.strip()
        host(["git", "-C", str(REPO), "log", "-1", "--format=%H %s"], shown="git log -1 --format='%H %s'")
        T.out(f"\nargv: run.py {shlex.join(sys.argv[1:])}\n")
        host(["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"])
        host(["uname", "-sr"])
        host(["qemu-system-x86_64", "--version"])
        host(["docker", "--version"])
        T.note("paths below are shown as $REPO/... (this checkout) and $RUN/... (the run directory)")
        T.note(f"pins: builder {RUST_IMAGE}, cargo-deb {CARGO_DEB_VERSION}, "
               f"cargo-zigbuild {CARGO_ZIGBUILD_VERSION}")
        T.note(f"pins: image {IMAGE_TARBALL} sha512 {IMAGE_SHA512}")
        T.note(f"pins: vector {VECTOR_DEB} sha256 {VECTOR_SHA256}")
        host(["sha256sum", *[str(p) for p in BLOCK_INVENTORY]],
             shown="sha256sum " + " ".join(str(p.relative_to(REPO)) for p in BLOCK_INVENTORY))
        try:
            blocks = self.blocks()
        except BlockInventoryError as err:
            T.require("proof block inventory matches design D4", False, str(err))
        T.check("proof block inventory matches design D4", True, f"{len(blocks)} blocks")
        for block in blocks.values():
            T.note(f"proof:{block.name:<22} {block.where()}")

    # -- phase 2: build ---------------------------------------------------

    def phase_build(self) -> None:
        T.phase("2 build")
        assert self.run_dir is not None
        # The only package set this run hashes, copies into the guest and
        # installs. The run owns this directory; nothing else writes to it.
        debs = self.run_dir / "debs"
        debs.mkdir()
        if self.args.packages:
            src = Path(self.args.packages).resolve()
            if not src.is_dir():
                die(f"--packages {src} is not a directory")
            T.note(f"skipped the build: copying the .debs in --packages {src} into $RUN/debs")
            copy_debs(src, debs)
        else:
            self.build_debs(debs)
        self.collect_packages(debs)

    def build_debs(self, out: Path) -> None:
        """Build into the shared cargo cache under its lock; package into `out`."""
        BUILD_DIR.mkdir(parents=True, exist_ok=True)
        lock_fd = os.open(BUILD_LOCK, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                progress(f"waiting for {BUILD_LOCK}: another run is building")
                fcntl.flock(lock_fd, fcntl.LOCK_EX)
            T.note(f"holding an exclusive lock on {BUILD_LOCK} until packaging ends")
            try:
                self.build_locked(out)
            finally:
                # Before the lock goes: nothing of this run's may still be
                # writing to the shared cache.
                self.remove_build_container()
        finally:
            os.close(lock_fd)  # releases the flock

    def build_locked(self, out: Path) -> None:
        uid_gid = f"{os.getuid()}:{os.getgid()}"
        dockerfile = f"""FROM {RUST_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends python3 curl dpkg-dev \\
 && rm -rf /var/lib/apt/lists/*
# Only amd64 uses Zig; arm64 uses the native GNU compiler in the Rust image.
RUN if [ "$(uname -m)" = x86_64 ]; then \\
      curl --proto '=https' --tlsv1.2 -fLsS {ZIG_URL} -o /tmp/zig.tar.xz \\
      && echo '{ZIG_SHA256}  /tmp/zig.tar.xz' | sha256sum --check --strict \\
      && mkdir /opt/zig && tar -xf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig \\
      && ln -s /opt/zig/zig /usr/local/bin/zig && rm /tmp/zig.tar.xz; fi
RUN cargo install cargo-zigbuild --version {CARGO_ZIGBUILD_VERSION} --locked
RUN rustup component add clippy rustfmt rust-analyzer
RUN cargo install cargo-deb --version {CARGO_DEB_VERSION} --locked \\
 && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"
"""
        T.command("$", f"docker build -t {BUILDER_IMAGE} -q - <<'EOF'\n{dockerfile}EOF")
        built = subprocess.run(["docker", "build", "-t", BUILDER_IMAGE, "-q", "-"],
                               input=dockerfile, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
        T.output(built.stdout)
        if built.returncode != 0:
            die("the builder image did not build")
        # Run the image this build produced, by id: the tag is shared.
        image = (built.stdout.strip().splitlines() or [""])[-1].strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
            die(f"docker build -q printed no image id: {image!r}")
        host(["docker", "volume", "create", CARGO_VOLUME])
        # A fresh named volume is root-owned; the build runs as the invoking uid.
        host(["docker", "run", "--rm", *self.build_container("chown"), "--user", "0:0", "-v",
              f"{CARGO_VOLUME}:/usr/local/cargo/registry", image,
              "chown", "-R", uid_gid, "/usr/local/cargo/registry"])
        self.build_cidfile = None

        # rust-embed bakes crates/trawl-web-ui/dist into trawl-web at compile
        # time, from a path fixed in trawl-web's source with no build input to
        # redirect it. Nothing here touches the web UI, so the builder sees a
        # one-line stub SPA from this run's directory, bind-mounted read-only
        # over that path inside the container. The checkout's dist/ is only
        # the mount point: created empty if missing (as trawl-web's build.rs
        # would), never written into, and never removed.
        assert self.run_dir is not None
        spa = self.run_dir / "spa"
        spa.mkdir()
        (spa / "index.html").write_text(SPA_STUB_TEXT, encoding="utf-8")
        SPA_DIST.mkdir(parents=True, exist_ok=True)
        T.note("STUBBED the SPA: $RUN/spa is mounted over crates/trawl-web-ui/dist in the "
               "builder only; trawl-web in these .debs serves no real UI")

        common_git = subprocess.run(
            ["git", "-C", str(REPO), "rev-parse", "--path-format=absolute", "--git-common-dir"],
            stdout=subprocess.PIPE, text=True, check=True).stdout.strip()
        T.redact(Path(common_git), "$GIT_COMMON_DIR")
        T.note(f"CARGO_TARGET_DIR {BUILD_DIR}; cargo registry cache in docker volume {CARGO_VOLUME}")
        builder = ["docker", "run", "--rm", *self.build_container("build"), "--user", uid_gid,
                   "-v", f"{REPO}:{REPO}", "-w", str(REPO),
                   "-v", f"{spa}:{SPA_DIST}:ro",
                   "-v", f"{common_git}:{common_git}:ro",
                   "-v", f"{CARGO_VOLUME}:/usr/local/cargo/registry",
                   "-e", f"CARGO_TARGET_DIR={BUILD_DIR}", "-e", f"XDG_CACHE_HOME={BUILD_DIR}/cache",
                   image]
        host(builder + ["bash", "scripts/release/build-distribution.sh", str(REPO), BUILD_TARGET,
                        str(BUILD_DIR / "runtime")], stream=True)
        self.build_cidfile = None

        # The packages go straight into this run's own directory, never into
        # the shared target dir, so no other run's .deb is read or deleted.
        packager = ["docker", "run", "--rm", *self.build_container("package"), "--user", uid_gid,
                    "-v", f"{REPO}:{REPO}:ro", "-v", f"{BUILD_DIR}:{BUILD_DIR}",
                    "-v", f"{out}:{out}", "-w", str(REPO),
                    "-v", f"{common_git}:{common_git}:ro",
                    "-v", f"{CARGO_VOLUME}:/usr/local/cargo/registry",
                    "-e", f"CARGO_TARGET_DIR={BUILD_DIR}", image]
        host(packager + ["python3", "crates/trawl-server/debian/tests/package-snapshot.py",
                         "--source", str(REPO), "--work-dir", "/tmp",
                         "--binaries", str(BUILD_DIR / BUILD_TARGET / "release"),
                         "--runtime", str(BUILD_DIR / "runtime"),
                         "--output", str(out), "--target", BUILD_TARGET], stream=True)
        self.build_cidfile = None

    def build_container(self, step: str) -> list[str]:
        """`docker run` flags that name this run's container for `step` and record its id.

        The name carries the run directory's unique basename, so concurrent runs
        never share one. Cleanup removes by the id in the cidfile, not by name.
        """
        assert self.run_dir is not None
        self.build_cidfile = self.run_dir / f"{step}.cid"
        return ["--name", f"{BUILD_CONTAINER_PREFIX}-{self.run_dir.name}-{step}",
                "--cidfile", str(self.build_cidfile)]

    def collect_packages(self, pkg_dir: Path) -> None:
        debs = sorted(pkg_dir.glob("*.deb"))
        T.require("exactly three distribution .debs", len(debs) == 3,
                  ", ".join(d.name for d in debs))
        version = arch = ""
        for deb in debs:
            fields = host(["dpkg-deb", "-f", str(deb), "Package", "Version", "Architecture",
                           "Depends"], shown=f"dpkg-deb -f {deb.name} Package Version Architecture Depends")
            info = dict(line.split(": ", 1) for line in fields.stdout.splitlines() if ": " in line)
            name = info.get("Package", "")
            T.require(f"{deb.name} is a distribution package",
                      name in ("trawl-cli", "trawl-server", "trawl-runtime"), name)
            T.require(f"no duplicate {name}", name not in self.packages)
            self.packages[name] = deb
            version = version or info["Version"]
            arch = arch or info["Architecture"]
            T.require(f"{name} matches the set's version and architecture",
                      info["Version"] == version and info["Architecture"] == arch)
            if name != "trawl-runtime":
                T.require(f"{name} requires the exact trawl-runtime",
                          f"trawl-runtime (= {version})" in info.get("Depends", ""))
        self.package_version = version
        host(["sha256sum", *[str(d) for d in debs]],
             shown="sha256sum " + " ".join(d.name for d in debs))

    # -- phase 3: image ---------------------------------------------------

    def fetch(self, url: str, dest: Path) -> None:
        if dest.exists():
            T.note(f"cached: {dest.name}")
            return
        # A private partial file: a concurrent fetch of the same name never
        # writes into it. The rename publishes a whole file; the caller then
        # checks its digest.
        fd, name = tempfile.mkstemp(prefix=f"{dest.name}.", suffix=".part", dir=dest.parent)
        os.close(fd)
        part = Path(name)
        try:
            host(["curl", "--proto", "=https", "--tlsv1.2", "-fsSL", "-o", str(part), url],
                 shown=f"curl --proto =https --tlsv1.2 -fsSL -o {dest.name} {url}", timeout=1800)
            part.rename(dest)
        finally:
            part.unlink(missing_ok=True)

    def phase_image(self) -> Path:
        T.phase("3 image")
        self.cache.mkdir(parents=True, exist_ok=True)
        T.redact(self.cache, "$CACHE")
        T.note("$CACHE is the --image-cache directory")
        tarball = self.cache / IMAGE_TARBALL
        self.fetch(IMAGE_URL, tarball)
        digest = host(["sha512sum", str(tarball)]).stdout.split()[0]
        T.require("image sha512 matches the pinned SHA512SUMS entry", digest == IMAGE_SHA512)
        raw = self.cache / f"img-{IMAGE_SERIAL}" / "disk.raw"
        if not raw.exists():
            staging = Path(tempfile.mkdtemp(prefix=f"img-{IMAGE_SERIAL}.", dir=self.cache))
            try:
                host(["tar", "-xSJf", str(tarball), "-C", str(staging), "disk.raw"],
                     shown=f"tar -xSJf {IMAGE_TARBALL} disk.raw", timeout=600)
                staging.rename(raw.parent)
            except OSError:
                if not raw.exists():
                    raise
                T.note("another run published the extracted image first; using it")
            finally:
                # Only this run's staging copy, if the rename did not consume it.
                shutil.rmtree(staging, ignore_errors=True)
        vector_deb = self.cache / VECTOR_DEB
        self.fetch(VECTOR_URL, vector_deb)
        digest = host(["sha256sum", str(vector_deb)]).stdout.split()[0]
        T.require("vector .deb sha256 matches the v0.57.0 release SHA256SUMS", digest == VECTOR_SHA256)
        return raw

    # -- phase 4: boot ----------------------------------------------------

    def phase_boot(self, raw: Path) -> None:
        T.phase("4 boot")
        assert self.run_dir is not None
        rd = self.run_dir
        disk = rd / "disk.raw"
        host(["cp", "--reflink=auto", "--sparse=always", str(raw), str(disk)])
        host(["truncate", "-s", GUEST_DISK_SIZE, str(disk)])
        host(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "trawl-proof-client",
              "-f", str(rd / "client_ed25519")])
        host(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "trawl-proof-host",
              "-f", str(rd / "host_ed25519")])
        client_pub = (rd / "client_ed25519.pub").read_text().strip()
        host_pub = (rd / "host_ed25519.pub").read_text().strip()
        host_priv = (rd / "host_ed25519").read_text()

        seed = rd / "seed"
        seed.mkdir()
        user_data = {
            "hostname": GUEST_HOSTNAME,
            "manage_etc_hosts": True,
            "users": [{
                "name": GUEST_USER, "shell": "/bin/bash", "sudo": "ALL=(ALL) NOPASSWD:ALL",
                "lock_passwd": True, "ssh_authorized_keys": [client_pub],
            }],
            "ssh_pwauth": False,
            "disable_root": True,
            # The host key is generated by this run and pinned in known_hosts below.
            "ssh_deletekeys": True,
            "ssh_genkeytypes": ["ed25519"],
            "ssh_keys": {"ed25519_private": host_priv, "ed25519_public": host_pub},
            "growpart": {"mode": "auto", "devices": ["/"]},
            "resize_rootfs": True,
            "package_update": False,
        }
        # JSON is valid YAML, so the cloud-config needs no YAML library.
        (seed / "user-data").write_text("#cloud-config\n" + json.dumps(user_data, indent=2) + "\n")
        instance = f"trawl-proof-{uuid.uuid4()}"
        (seed / "meta-data").write_text(f"instance-id: {instance}\nlocal-hostname: {GUEST_HOSTNAME}\n")
        (seed / "vendor-data").write_text("")
        T.command("$", "cat $RUN/seed/user-data   # host private key elided")
        shown_ud = dict(user_data, ssh_keys={"ed25519_private": "<elided: generated for this run>",
                                             "ed25519_public": host_pub})
        T.output("#cloud-config\n" + json.dumps(shown_ud, indent=2))
        T.command("$", "cat $RUN/seed/meta-data")
        T.output((seed / "meta-data").read_text())

        self.start_httpd(seed)
        assert self.httpd is not None
        seed_port = self.httpd.server_address[1]
        T.note(f"seed served on 127.0.0.1:{seed_port}; the guest reaches it as 10.0.2.2:{seed_port}")

        for attempt in range(1, QEMU_PORT_TRIES + 1):
            ssh_port = free_port()
            if self.boot_qemu(disk, seed_port, ssh_port, host_pub):
                break
            T.note(f"qemu could not bind the SSH forward on 127.0.0.1:{ssh_port} "
                   f"(attempt {attempt} of {QEMU_PORT_TRIES}); another process took the port")
        else:
            die(f"qemu could not bind an SSH forward port in {QEMU_PORT_TRIES} attempts")
        assert self.guest is not None
        T.check("SSH answers with the pinned host key", True,
                f"StrictHostKeyChecking=yes against $RUN/known_hosts, port {ssh_port}")
        self.guest.run("cloud-init status --wait --long", timeout=CLOUD_INIT_WAIT_SECS)
        self.stop_httpd()
        fetched = sorted(set(self.seed_requests))
        T.note(f"seed requests served: {fetched}")
        T.check("cloud-init fetched user-data and meta-data over SMBIOS nocloud",
                {"/user-data", "/meta-data"} <= set(fetched))
        self.guest.run("hostname; uname -r; cat /etc/debian_version; "
                       "grep PRETTY_NAME /etc/os-release; findmnt -no SIZE,SOURCE /")
        size = self.guest.run("findmnt -bno SIZE /", echo=False).stdout.strip()
        T.check("growpart resized / to fill the 20G disk", int(size) > 15 * 1024**3,
                f"{int(size) // 1024**2} MiB")

    def boot_qemu(self, disk: Path, seed_port: int, ssh_port: int, host_pub: str) -> bool:
        """Start qemu and wait for SSH. False only when the port forward failed.

        The port was picked by bind-and-release, and qemu binds it itself, so
        another process can take it in between. qemu then exits at startup,
        and the caller retries on a new port. The pinned host key means a
        foreign listener on the port is never mistaken for the guest.
        """
        assert self.run_dir is not None
        rd = self.run_dir
        (rd / "known_hosts").write_text(f"[127.0.0.1]:{ssh_port} {host_pub}\n")
        self.guest = Guest(ssh_port, rd / "client_ed25519", rd / "known_hosts")
        self.shell = ProofShell(self.guest)
        argv = [
            "qemu-system-x86_64", "-name", GUEST_HOSTNAME,
            "-machine", "q35,accel=kvm", "-cpu", "host",
            "-smp", str(GUEST_CPUS), "-m", str(GUEST_MEMORY_MB),
            "-drive", f"file={disk},format=raw,if=virtio,discard=unmap",
            "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22",
            "-device", "virtio-net-pci,netdev=n0",
            "-object", "rng-random,filename=/dev/urandom,id=rng0",
            "-device", "virtio-rng-pci,rng=rng0",
            "-smbios", f"type=1,serial=ds=nocloud;s=http://10.0.2.2:{seed_port}/",
            "-display", "none", "-monitor", "none",
            "-serial", f"file:{rd / 'console.log'}",
            "-pidfile", str(rd / "qemu.pid"),
        ]
        T.command("$", shlex.join(argv))
        # A new session: a Ctrl-C at the terminal reaches this script, whose
        # cleanup then stops qemu (or leaves it under --keep).
        with open(rd / "qemu.log", "w") as log:
            self.qemu = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log,
                                         stderr=subprocess.STDOUT, start_new_session=True)
        T.note(f"qemu pid {self.qemu.pid}; serial console in $RUN/console.log")

        deadline = time.monotonic() + SSH_WAIT_SECS
        while True:
            if self.qemu.poll() is not None:
                log_text = (rd / "qemu.log").read_text(errors="replace")
                T.output(log_text)
                if re.search(r"host forwarding", log_text, re.IGNORECASE):
                    self.qemu = None
                    return False
                die(f"qemu exited with status {self.qemu.returncode} before SSH came up")
            if self.guest.reachable():
                return True
            if time.monotonic() > deadline:
                die(f"no SSH with the pinned host key after {SSH_WAIT_SECS}s")
            time.sleep(3)

    def start_httpd(self, root: Path) -> None:
        run = self

        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, fmt: str, *args) -> None:  # noqa: A003
                run.seed_requests.append(self.path.split("?")[0])
                progress("seed http: " + (fmt % args))

        handler = functools.partial(Handler, directory=str(root))
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    # -- phase 5: provision -----------------------------------------------

    def phase_provision(self) -> None:
        T.phase("5 provision")
        g = self.guest
        assert g is not None
        g.run("DEBIAN_FRONTEND=noninteractive apt-get update -q\n"
              "DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \\\n"
              "  postgresql nginx docker.io docker-cli ufw curl jq ca-certificates 2>&1 | tail -3",
              root=True, timeout=1500)

        g.run(f"mkdir -p {GUEST_PACKAGES} {GUEST_CONFIG}")
        g.copy_in([self.packages[n] for n in ("trawl-runtime", "trawl-cli", "trawl-server")]
                  + [self.cache / VECTOR_DEB], GUEST_PACKAGES)
        g.copy_in([VECTOR_CONFIG_DIR / name for name in VECTOR_DROP_INS], GUEST_CONFIG)
        g.run(f"cd {GUEST_PACKAGES} && sha256sum *.deb\n"
              f"echo '{VECTOR_SHA256}  {VECTOR_DEB}' | sha256sum --check --strict")
        g.run(f"cd {GUEST_CONFIG} && sha256sum *.toml")

        # -- PostgreSQL: both databases, passwords generated here, never printed.
        g.run(f"""install -d -m 0700 {GUEST_EVIDENCE}
umask 077
fleet_pw=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \\n')
trawl_pw=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \\n')
runuser -u postgres -- psql -q -v ON_ERROR_STOP=1 <<SQL
CREATE ROLE fleet LOGIN PASSWORD '$fleet_pw';
CREATE DATABASE fleet OWNER fleet;
CREATE ROLE trawl LOGIN PASSWORD '$trawl_pw';
CREATE DATABASE trawl OWNER trawl;
SQL
printf 'DATABASE_URL=postgres://fleet:%s@127.0.0.1:5432/fleet\\n' "$fleet_pw" > {GUEST_EVIDENCE}/fleet.env
printf 'FLEET_DATABASE_URL=postgres://fleet:%s@127.0.0.1:5432/fleet\\nTRAWL_DATABASE_URL=postgres://trawl:%s@127.0.0.1:5432/trawl\\n' \\
  "$fleet_pw" "$trawl_pw" > {GUEST_EVIDENCE}/trawld.env
runuser -u postgres -- psql -Atc "SELECT datname FROM pg_database WHERE datname IN ('fleet','trawl') ORDER BY 1"
""", root=True)

        # -- trawl packages.
        g.run(f"cd {GUEST_PACKAGES}\n"
              "DEBIAN_FRONTEND=noninteractive apt-get install -y -q "
              + " ".join(f"./{self.packages[n].name}" for n in ("trawl-runtime", "trawl-cli", "trawl-server"))
              + " 2>&1 | tail -4", root=True, timeout=600)
        for name in ("trawl-runtime", "trawl-cli", "trawl-server"):
            status = g.run(f"dpkg-query -W -f='${{Status}}' {name}", echo=False).stdout
            T.check(f"dpkg: {name} is 'install ok installed'", status == "install ok installed", status)
        version = g.run("trawld --version").stdout.strip()
        T.require("trawld --version names the tested commit", f"({self.short_sha}" in version,
                  f"expected ({self.short_sha}")
        self.check_not_dirty(version, stop=True)
        cli_version = g.run("trawl --version").stdout.strip()
        self.require_cli_build(cli_version)

        # -- trawld config: DSNs from the root-only env file, syslog listener.
        g.run(f"""cat {GUEST_EVIDENCE}/trawld.env >> /etc/default/trawld
chown root:trawl /etc/default/trawld && chmod 0640 /etc/default/trawld
grep -c '^[A-Z_]*DATABASE_URL=' /etc/default/trawld
(set -a; . /etc/default/trawld; set +a; trawld --check-config --config /etc/trawl/trawld.toml)
set -a; . {GUEST_EVIDENCE}/fleet.env; set +a
fleet-admin migrate
systemctl enable trawld 2>&1 | tail -1
systemctl restart trawld""", root=True)
        g.run(f"""for i in $(seq 60); do
  if [ -s {TRAWLD_CA} ] && curl -fsS --cacert {TRAWLD_CA} {TRAWLD_URL}/api/v1/health >/dev/null 2>&1; then break; fi
  sleep 2
done
systemctl is-active trawld
curl -fsS --cacert {TRAWLD_CA} {TRAWLD_URL}/api/v1/health | jq -c .
openssl x509 -in {TRAWLD_CA} -noout -subject -ext subjectAltName""", root=True)

        # -- keys: minted into root-only files. Only lengths are printed.
        # Set first: a mint that fails halfway may still have written a key.
        self.state["keys_minted"] = "1"
        g.run(f"""set -a; . {GUEST_EVIDENCE}/fleet.env; set +a
fleet-admin roles create --name trawl-ingest --perm trawl:ingest
fleet-admin roles create --name trawl-reader \\
  --perm trawl:query --perm trawl:schema_read --perm trawl:validate \\
  --perm trawl:export --perm trawl:stream --perm trawl:saved_query \\
  --perm trawl:query_cancel
(umask 077
 fleet-admin keys create --name vector --kind service --role trawl-ingest > {GUEST_EVIDENCE}/vector.token
 fleet-admin keys create --name evidence-reader --kind human --role trawl-reader > {GUEST_EVIDENCE}/reader.token)
stat -c '%a %U:%G %s bytes %n' {GUEST_EVIDENCE}/vector.token {GUEST_EVIDENCE}/reader.token""", root=True)

        # -- CLI profile for the operator account, like operate/deployment.md.
        g.run(f"""install -d -m 0700 ~/.config/trawl
sudo cat {TRAWLD_CA} > ~/.config/trawl/{PROFILE}-ca.pem
(umask 077
 {{ printf '[profiles.{PROFILE}]\\nurl = "{TRAWLD_URL}"\\nca_cert = "~/.config/trawl/{PROFILE}-ca.pem"\\n'
   sudo sed 's/.*/token = "&"/' {GUEST_EVIDENCE}/reader.token; }} > ~/.config/trawl/config.toml)
stat -c '%a %U %n' ~/.config/trawl/config.toml
grep -c '^token = ' ~/.config/trawl/config.toml
trawl doctor -p {PROFILE} --format table""")

        # -- nginx, Docker, UFW.
        g.run("systemctl is-active nginx\n"
              "curl -s -o /dev/null -w '%{http_code}\\n' http://127.0.0.1/\n"
              "tail -1 /var/log/nginx/access.log", root=True)
        g.run("docker pull -q alpine\n"
              "docker image inspect alpine --format '{{index .RepoDigests 0}}'", root=True, timeout=600)
        self.state["alpine_digest"] = g.run(
            "docker image inspect alpine --format '{{index .RepoDigests 0}}'",
            root=True, echo=False).stdout.strip()
        g.run("ufw default deny incoming\n"
              "ufw default allow outgoing\n"
              "ufw allow 22/tcp\n"
              "ufw logging low\n"
              "ufw --force enable\n"
              "ufw status verbose", root=True)

        # -- the two peers, each in its own network namespace.
        for peer in (PEER_UFW, PEER_DEVICE):
            T.note(f"{peer.netns}: {peer.peer_ip}, {peer.role}")
            g.run(f"""ip netns add {peer.netns}
ip link add {peer.netns}-h type veth peer name {peer.netns}-n
ip link set {peer.netns}-n netns {peer.netns}
ip addr add {peer.guest_ip}/24 dev {peer.netns}-h
ip link set {peer.netns}-h up
ip -n {peer.netns} addr add {peer.peer_ip}/24 dev {peer.netns}-n
ip -n {peer.netns} link set {peer.netns}-n up
ip -n {peer.netns} link set lo up
ip -n {peer.netns} -brief addr show {peer.netns}-n
ip netns exec {peer.netns} ping -c 1 -W 2 {peer.guest_ip} | tail -2""", root=True)

        # -- Vector: pinned package, the three drop-ins, trust in trawld's CA.
        g.run(f"""cd {GUEST_PACKAGES}
DEBIAN_FRONTEND=noninteractive apt-get install -y -q ./{VECTOR_DEB} 2>&1 | tail -2
vector --version
systemctl is-enabled vector || true
systemctl is-active vector || true
systemctl cat vector | grep -E '^(EnvironmentFile|ExecStartPre|ExecStart)='""", root=True)
        active = g.run("systemctl is-active vector || true", root=True, echo=False).stdout.strip()
        T.require("vector is not running before the history seed", active != "active", active)
        g.run(f"""usermod -aG systemd-journal,adm,docker vector
id vector
install -d -m 0755 /etc/vector/vector.d
install -m 0644 {" ".join(f"{GUEST_CONFIG}/{n}" for n in VECTOR_DROP_INS)} /etc/vector/vector.d/
install -m 0644 {TRAWLD_CA} /etc/vector/trawl-ca.pem
grep -c '^# ca_file = "/etc/vector/trawl-ca.pem"$' /etc/vector/vector.d/base.toml
sed -i 's|^# ca_file = "/etc/vector/trawl-ca.pem"$|ca_file = "/etc/vector/trawl-ca.pem"|' /etc/vector/vector.d/base.toml
sed -n '/^\\[sinks.trawld.tls\\]/,/^$/p' /etc/vector/vector.d/base.toml
sha256sum /etc/vector/vector.d/*.toml /etc/vector/trawl-ca.pem""", root=True)
        T.note("the only edit to the shipped drop-ins: base.toml's commented ca_file line is "
               "uncommented, so Vector trusts trawld's generated localhost certificate")
        # -- the ingest key: proof:key-write runs verbatim in the directory that
        # holds vector.token. The token file is copied from the root-only mint
        # by a redirect, so the key reaches no argv and no stdout.
        assert self.shell is not None
        blocks = self.blocks()
        g.run(f"(umask 077; sudo cat {GUEST_EVIDENCE}/vector.token > vector.token)\n"
              "stat -c '%a %U %s bytes %n' vector.token")
        self.shell.reset()
        self.shell.run_block(blocks["key-write"])
        g.run("test ! -e vector.token && echo 'vector.token is gone'")
        T.note("step 3 of the guide adds these settings with sudoedit; the harness appends the "
               "same lines, with this site's TRAWL_URL")
        g.run(f"""cat >> /etc/default/vector <<'EOF'
VECTOR_CONFIG_DIR=/etc/vector/vector.d
VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true
TRAWL_URL={TRAWLD_URL}
TRAWL_ENV={TRAWL_ENV}
EOF
stat -c '%a %U:%G %n' /etc/default/vector
cut -d= -f1 /etc/default/vector""", root=True)
        mode = g.run("stat -c '%a %U:%G' /etc/default/vector", root=True, echo=False).stdout.strip()
        T.check("/etc/default/vector is 0600 root:root", mode == "600 root:root", mode)
        # test -s: two empty files would compare equal.
        same = g.run(f"test -s {GUEST_EVIDENCE}/vector.token && "
                     f"cmp -s <(sed -n 's/^TRAWL_INGEST_TOKEN=//p' /etc/default/vector) "
                     f"{GUEST_EVIDENCE}/vector.token", root=True, check=False)
        T.check("the key in /etc/default/vector is the minted ingest key", same.returncode == 0)

    # -- phase 6: history seed, before Vector starts ------------------------

    def phase_seed(self) -> None:
        T.phase("6 history seed (before Vector starts)")
        g = self.guest
        assert g is not None
        seed = f"trawl-seed-{uuid.uuid4()}"
        self.state["seed"] = seed
        T.note("one event per sender, written while Vector is stopped; the Docker container "
               "prints docker-pre now and docker-post once /tmp/go exists")
        g.run(f"""sudo systemd-run --collect --quiet --unit {seed} /bin/echo "{seed} journald-pre"
curl -s -o /dev/null -w '%{{http_code}}\\n' http://127.0.0.1/{seed}-nginx-pre
sudo docker run -d --name {seed} alpine sh -c \\
  'echo "{seed} docker-pre"; while [ ! -e /tmp/go ]; do sleep 1; done; echo "{seed} docker-post"; sleep 3600'
sleep 2
sudo docker logs {seed}
sudo grep -c '{seed}-nginx-pre' /var/log/nginx/access.log
sudo journalctl --no-pager -o cat -u {seed}
systemctl is-active vector || true""")

    # -- phase 7: Vector start ------------------------------------------------

    def phase_vector(self) -> None:
        T.phase("7 Vector start")
        g, sh = self.guest, self.shell
        assert g is not None and sh is not None
        sh.run_block(self.blocks()["vector-start"])
        vs = sh.var("VECTOR_START")
        self.state["vector_start"] = vs
        since = vs.replace("T", " ").replace("Z", " UTC")
        self.state["vector_since"] = since
        T.note(f"VECTOR_START={vs}; waiting for the first-start backfill to reach an outcome "
               f"(up to {BACKFILL_WAIT_SECS}s), then 10s more")
        g.run(f"""for i in $(seq {BACKFILL_WAIT_SECS}); do
  if journalctl -u trawld --since '{since}' -o cat | grep -q 'ingest_complete.*user=vector'; then break; fi
  if journalctl -u vector --since '{since}' -o cat | grep -q 'dropping the request'; then break; fi
  sleep 1
done
sleep 10
journalctl -u vector --no-pager -o cat --since '{since}' | sed -e 's/\\x1b\\[[0-9;]*m//g' \\
  | grep -E ' (ERROR|WARN) ' | grep -v 'Failed to glob path' | cut -c1-400 | head -20 || true
journalctl -u vector --no-pager -o cat --since '{since}' | grep -c 'Failed to glob path' || true""",
              root=True)
        # The drop read covers the whole first run, so it is taken just
        # before the first restart; see restart_vector(). The verdict waits
        # for the 401 case's control; see settle_first_start().
        T.note(f"first start: Vector's drops are read over its whole first run, from {since} "
               "to just before its first restart; the verdict follows the 401 case's control")

    # -- phase 8: history -------------------------------------------------

    def phase_history(self) -> None:
        T.phase("8 history")
        blocks = self.blocks()
        g, sh = self.guest, self.shell
        assert g is not None and sh is not None
        seed = self.state["seed"]
        vs = self.state["vector_start"]
        T.note("post-start controls: the same nginx and Docker senders, after Vector started")
        g.run(f"sudo docker exec {seed} touch /tmp/go\n"
              f"curl -s -o /dev/null -w '%{{http_code}}\\n' http://127.0.0.1/{seed}-nginx-post\n"
              f"sleep 2\nsudo docker logs {seed}")

        sh.run_block(blocks["vars"])
        rows = poll_block(sh, blocks["history-finder"], "history finder",
                          until=lambda rs: any(r.get("service") == seed for r in rs)).rows
        seed_rows = [r for r in rows if r.get("service") == seed]
        T.check("history finder lists the unit that ran before Vector started", bool(seed_rows),
                f"service={seed}")
        if seed_rows:
            mins = [v for k, v in seed_rows[0].items() if k.startswith("min")]
            earliest = utc_prefix(mins[0]) if mins else ""
            T.check("its min(_time) is earlier than VECTOR_START", bool(earliest)
                    and earliest < utc_prefix(vs), f"min(_time) {earliest} < {vs}")

        T.note("the next queries are the harness's, not the guide's: one per seeded line")
        rows = poll_query(g, f'service={seed} "{seed} journald-pre" last=1d _ingested>="{vs}" '
                             "| table _time, _ingested, service, message", "journald-pre").rows
        T.check("journald: the pre-start line arrived after Vector started", bool(rows))
        rows = poll_query(g, f'service=nginx "{seed}-nginx-post" last=1d _ingested>="{vs}" '
                             "| table _time, _ingested, service, uri", "nginx-post control").rows
        T.check("nginx control: the post-start request arrived", bool(rows))
        if rows:
            rows = once_query(g, f'service=nginx "{seed}-nginx-pre" last=1d '
                                 "| table _time, _ingested, service, uri")
            T.check("nginx: the pre-start access line is absent", not rows, f"{len(rows)} row(s)")
        rows = poll_query(g, f'host={seed} "{seed} docker-post" last=1d '
                             "| table _time, _ingested, service, host, message", "docker-post control").rows
        T.check("Docker control: the post-start line arrived", bool(rows))
        if rows:
            rows = once_query(g, f'host={seed} "{seed} docker-pre" last=1d '
                                 "| table _time, _ingested, service, host, message")
            T.check("Docker: the pre-start line is absent", not rows, f"{len(rows)} row(s)")
        g.run(f"sudo docker rm -f {seed}")

    # -- phase 9: recipes -------------------------------------------------

    def recipe_vars(self) -> tuple[str, str]:
        assert self.shell is not None
        self.shell.run_block(self.blocks()["vars"])
        return self.shell.var("MARKER"), self.shell.var("HOST")

    def phase_recipes(self) -> None:
        T.phase("9 recipes")
        b = self.blocks()
        g, sh = self.guest, self.shell
        assert g is not None and sh is not None
        for recipe in (self.recipe_journald, self.recipe_nginx, self.recipe_docker,
                       self.recipe_ufw, self.recipe_syslog):
            try:
                recipe(b, g, sh)
            except HarnessError as err:
                T.check(f"{recipe.__name__} completed", False, str(err))

    def recipe_journald(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: journald\n")
        marker, host_ = self.recipe_vars()
        sh.run_block(b["journald-send"])
        rows = poll_block(sh, b["journald-check"], "journald check").rows
        expect_fields("journald", rows, {"env": TRAWL_ENV, "service": marker, "host": host_,
                                         "_producer": "http", "message": marker})

    def recipe_nginx(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: nginx\n")
        marker, host_ = self.recipe_vars()
        code = sh.run_block(b["nginx-send"]).stdout.strip()
        T.check("nginx-send: nginx answered 404", code == "404", code)
        found = sh.run_block(b["nginx-confirm"], check=False)
        T.check("nginx-confirm: the access log holds the marker line",
                found.returncode == 0 and marker in found.stdout)
        rows = poll_block(sh, b["nginx-check"], "nginx check").rows
        expect_fields("nginx", rows, {"env": TRAWL_ENV, "service": "nginx", "host": host_,
                                      "_producer": "http", "uri": f"/{marker}", "status": "404"})

    def recipe_docker(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: Docker\n")
        marker, _ = self.recipe_vars()
        sh.run_block(b["docker-send"])
        T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$",
                  f"sleep {DOCKER_WAIT_SECS}   # the recipe: wait about 30 seconds")
        time.sleep(DOCKER_WAIT_SECS)
        rows = poll_block(sh, b["docker-check"], "docker check").rows
        expect_fields("docker", rows, {"env": TRAWL_ENV, "service": marker, "host": marker,
                                       "_producer": "http", "message": marker})
        sh.run_block(b["docker-cleanup"])

    def recipe_ufw(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: host firewall (UFW)\n")
        g.run("sudo ufw status verbose | head -3")
        self.recipe_vars()
        sh.run_block(b["ufw-vars"], substitute(b["ufw-vars"], {
            "PEER=192.0.2.20": f"PEER={PEER_UFW.peer_ip}",
            "COLLECTOR=192.0.2.10": f"COLLECTOR={PEER_UFW.guest_ip}",
        }))
        port = sh.var("PORT")
        sh.run_block(b["ufw-rule"])
        g.run("sudo ufw status numbered | head -6")
        T.note(f"the other machine is network namespace {PEER_UFW.netns}, address "
               f"{PEER_UFW.peer_ip}; it reaches this host at {PEER_UFW.guest_ip}")
        sh.run_block(b["ufw-send"], netns=PEER_UFW.netns)
        rows = poll_block(sh, b["ufw-check"], "ufw check").rows
        expect_fields("ufw", rows, {"env": TRAWL_ENV, "service": "ufw", "host": GUEST_HOSTNAME,
                                    "_producer": "http", "src_ip": PEER_UFW.peer_ip,
                                    "dst_port": port})
        T.note("step 5 of the UFW recipe, from the guide's prose")
        sh.run_text('sudo ufw delete deny log from "$PEER" to any port "$PORT" proto tcp')

    def recipe_syslog(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: network firewall appliance (native syslog, operate/ingestion.md)\n")
        cfg = b["syslog-config"]
        T.note(f"proof:syslog-config from {cfg.where()} (verbatim), appended to "
               "/etc/trawl/trawld.toml; then step 2, restart trawld")
        g.run(f"""cat >> /etc/trawl/trawld.toml <<'EOF'
{cfg.text}EOF
(set -a; . /etc/default/trawld; set +a; trawld --check-config --config /etc/trawl/trawld.toml)
systemctl restart trawld
for i in $(seq 60); do
  curl -fsS --cacert {TRAWLD_CA} {TRAWLD_URL}/api/v1/health >/dev/null 2>&1 && break
  sleep 1
done
systemctl is-active trawld
ss -Hlun 'sport = :1514'
ss -Hltn 'sport = :1514'""", root=True)
        sh.run_block(b["syslog-vars"])
        sh.run_block(b["syslog-firewall-allow"])
        device_marker = f"trawl-device-{uuid.uuid4()}"
        # The frame's timestamp is fixed here so the expected event can name
        # it. <134> is facility 16 (local0) and severity 6 (informational).
        frame_time = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        T.note(f"SIMULATED DEVICE: namespace {PEER_DEVICE.netns} at {PEER_DEVICE.peer_ip} stands in "
               "for the appliance and sends one RFC 5424 test message; this is not a doc block")
        g.run(f"""ip netns exec {PEER_DEVICE.netns} bash -c 'printf "<134>1 %s fw01 filterlog - - - test message {device_marker}\\n" \\
  "{frame_time}" > /dev/udp/{PEER_DEVICE.guest_ip}/1514'""", root=True)
        rows = poll_block(sh, b["syslog-check"], "syslog check").rows
        expect_fields("syslog", rows, {"env": TRAWL_ENV, "service": "firewall", "host": "fw01",
                                       "_producer": "syslog",
                                       "syslog_source_ip": PEER_DEVICE.peer_ip,
                                       "syslog_severity": "6", "syslog_facility": "local0"},
                      # syslog_timestamp prints as "YYYY-MM-DDTHH:MM:SS.000000Z"
                      # under a VARCHAR pin and "YYYY-MM-DD HH:MM:SS" under a
                      # TIMESTAMP pin; both carry the frame's UTC time of day.
                      contains={"message": device_marker,
                                "syslog_timestamp": frame_time[11:19]})

    # -- phase 10: negatives ------------------------------------------------

    def rejected(self, reason: str) -> int:
        """The rejection counter for `reason`. A failed /metrics read raises,
        so it can never pose as 0; a counter not yet emitted is 0."""
        assert self.guest is not None
        out = self.guest.run(
            f"m=$(curl -fsS --cacert {TRAWLD_CA} {TRAWLD_URL}/metrics)\n"
            f"printf '%s\\n' \"$m\" "
            f"| grep -F 'trawl_ingest_events_rejected_total{{reason=\"{reason}\"}}' || true").stdout
        m = re.search(r"\}\s+(\d+)", out)
        return int(m.group(1)) if m else 0

    def sink_errors(self, since: str) -> str:
        """Vector's sink errors since `since`. The journal read must succeed."""
        assert self.guest is not None
        return self.guest.run(
            f"j=$(journalctl -u vector --since '{since}' -o cat)\n"
            "printf '%s\\n' \"$j\" | sed -e 's/\\x1b\\[[0-9;]*m//g' "
            "| { grep -E ' ERROR sink' || true; } "
            "| { grep -v 'has been suppressed' || true; } | cut -c1-300",
            root=True, echo=False).stdout

    def vector_drops(self, since: str, until: str | None = None) -> tuple[int, list[str]]:
        """Vector's journal line count from `since` (to `until`, if given), and
        its drop lines, through drop_extraction(). The journal read must
        succeed."""
        assert self.guest is not None
        bound = f" --until '{until}'" if until else ""
        journal = self.guest.run(f"journalctl -u vector --since '{since}'{bound} -o cat",
                                 root=True, echo=False, merge=False).stdout
        return drop_extraction(journal)

    def settle_first_start(self, control: bool, why: str) -> None:
        """The first-start "no drops" verdict, written once. It passes only when
        the read over the whole first run held Vector's lines, found no drop
        line, and `control` says the same extraction found the 401 case's
        drops. With no read (Vector was never restarted) it fails."""
        if self.first_start_settled:
            return
        self.first_start_settled = True
        name = "Vector dropped no events on its first start"
        since = self.state.get("vector_since", "?")
        if self.first_start is None:
            T.check(name, False, f"no read: the first run, since {since}, never ended in a "
                                 f"restart; {why}")
            return
        lines, drops, until = self.first_start
        T.check(name, lines > 0 and not drops and control,
                f"{len(drops)} 'Events dropped' line(s) in {lines} line(s) of "
                f"journalctl -u vector --since '{since}' --until '{until}'; {why}"
                + (f"; first: {drops[0][:200]}" if drops else ""))

    def read_first_start(self, until: str) -> None:
        """The first run's drop read, over [VECTOR_START, `until`], where `until`
        is taken just before the first restart. Later calls do nothing."""
        if self.first_start is not None or "vector_since" not in self.state:
            return
        since = self.state["vector_since"]
        lines, drops = self.vector_drops(since, until)
        self.first_start = (lines, drops, until)
        T.note(f"first start: {len(drops)} 'Events dropped' line(s) in {lines} line(s) of "
               f"journalctl -u vector --since '{since}' --until '{until}' (the whole first "
               "run); the verdict follows the 401 case's control")

    def restart_vector(self, script: str) -> str:
        """Run `script`, then restart Vector. Returns the time taken just before
        the restart. The first call also closes the first run's drop window."""
        assert self.guest is not None
        out = self.guest.run(f"""{script}
grep -E '^TRAWL_(URL|ENV)=' /etc/default/vector
since=$(date -u '+%Y-%m-%d %H:%M:%S UTC'); echo "since=$since"
until=$(date -u '+%Y-%m-%d %H:%M:%S.%6N UTC'); echo "until=$until"
systemctl restart vector
systemctl is-active vector""", root=True).stdout
        self.read_first_start(re.search(r"until=(.+)", out).group(1).strip())  # type: ignore[union-attr]
        return re.search(r"since=(.+)", out).group(1).strip()  # type: ignore[union-attr]

    def positive_control(self, label: str) -> None:
        """After the restore: the doc's check finds a new marker, and so does the
        env-free query the negative's absence used, so that absence was not
        vacuous."""
        assert self.shell is not None and self.guest is not None
        b = self.blocks()
        marker, host_ = self.recipe_vars()
        t0 = self.shell.var("T0")
        self.shell.run_block(b["journald-send"])
        rows = poll_block(self.shell, b["journald-check"], f"{label} positive control").rows
        expect_fields(f"{label}: positive control after restore", rows, {
            "env": TRAWL_ENV, "service": marker, "host": host_, "_producer": "http",
            "message": marker})
        T.note(f"{label}: the env-free marker query from the negative, for this marker")
        rows = poll_query(self.guest, marker_query(marker, t0),
                          f"{label} env-free control").rows
        T.check(f"{label}: control, the env-free marker query finds the restored marker",
                any(r.get("service") == marker for r in rows), f"{len(rows)} row(s)")

    def phase_negatives(self) -> None:
        T.phase("10 negatives")
        b = self.blocks()
        g, sh = self.guest, self.shell
        assert g is not None and sh is not None
        backup = f"{GUEST_EVIDENCE}/vector.default"
        g.run(f"cp -p /etc/default/vector {backup}", root=True)
        restore = f"cp -p {backup} /etc/default/vector"
        cases = (
            ("401", "s/^TRAWL_INGEST_TOKEN=.*/TRAWL_INGEST_TOKEN=flt_not_a_real_key/", None),
            ("invalid_env", "s/^TRAWL_ENV=.*/TRAWL_ENV=Prod/", "invalid_env"),
            ("env_not_allowed", "s/^TRAWL_ENV=.*/TRAWL_ENV=staging/", "env_not_allowed"),
        )
        # Set when the 401 case's sink-error read found the 401: the control
        # for the env cases' "no sink error" checks, which use the same read.
        sink_read_proven = False
        try:
            for label, edit, reason in cases:
                T.out(f"\n--- negative: {label}\n")
                before = self.rejected(reason) if reason else 0
                since = self.restart_vector(f"sed -i '{edit}' /etc/default/vector")
                marker, _ = self.recipe_vars()
                t0 = sh.var("T0")
                sh.run_block(b["journald-send"])
                T.note(f"{label}: the assertion is an env-free query for the marker; an event "
                       "wrongly accepted under the edited env still matches it")
                polled = poll_query(g, marker_query(marker, t0), f"{label} env-free absence")
                expect_absent(f"{label}: no event for the marker, in any env", polled)
                T.note(f"{label}: the doc's journald check, for the transcript only; its env= "
                       "predicate could hide a wrongly accepted event")
                sh.run_block(b["journald-check"], check=False)
                local = g.run(f"journalctl --no-pager -o cat -u {marker}", root=True, check=False)
                T.check(f"{label}: the marker's unit wrote its line to the local journal",
                        local.returncode == 0 and marker in local.stdout)
                active = g.run("systemctl is-active vector || true", root=True,
                               echo=False).stdout.strip()
                T.check(f"{label}: Vector was still active at the end of the window",
                        active == "active", active)
                errors = self.sink_errors(since)
                T.command(f"root@{GUEST_HOSTNAME}#", f"journalctl -u vector --since '{since}' -o cat "
                          "| sed -e 's/\\x1b\\[[0-9;]*m//g' | grep -E ' ERROR sink' "
                          "| grep -v 'has been suppressed' | cut -c1-300 "
                          "| grep -E 'Unauthorized|Bad Request|dropped'")
                T.output("\n".join(line for line in errors.splitlines()
                                   if re.search(r"Unauthorized|Bad Request|dropped", line))
                         or "(none)")
                if reason is None:
                    sink_read_proven = T.check("401: Vector logs the 401 from the trawld sink",
                                               bool(re.search(r"401|Unauthorized", errors)))
                    _, drops = self.vector_drops(since)
                    counted = drop_count(drops)
                    control = T.check(
                        "401: control, the first-start drop extraction counts the rejected events",
                        counted > 0, f"{len(drops)} 'Events dropped' line(s), count={counted} "
                        "in total")
                    self.settle_first_start(control, "control: the 401 case's drops counted"
                                            if control else "unproven: the same extraction "
                                            "counted no drops in the 401 case")
                else:
                    after = self.rejected(reason)
                    T.check(f"{label}: trawl_ingest_events_rejected_total{{reason=\"{reason}\"}} rose",
                            after > before, f"{before} -> {after}; attributable to the edited "
                            "env, not to the marker's batch alone")
                    T.check(f"{label}: Vector logs no sink error (trawld answered 200)",
                            errors.strip() == "" and sink_read_proven,
                            errors.strip()[:200] if errors.strip() else
                            ("the same read found the 401 in the 401 case" if sink_read_proven
                             else "unproven: the 401 case's read did not find its error"))
                self.restart_vector(restore)
                self.positive_control(label)
        finally:
            g.run(f"cmp -s {backup} /etc/default/vector || {{ {restore}; systemctl restart vector; }}\n"
                  f"rm -f {backup}", root=True, check=False)

    # -- versions and hashes -------------------------------------------------

    def phase_versions(self) -> None:
        T.phase("11 versions and hashes")
        g = self.guest
        assert g is not None
        T.note(f"commit {self.head}")
        T.note(f"image {IMAGE_TARBALL} sha512 {IMAGE_SHA512}")
        g.run("""uname -r
cat /etc/debian_version
trawld --version
trawl --version
vector --version
nginx -v 2>&1
docker version --format 'docker server {{.Server.Version}}'
runuser -u postgres -- psql -Atc 'SHOW server_version'
ufw version | head -1
cloud-init --version 2>&1
dpkg-query -W -f='${Package} ${Version}\\n' trawl-server trawl-cli trawl-runtime vector nginx docker.io postgresql ufw cloud-init
docker image inspect alpine --format '{{index .RepoDigests 0}}'
sha256sum /etc/vector/vector.d/*.toml /etc/trawl/trawld.toml""", root=True)
        version = g.run("trawld --version", echo=False).stdout
        T.check("trawld --version names the tested commit", f"({self.short_sha}" in version,
                version.strip())
        self.check_not_dirty(version)
        host(["sha256sum", *[str(VECTOR_CONFIG_DIR / n) for n in VECTOR_DROP_INS]],
             shown="sha256sum " + " ".join(f"config/vector/debian/{n}" for n in VECTOR_DROP_INS))

    def require_cli_build(self, version: str) -> None:
        """The CLI that runs every proof query must be the tested commit's
        clean build, like trawld. Otherwise a stale trawl-cli deb of the
        same package version, passed in with --packages, could run the
        recipes. A miss stops the run before any recipe."""
        T.require("trawl --version names the tested commit", f"({self.short_sha}" in version,
                  f"expected ({self.short_sha}; got {version}")
        if f"({self.short_sha}*" in version and self.args.allow_dirty:
            T.note("--allow-dirty: trawl was built from a dirty tree ('*'); not asserted")
        else:
            T.require("trawl --version is not a dirty build",
                      f"({self.short_sha}*" not in version, version)

    def check_not_dirty(self, version: str, stop: bool = False) -> None:
        """At provision (stop=True) a dirty trawld stops the run before any
        recipe, exactly like the CLI check."""
        dirty_build = f"({self.short_sha}*" in version
        if dirty_build and self.args.allow_dirty:
            T.note("--allow-dirty: trawld was built from a dirty tree ('*'); not asserted")
        else:
            (T.require if stop else T.check)("trawld --version is not a dirty build",
                                             not dirty_build, version.strip())

    # -- token absence -------------------------------------------------------

    def phase_tokens(self) -> None:
        T.phase("12 token absence")
        g = self.guest
        assert g is not None and self.run_dir is not None
        transcript = self.run_dir / "transcript.txt"
        targets = [str(transcript), str(EVIDENCE_DIR)]
        for key in ("vector", "reader"):
            source = f"{GUEST_EVIDENCE}/{key}.token"
            fetch = g.ssh_base() + ["--", f"sudo cat {source}"]
            # Positive control: the same pipe must find the key in a file that
            # holds it, or an empty or mangled pattern would pass the real check.
            control = self.run_dir / f"{key}.control"
            fd = os.open(control, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as sink:
                subprocess.run(fetch, stdout=sink, stdin=subprocess.DEVNULL, check=True)
            ok_control = self.pipe_grep(fetch, ["grep", "-cF", "-f", "-", str(control)])
            control.unlink()
            T.command("$", f"ssh {GUEST_HOSTNAME} sudo cat {source} | grep -cF -f - $RUN/{key}.control")
            T.output(ok_control.stdout)
            T.check(f"{key} key: positive control finds the key through the same pipe",
                    ok_control.returncode == 0 and ok_control.stdout.strip() == "1")
            shown = (f"ssh {GUEST_HOSTNAME} sudo cat {source} | grep -rlF -f - "
                     + " ".join(shlex.quote(t) for t in targets))
            T.command("$", shown)
            found = self.pipe_grep(fetch, ["grep", "-rlF", "-f", "-", *targets])
            T.output(found.stdout or "")
            T.out(f"[grep exit {found.returncode}]\n")
            T.check(f"{key} key: absent from the transcript and the evidence directory",
                    found.returncode == 1, "grep exit 1 means no match")

    @staticmethod
    def pipe_grep(fetch: list[str], grep: list[str]) -> subprocess.CompletedProcess:
        src = subprocess.Popen(fetch, stdout=subprocess.PIPE, stdin=subprocess.DEVNULL)
        assert src.stdout is not None
        result = subprocess.run(grep, stdin=src.stdout, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True)
        src.stdout.close()
        if src.wait() != 0:
            return subprocess.CompletedProcess(grep, 2, "the key fetch failed\n", "")
        return result

    # -- driver ------------------------------------------------------------

    def main(self) -> int:
        target = REPO / "target"
        target.mkdir(exist_ok=True)
        self.run_dir = Path(tempfile.mkdtemp(prefix="issue-198-vm.", dir=target))
        T.redact(self.run_dir, "$RUN")
        T.redact(REPO, "$REPO")
        T.attach_copy(self.run_dir / "transcript.txt")
        progress(f"run directory {self.run_dir}")

        try:
            self.phase_header()
            self.phase_build()
            raw = self.phase_image()
            self.phase_boot(raw)
            self.phase_provision()
            self.phase_seed()
            self.phase_vector()
        except HarnessError as err:
            T.out(f"\nFATAL {err}\n")
            # Keys that exist must still be proven absent from what was written.
            if self.state.get("keys_minted") and self.qemu is not None and self.qemu.poll() is None:
                self.phase_tokens_safely()
            return EXIT_FATAL

        for phase in (self.phase_history, self.phase_recipes, self.phase_negatives):
            try:
                phase()
            except BlockInventoryError as err:
                T.check("proof block inventory", False, str(err))
            except NotImplementedError as err:
                T.check(f"{phase.__name__}: not implemented yet", False, str(err))
            except HarnessError as err:
                T.check(f"{phase.__name__} completed", False, str(err))
        # No-op once the 401 case has settled it; otherwise its control never ran.
        self.settle_first_start(False, "unproven: the 401 case's control never ran")
        try:
            self.phase_versions()
        except HarnessError as err:
            T.check("versions phase completed", False, str(err))
        self.phase_tokens_safely()

        T.out(f"\nSUMMARY {T.passes} passed, {len(T.failures)} failed\n")
        for name in T.failures:
            T.out(f"  FAILED {name}\n")
        return EXIT_FAIL if T.failures else 0

    def phase_tokens_safely(self) -> None:
        try:
            self.phase_tokens()
        except (HarnessError, subprocess.CalledProcessError) as err:
            T.check("token absence phase completed", False, str(err))


def copy_debs(src: Path, dest: Path) -> None:
    """Copy every .deb in `src` into `dest`, which this run owns.

    Only the copies are hashed, copied into the guest and installed, so a
    change to `src` after this point cannot reach the run. `src` is never
    modified.
    """
    for deb in sorted(src.glob("*.deb")):
        with open(deb, "rb") as reader, open(dest / deb.name, "xb") as writer:
            shutil.copyfileobj(reader, writer)
    T.note(f"copied {len(list(dest.glob('*.deb')))} .deb(s) into $RUN/{dest.name}")


def free_port() -> int:
    # qemu binds the port itself, so this is a best-effort pick; a collision
    # makes qemu exit at startup, and phase_boot retries on a new port.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--build", action="store_true", help="build the .debs from HEAD")
    source.add_argument("--packages", metavar="DIR", help="use the three .debs in DIR")
    parser.add_argument("--image-cache", metavar="DIR", default="~/.cache/trawl-evidence")
    parser.add_argument("--keep", action="store_true", help="leave the VM and run directory")
    parser.add_argument("--allow-dirty", action="store_true", help="run against a dirty tree")
    return parser.parse_args(argv)


def main() -> int:
    run = Run(parse_args(sys.argv[1:]))
    atexit.register(run.cleanup)

    def on_signal(signum: int, _frame) -> None:
        progress(f"signal {signum}: cleaning up")
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)
    try:
        return run.main()
    except HarnessError as err:
        T.out(f"\nFATAL {err}\n")
        return EXIT_FATAL


if __name__ == "__main__":
    sys.exit(main())
