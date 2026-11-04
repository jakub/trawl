#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Issue #335's RSS run: one release trawld under 32 live streams and a
burst of batch queries, sampled from /proc (ADR-0063).

scripts/evidence/issue-335/rss.sh builds trawld and fleet-admin at a pinned
SHA and runs this driver. Its stdout is the body of
docs/evidence/issue-335/rss.txt; progress goes to stderr. It uses the
standard library only.

What it does, in order:

  1. Creates two fresh databases on the private Postgres the caller names
     (never 5433 or 5434), migrates the Fleet one, and makes a reader key
     and a sender key that live two hours. The keys stay in memory; no
     output prints them.
  2. Starts the release trawld on 127.0.0.1, port 0, with a config of its
     own in a private run directory, and pins the certificate trawld
     generates there.
  3. Seeds the corpus: rows of 64 to 512 characters, seven in eight over
     three Deseret letters and the rest over CJK, ASCII and Specials, as the
     RE2 harness does (crates/trawl-engine/tests/re2_memory.rs), and waits
     until compaction has drained the hot buffer into Parquet.
  4. Finds the batch phase's body: CP8's worst `matches` body, `a (?:C){k}
     N` over Deseret padded with a Cyrillic class, at the largest count and
     then the largest padding for which n copies pass /api/v1/validate,
     where n is the most `matches` sites whose RE2 allowance fits the
     per-query cap on the configured query threads. It checks the cap
     itself with cheap bodies: n sites run, n + 1 are refused with the
     budget cause.
  5. Starts a feeder that ingests varied Unicode events through the whole
     measured run, warms the server up, and takes the idle baseline.
  6. Phase A opens 32 live streams, each holding the same four corpus
     patterns, and holds them while they match.
  7. Phase B, with the streams open, sends max_concurrent_queries batch
     queries of n copies of the body at once. While the one the
     reservation admits still runs, it opens extra streams that each hold
     five corpus patterns, up to the first refusal.
  8. Closes everything, samples what stays resident, and reports the peak
     growth over the baseline against the reservation plus the allocator
     retention the RE2 measurement recorded.

A 503's cause is not on the wire: the driver joins each response's
x-request-id to trawld's http_failure event in its JSON log file.

The sampler reads VmRSS from /proc/<pid>/status and Private_Dirty from
/proc/<pid>/smaps_rollup every quarter second.

Every process and directory it creates is removed on the way out, failure
included: trawld is stopped (and dies with the driver if the driver is
killed), the two databases are dropped and the run directory is deleted.

Exit status: 0 when every check passes, 3 when a check fails, 1 on an
error that stopped the run.
"""

import argparse
import concurrent.futures
import ctypes
import http.client
import json
import os
import random
import re
import secrets
import shutil
import signal
import socket
import ssl
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse
from datetime import datetime, timezone

MIB = 1 << 20
KIB = 1 << 10

# Phase A's stream: four LOG_REGEXES corpus patterns (trawl-core
# pattern/charge.rs) that charge the most, so 32 of them hold a large share
# of the reservation and still leave room for one batch query at the cap.
STREAM_PATTERNS = [r"\w{20}", r"(?i)\w{20}", r"\pL{12}", r"\w+@\w+\.\w{2,10}"]
# The extra heavy stream: the same four and one more corpus pattern, near
# the per-query budget.
HEAVY_PATTERNS = STREAM_PATTERNS + [r"[\w.+-]+@[\w-]+\.[\w.]{2,10}"]

DESERET = ("\U00010428", "\U00010429", "\U0001042A")
OTHERS = [("漢", "字", "文"), ("a", "b", "c"), ("�", "￼", "￻")]
# The padding class starts at Cyrillic, which no seeded row holds (CP8's
# PAD_START), and takes every other code point.
PAD_START = 0x400
MAX_K = 64
MAX_PAD = 1024

# The feeder's scripts: (first, last) letters of each.
SCRIPTS = [
    (0x61, 0x7A),        # Latin
    (0xE0, 0xFD),        # Latin-1 letters
    (0x430, 0x44F),      # Cyrillic
    (0x3B1, 0x3C9),      # Greek
    (0x5D0, 0x5EA),      # Hebrew
    (0x627, 0x64A),      # Arabic
    (0x915, 0x939),      # Devanagari
    (0xE01, 0xE2E),      # Thai
    (0x10D0, 0x10F0),    # Georgian
    (0x4E00, 0x4FFF),    # Han
    (0xAC00, 0xACFF),    # Hangul
    (0x10400, 0x1044F),  # Deseret
]
SEPARATORS = [" ", ", ", " - ", " / ", " \U0001F600 ", " — ", "; "]

SAMPLE_SECS = 0.25
# Compaction writes one Parquet file a service an hour, so the seeded rows
# take this many services to give DuckDB as many files to share among its
# threads, as the RE2 harness's 16 files do.
SEED_SERVICES = 16


def log(msg):
    print(f"rss: {msg}", file=sys.stderr, flush=True)


class Stop(Exception):
    """The run cannot go on; the message says why, with no secret in it."""


class Secrets:
    """Every secret the run makes, so no message carries one."""

    def __init__(self):
        self.values = []

    def add(self, value):
        if value:
            self.values.append(value)
        return value

    def scrub(self, text):
        for value in self.values:
            text = text.replace(value, "<redacted>")
        return text


SECRETS = Secrets()


# --------------------------------------------------------------------------
# Processes and the databases


def run(argv, env=None, what="command"):
    proc = subprocess.run(argv, env=env, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        tail = SECRETS.scrub((proc.stderr or proc.stdout or "").strip())[-600:]
        raise Stop(f"{what} exited {proc.returncode}: {tail}")
    return proc.stdout


class Postgres:
    """The private test cluster, addressed by the caller's URL."""

    def __init__(self, url):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme not in ("postgres", "postgresql"):
            raise Stop("RSS_DATABASE_URL is not a postgres URL")
        if parts.port in (5433, 5434):
            raise Stop("RSS_DATABASE_URL names port 5433 or 5434, which this run never uses")
        self.host = parts.hostname or "127.0.0.1"
        self.port = parts.port or 5432
        self.user = urllib.parse.unquote(parts.username or "")
        self.password = SECRETS.add(urllib.parse.unquote(parts.password or ""))
        self.admin_db = parts.path.lstrip("/") or "postgres"
        self.created = []

    def url(self, db):
        user = urllib.parse.quote(self.user, safe="")
        password = urllib.parse.quote(self.password, safe="")
        return SECRETS.add(f"postgres://{user}:{password}@{self.host}:{self.port}/{db}")

    def psql(self, sql):
        env = dict(os.environ, PGPASSWORD=self.password, PGCONNECT_TIMEOUT="10")
        run(["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-h", self.host, "-p", str(self.port),
             "-U", self.user, "-d", self.admin_db, "-c", sql], env=env, what="psql")

    def create(self, name):
        self.psql(f'CREATE DATABASE "{name}"')
        self.created.append(name)

    def drop_all(self):
        for name in reversed(self.created):
            try:
                self.psql(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')
            except Stop as error:
                log(f"could not drop a run database: {error}")
        self.created.clear()


def die_with_parent():
    """In the child, before exec: SIGKILL when the driver dies."""
    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    libc.prctl(1, signal.SIGKILL)  # PR_SET_PDEATHSIG


class Trawld:
    """The release trawld this run starts, and stops."""

    def __init__(self, binary, lib_dir, config, run_dir, env):
        self.out = open(os.path.join(run_dir, "trawld.stdout"), "wb")
        self.err = open(os.path.join(run_dir, "trawld.stderr"), "wb")
        child_env = dict(os.environ, **env)
        child_env["LD_LIBRARY_PATH"] = lib_dir + (":" + os.environ["LD_LIBRARY_PATH"] if os.environ.get("LD_LIBRARY_PATH") else "")
        child_env.pop("RUST_LOG", None)
        self.proc = subprocess.Popen([binary, "--config", config], env=child_env, stdout=self.out,
                                     stderr=self.err, stdin=subprocess.DEVNULL, start_new_session=True,
                                     preexec_fn=die_with_parent)
        self.pid = self.proc.pid
        self.stdout_path = self.out.name

    def wait_listening(self, timeout=120):
        deadline = time.monotonic() + timeout
        pattern = re.compile(rb"HTTPS server listening[^\n]*?addr[=:]\s*\"?127\.0\.0\.1:(\d+)")
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise Stop(f"trawld exited {self.proc.returncode} before it listened")
            with open(self.stdout_path, "rb") as f:
                text = re.sub(rb"\x1b\[[0-9;]*m", b"", f.read())
            found = pattern.search(text)
            if found:
                return int(found.group(1))
            time.sleep(0.2)
        raise Stop("trawld did not listen within two minutes")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        self.out.close()
        self.err.close()


# --------------------------------------------------------------------------
# HTTP


class Api:
    def __init__(self, port, cafile):
        self.port = port
        self.ctx = ssl.create_default_context(cafile=cafile)

    def connect(self, timeout):
        return http.client.HTTPSConnection("127.0.0.1", self.port, context=self.ctx, timeout=timeout)

    def call(self, method, path, key=None, body=None, content_type="application/json", timeout=600):
        """(status, request id, body bytes) of one request on a fresh connection."""
        conn = self.connect(timeout)
        try:
            headers = {}
            if key:
                headers["Authorization"] = "Bearer " + key
            data = None
            if body is not None:
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                headers["Content-Type"] = content_type
            conn.request(method, path, body=data, headers=headers)
            response = conn.getresponse()
            payload = response.read()
            return response.status, response.getheader("x-request-id"), payload
        finally:
            conn.close()


def error_code(payload):
    try:
        return json.loads(payload)["error"]["code"]
    except (ValueError, KeyError, TypeError):
        return "?"


class Stream:
    """One live stream: opened on its own connection, read on a thread
    until closed."""

    def __init__(self, api, key, dsl, opened):
        self.conn = api.connect(timeout=None)
        self.frames = 0
        self.first_frame = None
        self.opened = opened
        self.thread = None
        self.closing = False
        path = "/api/v1/stream?query=" + urllib.parse.quote(dsl, safe="")
        self.conn.request("GET", path, headers={"Authorization": "Bearer " + key, "Accept": "text/event-stream"})
        self.response = self.conn.getresponse()
        self.status = self.response.status
        self.request_id = self.response.getheader("x-request-id")
        if self.status == 200:
            self.thread = threading.Thread(target=self.read, daemon=True)
            self.thread.start()
        else:
            self.code = error_code(self.response.read())
            self.conn.close()

    def read(self):
        try:
            while True:
                line = self.response.readline()
                if not line:
                    return
                if line.rstrip(b"\r\n") == b"event: data":
                    self.frames += 1
                    if self.first_frame is None:
                        self.first_frame = time.monotonic() - self.opened
        except (OSError, http.client.HTTPException, ValueError):
            return

    def close(self):
        self.closing = True
        if self.status == 200:
            try:
                self.conn.sock.shutdown(socket.SHUT_RDWR)
            except (OSError, AttributeError):
                pass
            self.conn.close()
            if self.thread:
                self.thread.join(timeout=10)


# --------------------------------------------------------------------------
# The corpus and the feeder


def seed_row(rng, r):
    """A row as re2_memory.rs's: 64 to 512 characters, seven rows in eight
    over Deseret's a, b and c, the rest over another alphabet in turn; `a`
    at three or eight tenths, the rest b and c."""
    x = OTHERS[(r // 8) % len(OTHERS)] if r % 8 == 7 else DESERET
    length = rng.randint(64, 512)
    a_share = 3 if rng.randrange(2) == 0 else 8
    chars = []
    for _ in range(length):
        if rng.randrange(10) < a_share:
            chars.append(x[0])
        elif rng.randrange(2) == 0:
            chars.append(x[2])
        else:
            chars.append(x[1])
    return "".join(chars)


def word(rng, lo, hi):
    first, last = SCRIPTS[rng.randrange(len(SCRIPTS))]
    return "".join(chr(rng.randint(first, last)) for _ in range(rng.randint(lo, hi)))


def feed_message(rng, seq):
    """Words of 2 to 11 letters over twelve scripts, so most events match
    none of the stream patterns and every pattern scans the whole message.
    One event in eight holds a word of 20 to 24 letters or an address, so
    every stream has matches to send."""
    parts = [f"op {seq}"]
    for _ in range(rng.randint(20, 60)):
        parts.append(rng.choice(SEPARATORS))
        parts.append(word(rng, 2, 11))
    if seq % 8 == 0:
        parts.append(" ")
        if rng.randrange(2) == 0:
            parts.append(word(rng, 20, 24))
        else:
            parts.append(f"{word(rng, 3, 8)}@{word(rng, 3, 8)}.{word(rng, 2, 4)}")
    return "".join(parts)


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def ndjson(events):
    return "".join(json.dumps(e, ensure_ascii=False) + "\n" for e in events).encode()


def ingest(api, key, events):
    """Send a batch; retry while the hot buffer is full."""
    body = ndjson(events)
    for _ in range(120):
        status, _, payload = api.call("POST", "/api/v1/ingest", key, body, "application/x-ndjson")
        if status == 200:
            result = json.loads(payload)
            if result.get("rejected"):
                raise Stop(f"ingest rejected {result['rejected']} events")
            return result["accepted"]
        if status == 503:
            time.sleep(1)
            continue
        raise Stop(f"ingest answered {status} {error_code(payload)}")
    raise Stop("the hot buffer stayed full for two minutes")


class Feeder:
    """Ingests `per_tick` varied Unicode events every quarter second."""

    def __init__(self, api, key, per_tick):
        self.api = api
        self.key = key
        self.per_tick = per_tick
        self.rng = random.Random(0x335F)
        self.seq = 0
        self.sent = 0
        self.errors = 0
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def start(self):
        self.thread.start()

    def loop(self):
        while not self.stopping.is_set():
            started = time.monotonic()
            events = []
            for _ in range(self.per_tick):
                events.append({"timestamp": now_iso(), "service": "feed", "level": "info",
                               "message": feed_message(self.rng, self.seq)})
                self.seq += 1
            try:
                self.sent += ingest(self.api, self.key, events)
            except (Stop, OSError, http.client.HTTPException):
                self.errors += 1
            self.stopping.wait(max(0.0, 0.25 - (time.monotonic() - started)))

    def stop(self):
        self.stopping.set()
        self.thread.join(timeout=30)


# --------------------------------------------------------------------------
# The sampler


def read_kib(path, field):
    with open(path, encoding="ascii") as f:
        for line in f:
            if line.startswith(field):
                return int(line.split()[1])
    raise Stop(f"{field} missing from {os.path.basename(path)}")


class Sampler:
    def __init__(self, pid):
        self.pid = pid
        self.samples = []  # (seconds since start, phase, VmRSS KiB, Private_Dirty KiB)
        self.phase = "setup"
        self.t0 = time.monotonic()
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)

    def start(self):
        self.thread.start()

    def sample(self):
        rss = read_kib(f"/proc/{self.pid}/status", "VmRSS:")
        dirty = read_kib(f"/proc/{self.pid}/smaps_rollup", "Private_Dirty:")
        self.samples.append((time.monotonic() - self.t0, self.phase, rss, dirty))

    def loop(self):
        while not self.stopping.is_set():
            try:
                self.sample()
            except (OSError, Stop):
                return
            self.stopping.wait(SAMPLE_SECS)

    def set(self, phase):
        self.phase = phase
        log(f"phase {phase}")

    def stop(self):
        self.stopping.set()
        self.thread.join(timeout=10)

    def of(self, *phases):
        return [s for s in self.samples if s[1] in phases]


def mib(kib):
    return kib / KIB


# --------------------------------------------------------------------------
# trawld's log


def failures(log_path):
    """request id -> (status, cause_kind) of every http_failure event."""
    found = {}
    try:
        with open(log_path, encoding="utf-8", errors="replace") as f:
            for line in f:
                try:
                    record = json.loads(line)
                except ValueError:
                    continue
                fields = record.get("fields", record)
                if fields.get("event_type") != "http_failure":
                    continue
                found[fields.get("request_id")] = (fields.get("status"), fields.get("cause_kind"))
    except FileNotFoundError:
        pass
    return found


def cause(log_path, request_id, wait=5.0):
    """The cause trawld logged for a 5xx, read once its event is written."""
    deadline = time.monotonic() + wait
    while True:
        found = failures(log_path).get(request_id)
        if found or time.monotonic() > deadline:
            return found[1] if found else "not logged"
        time.sleep(0.2)


# --------------------------------------------------------------------------
# Bodies


def class3():
    return "[" + "".join(DESERET) + "]"


def not3():
    return "[^" + "".join(DESERET) + "]"


def core(k):
    return f"{DESERET[0]}(?:{class3()}){{{k}}}{not3()}"


def padded(body, pad):
    if pad == 0:
        return body
    cls = "".join(chr(PAD_START + 2 * i) for i in range(pad))
    return f"(?:{body}|[{cls}])"


def copies(body, n):
    return "message=" + ",".join(f"/{body}{i:03}/" for i in range(n))


def largest(maximum, admits):
    """The largest n in 0..=maximum with admits(n), which holds up to some n
    and fails after; None when admits(0) fails."""
    if not admits(0):
        return None
    ok, bad = 0, maximum + 1
    while bad - ok > 1:
        mid = ok + (bad - ok) // 2
        if admits(mid):
            ok = mid
        else:
            bad = mid
    return ok


# --------------------------------------------------------------------------
# The run


def config_text(args, run_dir):
    return f"""[server]
http_addr = "127.0.0.1:0"
timeout_secs = {args.timeout_secs}
max_concurrent_queries = {args.max_concurrent_queries}
query_threads = {args.query_threads}
max_concurrent_requests = 128
max_sse_connections = 40
shutdown_drain_secs = 3
log_file = {json.dumps(os.path.join(run_dir, "trawld.log"))}
[data]
path = {json.dumps(os.path.join(run_dir, "state", "data"))}
[ingest]
enabled = true
internal_telemetry = false
daily_rollup = false
compaction_interval_secs = 2
compaction_chunk_size = 512
default_env = "rss"
envs = ["rss"]
[retention]
max_age_days = 0
min_free_disk_bytes = 0
[scheduler]
enabled = false
[syslog]
enabled = false
"""


def hot_events(api):
    status, _, payload = api.call("GET", "/metrics", timeout=30)
    if status != 200:
        raise Stop(f"/metrics answered {status}")
    gauge = "trawl" + "_hot_buffer_events"
    total = 0.0
    for line in payload.decode().splitlines():
        if line.startswith(gauge) and (len(line) == len(gauge) or line[len(gauge)] in " {"):
            total += float(line.rsplit(" ", 1)[1])
    return total


def parquet_files(root):
    count = size = 0
    for dirpath, _, names in os.walk(root):
        for name in names:
            if name.endswith(".parquet"):
                count += 1
                size += os.path.getsize(os.path.join(dirpath, name))
    return count, size


def summary(samples):
    rss = [s[2] for s in samples]
    dirty = [s[3] for s in samples]
    return (statistics.median(rss), max(rss), statistics.median(dirty), max(dirty))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin-dir", required=True, help="the release target dir holding trawld, fleet-admin and deps/")
    parser.add_argument("--run-dir", required=True, help="a new private directory for this run; deleted at the end")
    parser.add_argument("--reservation-bytes", type=int, required=True)
    parser.add_argument("--query-cap-bytes", type=int, required=True)
    parser.add_argument("--match-site-bytes", type=int, required=True)
    parser.add_argument("--retention-mib", type=float, required=True)
    parser.add_argument("--max-concurrent-queries", type=int, default=8)
    parser.add_argument("--query-threads", type=int, default=4)
    parser.add_argument("--timeout-secs", type=int, default=600)
    parser.add_argument("--streams", type=int, default=32)
    parser.add_argument("--seed-rows", type=int, default=8192)
    parser.add_argument("--feed-per-tick", type=int, default=50)
    parser.add_argument("--hold-secs", type=float, default=20.0)
    args = parser.parse_args()

    url = os.environ.get("RSS_DATABASE_URL")
    if not url:
        raise Stop("set RSS_DATABASE_URL to the private test Postgres")
    pg = Postgres(url)

    run_dir = args.run_dir
    if os.path.exists(run_dir):
        raise Stop("the run directory exists already")
    os.makedirs(run_dir, mode=0o700)

    streams = []
    try:
        return measured_run(args, pg, run_dir, streams)
    finally:
        log("cleaning up")
        pg.drop_all()
        shutil.rmtree(run_dir, ignore_errors=True)


def measured_run(args, pg, run_dir, streams):
    out = print
    checks = []

    def check(name, ok, detail):
        checks.append((name, ok))
        out(f"CHECK {'PASS' if ok else 'FAIL'}: {name}: {detail}")

    threads = args.query_threads
    per_query_site = threads * args.match_site_bytes
    sites = args.query_cap_bytes // per_query_site
    allowance = sites * per_query_site

    out("Configuration (trawld.toml of the run; the keys this run sets):")
    out(f"  [server] max_concurrent_queries = {args.max_concurrent_queries}, query_threads = {threads}, "
        f"timeout_secs = {args.timeout_secs}")
    out("  [server] max_sse_connections = 40 and max_concurrent_requests = 128, so the run's 34 streams "
        "and 8 queries meet the regex bounds before the connection and request counts")
    out("  [ingest] compaction_interval_secs = 2, compaction_chunk_size = 512, internal_telemetry = false; "
        "[server] log_file holds the JSON log the causes are read from")
    out("  every other key at its default, query_memory_limit included")
    out()
    out("The bounds (from the pinned tree, passed in by rss.sh):")
    out(f"  reservation:          {args.reservation_bytes / MIB:.1f} MiB, server-wide")
    out(f"  per-query cap:        {args.query_cap_bytes / MIB:.1f} MiB of the reservation")
    out(f"  matches site:         {args.match_site_bytes / KIB:.0f} KiB a thread")
    out(f"  batch body sites n:   {sites}, the most whose allowance fits the cap on {threads} threads: "
        f"{sites} x {threads} x {args.match_site_bytes / KIB:.0f} KiB = {allowance / MIB:.1f} MiB; "
        f"{sites + 1} sites would hold {(sites + 1) * per_query_site / MIB:.1f} MiB")
    out(f"  allocator retention:  {args.retention_mib:.1f} MiB (docs/evidence/issue-335/re2-memory.txt, Retained)")
    bound_mib = args.reservation_bytes / MIB + args.retention_mib
    out(f"  RSS bound:            {bound_mib:.1f} MiB of growth over the idle baseline, the reservation plus the retention")
    out()

    fleet_db = f"rss335_{secrets.token_hex(4)}_fleet"
    app_db = fleet_db.replace("_fleet", "_app")
    log("creating the run databases")
    pg.create(fleet_db)
    pg.create(app_db)
    fleet_url = pg.url(fleet_db)
    app_url = pg.url(app_db)
    admin = os.path.join(args.bin_dir, "fleet-admin")
    admin_env = dict(os.environ, DATABASE_URL=fleet_url)
    run([admin, "migrate"], env=admin_env, what="fleet-admin migrate")
    run([admin, "roles", "create", "--name", "rss-reader", "--rate-rpm", "100000", "--perm", "trawl:query",
         "--perm", "trawl:stream", "--perm", "trawl:validate"], env=admin_env, what="fleet-admin roles create")
    run([admin, "roles", "create", "--name", "rss-sender", "--rate-rpm", "100000", "--perm", "trawl:ingest"],
        env=admin_env, what="fleet-admin roles create")
    reader = SECRETS.add(run([admin, "keys", "create", "--name", "rss-reader", "--kind", "human", "--role",
                              "rss-reader", "--expires", "2h"], env=admin_env, what="fleet-admin keys create").strip())
    writer = SECRETS.add(run([admin, "keys", "create", "--name", "rss-sender", "--kind", "service", "--role",
                              "rss-sender", "--expires", "2h"], env=admin_env, what="fleet-admin keys create").strip())
    if not reader or not writer:
        raise Stop("fleet-admin printed no key")

    config = os.path.join(run_dir, "trawld.toml")
    with open(config, "w", encoding="utf-8") as f:
        f.write(config_text(args, run_dir))
    log_path = os.path.join(run_dir, "trawld.log")

    trawld = Trawld(os.path.join(args.bin_dir, "trawld"), os.path.join(args.bin_dir, "deps"), config, run_dir,
                    {"FLEET_DATABASE_URL": fleet_url, "TRAWL_DATABASE_URL": app_url})
    try:
        return with_server(args, out, check, checks, trawld, reader, writer, log_path, run_dir, streams,
                           sites, allowance, bound_mib)
    finally:
        for s in streams:
            s.close()
        trawld.stop()


def with_server(args, out, check, checks, trawld, reader, writer, log_path, run_dir, streams, sites,
                allowance, bound_mib):
    port = trawld.wait_listening()
    api = Api(port, os.path.join(run_dir, "state", "tls", "cert.pem"))
    status, _, _ = api.call("GET", "/api/v1/health", timeout=30)
    if status != 200:
        raise Stop(f"health answered {status}")
    sampler = Sampler(trawld.pid)
    sampler.start()
    try:
        return phases(args, out, check, checks, api, sampler, reader, writer, log_path, run_dir, streams,
                      sites, allowance, bound_mib)
    finally:
        sampler.stop()


def phases(args, out, check, checks, api, sampler, reader, writer, log_path, run_dir, streams, sites,
           allowance, bound_mib):
    # ---- seed --------------------------------------------------------------
    sampler.set("seed")
    rng = random.Random(0x335E)
    seeded = chars = 0
    batch = []
    for r in range(args.seed_rows):
        message = seed_row(rng, r)
        chars += len(message)
        batch.append({"timestamp": now_iso(), "service": f"seed-{r % SEED_SERVICES:02d}", "level": "info",
                      "message": message})
        if len(batch) == 512:
            seeded += ingest(api, writer, batch)
            batch = []
    if batch:
        seeded += ingest(api, writer, batch)
    deadline = time.monotonic() + 300
    while hot_events(api) > 0:
        if time.monotonic() > deadline:
            raise Stop("compaction did not drain the seeded rows within five minutes")
        time.sleep(1)
    files, size = parquet_files(os.path.join(run_dir, "state"))
    out(f"Corpus: {seeded} seeded rows over {SEED_SERVICES} services, {chars} characters, 64 to 512 a row; seven "
        f"rows in eight over three Deseret letters, the rest CJK, ASCII and Specials in turn. Compaction drained "
        f"them into {files} Parquet files, {size / MIB:.1f} MiB.")
    out()

    # ---- the batch body ----------------------------------------------------
    sampler.set("prepare")

    def valid(dsl):
        status, _, payload = api.call("POST", "/api/v1/validate", reader, {"query": dsl}, timeout=120)
        if status != 200:
            raise Stop(f"validate answered {status} {error_code(payload)}")
        return json.loads(payload)["valid"]

    k = largest(MAX_K, lambda k: k == 0 or valid(copies(padded(core(k), 1), sites)))
    if not k:
        raise Stop("no count of the body passes validate")
    pad = largest(MAX_PAD, lambda p: valid(copies(padded(core(k), p), sites)))
    if pad is None:
        raise Stop("no padding of the body passes validate")
    body = padded(core(k), pad)
    heavy_dsl = copies(body, sites)
    out(f"Batch body: CP8's worst matches body, a (?:C){{k}} N over Deseret padded as (?:core|[P]), at the largest "
        f"k and then the largest P for which {sites} copies pass /api/v1/validate: k = {k}, P = {pad} Cyrillic "
        f"letters, {len(body.encode())} bytes a body, {len(heavy_dsl.encode())} bytes a query of {sites} copies "
        f"(each told apart by a three-digit suffix).")

    cheap = lambda n: "message=" + ",".join(f"/zq{i:03}x/" for i in range(n))  # noqa: E731
    status, rid, payload = api.call("POST", "/api/v1/query", reader, {"query": cheap(sites + 1), "limit": 10})
    over = (status, error_code(payload), cause(log_path, rid) if status >= 500 else "-")
    status, rid, payload = api.call("POST", "/api/v1/query", reader, {"query": cheap(sites), "limit": 10})
    fit = (status, error_code(payload) if status != 200 else "-")
    out(f"Cap check, alone on the server with cheap distinct bodies: {sites + 1} sites answer {over[0]} {over[1]} "
        f"(cause {over[2]}); {sites} sites answer {fit[0]}.")
    check("the per-query cap", over[0] == 503 and over[2] == "regex_budget_exhausted" and fit[0] == 200,
          f"{sites + 1} sites refused with the budget cause, {sites} sites run")
    out()

    # ---- warm-up and baseline ----------------------------------------------
    sampler.set("warm")
    feeder = Feeder(api, writer, args.feed_per_tick)
    feeder.start()
    try:
        return measured(args, out, check, checks, api, sampler, reader, log_path, streams, sites, allowance,
                        bound_mib, heavy_dsl, feeder)
    finally:
        feeder.stop()


def measured(args, out, check, checks, api, sampler, reader, log_path, streams, sites, allowance, bound_mib,
             heavy_dsl, feeder):
    status, _, _ = api.call("POST", "/api/v1/query", reader, {"query": r"message=/\w{10}/ | stats count()"})
    warm = Stream(api, reader, "message=" + ",".join(f"/{p}/" for p in STREAM_PATTERNS), time.monotonic())
    time.sleep(2)
    warm.close()
    time.sleep(3)
    sampler.set("baseline")
    time.sleep(5)
    base = summary(sampler.of("baseline"))
    base_rss, base_dirty = base[0], base[2]
    out(f"Feeder: {args.feed_per_tick} events every quarter second, words of 2 to 11 letters over twelve scripts "
        "(Latin, Latin-1, Cyrillic, Greek, Hebrew, Arabic, Devanagari, Thai, Georgian, Han, Hangul, Deseret) "
        "between punctuation and emoji; one event in eight adds a word of 20 to 24 letters or an address, so "
        "every stream matches some events and scans every event whole.")
    out(f"Warm-up: one batch query and one stream opened and closed, then 3 s. Idle baseline (median of "
        f"{len(sampler.of('baseline'))} samples over 5 s, feeder running): VmRSS {mib(base_rss):.1f} MiB, "
        f"Private_Dirty {mib(base_dirty):.1f} MiB.")
    out()

    # ---- phase A -----------------------------------------------------------
    sampler.set("A")
    stream_dsl = "message=" + ",".join(f"/{p}/" for p in STREAM_PATTERNS)
    out(f"PHASE A: {args.streams} live streams, each {stream_dsl}")
    opened = time.monotonic()
    for _ in range(args.streams):
        streams.append(Stream(api, reader, stream_dsl, time.monotonic()))
    open_secs = time.monotonic() - opened
    started = [s for s in streams if s.status == 200]
    deadline = time.monotonic() + 30
    while any(s.first_frame is None for s in started) and time.monotonic() < deadline:
        time.sleep(0.2)
    time.sleep(max(0.0, args.hold_secs - (time.monotonic() - opened)))
    framed = [s for s in started if s.frames > 0]
    firsts = sorted(s.first_frame for s in framed)
    frames = sorted(s.frames for s in started)
    statuses = sorted({s.status for s in streams})
    out(f"  opened in {open_secs:.2f} s; statuses {statuses}; {len(started)} of {args.streams} started")
    if firsts:
        out(f"  first data frame after {firsts[0]:.2f} to {firsts[-1]:.2f} s; data frames a stream over the hold: "
            f"{frames[0]} to {frames[-1]} (median {statistics.median(frames):.0f})")
    a = summary(sampler.of("A"))
    out(f"  RSS while they match ({len(sampler.of('A'))} samples): VmRSS median {mib(a[0]):.1f}, peak {mib(a[1]):.1f} MiB "
        f"(+{mib(a[1] - base_rss):.1f}); Private_Dirty median {mib(a[2]):.1f}, peak {mib(a[3]):.1f} MiB "
        f"(+{mib(a[3] - base_dirty):.1f})")
    check("every phase A stream starts and matches", len(framed) == args.streams,
          f"{len(started)} answered 200, {len(framed)} sent data frames")
    out()

    # ---- phase B -----------------------------------------------------------
    sampler.set("B")
    queries = args.max_concurrent_queries
    out(f"PHASE B: {queries} batch queries at once, each {sites} copies of the body "
        f"(RE2 allowance {allowance / MIB:.1f} MiB each), with the {len(started)} streams open")
    gate = threading.Barrier(queries + 1)
    results = [None] * queries

    def one(i):
        gate.wait()
        t = time.monotonic()
        status, rid, payload = api.call("POST", "/api/v1/query", reader, {"query": heavy_dsl, "limit": 10},
                                        timeout=args.timeout_secs + 120)
        elapsed = time.monotonic() - t
        detail = "-"
        if status == 200:
            body = json.loads(payload)
            detail = f"{body['pagination']['total']} rows, execution {body['execution']['duration_ms']} ms"
        results[i] = {"status": status, "rid": rid, "secs": elapsed, "code": error_code(payload) if status != 200 else "-",
                      "detail": detail, "done": time.monotonic()}

    pool = concurrent.futures.ThreadPoolExecutor(max_workers=queries)
    futures = [pool.submit(one, i) for i in range(queries)]
    gate.wait()
    sent = time.monotonic()
    # The refusals come back at once; the admitted query runs on.
    deadline = time.monotonic() + 60
    while sum(f.done() for f in futures) < queries - 1 and time.monotonic() < deadline:
        time.sleep(0.05)
    refused_back = time.monotonic() - sent

    extras = []
    for attempt in range(1, 5):
        in_flight = sum(not f.done() for f in futures)
        s = Stream(api, reader, "message=" + ",".join(f"/{p}/" for p in HEAVY_PATTERNS), time.monotonic())
        extras.append((attempt, s, in_flight))
        streams.append(s)
        if s.status != 200:
            break
    for f in futures:
        f.result(timeout=args.timeout_secs + 180)
    pool.shutdown()
    total_secs = time.monotonic() - sent

    admitted = refused = 0
    reservation_refusals = 0
    out("  query  status  code                      cause                        seconds  result")
    for i, r in enumerate(results):
        why = cause(log_path, r["rid"]) if r["status"] >= 500 else "-"
        if r["status"] == 200:
            admitted += 1
        else:
            refused += 1
            reservation_refusals += why == "regex_reservation_exhausted"
        out(f"  {i:>5}  {r['status']:>6}  {r['code']:<24}  {why:<27}  {r['secs']:>7.2f}  {r['detail']}")
    out(f"  {queries - 1} answers back {refused_back:.2f} s after the send; all {queries} back after {total_secs:.1f} s")
    check("phase B queries split", admitted >= 1 and refused >= 1 and reservation_refusals == refused,
          f"{admitted} ran, {refused} refused, {reservation_refusals} of them with regex_reservation_exhausted")
    out()
    out(f"  extra streams, each {'message=' + ','.join(f'/{p}/' for p in HEAVY_PATTERNS)}, opened while the "
        "admitted query runs:")
    extra_refused = False
    for attempt, s, in_flight in extras:
        why = cause(log_path, s.request_id) if s.status >= 500 else "-"
        code = getattr(s, "code", "-")
        out(f"  extra {attempt}: {s.status} {code} (cause {why}); batch queries in flight when it opened: {in_flight}")
        if s.status == 503 and why == "regex_reservation_exhausted" and in_flight >= 1:
            extra_refused = True
    check("one more heavy stream is refused", extra_refused,
          "an extra stream answered 503 with regex_reservation_exhausted while the admitted query held its allowance")
    b = summary(sampler.of("B"))
    out(f"  RSS through phase B ({len(sampler.of('B'))} samples): VmRSS median {mib(b[0]):.1f}, peak {mib(b[1]):.1f} MiB "
        f"(+{mib(b[1] - base_rss):.1f}); Private_Dirty median {mib(b[2]):.1f}, peak {mib(b[3]):.1f} MiB "
        f"(+{mib(b[3] - base_dirty):.1f})")
    out()

    # ---- close -------------------------------------------------------------
    sampler.set("close")
    for s in streams:
        s.close()
    feeder.stop()
    time.sleep(5)
    sampler.set("after")
    time.sleep(5)
    after = summary(sampler.of("after"))
    out(f"CLOSE: every stream closed and the feeder stopped ({feeder.sent} events fed, {feeder.errors} failed "
        f"sends); 5 s later, VmRSS median {mib(after[0]):.1f} MiB (+{mib(after[0] - base_rss):.1f} over the "
        f"baseline), Private_Dirty median {mib(after[2]):.1f} MiB (+{mib(after[2] - base_dirty):.1f}).")
    out()

    # ---- timeline and verdict ------------------------------------------------
    out("Timeline, the largest sample of each second (MiB; growth over the idle baseline):")
    out("      t  phase       VmRSS   growth  Private_Dirty   growth")
    by_second = {}
    for t, phase, rss, dirty in sampler.samples:
        key = int(t)
        held = by_second.get(key)
        if held is None or rss > held[2]:
            by_second[key] = (t, phase, rss, dirty)
    for key in sorted(by_second):
        t, phase, rss, dirty = by_second[key]
        if phase in ("setup", "seed", "prepare"):
            continue
        out(f"  {key:>5d}  {phase:<8}  {mib(rss):>8.1f}  {mib(rss - base_rss):>+7.1f}  {mib(dirty):>13.1f}  "
            f"{mib(dirty - base_dirty):>+7.1f}")
    out()
    measured_samples = sampler.of("A", "B", "close")
    peak = max(measured_samples, key=lambda s: s[2])
    peak_dirty = max(s[3] for s in measured_samples)
    growth = mib(peak[2] - base_rss)
    growth_dirty = mib(peak_dirty - base_dirty)
    out("RESULT")
    out(f"  peak VmRSS {mib(peak[2]):.1f} MiB in phase {peak[1]}, {growth:.1f} MiB over the idle baseline")
    out(f"  peak Private_Dirty {mib(peak_dirty):.1f} MiB, {growth_dirty:.1f} MiB over the idle baseline")
    out(f"  against the reservation alone ({args.reservation_bytes / MIB:.1f} MiB): "
        f"{'within' if growth <= args.reservation_bytes / MIB else 'over'}")
    check("peak RSS growth within the reservation plus the allocator retention", growth <= bound_mib,
          f"{growth:.1f} MiB against {bound_mib:.1f} MiB")
    failed = [name for name, ok in checks if not ok]
    out(f"VERDICT: {'PASS' if not failed else 'FAIL'} ({len(checks) - len(failed)} of {len(checks)} checks pass"
        + (f"; failed: {', '.join(failed)}" if failed else "") + ")")
    return 0 if not failed else 3


def on_term(signum, frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, on_term)
    try:
        sys.exit(main())
    except Stop as error:
        print(f"STOPPED: {SECRETS.scrub(str(error))}")
        log(SECRETS.scrub(str(error)))
        sys.exit(1)
