#!/usr/bin/env python3
"""Run the Debian collector with Vector 0.57.0 and disposable loopback inputs.

Also run the Vector guide's sample capture recipe, as written, and prove that it
captures the events the `trawld` sink posts, and prove that `base.toml`'s
`/var/log` catch-all never reads `/var/log/private`.

Requires Python 3.11+, OpenSSL, bash, awk, coreutils, and VECTOR_BIN (or vector on
PATH). No host journal, application files, Docker socket, credentials, or running
collector are used.
"""

import collections
import gzip
import http.server
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import tomllib


ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "config/vector/debian"
FIXTURES = ROOT / "target/vector-collector-tests"
VECTOR = os.environ.get("VECTOR_BIN", "vector")
ENV = dict(os.environ, VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION="true",
           TRAWL_URL="http://127.0.0.1:1", TRAWL_INGEST_TOKEN="fixture-token",
           TRAWL_ENV="prod")
# Do not inherit another installation's configuration selection.
ENV.pop("VECTOR_CONFIG", None)
ENV.pop("VECTOR_CONFIG_DIR", None)
ENV.pop("VAR", None)
ENV.pop("TRAWL_SUPPRESS_HOMELAB_NOISE", None)
# Marker shape used by the sender proof recipes in the Vector guide.
MARKER = "trawl-check-00000000-0000-4000-8000-000000000198"
GUIDE = ROOT / "docs/src/content/docs/getting-started/vector-integration.md"
# The guide's capture recipe, and the host paths it reads, which the run below
# points at fixture copies.
CAPTURE_MARKER = "<!-- proof:capture-sample -->"
# The guide's opt-in variant: a shell variable set before the capture block.
CAPTURE_RECENT_MARKER = "<!-- proof:capture-recent -->"
LIVE_CONFIG_DIR = "/etc/vector/vector.d"
LIVE_ENV_FILE = "/etc/vector/trawl.env"
# The Vector release the guide pins, which the capture's version check names.
PINNED_PACKAGE = "vector=0.57.0-1"
# The most events one ingest preview reads (trawl_api::ingest_preview).
MAX_PREVIEW_EVENTS = 500
# The largest body the preview reads under the default `[server]
# max_request_body_bytes` (trawl_config::DEFAULT_MAX_REQUEST_BODY_BYTES).
MAX_PREVIEW_BODY_BYTES = 128 * 1024
# A zone that is never UTC, for the postgresql drop-in's line times.
POSTGRES_ZONE = "America/New_York"


def load(tcp):
    config = {"sources": {}, "transforms": {}, "sinks": {}}
    for path in sorted(CONFIG.glob("*.toml")):
        text = path.read_text()
        if tcp and path.name == "unifi-syslog.toml":
            # Exercise exactly the advertised uncomment operation.
            for line in ("[sources.unifi_syslog_tcp]", 'type = "syslog"',
                         'address = "0.0.0.0:1514"', 'mode = "tcp"'):
                assert "# " + line in text, line
                text = text.replace("# " + line, line)
        fragment = tomllib.loads(text)
        for key, value in fragment.items():
            if key in ("sources", "transforms", "sinks"):
                assert not config[key].keys() & value.keys()
                config[key].update(value)
            else:
                config[key] = value
    return config


def fixtures(suppress):
    events, expected = [], {}

    def add(source, name, service, severity="info", **fields):
        if source == "journald":
            fields.setdefault("PRIORITY", "6")
        event = dict(message=fields.pop("message", name), fixture_id=name, fixture_source=source,
                     host="fixture-host", **fields)
        events.append(event)
        if service is not None:
            expected[name] = (service, severity)

    add("journald", "journal-accepted", "sshd", _SYSTEMD_UNIT="sshd.service", PRIORITY="6")
    add("journald", "journal-warning", "sshd", "warn", SYSLOG_IDENTIFIER="sshd", PRIORITY="4")
    add("journald", "serial-console", None if suppress else "serial-getty@ttyS0",
        _SYSTEMD_UNIT="serial-getty@ttyS0.service")
    add("journald", "serial-getty restart", None if suppress else "init", SYSLOG_IDENTIFIER="init")
    for service in ("networkd-dispatcher", "NetworkManager", "systemd-networkd"):
        add("journald", f"veth churn {service}", None if suppress else service, SYSLOG_IDENTIFIER=service)
        add("journald", f"normal {service}", service, SYSLOG_IDENTIFIER=service)
    add("journald", "ufw", "ufw", "warn", message="[UFW BLOCK] SRC=192.0.2.1 DST=192.0.2.2 PROTO=TCP SPT=123 DPT=443")
    add("varlog", "varlog", "dpkg", file="/var/log/dpkg.log")
    add("varlog", "varlog-subdir", "apt", file="/var/log/apt/history.log")
    add("docker", "docker-compose", "web", label={"com.docker.compose.service": "web"})
    add("docker", "docker-image", "redis", image="registry.example/library/redis:7")
    add("docker", "docker-swarm", "stack-web",
        container_name="/stack_web.1." + "a" * 25 + "." + "b" * 25)
    for service in ("apache", "nginx"):
        for filename in ("access.log", "error.log", "other.log", "access-error.log"):
            add(service, f"{service}-{filename}", service,
                "error" if filename == "error.log" else "info",
                file=f"/var/log/{service}/{filename}")
        add(service, f"{service}-parsed", service, "warn",
            file=f"/var/log/{service}/access.log",
            message='192.0.2.1 - alice [15/Jan/2024:10:30:45 +0000] "GET /test HTTP/1.1" 404 42 "-" "fixture"')
    for service in ("fail2ban", "mysql", "postgresql", "redis"):
        add(service, service, service)
    # run() sets TZ to a zone that is never UTC: a UTC line keeps its own
    # instant, and a line in another zone is read in Vector's local zone.
    add("postgresql", "postgresql-utc", "postgresql",
        message="2026-09-28 10:30:45.123 UTC [4242] LOG:  checkpoint starting: time")
    add("postgresql", "postgresql-local", "postgresql", "error",
        message="2026-09-28 10:30:45.123 EDT [4242] alice@app ERROR:  relation does not exist")
    # Sender proof recipes: each marker event must arrive under the identity
    # the guide tells the operator to query.
    add("journald", "proof-journald", MARKER, message=MARKER, _SYSTEMD_UNIT=f"{MARKER}.service")
    add("nginx", "proof-nginx", "nginx", "warn", file="/var/log/nginx/access.log",
        message=f'10.198.1.2 - - [28/Sep/2026:10:30:45 +0000] "GET /{MARKER} HTTP/1.1" 404 153 "-" "curl/8.14.1"')
    add("docker", "proof-docker", MARKER, message=MARKER, container_name="/" + MARKER,
        image="alpine", stream="stdout")
    add("journald", "proof-ufw", "ufw", "warn", SYSLOG_IDENTIFIER="kernel", PRIORITY="4",
        _TRANSPORT="kernel",
        message="[UFW BLOCK] IN=enp1s0 OUT= MAC=52:54:00:12:34:56:52:54:00:65:43:21:08:00 "
                "SRC=10.198.1.2 DST=10.198.1.1 LEN=60 TOS=0x00 PREC=0x00 TTL=64 ID=31337 DF "
                "PROTO=TCP SPT=41234 DPT=4919 WINDOW=64240 RES=0x00 SYN URGP=0")
    return events, expected


def certificates(directory):
    """Create a private fixture CA and a server certificate for loopback only."""
    directory = Path(directory)

    def openssl(*args):
        subprocess.run(["openssl", *args], cwd=directory, check=True,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=Trawl fixture CA", "-keyout", "ca.key", "-out", "ca.pem",
            "-addext", "basicConstraints=critical,CA:TRUE")
    openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
            "-keyout", "server.key", "-out", "server.csr")
    (directory / "server.ext").write_text(
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n")
    openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
            "-CAcreateserial", "-days", "1", "-extfile", "server.ext", "-out", "server.pem")
    return directory / "ca.pem", directory / "server.pem", directory / "server.key"


def run(tcp, suppress, tls="http"):
    config = load(tcp)
    # A final output must never also feed another transform.
    for name, transform in config["transforms"].items():
        assert not any(i.startswith("trawl_") for i in transform["inputs"]), name
    events, expected = fixtures(suppress)
    env = dict(ENV, TZ=POSTGRES_ZONE)
    if suppress:
        env["TRAWL_SUPPRESS_HOMELAB_NOISE"] = "true"
    assert config["sinks"]["trawld"]["tls"]["verify_certificate"] is True
    assert config["sinks"]["trawld"]["tls"]["verify_hostname"] is True
    received = []
    failures = []

    class Capture(http.server.BaseHTTPRequestHandler):
        def do_HEAD(self):
            self.send_response(200)
            self.end_headers()

        def do_POST(self):
            try:
                assert self.path == "/api/v1/ingest"
                assert self.headers["Authorization"] == "Bearer fixture-token"
                assert self.headers["Content-Encoding"] == "gzip"
                body = gzip.decompress(self.rfile.read(int(self.headers["Content-Length"])))
                batch = json.loads(body)
                assert isinstance(batch, list), batch
                received.extend(batch)
            except Exception as error:
                failures.append(repr(error))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_args):
            pass

    with tempfile.TemporaryDirectory(prefix="trawl-vector-", dir=FIXTURES) as directory:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Capture)
        if tls != "http":
            ca, cert, key = certificates(directory)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        process = None
        reservations = []
        try:
            config["data_dir"] = directory
            sink = config["sinks"]["trawld"]
            scheme = "http" if tls == "http" else "https"
            sink["uri"] = f"{scheme}://127.0.0.1:{server.server_port}/api/v1/ingest"
            if tls != "http":
                sink["healthcheck"] = {"enabled": True, "uri": sink["uri"]}
                if tls == "trusted":
                    sink["tls"]["ca_file"] = str(ca)
            sink["batch"]["timeout_secs"] = 0.1
            ports = {}
            for name, source in list(config["sources"].items()):
                if source["type"] == "syslog":
                    kind = socket.SOCK_STREAM if source["mode"] == "tcp" else socket.SOCK_DGRAM
                    reservation = socket.socket(socket.AF_INET, kind)
                    reservation.bind(("127.0.0.1", 0))
                    ports[source["mode"]] = reservation.getsockname()[1]
                    source["address"] = f"127.0.0.1:{reservation.getsockname()[1]}"
                    reservations.append(reservation)
                else:
                    del config["sources"][name]
                    config["transforms"][name] = dict(type="filter", inputs=["fixture"],
                        condition=f'.fixture_source == "{name}"')
            config["sources"]["fixture"] = dict(type="stdin", decoding={"codec": "json"})
            path = Path(directory) / "vector.json"
            path.write_text(json.dumps(config))
            # Ports are selected dynamically and scoped to loopback. Bind failure
            # is a test failure, never permission to contact another listener.
            for reservation in reservations:
                reservation.close()
            with (Path(directory) / "vector.log").open("w+") as log:
                process = subprocess.Popen([VECTOR, "--require-healthy", "true", "--config", str(path)], env=env,
                                           stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
                if tls == "untrusted":
                    process.communicate(timeout=20)
                    log.seek(0)
                    output = log.read()
                    assert process.returncode != 0, output
                    assert "certificate verify failed" in output, output
                    assert not received and not failures, (received, failures)
                    print("PASS untrusted HTTPS: certificate verification refused the server; zero events")
                    return
                # Vector's startup message can precede the source tasks' binds.
                # Wait for every syslog listener before sending any fixture;
                # a UDP send to an unbound port succeeds but loses the event.
                listeners = [
                    re.compile(rf'component_id={re.escape(name)} .*Listening\. '
                               rf'addr={re.escape(source["address"])}(?:\s|$)')
                    for name, source in config["sources"].items()
                    if source["type"] == "syslog"
                ]
                deadline = time.monotonic() + 20
                while True:
                    log.seek(0)
                    output = log.read()
                    assert process.poll() is None, output
                    if "Vector has started" in output and all(p.search(output) for p in listeners):
                        break
                    assert time.monotonic() < deadline, output
                    time.sleep(0.05)
                process.stdin.write("".join(json.dumps(e) + "\n" for e in events))
                process.stdin.flush()
                for mode, port in ports.items():
                    name = "unifi-" + mode
                    expected[name] = ("unifi-u6-lite", "info")
                    message = f"<14>1 2026-09-13T00:00:00Z ap-fixture f492bfa1554cU6-Lite-6.7.4915634 - - - {name}\n"
                    kind = socket.SOCK_STREAM if mode == "tcp" else socket.SOCK_DGRAM
                    with socket.socket(socket.AF_INET, kind) as sender:
                        sender.settimeout(3)
                        sender.connect(("127.0.0.1", port))
                        sender.sendall(message.encode())
                deadline = time.monotonic() + 15
                while len(received) < len(expected) and time.monotonic() < deadline:
                    assert process.poll() is None
                    time.sleep(0.05)
                process.send_signal(signal.SIGTERM)
                process.communicate(timeout=15)
                log.seek(0)
                output = log.read()
                assert process.returncode == 0, output
                assert not failures, failures
                counts = collections.Counter(e.get("fixture_id", e.get("message")) for e in received)
                assert counts == collections.Counter({name: 1 for name in expected}), (counts, expected, output)
                for event in received:
                    name = event.get("fixture_id", event["message"])
                    assert (event["service"], event["severity_text"]) == expected[name], event
                    assert event["env"] == "prod", event
                    if name.endswith("-parsed"):
                        assert event["status"] == 404 and event["user_name"] == "alice", event
                        assert (event["method"], event["uri"]) == ("GET", "/test"), event
                    if name == "ufw":
                        assert event["protocol"] == "tcp" and event["dst_port"] == 443, event
                    if name == "proof-journald":
                        assert event["message"] == MARKER, event
                        assert event["host"] == "fixture-host", event
                    if name == "proof-nginx":
                        assert (event["method"], event["uri"]) == ("GET", "/" + MARKER), event
                        assert MARKER in event["message"], event
                    if name == "proof-docker":
                        assert (event["host"], event["message"]) == (MARKER, MARKER), event
                        assert event["container_name"] == MARKER, event
                    if name == "proof-ufw":
                        assert (event["src_ip"], event["dst_ip"]) == ("10.198.1.2", "10.198.1.1"), event
                        assert event["dst_port"] == 4919 and type(event["dst_port"]) is int, event
                        assert event["src_port"] == 41234 and event["protocol"] == "tcp", event
                    if name == "docker-swarm":
                        assert event["host"] == "stack", event
                    if name.startswith("unifi-"):
                        assert event["host"] == "ap-fixture", event
                        assert event["syslog_source_ip"] == "127.0.0.1", event
                    if name == "postgresql-utc":
                        assert event["_time"] == "2026-09-28T10:30:45.123Z", event
                        assert event["message"] == "checkpoint starting: time", event
                    if name == "postgresql-local":
                        # EDT is UTC-4 in POSTGRES_ZONE on that date.
                        assert event["_time"] == "2026-09-28T14:30:45.123Z", event
                        assert event["user_name"] == "alice", event
                    if name == "postgresql":
                        assert "_time" not in event, event
                print(f"PASS {tls} {'UDP + TCP' if tcp else 'UDP only'}: {len(events)} synthetic inputs, "
                      f"{len(ports)} syslog inputs, {len(received)} HTTP events, zero duplicates; "
                      f"{5 if suppress else 0} journal events filtered")
        finally:
            if process and process.poll() is None:
                process.kill()
                process.communicate()
            for reservation in reservations:
                reservation.close()
            server.shutdown()
            thread.join()
            server.server_close()


def development_page(raw):
    """The page as a development docs build renders its release pins.

    docs/scripts/release-pins.mjs owns the placeholder grammar.
    """
    page = raw.replace(" --version {{release.version}}", "").replace("{{release.tag}}", "main")
    assert "{{release." not in page, "a release placeholder the docs plugin rejects"
    return page


def capture_block(marker=CAPTURE_MARKER):
    """A marked block exactly as the guide prints it, list indent removed.

    By default, the capture recipe.
    """
    lines = development_page(GUIDE.read_text()).splitlines()
    starts = [i for i, line in enumerate(lines) if line.strip() == marker]
    assert len(starts) == 1, starts
    marker = lines[starts[0]]
    indent = marker[:len(marker) - len(marker.lstrip())]
    assert lines[starts[0] + 1] == indent + "```bash", lines[starts[0] + 1]
    body = []
    for line in lines[starts[0] + 2:]:
        if line == indent + "```":
            break
        assert not line or line.startswith(indent), line
        body.append(line[len(indent):])
    else:
        raise AssertionError("the capture block has no closing fence")
    return "\n".join(body) + "\n"


# Equivalent TOML spellings of the `trawld` sink headers that the guide's awk
# filter does not match. The capture must refuse to run with either.
SINK_SPELLINGS = {
    "indented": lambda line: "  " + line,
    "quoted": lambda line: line.replace("[sinks.trawld", '[sinks."trawld"', 1),
}


def fixture_collector(directory, live_data, receiver_url, spelling=None):
    """Copy the Debian collector configuration as a host would install it.

    Each file keeps its text, sink tables included. Only the source tables are
    replaced, by filters that take the stdin fixture stream, so Vector ends at
    the end of input. `data_dir` names the fixture's service directory.
    `spelling` rewrites every `[sinks.trawld...]` header in `base.toml` with
    one of SINK_SPELLINGS, keeping the parsed configuration the same.
    """
    live = directory / "vector.d"
    live.mkdir()
    header = re.compile(r"^\[([^\]]+)\]\s*$")
    for path in sorted(CONFIG.glob("*.toml")):
        text = path.read_text()
        sources = set(tomllib.loads(text).get("sources", {}))
        kept, removed, skip = [], set(), False
        for line in text.splitlines():
            match = header.match(line)
            if match:
                skip = match[1].startswith("sources.")
                if skip:
                    removed.add(match[1].split(".")[1])
            if not skip:
                kept.append(line)
        assert removed == sources, (path.name, removed, sources)
        text = "\n".join(kept) + "\n"
        if path.name == "base.toml":
            shipped = 'data_dir = "/var/lib/vector"\n'
            assert text.count(shipped) == 1, "base.toml no longer sets the shipped data_dir"
            text = text.replace(shipped, f"data_dir = {json.dumps(str(live_data))}\n")
            text += '\n[sources.fixture]\ntype = "stdin"\ndecoding.codec = "json"\n'
            if spelling:
                respelled = "\n".join(
                    SINK_SPELLINGS[spelling](line) if line.startswith("[sinks.trawld") else line
                    for line in text.splitlines()) + "\n"
                assert respelled != text and tomllib.loads(respelled) == tomllib.loads(text), spelling
                text = respelled
        for name in sorted(removed):
            text += (f"\n[transforms.{name}]\ntype = \"filter\"\ninputs = [\"fixture\"]\n"
                     f"condition = {json.dumps(f'.fixture_source == {json.dumps(name)}')}\n")
        (live / path.name).write_text(text)
    env_file = directory / "trawl.env"
    env_file.write_text(
        f"VECTOR_CONFIG_DIR={live}\n"
        "VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true\n"
        f"TRAWL_URL={receiver_url}\n"
        # Not the shipped default, so a capture that misses the service's
        # environment sends a different env.
        "TRAWL_ENV=lab\n"
        "TRAWL_INGEST_TOKEN=fixture-token\n")
    return live, env_file


def clean_env(directory, crash=False, full=False, noconfig=False):
    """An environment without inherited Vector or Trawl settings.

    With `crash`, a `vector` run (not `vector config` or `vector vrl`) is
    killed with SIGKILL after Vector has written every event, as the kernel's
    OOM killer would end it. With `full`, `head` runs under a 1 KiB file size
    limit, so writing the capture file fails partway, as on a full disk. With
    `noconfig`, `vector config` fails as it does on Vector 0.58 and later,
    which removed it, and every other command runs the real Vector.
    """
    shims = directory / "bin"
    shims.mkdir(exist_ok=True)
    vector = shutil.which(VECTOR)
    assert vector, VECTOR
    (shims / "vector").unlink(missing_ok=True)
    if crash:
        (shims / "vector").write_text(
            "#!/bin/sh\n"
            f"{shlex.quote(vector)} \"$@\"\n"
            'if [ "$1" = --config-dir ]; then kill -KILL $$; fi\n')
        (shims / "vector").chmod(0o755)
    elif noconfig:
        (shims / "vector").write_text(
            "#!/bin/sh\n"
            'if [ "$1" = config ]; then\n'
            "  echo \"error: unrecognized subcommand 'config'\" >&2\n"
            "  exit 2\n"
            "fi\n"
            f"exec {shlex.quote(vector)} \"$@\"\n")
        (shims / "vector").chmod(0o755)
    else:
        (shims / "vector").symlink_to(vector)
    (shims / "head").unlink(missing_ok=True)
    if full:
        head = shutil.which("head")
        assert head
        (shims / "head").write_text(f"#!/bin/sh\nulimit -c 0\nulimit -f 2\nexec {shlex.quote(head)} \"$@\"\n")
        (shims / "head").chmod(0o755)
    return {"PATH": f"{shims}:{os.environ['PATH']}", "HOME": str(directory),
            "TMPDIR": str(directory), "LC_ALL": "C.UTF-8"}


def run_capture(directory, live, env_file, inputs, crash=False, full=False, noconfig=False,
                prelude="", sudo='sudo() { "$@"; }'):
    """Run the guide's capture block against the fixture collector.

    The block's host paths point at the fixture copies, and `sudo` runs its
    command as this user. Nothing else in the block changes, and no shell
    option is set around it. `prelude` runs first in the same shell, as an
    operator runs a variant's block before the capture block. Vector reads the
    fixture stream on stdin, which it inherits through the block. Returns the
    finished process, the block's private directory, and the operator's
    working directory.
    """
    block = capture_block()
    for path in (LIVE_CONFIG_DIR, LIVE_ENV_FILE):
        assert path in block, f"the capture block no longer reads {path}"
    script = (sudo + "\n" + prelude
              + block.replace(LIVE_CONFIG_DIR, str(live)).replace(LIVE_ENV_FILE, str(env_file)))
    work = directory / "operator"
    work.mkdir()
    with inputs.open("rb") as stdin:
        result = subprocess.run(["bash", "-c", script], cwd=work,
                                env=clean_env(directory, crash, full, noconfig),
                                stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=90)
    assert result.stdout == b"", "the capture leaked events to the terminal"
    [capture] = directory.glob("tmp.*")
    # The captured lines are root-readable logs: they stay in the operator's
    # private directory, and nothing lands in the working directory.
    assert capture.stat().st_mode & 0o777 == 0o700, oct(capture.stat().st_mode)
    assert capture.stat().st_uid == os.getuid()
    assert not any(work.iterdir()), list(work.iterdir())
    return result, capture, work


def capture_events(directory, live, env_file, inputs):
    """Run the capture block, assert it ran as the guide says, return its file."""
    result, capture, _work = run_capture(directory, live, env_file, inputs)
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    # The configuration the capture ran: the service's files less the sink
    # and the syslog drop-in, with a console sink and its own data_dir.
    files = {path.name: tomllib.loads(path.read_text())
             for path in (capture / "config").glob("*.toml")}
    assert set(files) == {path.name for path in live.glob("*.toml")} - {"unifi-syslog.toml"}, files
    assert [f["data_dir"] for f in files.values() if "data_dir" in f] == [str(capture / "data")]
    sinks = {name: sink for f in files.values() for name, sink in f.get("sinks", {}).items()}
    assert sinks == {"capture": {"type": "console", "inputs": ["trawl_*"], "target": "stdout",
                                 "encoding": {"codec": "json"}}}, sinks
    output = capture / "capture.ndjson"
    assert output.parent == capture and output.is_file(), output
    print(f"PASS capture privacy: capture.ndjson in a {oct(capture.stat().st_mode & 0o777)} "
          "directory owned by the operator; nothing written to the working directory")
    return output.read_bytes(), result.stderr.decode(errors="replace")


def preview_input(raw):
    """Assert the capture is a body the preview reads, and return its events."""
    assert raw[:2] != b"\x1f\x8b", "the capture is gzip"
    text = raw.decode("utf-8")
    assert text.endswith("\n"), "the capture does not end with a newline"
    lines = text[:-1].split("\n")
    assert 0 < len(lines) <= MAX_PREVIEW_EVENTS, len(lines)
    assert len(raw) <= MAX_PREVIEW_BODY_BYTES, len(raw)
    events = [json.loads(line) for line in lines]
    assert all(isinstance(event, dict) for event in events), "a capture line is not an object"
    return events


def canonical(event):
    return json.dumps(event, sort_keys=True)


def capture_recipe():
    events, expected = fixtures(False)
    # A journal entry carries its own time. Give every fixture one, so both
    # runs post the same `timestamp` instead of the moment Vector read it.
    for event in events:
        event["timestamp"] = "2026-09-28T10:30:45.123456Z"
    requests = []

    class Receiver(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            requests.append((self.path, self.headers.get("Content-Encoding"), body))
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Receiver)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            inputs.write_text("".join(json.dumps(e) + "\n" for e in events))

            raw, log = capture_events(directory, live, env_file, inputs)
            assert "ERROR" not in log, log
            # Without CAPTURE_SINCE, the journald source reads the whole boot.
            [capture] = directory.glob("tmp.*")
            assert "extra_args" not in (capture / "config/base.toml").read_text()
            assert not requests, "the capture posted to trawld"
            assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
            captured = preview_input(raw)

            # The same configuration and inputs, as the service runs them.
            env = clean_env(directory)
            for line in env_file.read_text().splitlines():
                key, value = line.split("=", 1)
                env[key] = value
            with inputs.open("rb") as stdin:
                service = subprocess.run([VECTOR, "--config-dir", str(live)], env=env, stdin=stdin,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
            assert service.returncode == 0, service.stderr.decode(errors="replace")
            posted = []
            for path, encoding, body in requests:
                assert (path, encoding) == ("/api/v1/ingest", "gzip"), (path, encoding)
                batch = json.loads(gzip.decompress(body))
                assert isinstance(batch, list), batch
                posted.extend(batch)
            assert len(posted) == len(expected), (len(posted), len(expected))
            assert all(event["env"] == "lab" for event in posted), posted
            assert (collections.Counter(map(canonical, captured))
                    == collections.Counter(map(canonical, posted))), (captured, posted)
            print(f"PASS capture recipe: {len(captured)} captured lines equal the "
                  f"{len(posted)} events the trawld sink posted; nothing posted or "
                  "checkpointed by the capture")

        # A source that outruns the cut: the capture stops at the event limit.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            many = [dict(message=f"line {i}", fixture_id=f"bulk-{i}", fixture_source="journald",
                         host="fixture-host", PRIORITY="6", SYSLOG_IDENTIFIER="bulk")
                    for i in range(MAX_PREVIEW_EVENTS + 100)]
            inputs.write_text("".join(json.dumps(e) + "\n" for e in many))
            raw, _log = capture_events(directory, live, env_file, inputs)
            assert not requests, "the capture posted to trawld"
            assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
            captured = preview_input(raw)
            ids = [event["fixture_id"] for event in captured]
            assert len(ids) == MAX_PREVIEW_EVENTS == len(set(ids)), len(ids)
            assert set(ids) <= {event["fixture_id"] for event in many}
            print(f"PASS capture recipe cut: {len(many)} inputs, {len(captured)} captured lines, "
                  f"{len(raw)} bytes")

        # Lines long enough that the byte limit, not the event limit, ends the
        # capture. Each message is mostly two-byte characters, so a cap that
        # counted characters instead of bytes would pass the limit.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            long = [dict(message=f"long {i} " + "\u00fc" * 500, fixture_id=f"long-{i}",
                         fixture_source="journald", host="fixture-host", PRIORITY="6",
                         SYSLOG_IDENTIFIER="long")
                    for i in range(MAX_PREVIEW_EVENTS + 100)]
            inputs.write_text("".join(json.dumps(e) + "\n" for e in long))
            raw, log = capture_events(directory, live, env_file, inputs)
            assert "capture:" not in log, log
            assert not requests, "the capture posted to trawld"
            assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
            captured = preview_input(raw)
            ids = [event["fixture_id"] for event in captured]
            assert len(ids) < MAX_PREVIEW_EVENTS and len(set(ids)) == len(ids), len(ids)
            # The cut is the byte limit: one more line would not have fit.
            longest = max(len(line) + 1 for line in raw.split(b"\n"))
            assert len(raw) > MAX_PREVIEW_BODY_BYTES - longest, (len(raw), longest)
            print(f"PASS capture recipe byte limit: {len(long)} inputs of about {longest} bytes, "
                  f"{len(captured)} captured lines, {len(raw)} bytes")

        # The first event alone is larger than the limit: the capture is empty,
        # and the block says so instead of leaving an empty sample. Every
        # event takes the journald path, so none overtakes the first, and the
        # events after it are more than a pipe buffer holds, so Vector is
        # still writing when the cap closes the pipe.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            # The stdin source drops a line over 100 KiB by default.
            base = live / "base.toml"
            fixture = '[sources.fixture]\ntype = "stdin"\n'
            assert base.read_text().count(fixture) == 1
            base.write_text(base.read_text().replace(fixture, fixture + (
                'framing.method = "newline_delimited"\n'
                "framing.newline_delimited.max_length = 1048576\n")))
            inputs = directory / "fixtures.ndjson"
            first = [dict(message=("x" * MAX_PREVIEW_BODY_BYTES if i == 0 else f"small {i}"),
                          fixture_id=f"first-{i}", fixture_source="journald",
                          host="fixture-host", PRIORITY="6", SYSLOG_IDENTIFIER="first")
                     for i in range(3000)]
            inputs.write_text("".join(json.dumps(e) + "\n" for e in first))
            result, capture, _work = run_capture(directory, live, env_file, inputs)
            log = result.stderr.decode(errors="replace")
            assert result.returncode != 0, log
            assert "capture: the capture is empty" in log, log
            # The cap, not a dropped input, emptied it: Vector wrote the event
            # and found the pipe closed when it wrote the next one.
            assert "Broken pipe" in log, log
            assert (capture / "capture.ndjson").read_bytes() == b""
            assert not requests, "the capture posted to trawld"
            print(f"PASS capture refuses an empty sample: exit {result.returncode} when the first "
                  f"event alone is over {MAX_PREVIEW_BODY_BYTES} bytes")

        # Vector 0.58 and later have no `vector config`: the block names the
        # pinned release and starts nothing.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            inputs.write_text("".join(json.dumps(e) + "\n" for e in events))
            result, capture, _work = run_capture(directory, live, env_file, inputs, noconfig=True)
            log = result.stderr.decode(errors="replace")
            time.sleep(1)
            assert result.returncode != 0, log
            assert PINNED_PACKAGE in log and "Vector did not start" in log, log
            assert "unrecognized subcommand" not in log, "the version check let `vector config` run"
            assert (capture / "capture.ndjson").read_bytes() == b""
            assert not any((capture / "data").iterdir()), "Vector ran"
            assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
            assert not requests, "the capture posted to trawld"
            print(f"PASS capture refuses a Vector without vector config: exit {result.returncode}, "
                  f"names {PINNED_PACKAGE}, {len(requests)} requests, empty capture")

        # The recent-window variant, as the guide prints it, on the shipped
        # files with their sources: `sudo` runs nothing, so the block only
        # prepares the copy, and no host journal is read.
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live = directory / "vector.d"
            live.mkdir()
            for path in CONFIG.glob("*.toml"):
                shutil.copy(path, live)
            env_file = directory / "trawl.env"
            env_file.write_text("TRAWL_ENV=lab\n")
            inputs = directory / "fixtures.ndjson"
            inputs.write_text("")
            variant = capture_block(CAPTURE_RECENT_MARKER)
            result, capture, _work = run_capture(directory, live, env_file, inputs,
                                                 prelude=variant, sudo="sudo() { :; }")
            copy = capture / "config"
            journald = tomllib.loads((copy / "base.toml").read_text())["sources"]["journald"]
            assert journald["extra_args"] == ["--since=-15min"], journald
            assert journald["current_boot_only"] is True, journald
            env = dict(ENV, CAPTURE=str(capture))
            subprocess.run([VECTOR, "validate", "--no-environment", "--config-dir", str(copy)],
                           env=env, check=True, stdout=subprocess.DEVNULL)
            # The block's own sink check accepts the copy.
            resolved = subprocess.run([VECTOR, "config", "--config-dir", str(copy)], env=env,
                                      check=True, stdout=subprocess.PIPE, text=True).stdout
            checked = subprocess.run([VECTOR, "vrl", "--input", "/dev/stdin", "--program",
                                      str(capture / "check.vrl")], env=env, input=resolved,
                                     check=True, stdout=subprocess.PIPE, text=True).stdout
            assert checked.strip() == "true", checked
            print(f"PASS capture recent window: {variant.strip()} adds "
                  f"extra_args = {journald['extra_args']} to the copy's journald source; "
                  "the copy validates and passes the sink check")

        # A sink the awk filter misses: Vector's own reading of the copy
        # refuses it before Vector starts, so nothing is posted or captured.
        for spelling in SINK_SPELLINGS:
            requests.clear()
            with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
                directory = Path(name)
                live_data = directory / "var-lib-vector"
                live_data.mkdir()
                live, env_file = fixture_collector(
                    directory, live_data, f"http://127.0.0.1:{server.server_port}", spelling)
                inputs = directory / "fixtures.ndjson"
                inputs.write_text("".join(json.dumps(e) + "\n" for e in events))
                result, capture, _work = run_capture(directory, live, env_file, inputs)
                log = result.stderr.decode(errors="replace")
                # Give a sink that did start the time to flush a batch.
                time.sleep(1)
                assert not requests, f"the capture posted to trawld ({spelling} header)"
                assert result.returncode != 0, log
                assert "Vector did not start" in log, log
                # The copy still holds the sink, so the check, not the filter,
                # refused it.
                assert "trawld" in tomllib.loads(
                    (capture / "config/base.toml").read_text())["sinks"], spelling
                assert (capture / "capture.ndjson").read_bytes() == b""
                assert not any((capture / "data").iterdir()), "Vector ran"
                assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
                print(f"PASS capture refuses {spelling} sink header: exit {result.returncode}, "
                      f"{len(requests)} requests, empty capture")

        # Vector dies after it has written events: the capture holds lines,
        # but the block fails instead of passing them off as a sample.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            inputs.write_text("".join(json.dumps(e) + "\n" for e in events))
            result, capture, _work = run_capture(directory, live, env_file, inputs, crash=True)
            log = result.stderr.decode(errors="replace")
            written = (capture / "capture.ndjson").read_bytes()
            lines = written.count(b"\n")
            assert lines == len(expected), written
            assert result.returncode != 0, log
            assert "Vector stopped with status 137" in log, log
            assert not requests, "the capture posted to trawld"
            print(f"PASS capture fails when Vector dies: exit {result.returncode} after "
                  f"{lines} captured lines")

        # Writing the capture file fails: Vector ends cleanly when the pipe
        # closes, but the block fails instead of passing off a cut file.
        requests.clear()
        with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
            directory = Path(name)
            live_data = directory / "var-lib-vector"
            live_data.mkdir()
            live, env_file = fixture_collector(
                directory, live_data, f"http://127.0.0.1:{server.server_port}")
            inputs = directory / "fixtures.ndjson"
            inputs.write_text("".join(json.dumps(e) + "\n" for e in events))
            result, capture, _work = run_capture(directory, live, env_file, inputs, full=True)
            log = result.stderr.decode(errors="replace")
            written = (capture / "capture.ndjson").read_bytes()
            assert 0 < len(written) <= 1024, len(written)
            assert written.count(b"\n") < len(expected), written
            assert result.returncode != 0, log
            assert "writing the capture stopped with status" in log, log
            assert not requests, "the capture posted to trawld"
            print(f"PASS capture fails when writing it fails: exit {result.returncode} after "
                  f"{len(written)} bytes written")

        # A drop-in the operator cannot read: the copy would differ from the
        # service, so the block stops before Vector starts.
        if os.geteuid() == 0:
            print("SKIP capture refuses an unreadable drop-in: root reads a mode 000 file")
        else:
            requests.clear()
            with tempfile.TemporaryDirectory(prefix="trawl-capture-", dir=FIXTURES) as name:
                directory = Path(name)
                live_data = directory / "var-lib-vector"
                live_data.mkdir()
                live, env_file = fixture_collector(
                    directory, live_data, f"http://127.0.0.1:{server.server_port}")
                (live / "nginx.toml").chmod(0)
                inputs = directory / "fixtures.ndjson"
                inputs.write_text("".join(json.dumps(e) + "\n" for e in events))
                result, capture, _work = run_capture(directory, live, env_file, inputs)
                log = result.stderr.decode(errors="replace")
                time.sleep(1)
                assert result.returncode != 0, log
                assert "nginx.toml" in log, log
                # The `sudo` command owns the capture file; it never ran.
                assert not (capture / "capture.ndjson").exists(), "Vector started"
                assert not any((capture / "data").iterdir()), "Vector ran"
                assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
                assert not requests, "the capture posted to trawld"
                print(f"PASS capture refuses an unreadable drop-in: exit {result.returncode}, "
                      f"{len(requests)} requests, no capture file")
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def varlog_glob():
    """base.toml's catch-all never reads /var/log/private.

    The include and exclude patterns run on 0.57 as shipped, with /var/log
    moved to a fixture root whose `private` directory no one can read, so
    Vector's walk would fail there with "Failed to glob path". The shipped
    patterns must collect every other file, and the old single `**` include
    shows the fixture reproduces the failure.
    """
    if os.geteuid() == 0:
        print("SKIP varlog catch-all skips /var/log/private: root reads a mode 000 directory")
        return
    varlog = tomllib.loads((CONFIG / "base.toml").read_text())["sources"]["varlog"]
    assert "/var/log/private/**" in varlog["exclude"], varlog["exclude"]
    with tempfile.TemporaryDirectory(prefix="trawl-varlog-", dir=FIXTURES) as name:
        root = Path(name) / "log"
        wanted = ["dpkg.log", "private.log", "apt/history.log", "apt/private/x.log",
                  "unattended-upgrades/unattended-upgrades-dpkg.log", "p/x.log", "prix/x.log",
                  "privat/x.log", "privates/x.log", "a/b/c.log"]
        for relative in wanted + ["private/secret.log", "nginx/access.log", "apt/history.txt"]:
            (root / relative).parent.mkdir(parents=True, exist_ok=True)
            (root / relative).write_text(f"fixture line for {relative}\n")
        (root / "private").chmod(0)

        def collect(include):
            data = Path(tempfile.mkdtemp(prefix="data-", dir=name))
            moved = lambda patterns: [p.replace("/var/log/", f"{root}/", 1) for p in patterns]
            for pattern in include + varlog["exclude"]:
                assert pattern.startswith("/var/log/"), pattern
            config = {"data_dir": str(data),
                      "sources": {"varlog": dict(varlog, include=moved(include),
                                                 exclude=moved(varlog["exclude"]),
                                                 read_from="beginning")},
                      "sinks": {"out": {"type": "console", "inputs": ["varlog"], "target": "stdout",
                                        "encoding": {"codec": "json"}}}}
            path = Path(name) / "varlog.json"
            path.write_text(json.dumps(config))
            with (Path(name) / "vector.out").open("w+") as out, \
                    (Path(name) / "vector.log").open("w+") as log:
                process = subprocess.Popen([VECTOR, "--config", str(path)], env=ENV,
                                           stdout=out, stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while True:
                        out.seek(0)
                        files = sorted(str(Path(json.loads(line)["file"]).relative_to(root))
                                       for line in out.read().splitlines() if line.strip())
                        if len(files) >= len(wanted) or time.monotonic() > deadline:
                            break
                        assert process.poll() is None
                        time.sleep(0.1)
                    # A second scan of the paths, which logs any glob error again.
                    time.sleep(1.5)
                finally:
                    process.send_signal(signal.SIGTERM)
                    process.communicate(timeout=15)
                log.seek(0)
                return files, log.read()

        try:
            files, log = collect(varlog["include"])
            old_files, old_log = collect(["/var/log/**/*.log"])
        finally:
            (root / "private").chmod(0o755)
        assert "Failed to glob path" not in log, log
        assert files == sorted(wanted), files
        assert "Failed to glob path" in old_log, "the fixture no longer reproduces the glob error"
        assert old_files == files, (old_files, files)
        print(f"PASS varlog catch-all skips /var/log/private: {len(files)} files collected, "
              "no glob error; a single ** include logs one for the same tree")


if __name__ == "__main__":
    version = subprocess.check_output([VECTOR, "--version"], text=True).strip()
    assert version.startswith("vector 0.57.0 "), version
    print(version, flush=True)
    subprocess.run([VECTOR, "validate", "--no-environment", "--config-dir", str(CONFIG)],
                   env=ENV, check=True)
    FIXTURES.mkdir(parents=True, exist_ok=True)
    for tcp in (False, True):
        for suppress in (False, True):
            run(tcp, suppress)
    run(True, False, "trusted")
    run(False, False, "untrusted")
    varlog_glob()
    capture_recipe()
