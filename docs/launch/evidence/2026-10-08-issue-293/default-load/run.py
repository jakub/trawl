#!/usr/bin/env python3
"""AC23 of issue #293: the default of 32 refuses nothing in normal use.

One disposable run against a debug trawld at the default
max_concurrent_requests:

* three Vector 0.57.0 processes replay
  crates/trawl-server/tests/fixtures/vector-capture/debian.ndjson through the
  shipped sink, sinks.trawld of config/vector/debian/base.toml, read from
  that file at run time (1 MB gzip batches, 5 s batch timeout, adaptive
  concurrency, disk buffer);
* one search-query loop stands in for the SPA;
* compaction runs on trawld's default 10 s interval, and on hot-buffer
  pressure, while the senders deliver. A rollup needs hourly files under a
  date before today, and trawld files events by arrival time, so two short
  seed sessions compact fixture cycles and their hour directories are moved
  under yesterday's date while trawld is down (see seed_history). The
  measured session's first normal pass, 10 s after boot, rolls them up
  while the senders drain their backlog.

Each sender first writes a backlog of whole fixture cycles to its Vector as
fast as Vector reads them, as on a first start, then follows at a steady
rate.

A scraper reads /metrics at a fixed cadence. The run passes when
trawl_http_requests_refused_total is 0 for both allowances.

Usage, from the repository root:

    python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/run.py \\
        --work .flow-scratch/default-load --vector /path/to/vector

--work is private: it holds the database password, the API keys, the data
directory and the raw logs, and is deleted at the end unless --keep-work is
given. Only summaries reach --out. The script owns one Postgres container,
one trawld and three Vector processes, and removes all of them on exit.
"""

import argparse
import copy
import csv
import gzip
import hashlib
import http.client
import json
import os
import platform
import re
import secrets
import shutil
import signal
import ssl
import statistics
import subprocess
import sys
import threading
import time
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = Path(
    subprocess.run(["git", "-C", str(HERE), "rev-parse", "--show-toplevel"],
                   check=True, capture_output=True, text=True).stdout.strip())
FIXTURE = ROOT / "crates/trawl-server/tests/fixtures/vector-capture/debian.ndjson"
VECTOR_BASE = ROOT / "config/vector/debian/base.toml"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
if not TARGET.is_absolute():
    TARGET = ROOT / TARGET
POSTGRES_IMAGE = "postgres:18"
DOCKER = ["docker", "--host", "unix:///var/run/docker.sock"]
# Wide enough for every fixture timestamp (2024-01-15 to 2026-09-28).
WINDOW = 'earliest="2024-01-01T00:00:00Z" latest="2026-10-01T00:00:00Z"'

LOGFMT_FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')
METRIC_LINE = re.compile(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+(\S+)$')
SAMPLED = [
    ("in_progress_regular", "trawl_http_requests_in_progress", 'allowance="regular"'),
    ("in_progress_control", "trawl_http_requests_in_progress", 'allowance="control"'),
    ("refused_regular", "trawl_http_requests_refused_total", 'allowance="regular"'),
    ("refused_control", "trawl_http_requests_refused_total", 'allowance="control"'),
    ("allowance_regular", "trawl_http_request_allowance", 'allowance="regular"'),
    ("allowance_control", "trawl_http_request_allowance", 'allowance="control"'),
    ("hot_buffer_events", "trawl_hot_buffer_events", None),
    ("hot_buffer_admission_state", "trawl_hot_buffer_admission_state", None),
    ("wal_files", "trawl_wal_files", None),
    ("active_connections", "trawl_active_connections", None),
]


def log(message):
    stamp = datetime.now(timezone.utc).strftime("%H:%M:%S")
    print(f"[{stamp}] {message}", flush=True)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_metrics(text):
    """Map (name, label string) to value, plus name to the sum over labels."""
    series, totals = {}, {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        m = METRIC_LINE.match(line)
        if not m:
            continue
        name, labels, value = m.group(1), (m.group(2) or "")[1:-1], float(m.group(3))
        series[(name, labels)] = value
        totals[name] = totals.get(name, 0.0) + value
    return series, totals


def pick(series, totals, name, label):
    if label is None:
        return totals.get(name)
    for (n, labels), value in series.items():
        if n == name and label in labels.split(","):
            return value
    return None


class Run:
    def __init__(self, args):
        self.args = args
        self.work = Path(args.work).resolve()
        self.out = Path(args.out).resolve()
        self.run_id = f"ac23-{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}-{secrets.token_hex(3)}"
        self.container = f"trawl-ac23-{secrets.token_hex(8)}"
        self.secrets = []
        self.children = []
        self.stop = threading.Event()
        self.samples = []
        self.queries = []
        self.summary = {"runId": self.run_id, "status": "running"}
        self.feed_lock = threading.Lock()

    # -- process plumbing ------------------------------------------------

    def redact(self, text):
        for s in self.secrets:
            text = text.replace(s, "[redacted]")
        return text

    def cmd(self, argv, env=None, timeout=300, capture=True):
        full_env = {k: os.environ[k] for k in ("PATH", "HOME", "USER", "LANG") if k in os.environ}
        full_env.update(env or {})
        result = subprocess.run(argv, env=full_env, timeout=timeout, capture_output=capture,
                                text=True, cwd=ROOT)
        if result.returncode != 0:
            raise RuntimeError(f"{Path(argv[0]).name} {argv[1] if len(argv) > 1 else ''} failed: "
                               f"{self.redact(result.stderr)[-1500:]}")
        return result.stdout.strip() if capture else ""

    def spawn(self, argv, env, log_path, stdin=None):
        full_env = {k: os.environ[k] for k in ("PATH", "HOME", "USER", "LANG") if k in os.environ}
        full_env.update(env)
        handle = open(log_path, "ab")
        child = subprocess.Popen(argv, env=full_env, stdin=stdin, stdout=handle, stderr=handle,
                                 cwd=ROOT, start_new_session=True)
        handle.close()
        self.children.append(child)
        return child

    def terminate(self, child, grace=30):
        if child.poll() is not None:
            return
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            child.wait(grace)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()

    # -- setup -------------------------------------------------------------

    def preflight(self):
        for forbidden in (Path("/tmp"), HERE):
            if self.work == forbidden or forbidden in self.work.parents:
                raise SystemExit(f"--work must not be under {forbidden}")
        if self.work.exists():
            raise SystemExit(f"--work {self.work} already exists; remove it or pick another")
        dirty = self.cmd(["git", "-C", str(ROOT), "status", "--porcelain", "--", "crates",
                          "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "bin/trawld-dev"])
        if dirty:
            raise SystemExit(f"application sources differ from HEAD; refusing:\n{dirty}")
        head = self.cmd(["git", "-C", str(ROOT), "rev-parse", "HEAD"])
        log(f"building debug trawld and fleet-admin at {head}")
        self.cmd(["cargo", "build", "--locked", "--no-default-features",
                  "-p", "trawl-server", "-p", "fleet-admin"],
                 env={k: os.environ[k] for k in ("CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR")
                      if k in os.environ}, timeout=3600, capture=True)
        # A concurrent writer could have committed during the build.
        if self.cmd(["git", "-C", str(ROOT), "rev-parse", "HEAD"]) != head:
            raise SystemExit("HEAD moved during the build; run again")
        version = self.cmd([self.args.vector, "--version"])
        if not version.startswith("vector 0.57.0 "):
            raise SystemExit(f"expected Vector 0.57.0, got {version}")
        trawld = TARGET / "debug/trawld"
        self.summary["build"] = {
            "head": head,
            "profile": "debug (cargo build --locked --no-default-features)",
            "trawldSha256": sha256(trawld),
            "vector": version,
            "vectorSha256": sha256(self.args.vector),
        }
        self.summary["host"] = {
            "platform": platform.platform(),
            "cpu": platform.processor() or platform.machine(),
            "logicalCpus": os.cpu_count(),
            "memTotalKiB": int(next(l.split()[1] for l in open("/proc/meminfo")
                                    if l.startswith("MemTotal:"))),
        }
        self.work.mkdir(parents=True, mode=0o700)
        (self.work / "private").mkdir(mode=0o700)

    def start_postgres(self):
        password = secrets.token_hex(24)
        self.secrets.append(password)
        env_file = self.work / "private/postgres.env"
        env_file.write_text(f"POSTGRES_USER=ac23\nPOSTGRES_PASSWORD={password}\nPOSTGRES_DB=fleet\n")
        env_file.chmod(0o600)
        log(f"starting Postgres container {self.container}")
        self.cmd(DOCKER + ["run", "--detach", "--name", self.container,
                           "--label", f"trawl.experiment={self.run_id}",
                           "--publish", "127.0.0.1::5432", "--env-file", str(env_file),
                           "--memory", "1g", "--tmpfs", "/var/lib/postgresql:size=512m",
                           POSTGRES_IMAGE])
        mapping = self.cmd(DOCKER + ["port", self.container, "5432/tcp"]).splitlines()[0]
        if not re.fullmatch(r"127\.0\.0\.1:\d+", mapping):
            raise RuntimeError(f"unexpected Postgres port mapping {mapping}")
        deadline = time.monotonic() + 90
        while True:
            probe = subprocess.run(DOCKER + ["exec", self.container, "pg_isready", "-h", "127.0.0.1",
                                             "-U", "ac23", "-d", "fleet"], capture_output=True)
            if probe.returncode == 0:
                break
            if time.monotonic() > deadline:
                raise RuntimeError("Postgres readiness deadline")
            time.sleep(0.5)
        self.cmd(DOCKER + ["exec", self.container, "createdb", "-U", "ac23", "trawl"])
        self.fleet_dsn = f"postgres://ac23:{password}@{mapping}/fleet"
        self.app_dsn = f"postgres://ac23:{password}@{mapping}/trawl"
        self.secrets += [self.fleet_dsn, self.app_dsn]

    def provision_keys(self):
        admin = str(TARGET / "debug/fleet-admin")
        env = {"DATABASE_URL": self.fleet_dsn}
        self.cmd([admin, "migrate"], env=env)
        # The role rate limit is raised so the per-key token bucket (ADR-0010)
        # never throttles a sender or the query loop: the run measures the
        # request limit, and a 429 would hide load from it.
        roles = [("ac23-reader", ["query", "schema_read", "validate"]), ("ac23-ingest", ["ingest"])]
        for name, perms in roles:
            self.cmd([admin, "roles", "create", "--name", name, "--rate-rpm", "100000",
                      *[a for p in perms for a in ("--perm", f"trawl:{p}")]], env=env)
        self.reader = self.cmd([admin, "keys", "create", "--name", "ac23-spa", "--kind", "human",
                                "--role", "ac23-reader", "--expires", "2h"], env=env)
        self.secrets.append(self.reader)
        self.ingest_keys = []
        for i in range(self.args.senders):
            key = self.cmd([admin, "keys", "create", "--name", f"ac23-vector-{i + 1}", "--kind",
                            "service", "--role", "ac23-ingest", "--expires", "2h"], env=env)
            self.secrets.append(key)
            self.ingest_keys.append(key)

    def start_trawld(self, log_name):
        state = self.work / "state"
        self.data = state / "data"
        config = (HERE / "trawld.toml").read_text().replace("@DATA_DIR@", str(self.data))
        config_path = self.work / "private/trawld.toml"
        config_path.write_text(config)
        self.trawld_log = self.work / log_name
        env = {"FLEET_DATABASE_URL": self.fleet_dsn, "TRAWL_DATABASE_URL": self.app_dsn,
               "RUST_LOG": "trawl_server=info,fleet_auth=info", "NO_COLOR": "1"}
        for k in ("CARGO_TARGET_DIR",):
            if k in os.environ:
                env[k] = os.environ[k]
        log(f"starting trawld ({log_name})")
        self.trawld = self.spawn([str(ROOT / "bin/trawld-dev"), "--config", str(config_path)],
                                 env, self.trawld_log)
        deadline = time.monotonic() + 180
        addr = None
        while addr is None:
            if self.trawld.poll() is not None:
                raise RuntimeError("trawld exited during startup; see its log in --work")
            if time.monotonic() > deadline:
                raise RuntimeError("trawld readiness deadline")
            text = self.trawld_log.read_text(errors="replace")
            m = re.search(r'HTTPS server listening[^\n]*?addr[=:]\s*"?(127\.0\.0\.1:\d+)', text) or \
                re.search(r'"addr"\s*:\s*"(127\.0\.0\.1:\d+)"[^\n]*HTTPS server listening', text)
            if m:
                addr = m.group(1)
            else:
                time.sleep(0.2)
        self.host, self.port = addr.split(":")[0], int(addr.split(":")[1])
        self.cert = state / "tls/cert.pem"
        self.tls = ssl.create_default_context(cafile=str(self.cert))
        status, body = self.http("GET", "/api/v1/health")
        if status != 200:
            raise RuntimeError(f"health answered {status}: {body[:300]}")
        log(f"trawld ready on {addr}")

    def stop_trawld(self):
        self.terminate(self.trawld)
        self.children.remove(self.trawld)
        if self.trawld.returncode != 0:
            raise RuntimeError(f"trawld exited {self.trawld.returncode} on SIGTERM")

    def seed_history(self):
        """Give the measured session a day to roll up.

        trawld files events under the UTC date and hour they arrive, not
        their _time, and rolls up only date directories before today. No
        endpoint or setting starts a rollup, so two short sessions ingest
        fixture cycles, wait until compaction has written them to hourly
        parquet, and stop. Each session's hour directory is then moved
        under yesterday's date while trawld is down. The measured session's
        first normal compaction pass, 10 s after boot and after the senders
        start, must roll yesterday's hourly files into daily files.
        """
        planted = []
        hours = ["22", "23"]
        for session in range(len(hours)):
            self.start_trawld(f"trawld-seed-{session + 1}.log")
            if session == 0:
                self.calibrate()
            posted = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
            self.post_cycles(f"seed-{session + 1}", self.args.seed_cycles)
            # A normal pass compacts WAL older than the 10 s interval, so the
            # seed reaches parquet within about two intervals.
            deadline = time.monotonic() + 90
            needed = {"init", "nginx", "apache", "postgresql"}
            while True:
                done = {fields.get("compact_service") for fields in self.events(self.trawld_log)
                        if fields.get("event_type") == "compaction_complete"
                        and fields["timestamp"] >= posted}
                if needed <= done:
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(f"seed {session + 1} did not compact: {needed - done}")
                time.sleep(1)
            self.stop_trawld()
            env_root = self.data / "prod"
            days = [d for d in env_root.iterdir() if re.fullmatch(r"\d{4}-\d{2}-\d{2}", d.name)
                    and d.name not in {p["day"] for p in planted}]
            if len(days) != 1 or len(list(days[0].iterdir())) != 1:
                raise RuntimeError(f"expected one day with one hour directory, found {days}")
            today = datetime.strptime(days[0].name, "%Y-%m-%d")
            yesterday = (today - timedelta(days=1)).date().isoformat()
            source = next(days[0].iterdir())
            target = env_root / yesterday / hours[session]
            target.parent.mkdir(exist_ok=True)
            files = sorted(f.name for f in source.iterdir())
            source.rename(target)
            days[0].rmdir()
            planted.append({"day": yesterday, "hour": hours[session],
                            "movedFrom": f"{days[0].name}/{source.name}", "parquetFiles": len(files)})
            log(f"seed {session + 1}: moved {len(files)} hourly files to prod/{yesterday}/{hours[session]}")
        self.summary["seededHistory"] = planted

    def http(self, method, path, body=None, token=None, headers=None, timeout=120):
        conn = http.client.HTTPSConnection(self.host, self.port, context=self.tls, timeout=timeout)
        try:
            hdrs = dict(headers or {})
            if token:
                hdrs["Authorization"] = f"Bearer {token}"
            if body is not None and not isinstance(body, bytes):
                body = json.dumps(body).encode()
                hdrs.setdefault("Content-Type", "application/json")
            conn.request(method, path, body=body, headers=hdrs)
            response = conn.getresponse()
            return response.status, response.read().decode(errors="replace")
        finally:
            conn.close()

    # -- workload ----------------------------------------------------------

    def load_fixture(self):
        self.fixture = [json.loads(l) for l in FIXTURE.read_text().splitlines() if l.strip()]
        self.summary["fixture"] = {"path": str(FIXTURE.relative_to(ROOT)),
                                   "events": len(self.fixture), "sha256": sha256(FIXTURE)}

    def post_cycles(self, sender, cycles):
        """POST fixture cycles as one gzip JSON array, the way Vector does."""
        events = [dict(e, load_run=self.run_id, load_sender=sender, load_seq=i * len(self.fixture) + j)
                  for i in range(cycles) for j, e in enumerate(self.fixture)]
        body = gzip.compress(json.dumps(events).encode())
        status, text = self.http("POST", "/api/v1/ingest", body=body, token=self.ingest_keys[0],
                                 headers={"Content-Type": "application/json",
                                          "Content-Encoding": "gzip"})
        if status != 200:
            raise RuntimeError(f"{sender} ingest answered {status}: {text[:300]}")
        return json.loads(text)

    @staticmethod
    def events(path):
        """trawld's info events, from tracing's text format:
        "<timestamp>  INFO target: message key=value ..."."""
        for line in path.read_text(errors="replace").splitlines():
            if "event_type=" not in line:
                continue
            fields = {k: v.strip('"') for k, v in LOGFMT_FIELD.findall(line)}
            fields["timestamp"] = line.split(" ", 1)[0]
            yield fields

    def calibrate(self):
        """Send one cycle of the fixture to learn how many of its events
        trawld accepts. The expected totals below are whole cycles times
        this number."""
        answer = self.post_cycles("calibration", 1)
        self.accepted_per_cycle = answer["accepted"]
        self.summary["fixture"]["acceptedPerCycle"] = answer["accepted"]
        self.summary["fixture"]["rejectedPerCycle"] = answer.get("rejected", 0)
        log(f"calibration: {answer['accepted']} of {len(self.fixture)} fixture events accepted")

    def vector_config(self, index):
        base = tomllib.loads(VECTOR_BASE.read_text())
        sink = copy.deepcopy(base["sinks"]["trawld"])
        self.shipped_sink = copy.deepcopy(base["sinks"]["trawld"])
        # Changed from the shipped sink: the input (the replay replaces the
        # shipped sources and transforms, whose output the fixture already
        # is) and the private CA that the shipped comment says to set.
        sink["inputs"] = ["replay"]
        sink["tls"]["ca_file"] = str(self.cert)
        data_dir = self.work / f"vector-{index + 1}"
        data_dir.mkdir()
        return {
            "data_dir": str(data_dir),
            "acknowledgements": base["acknowledgements"],
            "sources": {"replay": {"type": "stdin", "decoding": {"codec": "json"}}},
            "sinks": {"trawld": sink},
        }

    def start_vectors(self):
        self.vectors = []
        for i in range(self.args.senders):
            path = self.work / f"private/vector-{i + 1}.json"
            path.write_text(json.dumps(self.vector_config(i), indent=2))
            env = {"TRAWL_URL": f"https://127.0.0.1:{self.port}",
                   "TRAWL_INGEST_TOKEN": self.ingest_keys[i], "VECTOR_LOG": "info",
                   "VECTOR_COLOR": "never",
                   # As the shipped install sets it (base.toml step 3): Vector
                   # 0.57 resolves ${TRAWL_URL} and the token only with it.
                   "VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION": "true"}
            child = self.spawn([self.args.vector, "--config", str(path)], env,
                               self.work / f"vector-{i + 1}.log", stdin=subprocess.PIPE)
            self.vectors.append({"name": f"vector-{i + 1}", "child": child, "written": 0})
        self.summary["vectorSink"] = {
            "source": "config/vector/debian/base.toml [sinks.trawld], read at run time",
            "shipped": self.shipped_sink,
            "changed": {"inputs": ["replay"], "tls.ca_file": "trawld's generated certificate"},
            "senders": self.args.senders,
        }

    def write_cycles(self, sender, cycles):
        child = sender["child"]
        lines = []
        for _ in range(cycles):
            for event in self.fixture:
                lines.append(json.dumps(dict(event, load_run=self.run_id, load_sender=sender["name"],
                                             load_seq=sender["written"])))
                sender["written"] += 1
        child.stdin.write(("\n".join(lines) + "\n").encode())
        child.stdin.flush()

    def feed(self, sender, phases):
        """Write whole fixture cycles to one Vector's stdin."""
        try:
            for kind, amount, seconds in phases:
                if kind == "steady":
                    # amount = events per second, paced in 1 s ticks of whole cycles
                    per_tick = max(1, round(amount / len(self.fixture)))
                    start = time.monotonic()
                    for tick in range(int(seconds)):
                        if self.stop.is_set():
                            return
                        self.write_cycles(sender, per_tick)
                        delay = start + tick + 1 - time.monotonic()
                        if delay > 0:
                            time.sleep(delay)
                else:
                    # amount = cycles written as fast as Vector reads them
                    remaining = amount
                    while remaining and not self.stop.is_set():
                        step = min(remaining, 50)
                        self.write_cycles(sender, step)
                        remaining -= step
        except BrokenPipeError:
            sender["error"] = "Vector closed stdin"

    def scrape(self):
        interval = self.args.scrape_interval
        start = time.monotonic()
        tick = 0
        while not self.stop.is_set():
            at = time.monotonic()
            try:
                status, text = self.http("GET", "/metrics", timeout=30)
            except Exception as error:  # a scrape failure is recorded, not fatal
                status, text = 0, str(error)
            took = time.monotonic() - at
            row = {"t": round(at - self.t0, 3), "scrape_ms": round(took * 1000, 1), "status": status}
            if status == 200:
                series, totals = parse_metrics(text)
                for key, name, label in SAMPLED:
                    row[key] = pick(series, totals, name, label)
                row["parquet_files"] = totals.get("trawl_parquet_files_total")
                self.last_metrics = text
            self.samples.append(row)
            tick = max(tick + 1, int((time.monotonic() - start) / interval) + 1)
            delay = start + tick * interval - time.monotonic()
            if delay > 0:
                self.stop.wait(delay)

    def query_loop(self):
        # A search for one systemd line, one page of 50 rows. It matches
        # 1/67 of the corpus, which stays under max_result_rows (100000);
        # a search matching more is refused as result_too_large.
        page = (f'load_run="{self.run_id}" fixture_id="journal-pid1" {WINDOW}', 50)
        stats = (f'load_run="{self.run_id}" {WINDOW} | stats count() by load_sender', 1000)
        n = 0
        while not self.stop.is_set():
            text, limit = (page, stats)[n % 2]
            at = time.monotonic()
            try:
                status, _ = self.http("POST", "/api/v1/query", {"query": text, "limit": limit},
                                      token=self.reader)
            except Exception:
                status = 0
            self.queries.append({"t": round(at - self.t0, 3), "kind": ("page", "stats")[n % 2],
                                 "status": status, "ms": round((time.monotonic() - at) * 1000, 1)})
            n += 1
            self.stop.wait(self.args.query_think)

    def counts(self):
        """Per sender: stored events, and distinct load_seq values.

        Vector delivers at least once. A request that outlives the sink's
        30 s timeout is sent again, and trawld stores both copies, so
        delivery is complete when every written load_seq is stored."""
        status, text = self.http("POST", "/api/v1/query", token=self.reader, body={
            "query": f'load_run="{self.run_id}" {WINDOW} '
                     '| stats count() as stored, dc(load_seq) as distinct by load_sender',
            "limit": 1000})
        if status != 200:
            return None
        answer = json.loads(text)
        columns = [c["name"] for c in answer["columns"]]
        sender, stored, distinct = (columns.index(n) for n in ("load_sender", "stored", "distinct"))
        return {row[sender]: {"stored": int(row[stored]), "distinct": int(row[distinct])}
                for row in answer["rows"]}

    # -- the run -----------------------------------------------------------

    def run(self):
        a = self.args
        self.preflight()
        self.load_fixture()
        self.start_postgres()
        self.provision_keys()
        self.seed_history()
        self.start_trawld("trawld.log")
        self.t0 = time.monotonic()
        self.wall0 = datetime.now(timezone.utc)
        self.summary["measuredSessionStart"] = self.wall0.strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        threads = [threading.Thread(target=self.scrape, daemon=True),
                   threading.Thread(target=self.query_loop, daemon=True)]
        for t in threads:
            t.start()
        self.start_vectors()
        # A Vector first start: it drains the backlog its sources hold, as
        # fast as the sink allows, then follows new lines.
        phases = [("backlog", a.backlog_cycles, None),
                  ("steady", a.steady_rate, a.steady_seconds)]
        self.summary["workload"] = {
            "phases": [{"kind": k, "eventsPerSecondPerSender" if k == "steady" else "cyclesPerSender":
                        amt, **({"seconds": s} if s else {})} for k, amt, s in phases],
            "scrapeIntervalSeconds": a.scrape_interval,
            "queryThinkSeconds": a.query_think,
        }
        log(f"feeding {a.senders} Vector senders: backlog {a.backlog_cycles} cycles, "
            f"then {a.steady_rate}/s for {a.steady_seconds}s")
        feeders = [threading.Thread(target=self.feed, args=(s, phases), daemon=True)
                   for s in self.vectors]
        for f in feeders:
            f.start()
        for f in feeders:
            f.join()
        self.feed_done = round(time.monotonic() - self.t0, 3)
        if self.accepted_per_cycle != len(self.fixture):
            raise RuntimeError("trawld rejects fixture events; distinct load_seq cannot be the oracle")
        expected = {s["name"]: s["written"] for s in self.vectors}
        log(f"feeding done at t={self.feed_done}s; waiting for delivery of {expected}")
        deadline = time.monotonic() + a.drain_seconds

        def complete(counts):
            return counts is not None and all(
                counts.get(k, {}).get("distinct") == v for k, v in expected.items())

        counts = None
        while time.monotonic() < deadline:
            counts = self.counts()
            if complete(counts):
                break
            time.sleep(5)
        self.delivered_at = round(time.monotonic() - self.t0, 3)
        # One more interval so the last batches meet a normal compaction pass.
        time.sleep(12)
        self.stop.set()
        for t in threads:
            t.join(60)
        self.summary["delivery"] = {"expectedDistinct": expected, "counted": counts,
                                    "complete": complete(counts),
                                    "duplicates": counts and {
                                        k: counts[k]["stored"] - counts[k]["distinct"]
                                        for k in expected if k in counts},
                                    "written": {s["name"]: s["written"] for s in self.vectors},
                                    "feedDoneSeconds": self.feed_done,
                                    "deliveredSeconds": self.delivered_at}
        for s in self.vectors:
            if "error" in s:
                self.summary["delivery"].setdefault("feedErrors", {})[s["name"]] = s["error"]

    def collect(self):
        """Summaries only. Raw logs stay in --work."""
        self.out.mkdir(parents=True, exist_ok=True)
        columns = ["t", "scrape_ms", "status"] + [k for k, _, _ in SAMPLED] + ["parquet_files"]
        with open(self.out / "metrics-samples.csv", "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=columns, extrasaction="ignore")
            w.writeheader()
            for row in self.samples:
                w.writerow(row)
        with open(self.out / "queries.csv", "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=["t", "kind", "status", "ms"])
            w.writeheader()
            for row in self.queries:
                w.writerow(row)
        if getattr(self, "last_metrics", None):
            keep = re.compile(r"^(# (HELP|TYPE) )?trawl_(http_|hot_buffer_|ingest_events|"
                              r"compaction_|files_quarantined|parquet_files|wal_files|queries_total)")
            (self.out / "metrics-final.prom").write_text(
                "\n".join(l for l in self.last_metrics.splitlines() if keep.match(l)) + "\n")

        ok = [r for r in self.samples if r.get("status") == 200]
        gaps = [b["t"] - a["t"] for a, b in zip(ok, ok[1:])]
        peak = max((r["in_progress_regular"] for r in ok if r.get("in_progress_regular") is not None),
                   default=None)
        peak_row = next((r for r in ok if r.get("in_progress_regular") == peak), None)
        final = ok[-1] if ok else {}
        self.summary["metrics"] = {
            "samples": len(self.samples),
            "failedScrapes": len(self.samples) - len(ok),
            "intervalSeconds": {"target": self.args.scrape_interval,
                                "median": round(statistics.median(gaps), 3) if gaps else None,
                                "max": round(max(gaps), 3) if gaps else None},
            "allowance": {"regular": final.get("allowance_regular"),
                          "control": final.get("allowance_control")},
            "refusedTotalAtEnd": {"regular": final.get("refused_regular"),
                                  "control": final.get("refused_control")},
            "refusedTotalMaxSeen": {
                "regular": max((r.get("refused_regular") or 0 for r in ok), default=None),
                "control": max((r.get("refused_control") or 0 for r in ok), default=None)},
            "sampledPeakInProgress": {
                "regular": peak, "atSeconds": peak_row and peak_row["t"],
                "control": max((r.get("in_progress_control") or 0 for r in ok), default=None)},
            "inProgressRegularHistogram": {
                str(int(v)): sum(1 for r in ok if r.get("in_progress_regular") == v)
                for v in sorted({r.get("in_progress_regular") for r in ok
                                 if r.get("in_progress_regular") is not None})},
            "peakHotBufferEvents": max((r.get("hot_buffer_events") or 0 for r in ok), default=None),
            # 0 open, 1 pressure (compaction drains early), 2 refusing
            "hotBufferAdmissionStateSamples": {
                str(v): sum(1 for r in ok if (r.get("hot_buffer_admission_state") or 0) == v)
                for v in (0, 1, 2)},
        }
        if getattr(self, "last_metrics", None):
            series, totals = parse_metrics(self.last_metrics)
            self.summary["metrics"]["final"] = {
                f"{n}{{{l}}}" if l else n: v for (n, l), v in series.items()
                if n in ("trawl_ingest_events_total", "trawl_ingest_events_rejected_total",
                         "trawl_hot_buffer_admission_refusals_total",
                         "trawl_compaction_operation_failures_total",
                         "trawl_files_quarantined_total", "trawl_queries_total")}

        qs = self.queries
        self.summary["queries"] = {
            "count": len(qs),
            "byStatus": {str(s): sum(1 for q in qs if q["status"] == s) for s in sorted({q["status"] for q in qs})},
            "msP50": round(statistics.median([q["ms"] for q in qs]), 1) if qs else None,
            "msMax": max((q["ms"] for q in qs), default=None),
        }

        # Compaction and rollup proof: their completion events, by time.
        events = []
        wanted = {"compaction_complete", "rollup_complete", "compaction_pressure_pass",
                  "compaction_error", "rollup_error"}
        for fields in self.events(self.trawld_log):
            kind = fields.get("event_type")
            if kind in wanted:
                events.append({k: fields.get(k) for k in ("timestamp", "event_type", "compact_service",
                                                          "rows", "wal_files", "hourly_files",
                                                          "duration_ms", "error") if fields.get(k) is not None})
        with open(self.out / "compaction-rollup-events.ndjson", "w") as f:
            for e in events:
                f.write(json.dumps(e) + "\n")
        window_end = self.wall0.timestamp() + (self.delivered_at if hasattr(self, "delivered_at") else 0)

        def during(e):
            try:
                ts = datetime.fromisoformat(e["timestamp"].replace("Z", "+00:00")).timestamp()
            except (KeyError, ValueError):
                return False
            return self.wall0.timestamp() <= ts <= window_end

        by_kind = {}
        for e in events:
            key = e["event_type"]
            by_kind.setdefault(key, {"total": 0, "duringIngest": 0})
            by_kind[key]["total"] += 1
            by_kind[key]["duringIngest"] += during(e)
        self.summary["compactionAndRollup"] = by_kind
        if events:
            rollups = [e["timestamp"] for e in events if e["event_type"] == "rollup_complete"]
            if rollups:
                self.summary["compactionAndRollup"]["rollupWindow"] = [min(rollups), max(rollups)]

        # What the senders' accepted requests looked like, from trawld's
        # ingest_complete events (body_bytes is the decoded size).
        accepted = [f for f in self.events(self.trawld_log)
                    if f.get("event_type") == "ingest_complete" and f.get("user", "").startswith("ac23-vector-")]
        if accepted:
            body = sorted(int(f["body_bytes"]) for f in accepted)
            wire = sorted(int(f["wire_bytes"]) for f in accepted)
            took = sorted(int(f["duration_ms"]) for f in accepted)
            self.summary["ingestRequests"] = {
                "accepted": len(accepted),
                "events": sum(int(f["accepted"]) for f in accepted),
                "decodedBytes": {"p50": int(statistics.median(body)), "max": body[-1],
                                 "atLeast900KB": sum(b >= 900_000 for b in body)},
                "wireBytes": {"p50": int(statistics.median(wire)), "max": wire[-1]},
                "durationMs": {"p50": int(statistics.median(took)), "max": took[-1]},
            }
        self.summary["httpFailuresFromTrawld"] = {}
        for f in self.events(self.trawld_log):
            if f.get("event_type") == "http_failure":
                key = f'{f.get("route")} {f.get("status")} {f.get("cause_kind")}'
                self.summary["httpFailuresFromTrawld"][key] = self.summary["httpFailuresFromTrawld"].get(key, 0) + 1

        # What the senders saw: Vector's retry warnings, by the body code.
        vectors = {}
        for s in getattr(self, "vectors", []):
            text = (self.work / f"{s['name']}.log").read_text(errors="replace")
            retries = [l for l in text.splitlines() if "Retrying after" in l or "retry" in l.lower()]
            vectors[s["name"]] = {
                "retryLines": len(retries),
                "request_limit_reached": sum("request_limit_reached" in l for l in retries),
                "hot_buffer_full": sum("hot_buffer_full" in l for l in retries),
                "requestTimeouts": sum("Request timed out" in l for l in text.splitlines()),
                "errorLines": sum(" ERROR " in l for l in text.splitlines()),
            }
        self.summary["vectorLogs"] = vectors

    def cleanup(self):
        self.stop.set()
        for s in getattr(self, "vectors", []):
            try:
                s["child"].stdin.close()
            except (BrokenPipeError, OSError):
                pass
        cleaned = {"processes": True, "container": False}
        for child in reversed(self.children):
            self.terminate(child)
            cleaned["processes"] &= child.poll() is not None
        if subprocess.run(DOCKER + ["inspect", self.container], capture_output=True).returncode == 0:
            subprocess.run(DOCKER + ["rm", "--force", self.container], capture_output=True, timeout=60)
        cleaned["container"] = subprocess.run(DOCKER + ["inspect", self.container],
                                              capture_output=True).returncode != 0
        self.summary["cleanup"] = cleaned


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--work", required=True, help="private scratch directory (created; must not exist)")
    p.add_argument("--out", default=str(HERE / "output"), help="summary directory")
    p.add_argument("--vector", required=True, help="Vector 0.57.0 binary")
    p.add_argument("--senders", type=int, default=3)
    p.add_argument("--seed-cycles", type=int, default=30, help="fixture cycles per seed session")
    p.add_argument("--steady-rate", type=int, default=268, help="events/s per sender (whole fixture cycles)")
    p.add_argument("--steady-seconds", type=int, default=180)
    p.add_argument("--backlog-cycles", type=int, default=1500, help="fixture cycles per sender, unpaced")
    p.add_argument("--scrape-interval", type=float, default=0.25)
    p.add_argument("--query-think", type=float, default=1.0)
    p.add_argument("--drain-seconds", type=int, default=900)
    p.add_argument("--keep-work", action="store_true")
    run = Run(p.parse_args())
    def interrupt(*_):
        raise KeyboardInterrupt
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupt)
    code = 1
    try:
        run.run()
        code = 0
    except KeyboardInterrupt:
        run.summary["status"] = "interrupted"
        log("interrupted")
    except Exception as error:
        run.summary["status"] = "error"
        run.summary["error"] = run.redact(str(error))
        log(f"error: {run.summary['error']}")
    finally:
        run.cleanup()
        try:
            run.collect()
        except Exception as error:
            run.summary["collectError"] = run.redact(repr(error))
        m = run.summary.get("metrics", {})
        refused = m.get("refusedTotalMaxSeen", {})
        delivery = run.summary.get("delivery", {})
        if code == 0:
            passed = (refused.get("regular") == 0 and refused.get("control") == 0
                      and delivery.get("complete") and m.get("failedScrapes") == 0
                      and run.summary.get("cleanup", {}).get("processes")
                      and run.summary.get("cleanup", {}).get("container"))
            run.summary["status"] = "passed" if passed else "failed"
            code = 0 if passed else 1
        run.out.mkdir(parents=True, exist_ok=True)
        (run.out / "summary.json").write_text(run.redact(json.dumps(run.summary, indent=2)) + "\n")
        log(f"status {run.summary['status']}; summary in {run.out / 'summary.json'}")
        if not run.args.keep_work and run.work.exists() and run.work != HERE:
            shutil.rmtree(run.work, ignore_errors=True)
    sys.exit(code)


if __name__ == "__main__":
    main()
