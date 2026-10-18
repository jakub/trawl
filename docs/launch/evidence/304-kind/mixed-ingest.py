"""Steady ingest for the #304 mixed run (AC19).

    python3 -I mixed-ingest.py ADDR CACERT AUTH_HEADER_FILE RATE SECS SEED SERVICE [INT_FIELD]

Posts RATE events a second to POST /api/v1/ingest for SECS seconds, as one
NDJSON batch every 2 s, with _time set to the moment the batch is built.
Every event is {"_time", "service": SERVICE, "host", "status", "message"},
where message is 200 characters of base64 over seeded random bytes and status
cycles through a small set of HTTP codes. INT_FIELD, when given, adds that
field with a small random integer, the field the run repins.

ADDR is HOST:PORT of trawld's NodePort. The TLS name checked against trawld's
pinned certificate stays "localhost", which its SAN carries. A 503
hot_buffer_full is retried after its Retry-After; any other status is logged
and the batch is dropped. Each POST prints one line: the time, the HTTP
status, the events accepted and the latency. The last line counts the events
sent and accepted and the batches dropped. The script exits 1 when it sent
nothing, dropped a batch, or had fewer events accepted than it sent. The
bearer header (a "Name: value" line) is read from a file and never printed.
"""

import base64
import datetime as dt
import http.client
import json
import random
import socket
import ssl
import sys
import time

addr, cacert, header_file, rate, secs, seed, service = sys.argv[1:8]
int_field = sys.argv[8] if len(sys.argv) > 8 else None
host, port = addr.rsplit(":", 1)
rate, secs, seed, port = int(rate), float(secs), int(seed), int(port)
with open(header_file, encoding="utf-8") as f:
    header_name, header_value = (part.strip() for part in f.read().split(":", 1))
ctx = ssl.create_default_context(cafile=cacert)
rng = random.Random(seed)
STATUSES = [200, 200, 200, 200, 201, 204, 301, 304, 404, 500, 503]
PERIOD = 2.0


class Conn(http.client.HTTPSConnection):
    """Connect to ADDR, but verify the certificate as "localhost"."""

    def connect(self):
        sock = socket.create_connection((host, port), self.timeout)
        self.sock = ctx.wrap_socket(sock, server_hostname="localhost")


def ts():
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def post(body):
    """POST one batch; returns (status, accepted)."""
    while True:
        t0 = time.monotonic()
        conn = Conn("localhost", port, timeout=60)
        try:
            conn.request("POST", "/api/v1/ingest", body=body, headers={
                header_name: header_value, "Content-Type": "application/x-ndjson"})
            resp = conn.getresponse()
            data = resp.read()
            ms = (time.monotonic() - t0) * 1000
            if resp.status == 503 and resp.getheader("Retry-After"):
                print(f"{ts()} 503 retry hot_buffer_full {ms:.0f}ms", flush=True)
                time.sleep(float(resp.getheader("Retry-After")))
                continue
            if resp.status != 200:
                print(f"{ts()} {resp.status} dropped {ms:.0f}ms {data[:200]!r}", flush=True)
                return resp.status, 0
            accepted = json.loads(data)["accepted"]
            print(f"{ts()} 200 {accepted} {ms:.0f}ms", flush=True)
            return 200, accepted
        except (OSError, http.client.HTTPException) as e:
            print(f"{ts()} 000 dropped {type(e).__name__}: {e}", flush=True)
            return 0, 0
        finally:
            conn.close()


per_batch = int(rate * PERIOD)
raw_bytes = 150
sent = accepted = dropped = seq = 0
began = time.monotonic()
next_at = began
while time.monotonic() - began < secs:
    now = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    lines = []
    for _ in range(per_batch):
        ev = {"_time": now, "service": service, "host": f"gen-{seq % 16}",
              "status": STATUSES[seq % len(STATUSES)],
              "message": base64.b64encode(rng.randbytes(raw_bytes)).decode()[:200]}
        if int_field:
            ev[int_field] = rng.randrange(1000)
        lines.append(json.dumps(ev, separators=(",", ":")))
        seq += 1
    sent += per_batch
    status, n = post(("\n".join(lines) + "\n").encode())
    accepted += n
    dropped += status != 200
    next_at += PERIOD
    time.sleep(max(0.0, next_at - time.monotonic()))
print(f"{ts()} done sent={sent} accepted={accepted} dropped={dropped} secs={time.monotonic() - began:.0f}", flush=True)
sys.exit(0 if sent > 0 and dropped == 0 and accepted == sent else 1)
