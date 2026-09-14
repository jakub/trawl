#!/usr/bin/env python3
"""Run the Debian collector with Vector 0.57.0 and disposable loopback inputs.

Requires Python 3.11+ and VECTOR_BIN (or vector on PATH). No host journal,
application files, Docker socket, credentials, or running collector are used.
"""

import collections
import gzip
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import tomllib


ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "config/vector/debian"
VECTOR = os.environ.get("VECTOR_BIN", "vector")
ENV = dict(os.environ, VECTOR_DANGEROUSLY_ALLOW_ENV_VAR_INTERPOLATION="true",
           TRAWL_URL="http://127.0.0.1:1", TRAWL_INGEST_TOKEN="fixture-token",
           TRAWL_ENV="prod")
# Do not inherit another installation's configuration selection.
ENV.pop("VECTOR_CONFIG", None)
ENV.pop("VECTOR_CONFIG_DIR", None)
ENV.pop("VAR", None)


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


def fixtures():
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
    add("journald", "journal-rejected", None, _SYSTEMD_UNIT="serial-getty-ttyS0.service")
    add("journald", "serial-getty restart", None, SYSLOG_IDENTIFIER="init")
    for service in ("networkd-dispatcher", "NetworkManager", "systemd-networkd"):
        add("journald", f"veth churn {service}", None, SYSLOG_IDENTIFIER=service)
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
    return events, expected


def run(tcp):
    config = load(tcp)
    # A final output must never also feed another transform.
    for name, transform in config["transforms"].items():
        assert not any(i.startswith("trawl_") for i in transform["inputs"]), name
    events, expected = fixtures()
    received = []
    failures = []

    class Capture(http.server.BaseHTTPRequestHandler):
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

    with tempfile.TemporaryDirectory(prefix="trawl-vector-") as directory:
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Capture)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        process = None
        reservations = []
        try:
            config["data_dir"] = directory
            sink = config["sinks"]["trawld"]
            sink["uri"] = f"http://127.0.0.1:{server.server_port}/api/v1/ingest"
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
                process = subprocess.Popen([VECTOR, "--config", str(path)], env=ENV,
                                           stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
                deadline = time.monotonic() + 20
                while True:
                    log.seek(0)
                    output = log.read()
                    if "Vector has started" in output:
                        break
                    assert process.poll() is None, output
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
                    if name == "ufw":
                        assert event["protocol"] == "tcp" and event["dst_port"] == 443, event
                    if name == "docker-swarm":
                        assert event["host"] == "stack", event
                    if name.startswith("unifi-"):
                        assert event["host"] == "ap-fixture", event
                        assert event["syslog_source_ip"] == "127.0.0.1", event
                print(f"PASS {'UDP + TCP' if tcp else 'UDP only'}: {len(events)} synthetic inputs, "
                      f"{len(ports)} syslog inputs, {len(received)} HTTP events, zero duplicates; five journal events filtered")
        finally:
            if process and process.poll() is None:
                process.kill()
                process.communicate()
            for reservation in reservations:
                reservation.close()
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    version = subprocess.check_output([VECTOR, "--version"], text=True).strip()
    assert version.startswith("vector 0.57.0 "), version
    print(version, flush=True)
    subprocess.run([VECTOR, "validate", "--no-environment", "--config-dir", str(CONFIG)],
                   env=ENV, check=True)
    run(False)
    run(True)
