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

from __future__ import annotations

import argparse
import atexit
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
BUILD_CONTAINER = "trawl-evidence-198-build"
CARGO_VOLUME = "trawl-evidence-cargo-registry"
BUILD_TARGET = "x86_64-unknown-linux-gnu"
BUILD_DIR = REPO / "target/issue-198-deb"
SPA_STUB = REPO / "crates/trawl-web-ui/dist/index.html"
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


def expect_fields(label: str, rows: list[dict], expected: dict[str, str]) -> None:
    """At least one row carries every expected value (retries can duplicate)."""
    T.check(f"{label}: at least one row", bool(rows), f"{len(rows)} row(s)")
    for field, value in expected.items():
        got = sorted({str(r.get(field)) for r in rows})
        T.check(f"{label}: {field}={value}", value in got, f"seen {got}")


def poll_block(shell: ProofShell, block: Block, label: str, until=None,
               diagnose: bool = True) -> list[dict]:
    """Rerun a check block until it yields rows (and `until(rows)`), or give up.

    When a positive check gives up, the sink errors Vector logged meanwhile
    go into the transcript, so a miss names its cause.
    """
    T.note(f"proof:{block.name} from {block.where()} (verbatim; rerun up to {POLL_TRIES} "
           f"times, {POLL_SECS}s apart, as the guide says to rerun a check)")
    T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$", block.text)
    started = time.time()
    rows = _poll(lambda: shell.run_text(block.text, check=False, echo=False), label, until)
    if not rows and diagnose:
        window = int(time.time() - started) + 60
        T.note(f"{label}: diagnostics, Vector's sink errors in the last {window}s")
        shell.guest.run(f"journalctl -u vector --since '-{window}s' -o cat "
                        "| sed -e 's/\\x1b\\[[0-9;]*m//g' | grep -E ' ERROR sink' "
                        "| grep -v 'has been suppressed' | cut -c1-300 | tail -6 || true",
                        root=True, check=False)
    return rows


def poll_query(guest: Guest, query: str, label: str, until=None) -> list[dict]:
    """A harness query, not a doc block, rerun until it yields rows."""
    shown = f"trawl -p {PROFILE} query {shlex.quote(query)}"
    T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$", shown)
    return _poll(lambda: guest.run(shown, check=False, echo=False, merge=False), label, until)


def once_query(guest: Guest, query: str) -> list[dict]:
    shown = f"trawl -p {PROFILE} query {shlex.quote(query)}"
    result = guest.run(shown, check=False, merge=False)
    if result.returncode != 0:
        raise HarnessError(f"query failed: {query}")
    return json_rows(result.stdout)[0]


def _poll(attempt, label: str, until) -> list[dict]:
    last = ""
    for n in range(1, POLL_TRIES + 1):
        result = attempt()
        last = result.stdout
        rows, _ = json_rows(last)
        if result.returncode == 0 and rows and (until is None or until(rows)):
            T.output(last)
            T.note(f"{label}: rows on attempt {n} of {POLL_TRIES}")
            return rows
        progress(f"{label}: attempt {n}/{POLL_TRIES}: no matching rows")
        if n < POLL_TRIES:
            time.sleep(POLL_SECS)
    T.output(last or "(no output)")
    T.note(f"{label}: no matching rows after {POLL_TRIES} attempts, {POLL_SECS}s apart")
    return []


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
        self.stub_created = False
        self.build_started = False
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
        if self.build_started:
            subprocess.run(["docker", "rm", "-f", BUILD_CONTAINER], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
        if self.stub_created and SPA_STUB.exists() and SPA_STUB.read_text() == SPA_STUB_TEXT:
            SPA_STUB.unlink()
            progress("removed the stub SPA this run created")
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
        if self.args.packages:
            pkg_dir = Path(self.args.packages).resolve()
            if not pkg_dir.is_dir():
                die(f"--packages {pkg_dir} is not a directory")
            T.note(f"skipped: using --packages {pkg_dir}")
        else:
            pkg_dir = self.build_debs()
        self.collect_packages(pkg_dir)

    def build_debs(self) -> Path:
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
        host(["docker", "volume", "create", CARGO_VOLUME])
        # A fresh named volume is root-owned; the build runs as the invoking uid.
        host(["docker", "run", "--rm", "--user", "0:0", "-v",
              f"{CARGO_VOLUME}:/usr/local/cargo/registry", BUILDER_IMAGE,
              "chown", "-R", uid_gid, "/usr/local/cargo/registry"])

        # rust-embed scans the SPA dist at compile time. Nothing here touches the
        # web UI, so a one-line placeholder stands in when no real build exists.
        if SPA_STUB.exists():
            T.note("SPA dist present; not stubbed")
        else:
            SPA_STUB.parent.mkdir(parents=True, exist_ok=True)
            self.stub_created = True
            SPA_STUB.write_text(SPA_STUB_TEXT)
            T.note("STUBBED the SPA with a one-line placeholder; trawl-web in these .debs "
                   "serves no real UI. Teardown removes it.")

        BUILD_DIR.mkdir(parents=True, exist_ok=True)
        common_git = subprocess.run(
            ["git", "-C", str(REPO), "rev-parse", "--path-format=absolute", "--git-common-dir"],
            stdout=subprocess.PIPE, text=True, check=True).stdout.strip()
        T.redact(Path(common_git), "$GIT_COMMON_DIR")
        T.note(f"CARGO_TARGET_DIR {BUILD_DIR}; cargo registry cache in docker volume {CARGO_VOLUME}")
        self.build_started = True
        builder = ["docker", "run", "--rm", "--name", BUILD_CONTAINER, "--user", uid_gid,
                   "-v", f"{REPO}:{REPO}", "-w", str(REPO),
                   "-v", f"{common_git}:{common_git}:ro",
                   "-v", f"{CARGO_VOLUME}:/usr/local/cargo/registry",
                   "-e", f"CARGO_TARGET_DIR={BUILD_DIR}", "-e", f"XDG_CACHE_HOME={BUILD_DIR}/cache",
                   BUILDER_IMAGE]
        host(builder + ["bash", "scripts/release/build-distribution.sh", str(REPO), BUILD_TARGET,
                        str(BUILD_DIR / "runtime")], stream=True)

        pkg_dir = BUILD_DIR / BUILD_TARGET / "debian"
        # A reused target dir can hold an older version's .deb; clear it so the
        # package set below is only what this run built.
        for stale in pkg_dir.glob("*.deb"):
            stale.unlink()
        packager = ["docker", "run", "--rm", "--name", BUILD_CONTAINER, "--user", uid_gid,
                    "-v", f"{REPO}:{REPO}:ro", "-v", f"{BUILD_DIR}:{BUILD_DIR}", "-w", str(REPO),
                    "-v", f"{common_git}:{common_git}:ro",
                    "-v", f"{CARGO_VOLUME}:/usr/local/cargo/registry",
                    "-e", f"CARGO_TARGET_DIR={BUILD_DIR}", BUILDER_IMAGE]
        host(packager + ["python3", "crates/trawl-server/debian/tests/package-snapshot.py",
                         "--source", str(REPO), "--work-dir", "/tmp",
                         "--binaries", str(BUILD_DIR / BUILD_TARGET / "release"),
                         "--runtime", str(BUILD_DIR / "runtime"),
                         "--output", str(pkg_dir), "--target", BUILD_TARGET], stream=True)
        self.build_started = False
        return pkg_dir

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
        part = dest.with_name(dest.name + ".part")
        host(["curl", "--proto", "=https", "--tlsv1.2", "-fsSL", "-o", str(part), url],
             shown=f"curl --proto =https --tlsv1.2 -fsSL -o {dest.name} {url}", timeout=1800)
        part.rename(dest)

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
            host(["tar", "-xSJf", str(tarball), "-C", str(staging), "disk.raw"],
                 shown=f"tar -xSJf {IMAGE_TARBALL} disk.raw", timeout=600)
            staging.rename(raw.parent)
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

        ssh_port = free_port()
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
        self.qemu = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                     stdout=open(rd / "qemu.log", "w"),
                                     stderr=subprocess.STDOUT, start_new_session=True)
        T.note(f"qemu pid {self.qemu.pid}; serial console in $RUN/console.log")

        deadline = time.monotonic() + SSH_WAIT_SECS
        while True:
            if self.qemu.poll() is not None:
                T.output((rd / "qemu.log").read_text())
                die(f"qemu exited with status {self.qemu.returncode} before SSH came up")
            if self.guest.reachable():
                break
            if time.monotonic() > deadline:
                die(f"no SSH with the pinned host key after {SSH_WAIT_SECS}s")
            time.sleep(3)
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
        T.check("trawld --version names the tested commit", f"({self.short_sha}" in version,
                f"expected ({self.short_sha}")
        self.check_not_dirty(version)
        g.run("trawl --version")

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
        same = g.run(f"cmp -s <(sed -n 's/^TRAWL_INGEST_TOKEN=//p' /etc/default/vector) "
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
        dropped = g.run(f"journalctl -u vector --since '{since}' -o cat | grep -c 'Events dropped' "
                        "|| true", root=True, echo=False).stdout.strip()
        T.check("Vector dropped no events on its first start", dropped == "0",
                f"{dropped} 'Events dropped' line(s) in journalctl -u vector")

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
                          until=lambda rs: any(r.get("service") == seed for r in rs))
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
                             "| table _time, _ingested, service, message", "journald-pre")
        T.check("journald: the pre-start line arrived after Vector started", bool(rows))
        rows = poll_query(g, f'service=nginx "{seed}-nginx-post" last=1d _ingested>="{vs}" '
                             "| table _time, _ingested, service, uri", "nginx-post control")
        T.check("nginx control: the post-start request arrived", bool(rows))
        if rows:
            rows = once_query(g, f'service=nginx "{seed}-nginx-pre" last=1d '
                                 "| table _time, _ingested, service, uri")
            T.check("nginx: the pre-start access line is absent", not rows, f"{len(rows)} row(s)")
        rows = poll_query(g, f'host={seed} "{seed} docker-post" last=1d '
                             "| table _time, _ingested, service, host, message", "docker-post control")
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
        rows = poll_block(sh, b["journald-check"], "journald check")
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
        rows = poll_block(sh, b["nginx-check"], "nginx check")
        expect_fields("nginx", rows, {"env": TRAWL_ENV, "service": "nginx", "host": host_,
                                      "_producer": "http", "uri": f"/{marker}", "status": "404"})

    def recipe_docker(self, b: dict[str, Block], g: Guest, sh: ProofShell) -> None:
        T.out("\n--- recipe: Docker\n")
        marker, _ = self.recipe_vars()
        sh.run_block(b["docker-send"])
        T.command(f"{GUEST_USER}@{GUEST_HOSTNAME}$",
                  f"sleep {DOCKER_WAIT_SECS}   # the recipe: wait about 30 seconds")
        time.sleep(DOCKER_WAIT_SECS)
        rows = poll_block(sh, b["docker-check"], "docker check")
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
        rows = poll_block(sh, b["ufw-check"], "ufw check")
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
        T.note(f"SIMULATED DEVICE: namespace {PEER_DEVICE.netns} at {PEER_DEVICE.peer_ip} stands in "
               "for the appliance and sends one RFC 5424 test message; this is not a doc block")
        g.run(f"""ip netns exec {PEER_DEVICE.netns} bash -c 'printf "<134>1 %s fw01 filterlog - - - test message {device_marker}\\n" \\
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > /dev/udp/{PEER_DEVICE.guest_ip}/1514'""", root=True)
        rows = poll_block(sh, b["syslog-check"], "syslog check")
        expect_fields("syslog", rows, {"env": TRAWL_ENV, "service": "firewall", "host": "fw01",
                                       "_producer": "syslog",
                                       "syslog_source_ip": PEER_DEVICE.peer_ip})
        T.check("syslog: the test message carries the device's marker",
                any(device_marker in str(r.get("message")) for r in rows))

    # -- phase 10: negatives ------------------------------------------------

    def rejected(self, reason: str) -> int:
        assert self.guest is not None
        out = self.guest.run(
            f"curl -fsS --cacert {TRAWLD_CA} {TRAWLD_URL}/metrics "
            f"| grep -F 'trawl_ingest_events_rejected_total{{reason=\"{reason}\"}}' || true").stdout
        m = re.search(r"\}\s+(\d+)", out)
        return int(m.group(1)) if m else 0

    def restart_vector(self, script: str) -> str:
        assert self.guest is not None
        out = self.guest.run(f"""{script}
grep -E '^TRAWL_(URL|ENV)=' /etc/default/vector
since=$(date -u '+%Y-%m-%d %H:%M:%S UTC'); echo "since=$since"
systemctl restart vector
systemctl is-active vector""", root=True).stdout
        return re.search(r"since=(.+)", out).group(1).strip()  # type: ignore[union-attr]

    def positive_control(self, label: str) -> None:
        assert self.shell is not None
        b = self.blocks()
        marker, host_ = self.recipe_vars()
        self.shell.run_block(b["journald-send"])
        rows = poll_block(self.shell, b["journald-check"], f"{label} positive control")
        expect_fields(f"{label}: positive control after restore", rows, {
            "env": TRAWL_ENV, "service": marker, "host": host_, "message": marker})

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
        try:
            for label, edit, reason in cases:
                T.out(f"\n--- negative: {label}\n")
                before = self.rejected(reason) if reason else 0
                since = self.restart_vector(f"sed -i '{edit}' /etc/default/vector")
                self.recipe_vars()
                sh.run_block(b["journald-send"])
                rows = poll_block(sh, b["journald-check"], f"{label} check", diagnose=False)
                T.check(f"{label}: the journald check finds nothing", not rows, f"{len(rows)} row(s)")
                errors = g.run(f"journalctl -u vector --since '{since}' -o cat "
                               "| sed -e 's/\\x1b\\[[0-9;]*m//g' | grep -E ' ERROR sink' "
                               "| grep -v 'has been suppressed' | cut -c1-300 || true",
                               root=True, echo=False).stdout
                T.command(f"root@{GUEST_HOSTNAME}#", f"journalctl -u vector --since '{since}' -o cat "
                          "| sed -e 's/\\x1b\\[[0-9;]*m//g' | grep -E ' ERROR sink' "
                          "| grep -v 'has been suppressed' | cut -c1-300 "
                          "| grep -E 'Unauthorized|Bad Request|dropped'")
                T.output("\n".join(line for line in errors.splitlines()
                                   if re.search(r"Unauthorized|Bad Request|dropped", line))
                         or "(none)")
                if reason is None:
                    T.check("401: Vector logs the 401 from the trawld sink",
                            bool(re.search(r"401|Unauthorized", errors)))
                else:
                    after = self.rejected(reason)
                    T.check(f"{label}: trawl_ingest_events_rejected_total{{reason=\"{reason}\"}} rose",
                            after > before, f"{before} -> {after}")
                    T.check(f"{label}: Vector logs no sink error (trawld answered 200)",
                            errors.strip() == "", errors.strip()[:200])
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

    def check_not_dirty(self, version: str) -> None:
        dirty_build = f"({self.short_sha}*" in version
        if dirty_build and self.args.allow_dirty:
            T.note("--allow-dirty: trawld was built from a dirty tree ('*'); not asserted")
        else:
            T.check("trawld --version is not a dirty build", not dirty_build, version.strip())

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


def free_port() -> int:
    # qemu binds the port itself, so this is a best-effort pick; a collision
    # shows up as qemu exiting before SSH answers.
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
