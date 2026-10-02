#!/usr/bin/env python3
"""Run the Debian collector with Vector 0.57.0 and disposable loopback inputs.

Also run the Vector guide's sample capture recipe, as written, and prove that it
captures the events the `trawld` sink posts.

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
LIVE_CONFIG_DIR = "/etc/vector/vector.d"
LIVE_ENV_FILE = "/etc/default/vector"
# The most events one ingest preview reads (trawl_api::ingest_preview).
MAX_PREVIEW_EVENTS = 500


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
    env = dict(ENV)
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


def capture_block():
    """The capture recipe exactly as the guide prints it, list indent removed."""
    lines = GUIDE.read_text().splitlines()
    starts = [i for i, line in enumerate(lines) if line.strip() == CAPTURE_MARKER]
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


def fixture_collector(directory, live_data, receiver_url):
    """Copy the Debian collector configuration as a host would install it.

    Each file keeps its text, sink tables included. Only the source tables are
    replaced, by filters that take the stdin fixture stream, so Vector ends at
    the end of input. `data_dir` names the fixture's service directory.
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
        for name in sorted(removed):
            text += (f"\n[transforms.{name}]\ntype = \"filter\"\ninputs = [\"fixture\"]\n"
                     f"condition = {json.dumps(f'.fixture_source == {json.dumps(name)}')}\n")
        (live / path.name).write_text(text)
    env_file = directory / "default-vector"
    env_file.write_text(
        f"VECTOR_CONFIG_DIR={live}\n"
        "VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION=true\n"
        f"TRAWL_URL={receiver_url}\n"
        # Not the shipped default, so a capture that misses the service's
        # environment sends a different env.
        "TRAWL_ENV=lab\n"
        "TRAWL_INGEST_TOKEN=fixture-token\n")
    return live, env_file


def clean_env(directory):
    """An environment without inherited Vector or Trawl settings."""
    shims = directory / "bin"
    shims.mkdir(exist_ok=True)
    vector = shutil.which(VECTOR)
    assert vector, VECTOR
    (shims / "vector").unlink(missing_ok=True)
    (shims / "vector").symlink_to(vector)
    return {"PATH": f"{shims}:{os.environ['PATH']}", "HOME": str(directory),
            "TMPDIR": str(directory), "LC_ALL": "C.UTF-8"}


def run_capture(directory, live, env_file, inputs):
    """Run the guide's capture block against the fixture collector.

    The block's host paths point at the fixture copies, and `sudo` runs its
    command as this user. Nothing else in the block changes. Vector reads the
    fixture stream on stdin, which it inherits through the block.
    """
    block = capture_block()
    for path in (LIVE_CONFIG_DIR, LIVE_ENV_FILE):
        assert path in block, f"the capture block no longer reads {path}"
    script = ("set -eu\nsudo() { \"$@\"; }\n"
              + block.replace(LIVE_CONFIG_DIR, str(live)).replace(LIVE_ENV_FILE, str(env_file)))
    work = directory / "operator"
    work.mkdir()
    with inputs.open("rb") as stdin:
        result = subprocess.run(["bash", "-c", script], cwd=work, env=clean_env(directory),
                                stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=90)
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    assert result.stdout == b"", "the capture leaked events to the terminal"
    # The configuration the capture ran: the service's files less the sink
    # and the syslog drop-in, with a console sink and its own data_dir.
    [capture] = directory.glob("tmp.*")
    files = {path.name: tomllib.loads(path.read_text())
             for path in (capture / "config").glob("*.toml")}
    assert set(files) == {path.name for path in live.glob("*.toml")} - {"unifi-syslog.toml"}, files
    assert [f["data_dir"] for f in files.values() if "data_dir" in f] == [str(capture / "data")]
    sinks = {name: sink for f in files.values() for name, sink in f.get("sinks", {}).items()}
    assert sinks == {"capture": {"type": "console", "inputs": ["trawl_*"], "target": "stdout",
                                 "encoding": {"codec": "json"}}}, sinks
    return (work / "capture.ndjson").read_bytes(), result.stderr.decode(errors="replace")


def preview_input(raw):
    """Assert the capture is a body the preview reads, and return its events."""
    assert raw[:2] != b"\x1f\x8b", "the capture is gzip"
    text = raw.decode("utf-8")
    assert text.endswith("\n"), "the capture does not end with a newline"
    lines = text[:-1].split("\n")
    assert 0 < len(lines) <= MAX_PREVIEW_EVENTS, len(lines)
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

            raw, log = run_capture(directory, live, env_file, inputs)
            assert "ERROR" not in log, log
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
            raw, _log = run_capture(directory, live, env_file, inputs)
            assert not requests, "the capture posted to trawld"
            assert not any(live_data.iterdir()), "the capture wrote the service's data_dir"
            captured = preview_input(raw)
            ids = [event["fixture_id"] for event in captured]
            assert len(ids) == MAX_PREVIEW_EVENTS == len(set(ids)), len(ids)
            assert set(ids) <= {event["fixture_id"] for event in many}
            print(f"PASS capture recipe cut: {len(many)} inputs, {len(captured)} captured lines")
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


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
    capture_recipe()
